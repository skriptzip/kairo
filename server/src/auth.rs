//! Accounts and sessions.
//!
//! - Passwords are hashed with Argon2id (PHC string in `users.password_hash`).
//! - `register` / `login` return an opaque random bearer token (one session per device).
//!   Only its SHA-256 is stored, so a leaked database does not leak usable tokens.
//! - Sessions expire after 30 days of inactivity and can be revoked individually.
//! - Failed logins are rate limited per client IP and per username; registrations per IP.
//! - The iCalendar feed uses a separate, rotatable read-only `feed_token` per user.

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
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

use crate::{AppState, Error, User};

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

pub fn init_db(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS users (
             username TEXT PRIMARY KEY, password_hash TEXT NOT NULL,
             feed_token TEXT NOT NULL UNIQUE, created_at INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS sessions (
             id TEXT PRIMARY KEY, token_hash TEXT NOT NULL UNIQUE, username TEXT NOT NULL,
             device TEXT NOT NULL, created_at INTEGER NOT NULL, last_used INTEGER NOT NULL,
             expires_at INTEGER NOT NULL);
         CREATE INDEX IF NOT EXISTS sessions_username ON sessions (username);",
    )
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
pub struct ClientIp(String);

#[derive(Clone)]
pub struct SessionId(pub String);

pub fn session_valid(db: &Connection, id: &str) -> bool {
    db.query_row("SELECT 1 FROM sessions WHERE id = ?1 AND expires_at > ?2", params![id, now()], |_| Ok(()))
        .optional()
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
/// that unknown and known usernames take equally long.
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

fn normalize_username(raw: &str) -> Result<String, Error> {
    let name = raw.trim().to_lowercase();
    let valid_chars = name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_.-".contains(c));
    let starts_ok = name.chars().next().is_some_and(|c| c.is_ascii_alphanumeric());
    if !(3..=32).contains(&name.len()) || !valid_chars || !starts_ok {
        return Err(Error::Invalid(
            "username must be 3-32 characters (a-z, 0-9, _ . -) and start with a letter or digit".into(),
        ));
    }
    Ok(name)
}

fn validate_password(password: &str, username: &str) -> Result<(), Error> {
    let len = password.chars().count();
    if !(10..=128).contains(&len) {
        return Err(Error::Invalid("password must be 10-128 characters".into()));
    }
    if password.to_lowercase() == username {
        return Err(Error::Invalid("password must not equal the username".into()));
    }
    Ok(())
}

fn short_device(device: Option<String>) -> String {
    let device = device.unwrap_or_default();
    let device = device.trim();
    if device.is_empty() { "unknown device".into() } else { device.chars().take(64).collect() }
}

/// Creates a session and returns its bearer token (shown to the client exactly once).
fn create_session(db: &Connection, username: &str, device: &str) -> rusqlite::Result<String> {
    let token = random_hex(32);
    let t = now();
    db.execute("DELETE FROM sessions WHERE expires_at < ?1", [t])?;
    db.execute(
        "INSERT INTO sessions (id, token_hash, username, device, created_at, last_used, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)",
        params![random_hex(8), hash_token(&token), username, device, t, t + SESSION_TTL_SECS],
    )?;
    // Keep only the newest sessions so abandoned devices cannot pile up.
    db.execute(
        "DELETE FROM sessions WHERE username = ?1 AND id NOT IN
           (SELECT id FROM sessions WHERE username = ?1 ORDER BY last_used DESC, created_at DESC LIMIT ?2)",
        params![username, MAX_SESSIONS_PER_USER],
    )?;
    Ok(token)
}

pub fn user_for_feed_token(db: &Connection, token: &str) -> rusqlite::Result<Option<String>> {
    db.query_row("SELECT username FROM users WHERE feed_token = ?1", [token], |r| r.get(0)).optional()
}

fn session_response(status: StatusCode, token: &str, username: &str, feed_token: &str) -> Response {
    (status, Json(json!({ "token": token, "username": username, "feed_token": feed_token }))).into_response()
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
    let (id, username) = {
        let db = state.db.lock().unwrap();
        let row: Option<(String, String, i64, i64)> = db
            .query_row(
                "SELECT id, username, expires_at, last_used FROM sessions WHERE token_hash = ?1",
                [&token_hash],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((id, username, _, last_used)) = row.filter(|row| row.2 > t) else {
            return Err(Error::Unauthorized("invalid or expired token"));
        };
        if t - last_used > TOUCH_AFTER_SECS {
            db.execute(
                "UPDATE sessions SET last_used = ?1, expires_at = ?2 WHERE id = ?3",
                params![t, t + SESSION_TTL_SECS, id],
            )?;
        }
        (id, username)
    };
    req.extensions_mut().insert(User(username));
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
    username: String,
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
    let username = normalize_username(&body.username)?;
    validate_password(&body.password, &username)?;

    let password_hash = hash_async(&state, body.password).await?;
    let feed_token = random_hex(24);
    let db = state.db.lock().unwrap();
    let inserted = db.execute(
        "INSERT INTO users (username, password_hash, feed_token, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![username, password_hash, feed_token, now()],
    );
    match inserted {
        Ok(_) => {}
        Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::ConstraintViolation => {
            return Err(Error::Taken);
        }
        Err(e) => return Err(e.into()),
    }
    let token = create_session(&db, &username, &short_device(body.device))?;
    Ok(session_response(StatusCode::CREATED, &token, &username, &feed_token))
}

#[derive(Deserialize)]
struct Login {
    username: String,
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
    let username = body.username.trim().to_lowercase();
    let ip_key = format!("login-ip:{ip}");
    let user_key = format!("login-user:{username}");
    limiter.check(&ip_key, LOGIN_MAX_PER_IP)?;
    limiter.check(&user_key, LOGIN_MAX_PER_USER)?;

    let stored: Option<(String, String)> = state
        .db
        .lock()
        .unwrap()
        .query_row(
            "SELECT password_hash, feed_token FROM users WHERE username = ?1",
            [&username],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let (hash, feed_token) = match stored {
        Some((hash, feed)) => (Some(hash), feed),
        None => (None, String::new()),
    };
    let ok = verify_async(&state, body.password, hash).await?;
    if !ok {
        limiter.hit(&ip_key, LOGIN_WINDOW);
        limiter.hit(&user_key, LOGIN_WINDOW);
        return Err(Error::Unauthorized("invalid username or password"));
    }
    limiter.clear(&user_key);
    let token = create_session(&state.db.lock().unwrap(), &username, &short_device(body.device))?;
    Ok(session_response(StatusCode::OK, &token, &username, &feed_token))
}

async fn me(State(state): State<AppState>, Extension(User(username)): Extension<User>) -> Result<Response, Error> {
    let (feed_token, created_at): (String, i64) = state.db.lock().unwrap().query_row(
        "SELECT feed_token, created_at FROM users WHERE username = ?1",
        [&username],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(Json(json!({ "username": username, "feed_token": feed_token, "created_at": created_at })).into_response())
}

async fn logout(
    State(state): State<AppState>,
    Extension(SessionId(id)): Extension<SessionId>,
) -> Result<StatusCode, Error> {
    state.db.lock().unwrap().execute("DELETE FROM sessions WHERE id = ?1", [id])?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_sessions(
    State(state): State<AppState>,
    Extension(User(username)): Extension<User>,
    Extension(SessionId(current)): Extension<SessionId>,
) -> Result<Response, Error> {
    let db = state.db.lock().unwrap();
    let sessions = db
        .prepare(
            "SELECT id, device, created_at, last_used FROM sessions
             WHERE username = ?1 AND expires_at > ?2 ORDER BY last_used DESC",
        )?
        .query_map(params![username, now()], |r| {
            let id: String = r.get(0)?;
            Ok(json!({
                "current": id == current, "id": id, "device": r.get::<_, String>(1)?,
                "created_at": r.get::<_, i64>(2)?, "last_used": r.get::<_, i64>(3)?,
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Json(sessions).into_response())
}

async fn revoke_session(
    State(state): State<AppState>,
    Extension(User(username)): Extension<User>,
    Path(id): Path<String>,
) -> Result<StatusCode, Error> {
    let n = state
        .db
        .lock()
        .unwrap()
        .execute("DELETE FROM sessions WHERE id = ?1 AND username = ?2", params![id, username])?;
    if n == 0 { Err(Error::NotFound("session")) } else { Ok(StatusCode::NO_CONTENT) }
}

/// Re-checks the current password for sensitive actions, sharing the login lockout.
async fn confirm_password(state: &AppState, username: &str, password: String) -> Result<(), Error> {
    let key = format!("login-user:{username}");
    state.auth.limiter.check(&key, LOGIN_MAX_PER_USER)?;
    let stored: String = state.db.lock().unwrap().query_row(
        "SELECT password_hash FROM users WHERE username = ?1",
        [username],
        |r| r.get(0),
    )?;
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
    Extension(User(username)): Extension<User>,
    Extension(SessionId(current)): Extension<SessionId>,
    Json(body): Json<ChangePassword>,
) -> Result<StatusCode, Error> {
    validate_password(&body.new_password, &username)?;
    confirm_password(&state, &username, body.current_password).await?;
    let new_hash = hash_async(&state, body.new_password).await?;
    let mut db = state.db.lock().unwrap();
    let tx = db.transaction()?;
    tx.execute("UPDATE users SET password_hash = ?1 WHERE username = ?2", params![new_hash, username])?;
    tx.execute("DELETE FROM sessions WHERE username = ?1 AND id != ?2", params![username, current])?;
    tx.commit()?;
    Ok(StatusCode::NO_CONTENT)
}

/// Invalidates the old calendar feed URL and returns a fresh token.
async fn rotate_feed_token(
    State(state): State<AppState>,
    Extension(User(username)): Extension<User>,
) -> Result<Response, Error> {
    let feed_token = random_hex(24);
    state
        .db
        .lock()
        .unwrap()
        .execute("UPDATE users SET feed_token = ?1 WHERE username = ?2", params![feed_token, username])?;
    Ok(Json(json!({ "feed_token": feed_token })).into_response())
}

#[derive(Deserialize)]
struct DeleteAccount {
    password: String,
}

/// Permanently removes the account with all calendars, events and sessions.
async fn delete_account(
    State(state): State<AppState>,
    Extension(User(username)): Extension<User>,
    Json(body): Json<DeleteAccount>,
) -> Result<StatusCode, Error> {
    confirm_password(&state, &username, body.password).await?;
    let mut db = state.db.lock().unwrap();
    let tx = db.transaction()?;
    tx.execute(
        "DELETE FROM calendar_members WHERE username = ?1
         OR calendar_id IN (SELECT id FROM calendars WHERE owner = ?1)",
        [&username],
    )?;
    for table in ["events", "calendars"] {
        tx.execute(&format!("DELETE FROM {table} WHERE owner = ?1"), [&username])?;
    }
    tx.execute("DELETE FROM sessions WHERE username = ?1", [&username])?;
    tx.execute("DELETE FROM users WHERE username = ?1", [&username])?;
    tx.commit()?;
    Ok(StatusCode::NO_CONTENT)
}
