//! Accounts and sessions.
//!
//! - Passwords are hashed with Argon2id (PHC string in `users.password_hash`).
//! - `register` / `login` return an opaque random bearer token (one session per device).
//!   Only its SHA-256 is stored, so a leaked database does not leak usable tokens.
//! - Sessions expire after 30 days of inactivity and can be revoked individually.
//! - Failed logins are rate limited per client IP and per email; registrations per IP.
//! - The iCalendar feed uses a separate, rotatable read-only `feed_token` per user. Like session
//!   tokens it is stored hashed, so it is only returned once, at registration or rotation.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use argon2::{
    Argon2,
    password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash},
};
use axum::{
    Extension, Json, Router,
    extract::{ConnectInfo, Path, Request, State},
    http::{StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

use crate::{
    AppState, Error, User,
    db::{self, Conn, n, query, s},
};

const SESSION_TTL_SECS: i64 = 30 * 24 * 3600;
/// `last_used` / expiry are only refreshed when older than this, to avoid a write per request.
const TOUCH_AFTER_SECS: i64 = 3600;
const MAX_SESSIONS_PER_USER: i64 = 20;

const LOGIN_WINDOW: Duration = Duration::from_secs(15 * 60);
const LOGIN_MAX_PER_IP: u32 = 30;
const LOGIN_MAX_PER_USER: u32 = 8;
const REGISTER_WINDOW: Duration = Duration::from_secs(3600);
const REGISTER_MAX_PER_IP: u32 = 10;

// ---------------------------------------------------------------------------------------
// State & configuration
// ---------------------------------------------------------------------------------------

pub struct Config {
    registration_open: bool,
    /// SHA-256 of the invite code, if one is required.
    invite_hash: Option<[u8; 32]>,
    trust_proxy: bool,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let registration_open = match std::env::var("KAIRO_REGISTRATION").as_deref() {
            Err(_) | Ok("open") => true,
            Ok("closed") => false,
            Ok(other) => return Err(format!("KAIRO_REGISTRATION must be open or closed, got {other:?}")),
        };
        let invite_hash = std::env::var("KAIRO_INVITE_CODE")
            .ok()
            .filter(|code| !code.is_empty())
            .map(|code| Sha256::digest(code.as_bytes()).into());
        let trust_proxy = std::env::var("KAIRO_TRUST_PROXY").is_ok_and(|v| v == "1");
        Ok(Config { registration_open, invite_hash, trust_proxy })
    }

    #[cfg(test)]
    pub fn test(registration_open: bool, invite_code: Option<&str>) -> Self {
        Config {
            registration_open,
            invite_hash: invite_code.map(|c| Sha256::digest(c.as_bytes()).into()),
            trust_proxy: false,
        }
    }
}

pub struct AuthState {
    config: Config,
    limiter: Limiter,
    /// Argon2 uses ~19 MiB per hash; bound how many run at once.
    hash_slots: Semaphore,
}

impl AuthState {
    pub fn new(config: Config) -> Self {
        AuthState { config, limiter: Limiter::default(), hash_slots: Semaphore::new(4) }
    }
}

const FEED_MISS_WINDOW: Duration = Duration::from_secs(15 * 60);
const FEED_MISS_MAX_PER_IP: u32 = 30;

impl AuthState {
    /// Errors if this IP guessed wrong feed tokens too often.
    pub fn feed_check(&self, ip: &str) -> Result<(), Error> {
        self.limiter.check(&format!("feed:{ip}"), FEED_MISS_MAX_PER_IP)
    }

    pub fn feed_miss(&self, ip: &str) {
        self.limiter.hit(&format!("feed:{ip}"), FEED_MISS_WINDOW);
    }
}

pub async fn init_db(conn: &mut Conn) -> db::Result<()> {
    db::create_table(
        conn,
        "users",
        "email VARCHAR(255) PRIMARY KEY, password_hash VARCHAR(512) NOT NULL,
         feed_token VARCHAR(255) NOT NULL UNIQUE, created_at BIGINT NOT NULL",
    )
    .await?;
    db::create_table(
        conn,
        "sessions",
        "id VARCHAR(255) PRIMARY KEY, token_hash VARCHAR(255) NOT NULL UNIQUE, email VARCHAR(255) NOT NULL,
         device VARCHAR(255) NOT NULL, created_at BIGINT NOT NULL, last_used BIGINT NOT NULL,
         expires_at BIGINT NOT NULL",
    )
    .await?;
    db::create_index(conn, "sessions_email", "sessions", "email").await
}

// ---------------------------------------------------------------------------------------
// Rate limiting (in memory, fixed window)
// ---------------------------------------------------------------------------------------

#[derive(Default)]
struct Limiter {
    hits: Mutex<HashMap<String, (u32, Instant)>>,
}

impl Limiter {
    /// Errors with the seconds to wait if `key` already used up `max` hits in its window.
    fn check(&self, key: &str, max: u32) -> Result<(), Error> {
        let hits = self.hits.lock().unwrap();
        match hits.get(key) {
            Some((count, expires)) if *count >= max && *expires > Instant::now() => {
                Err(Error::TooManyRequests((*expires - Instant::now()).as_secs() + 1))
            }
            _ => Ok(()),
        }
    }

    fn hit(&self, key: &str, window: Duration) {
        let mut hits = self.hits.lock().unwrap();
        let now = Instant::now();
        if hits.len() > 10_000 {
            hits.retain(|_, (_, expires)| *expires > now);
        }
        let entry = hits.entry(key.to_string()).or_insert((0, now + window));
        if entry.1 <= now {
            *entry = (0, now + window);
        }
        entry.0 += 1;
    }

    fn clear(&self, key: &str) {
        self.hits.lock().unwrap().remove(key);
    }
}

// ---------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------

#[derive(Clone)]
pub struct ClientIp(pub String);

#[derive(Clone)]
pub struct SessionId(pub String);

pub async fn session_valid(conn: &mut Conn, id: &str) -> bool {
    query("SELECT id FROM sessions WHERE id = ? AND expires_at > ?")
        .bind(id)
        .bind(now())
        .fetch_optional(conn)
        .await
        .ok()
        .flatten()
        .is_some()
}

fn now() -> i64 {
    Utc::now().timestamp()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn random_hex(len: usize) -> String {
    let mut bytes = vec![0u8; len];
    rand::fill(&mut bytes[..]);
    hex(&bytes)
}

fn hash_token(token: &str) -> String {
    hex(&Sha256::digest(token.as_bytes()))
}

fn hash_password(password: &str) -> Result<String, Error> {
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|h| h.to_string())
        .map_err(|e| Error::Internal(format!("hashing failed: {e}")))
}

fn verify_password(password: &str, stored: &str) -> bool {
    match stored.parse::<PasswordHash>() {
        Ok(parsed) => Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok(),
        Err(_) => false,
    }
}

/// A valid hash of a throwaway password, verified against when the user does not exist so
/// that unknown and known emails take equally long.
fn dummy_hash() -> &'static str {
    static HASH: OnceLock<String> = OnceLock::new();
    HASH.get_or_init(|| hash_password("kairo-dummy-password").unwrap_or_else(|_| panic!("argon2 works")))
}

async fn hash_async(state: &AppState, password: String) -> Result<String, Error> {
    let _slot = state.auth.hash_slots.acquire().await.map_err(|e| Error::Internal(e.to_string()))?;
    tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .map_err(|e| Error::Internal(e.to_string()))?
}

async fn verify_async(state: &AppState, password: String, stored: Option<String>) -> Result<bool, Error> {
    let _slot = state.auth.hash_slots.acquire().await.map_err(|e| Error::Internal(e.to_string()))?;
    tokio::task::spawn_blocking(move || match stored {
        Some(stored) => verify_password(&password, &stored),
        None => {
            verify_password(&password, dummy_hash());
            false
        }
    })
    .await
    .map_err(|e| Error::Internal(e.to_string()))
}

/// Accounts are identified by their email address (stored lowercase in the `email` columns).
/// Deliberately conservative: the address also travels unencoded in URL paths.
fn normalize_email(raw: &str) -> Result<String, Error> {
    let email = raw.trim().to_lowercase();
    let invalid = || Error::Invalid("please enter a valid email address".into());
    let (local, domain) = email.split_once('@').ok_or_else(invalid)?;
    let alnum = |s: &str| s.chars().next().zip(s.chars().last()).is_some_and(|(a, b)| a.is_ascii_alphanumeric() && b.is_ascii_alphanumeric());
    let local_ok = !local.is_empty()
        && local.len() <= 64
        && alnum(local)
        && local.chars().all(|c| c.is_ascii_alphanumeric() || "._+-".contains(c));
    let domain_ok = domain.contains('.')
        && alnum(domain)
        && !domain.contains("..")
        && domain.chars().all(|c| c.is_ascii_alphanumeric() || ".-".contains(c));
    if email.len() > 254 || !local_ok || !domain_ok {
        return Err(invalid());
    }
    Ok(email)
}

fn validate_password(password: &str, email: &str) -> Result<(), Error> {
    let len = password.chars().count();
    if !(10..=128).contains(&len) {
        return Err(Error::Invalid("password must be 10-128 characters".into()));
    }
    if password.to_lowercase() == email {
        return Err(Error::Invalid("password must not equal the email address".into()));
    }
    Ok(())
}

fn short_device(device: Option<String>) -> String {
    let device = device.unwrap_or_default();
    let device = device.trim();
    if device.is_empty() { "unknown device".into() } else { device.chars().take(64).collect() }
}

/// Creates a session and returns its bearer token (shown to the client exactly once).
async fn create_session(conn: &mut Conn, email: &str, device: &str) -> db::Result<String> {
    let token = random_hex(32);
    let t = now();
    query("DELETE FROM sessions WHERE expires_at < ?").bind(t).execute(&mut *conn).await?;
    query(
        "INSERT INTO sessions (id, token_hash, email, device, created_at, last_used, expires_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(random_hex(8))
    .bind(hash_token(&token))
    .bind(email)
    .bind(device)
    .bind(t)
    .bind(t)
    .bind(t + SESSION_TTL_SECS)
    .execute(&mut *conn)
    .await?;
    // Keep only the newest sessions so abandoned devices cannot pile up.
    let ids = query("SELECT id FROM sessions WHERE email = ? ORDER BY last_used DESC, created_at DESC")
        .bind(email)
        .fetch_all(&mut *conn)
        .await?;
    for row in ids.iter().skip(MAX_SESSIONS_PER_USER as usize) {
        query("DELETE FROM sessions WHERE id = ?").bind(s(row, 0)?).execute(&mut *conn).await?;
    }
    Ok(token)
}

pub async fn user_for_feed_token(conn: &mut Conn, token: &str) -> db::Result<Option<String>> {
    query("SELECT email FROM users WHERE feed_token = ?")
        .bind(hash_token(token))
        .fetch_optional(conn)
        .await?
        .map(|row| s(&row, 0))
        .transpose()
}

/// `feed_token` is only known in plain text when it was just created (registration).
fn session_response(status: StatusCode, token: &str, email: &str, feed_token: Option<&str>) -> Response {
    (status, Json(json!({ "token": token, "email": email, "feed_token": feed_token }))).into_response()
}

// ---------------------------------------------------------------------------------------
// Middleware
// ---------------------------------------------------------------------------------------

pub async fn client_ip(State(state): State<AppState>, mut req: Request, next: middleware::Next) -> Response {
    let from_proxy = state
        .auth
        .config
        .trust_proxy
        .then(|| {
            req.headers()
                .get("x-forwarded-for")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.rsplit(',').next())
                .map(|ip| ip.trim().to_string())
        })
        .flatten();
    let ip = from_proxy
        .or_else(|| req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip().to_string()))
        .unwrap_or_else(|| "unknown".into());
    req.extensions_mut().insert(ClientIp(ip));
    next.run(req).await
}

pub async fn require_session(State(state): State<AppState>, mut req: Request, next: middleware::Next) -> Result<Response, Error> {
    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(Error::Unauthorized("missing bearer token"))?;
    let token_hash = hash_token(token);
    let t = now();
    // The connection goes back to the pool before the request runs.
    let (id, email) = {
        let mut conn = state.db.conn().await?;
        let row = query("SELECT id, email, expires_at, last_used FROM sessions WHERE token_hash = ?")
            .bind(&token_hash)
            .fetch_optional(&mut *conn)
            .await?;
        let found = row.map(|r| Ok::<_, sqlx::Error>((s(&r, 0)?, s(&r, 1)?, n(&r, 2)?, n(&r, 3)?))).transpose()?;
        let Some((id, email, _, last_used)) = found.filter(|row| row.2 > t) else {
            return Err(Error::Unauthorized("invalid or expired token"));
        };
        if t - last_used > TOUCH_AFTER_SECS {
            query("UPDATE sessions SET last_used = ?, expires_at = ? WHERE id = ?")
                .bind(t)
                .bind(t + SESSION_TTL_SECS)
                .bind(&id)
                .execute(&mut *conn)
                .await?;
        }
        (id, email)
    };
    req.extensions_mut().insert(User(email));
    req.extensions_mut().insert(SessionId(id));
    Ok(next.run(req).await)
}

// ---------------------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------------------

pub fn public_routes() -> Router<AppState> {
    Router::new().route("/auth/register", post(register)).route("/auth/login", post(login))
}

pub fn protected_routes() -> Router<AppState> {
    Router::new()
        .route("/auth/me", get(me))
        .route("/auth/logout", post(logout))
        .route("/auth/sessions", get(list_sessions))
        .route("/auth/sessions/{id}", delete(revoke_session))
        .route("/auth/password", post(change_password))
        .route("/auth/feed-token", post(rotate_feed_token))
        .route("/auth/account", delete(delete_account))
}

#[derive(Deserialize)]
struct Register {
    email: String,
    password: String,
    #[serde(default)]
    device: Option<String>,
    #[serde(default)]
    invite_code: Option<String>,
}

async fn register(
    State(state): State<AppState>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    Json(body): Json<Register>,
) -> Result<Response, Error> {
    let auth = &state.auth;
    if !auth.config.registration_open {
        return Err(Error::Forbidden("registration is closed"));
    }
    let ip_key = format!("register:{ip}");
    auth.limiter.check(&ip_key, REGISTER_MAX_PER_IP)?;
    auth.limiter.hit(&ip_key, REGISTER_WINDOW);

    if let Some(expected) = auth.config.invite_hash {
        let given: [u8; 32] = Sha256::digest(body.invite_code.unwrap_or_default().as_bytes()).into();
        if given != expected {
            return Err(Error::Forbidden("invalid invite code"));
        }
    }
    let email = normalize_email(&body.email)?;
    validate_password(&body.password, &email)?;

    let password_hash = hash_async(&state, body.password).await?;
    let feed_token = random_hex(24);
    let mut conn = state.db.conn().await?;
    let inserted = query("INSERT INTO users (email, password_hash, feed_token, created_at) VALUES (?, ?, ?, ?)")
        .bind(&email)
        .bind(password_hash)
        .bind(hash_token(&feed_token))
        .bind(now())
        .execute(&mut *conn)
        .await;
    if let Err(err) = inserted {
        // Whatever the driver calls a duplicate key, the account existing is what matters.
        let taken = query("SELECT email FROM users WHERE email = ?")
            .bind(&email)
            .fetch_optional(&mut *conn)
            .await?
            .is_some();
        return Err(if taken { Error::Taken } else { err.into() });
    }
    let token = create_session(&mut conn, &email, &short_device(body.device)).await?;
    Ok(session_response(StatusCode::CREATED, &token, &email, Some(&feed_token)))
}

#[derive(Deserialize)]
struct Login {
    email: String,
    password: String,
    #[serde(default)]
    device: Option<String>,
}

async fn login(
    State(state): State<AppState>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    Json(body): Json<Login>,
) -> Result<Response, Error> {
    let limiter = &state.auth.limiter;
    let email = body.email.trim().to_lowercase();
    let ip_key = format!("login-ip:{ip}");
    let user_key = format!("login-user:{email}");
    limiter.check(&ip_key, LOGIN_MAX_PER_IP)?;
    limiter.check(&user_key, LOGIN_MAX_PER_USER)?;

    let stored = {
        let mut conn = state.db.conn().await?;
        let row = query("SELECT password_hash FROM users WHERE email = ?")
            .bind(&email)
            .fetch_optional(&mut *conn)
            .await?;
        row.map(|r| s(&r, 0)).transpose()?
    };
    let ok = verify_async(&state, body.password, stored).await?;
    if !ok {
        limiter.hit(&ip_key, LOGIN_WINDOW);
        limiter.hit(&user_key, LOGIN_WINDOW);
        return Err(Error::Unauthorized("invalid email or password"));
    }
    limiter.clear(&user_key);
    let mut conn = state.db.conn().await?;
    let token = create_session(&mut conn, &email, &short_device(body.device)).await?;
    Ok(session_response(StatusCode::OK, &token, &email, None))
}

async fn me(State(state): State<AppState>, Extension(User(email)): Extension<User>) -> Result<Response, Error> {
    let mut conn = state.db.conn().await?;
    let row = query("SELECT created_at FROM users WHERE email = ?").bind(&email).fetch_one(&mut *conn).await?;
    Ok(Json(json!({ "email": email, "created_at": n(&row, 0)? })).into_response())
}

async fn logout(
    State(state): State<AppState>,
    Extension(SessionId(id)): Extension<SessionId>,
) -> Result<StatusCode, Error> {
    query("DELETE FROM sessions WHERE id = ?").bind(id).execute(&mut *state.db.conn().await?).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_sessions(
    State(state): State<AppState>,
    Extension(User(email)): Extension<User>,
    Extension(SessionId(current)): Extension<SessionId>,
) -> Result<Response, Error> {
    let rows = query(
        "SELECT id, device, created_at, last_used FROM sessions
         WHERE email = ? AND expires_at > ? ORDER BY last_used DESC",
    )
    .bind(email)
    .bind(now())
    .fetch_all(&mut *state.db.conn().await?)
    .await?;
    let sessions = rows
        .iter()
        .map(|r| {
            let id = s(r, 0)?;
            Ok::<_, sqlx::Error>(json!({
                "current": id == current, "id": id, "device": s(r, 1)?,
                "created_at": n(r, 2)?, "last_used": n(r, 3)?,
            }))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(sessions).into_response())
}

async fn revoke_session(
    State(state): State<AppState>,
    Extension(User(email)): Extension<User>,
    Path(id): Path<String>,
) -> Result<StatusCode, Error> {
    let done = query("DELETE FROM sessions WHERE id = ? AND email = ?")
        .bind(id)
        .bind(email)
        .execute(&mut *state.db.conn().await?)
        .await?;
    if done.rows_affected() == 0 { Err(Error::NotFound("session")) } else { Ok(StatusCode::NO_CONTENT) }
}

/// Re-checks the current password for sensitive actions, sharing the login lockout.
async fn confirm_password(state: &AppState, email: &str, password: String) -> Result<(), Error> {
    let key = format!("login-user:{email}");
    state.auth.limiter.check(&key, LOGIN_MAX_PER_USER)?;
    let stored = {
        let mut conn = state.db.conn().await?;
        let row = query("SELECT password_hash FROM users WHERE email = ?")
            .bind(email)
            .fetch_one(&mut *conn)
            .await?;
        s(&row, 0)?
    };
    if verify_async(state, password, Some(stored)).await? {
        state.auth.limiter.clear(&key);
        Ok(())
    } else {
        state.auth.limiter.hit(&key, LOGIN_WINDOW);
        Err(Error::Unauthorized("wrong password"))
    }
}

#[derive(Deserialize)]
struct ChangePassword {
    current_password: String,
    new_password: String,
}

/// Changes the password and signs out every other device.
async fn change_password(
    State(state): State<AppState>,
    Extension(User(email)): Extension<User>,
    Extension(SessionId(current)): Extension<SessionId>,
    Json(body): Json<ChangePassword>,
) -> Result<StatusCode, Error> {
    validate_password(&body.new_password, &email)?;
    confirm_password(&state, &email, body.current_password).await?;
    let new_hash = hash_async(&state, body.new_password).await?;
    let mut tx = state.db.begin().await?;
    query("UPDATE users SET password_hash = ? WHERE email = ?").bind(new_hash).bind(&email).execute(&mut *tx).await?;
    query("DELETE FROM sessions WHERE email = ? AND id != ?").bind(&email).bind(current).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Invalidates the old calendar feed URL and returns a fresh token.
async fn rotate_feed_token(
    State(state): State<AppState>,
    Extension(User(email)): Extension<User>,
) -> Result<Response, Error> {
    let feed_token = random_hex(24);
    query("UPDATE users SET feed_token = ? WHERE email = ?")
        .bind(hash_token(&feed_token))
        .bind(email)
        .execute(&mut *state.db.conn().await?)
        .await?;
    Ok(Json(json!({ "feed_token": feed_token })).into_response())
}

#[derive(Deserialize)]
struct DeleteAccount {
    password: String,
}

/// Permanently removes the account with all calendars, events and sessions.
async fn delete_account(
    State(state): State<AppState>,
    Extension(User(email)): Extension<User>,
    Json(body): Json<DeleteAccount>,
) -> Result<StatusCode, Error> {
    confirm_password(&state, &email, body.password).await?;
    let mut tx = state.db.begin().await?;
    query(
        "DELETE FROM calendar_members WHERE email = ?
         OR calendar_id IN (SELECT id FROM calendars WHERE owner = ?)",
    )
    .bind(&email)
    .bind(&email)
    .execute(&mut *tx)
    .await?;
    for table in ["events", "calendars"] {
        query(&format!("DELETE FROM {table} WHERE owner = ?")).bind(&email).execute(&mut *tx).await?;
    }
    query("DELETE FROM sessions WHERE email = ?").bind(&email).execute(&mut *tx).await?;
    query("DELETE FROM users WHERE email = ?").bind(&email).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
