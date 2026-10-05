//! Kairo sync server.
//!
//! Every change (calendar or event, including deletions as tombstones) gets a global,
//! strictly increasing `rev`. A client syncs with `GET /api/v1/sync?since=<last rev>` and
//! writes with optimistic concurrency: `PUT /events/{id}?base_rev=<rev it last saw>`
//! answers `409` with the current server version if someone else changed the event first.
//!
//! `GET /api/v1/ws` (WebSocket, bearer auth) pushes `{"rev":N}` whenever the user's data changes,
//! so clients can run a delta sync right away instead of waiting for their next poll.
//!
//! Accounts live in the database (see `auth`): register/login hand out an opaque bearer
//! token per device, stored server-side only as a SHA-256 hash.
//!
//! Configuration (environment):
//! - `KAIRO_ADDR`          listen address, default `127.0.0.1:8080`
//! - `KAIRO_DB`            SQLite file, default `kairo.db`
//! - `KAIRO_REGISTRATION`  `open` (default) or `closed`
//! - `KAIRO_INVITE_CODE`   if set, registration additionally requires this code
//! - `KAIRO_TRUST_PROXY`   `1` to take the client IP from the last `X-Forwarded-For` entry
//!                         (only behind a reverse proxy that sets it)

mod auth;
mod live;
mod share;

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use chrono::{NaiveDateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use share::Access;
use serde::{Deserialize, Serialize};
use serde_json::json;

const DT_FORMAT: &str = "%Y-%m-%dT%H:%M:%S";

// ---------------------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------------------

fn default_tz() -> String {
    "Europe/Berlin".into()
}

fn default_status() -> String {
    "busy".into()
}

/// How the event shows up for availability, like Outlook's "Anzeigen als".
const STATUSES: [&str; 4] = ["free", "tentative", "busy", "oof"];

fn default_color() -> String {
    "#0f6cbd".into()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Calendar {
    #[serde(default)]
    id: String,
    name: String,
    #[serde(default = "default_color")]
    color: String,
    #[serde(default)]
    rev: i64,
    #[serde(default)]
    deleted: bool,
    /// Filled in by the server (ignored on upload).
    #[serde(default)]
    owner: String,
    /// What the requesting user may do: `owner`, `editor` or `viewer`.
    #[serde(default = "default_role")]
    role: String,
}

fn default_role() -> String {
    "owner".into()
}

/// Start and end are local wall-clock times in `tz` (an IANA zone name), like in iCalendar.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Event {
    #[serde(default)]
    id: String,
    calendar_id: String,
    title: String,
    #[serde(default)]
    location: String,
    #[serde(default)]
    description: String,
    start: NaiveDateTime,
    end: NaiveDateTime,
    #[serde(default = "default_tz")]
    tz: String,
    /// `free`, `tentative`, `busy` (default) or `oof` (out of office).
    #[serde(default = "default_status")]
    status: String,
    #[serde(default)]
    rev: i64,
    #[serde(default)]
    deleted: bool,
}

#[derive(Clone)]
struct User(String);

#[derive(Clone)]
struct AppState {
    db: Arc<Mutex<Connection>>,
    auth: Arc<auth::AuthState>,
    /// (user, new rev) after every committed write; WebSocket clients of that user are told.
    changes: tokio::sync::broadcast::Sender<(String, i64)>,
}

impl AppState {
    fn new(db: Connection, auth: auth::AuthState) -> Self {
        let (changes, _) = tokio::sync::broadcast::channel(256);
        AppState { db: Arc::new(Mutex::new(db)), auth: Arc::new(auth), changes }
    }

    fn notify_all(&self, users: &[String], rev: i64) {
        for user in users {
            self.notify(user, rev);
        }
    }

    fn notify(&self, user: &str, rev: i64) {
        let _ = self.changes.send((user.to_string(), rev)); // no listeners is fine
    }
}

// ---------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------

enum Error {
    Db(rusqlite::Error),
    NotFound(&'static str),
    Gone,
    Conflict(Event),
    Invalid(String),
    Unauthorized(&'static str),
    Forbidden(&'static str),
    /// Username already registered.
    Taken,
    /// Seconds until the client may retry.
    TooManyRequests(u64),
    Internal(String),
}

impl From<rusqlite::Error> for Error {
    fn from(err: rusqlite::Error) -> Self {
        Error::Db(err)
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        match self {
            Error::Db(err) => {
                eprintln!("database error: {err}");
                (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": "internal error" })))
                    .into_response()
            }
            Error::NotFound(what) => {
                (StatusCode::NOT_FOUND, Json(json!({ "error": format!("{what} not found") })))
                    .into_response()
            }
            Error::Gone => {
                (StatusCode::GONE, Json(json!({ "error": "already deleted" }))).into_response()
            }
            Error::Conflict(current) => (
                StatusCode::CONFLICT,
                Json(json!({ "error": "conflict", "current": current })),
            )
                .into_response(),
            Error::Invalid(msg) => {
                (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))).into_response()
            }
            Error::Unauthorized(msg) => {
                (StatusCode::UNAUTHORIZED, Json(json!({ "error": msg }))).into_response()
            }
            Error::Forbidden(msg) => {
                (StatusCode::FORBIDDEN, Json(json!({ "error": msg }))).into_response()
            }
            Error::Taken => {
                (StatusCode::CONFLICT, Json(json!({ "error": "username is already taken" })))
                    .into_response()
            }
            Error::TooManyRequests(secs) => (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, secs.to_string())],
                Json(json!({ "error": "too many attempts, try again later", "retry_after": secs })),
            )
                .into_response(),
            Error::Internal(msg) => {
                eprintln!("internal error: {msg}");
                (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": "internal error" })))
                    .into_response()
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// Database
// ---------------------------------------------------------------------------------------

fn init_db(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE IF NOT EXISTS counter (rev INTEGER NOT NULL);
         INSERT INTO counter SELECT 0 WHERE NOT EXISTS (SELECT 1 FROM counter);
         CREATE TABLE IF NOT EXISTS calendars (
             id TEXT PRIMARY KEY, owner TEXT NOT NULL, name TEXT NOT NULL, color TEXT NOT NULL,
             rev INTEGER NOT NULL, deleted INTEGER NOT NULL DEFAULT 0);
         CREATE INDEX IF NOT EXISTS calendars_owner_rev ON calendars (owner, rev);
         CREATE TABLE IF NOT EXISTS events (
             id TEXT PRIMARY KEY, owner TEXT NOT NULL, calendar_id TEXT NOT NULL,
             title TEXT NOT NULL, location TEXT NOT NULL, description TEXT NOT NULL,
             starts_at TEXT NOT NULL, ends_at TEXT NOT NULL, tz TEXT NOT NULL,
             status TEXT NOT NULL DEFAULT 'busy',
             rev INTEGER NOT NULL, deleted INTEGER NOT NULL DEFAULT 0);
         CREATE INDEX IF NOT EXISTS events_owner_rev ON events (owner, rev);
         CREATE INDEX IF NOT EXISTS events_calendar ON events (calendar_id);",
    )?;
    // Databases created before `status` existed.
    let has_status: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM pragma_table_info('events') WHERE name = 'status')",
        [],
        |r| r.get(0),
    )?;
    if !has_status {
        conn.execute("ALTER TABLE events ADD COLUMN status TEXT NOT NULL DEFAULT 'busy'", [])?;
    }
    share::init_db(conn)?;
    auth::init_db(conn)
}

fn next_rev(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("UPDATE counter SET rev = rev + 1 RETURNING rev", [], |r| r.get(0))
}

fn current_rev(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT rev FROM counter", [], |r| r.get(0))
}

fn parse_dt(text: String) -> rusqlite::Result<NaiveDateTime> {
    NaiveDateTime::parse_from_str(&text, DT_FORMAT)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))
}

const EVENT_COLUMNS: &str =
    "id, calendar_id, title, location, description, starts_at, ends_at, tz, rev, deleted, owner, status";

fn event_from_row(row: &rusqlite::Row) -> rusqlite::Result<(Event, String)> {
    let event = Event {
        id: row.get(0)?,
        calendar_id: row.get(1)?,
        title: row.get(2)?,
        location: row.get(3)?,
        description: row.get(4)?,
        start: parse_dt(row.get(5)?)?,
        end: parse_dt(row.get(6)?)?,
        tz: row.get(7)?,
        rev: row.get(8)?,
        deleted: row.get(9)?,
        status: row.get(11)?,
    };
    Ok((event, row.get(10)?))
}

fn load_event(conn: &Connection, id: &str) -> rusqlite::Result<Option<(Event, String)>> {
    conn.query_row(
        &format!("SELECT {EVENT_COLUMNS} FROM events WHERE id = ?1"),
        [id],
        event_from_row,
    )
    .optional()
}

// ---------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct SyncQuery {
    #[serde(default)]
    since: i64,
}

#[derive(Serialize)]
struct Invite {
    calendar_id: String,
    name: String,
    owner: String,
    role: String,
    invited_by: String,
}

#[derive(Serialize)]
struct SyncResponse {
    /// Store this and send it as `since` next time.
    rev: i64,
    calendars: Vec<Calendar>,
    events: Vec<Event>,
    /// Every calendar the user can currently see. A calendar missing from here (but known
    /// to the client) was unshared and must be dropped locally.
    calendar_ids: Vec<String>,
    /// Open invitations, always the complete list.
    invites: Vec<Invite>,
}

fn calendar_from_row(r: &rusqlite::Row) -> rusqlite::Result<Calendar> {
    Ok(Calendar {
        id: r.get(0)?,
        name: r.get(1)?,
        color: r.get(2)?,
        rev: r.get(3)?,
        deleted: r.get(4)?,
        owner: r.get(5)?,
        role: r.get(6)?,
    })
}

async fn sync(
    State(state): State<AppState>,
    Extension(User(user)): Extension<User>,
    Query(query): Query<SyncQuery>,
) -> Result<Json<SyncResponse>, Error> {
    let mut db = state.db.lock().unwrap();
    // One read transaction so `rev` matches the rows we return.
    let tx = db.transaction()?;
    let rev = current_rev(&tx)?;

    let calendars = tx
        .prepare(
            "SELECT c.id, c.name, c.color, c.rev, c.deleted, c.owner,
                    CASE WHEN c.owner = ?1 THEN 'owner' ELSE m.role END
             FROM calendars c
             LEFT JOIN calendar_members m
               ON m.calendar_id = c.id AND m.username = ?1 AND m.accepted = 1
             WHERE (c.owner = ?1 OR m.username IS NOT NULL) AND c.rev > ?2
             ORDER BY c.rev",
        )?
        .query_map(params![user, query.since], calendar_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let events = tx
        .prepare(&format!(
            "SELECT {EVENT_COLUMNS} FROM events WHERE calendar_id IN {} AND rev > ?2 ORDER BY rev",
            share::ACCESSIBLE
        ))?
        .query_map(params![user, query.since], event_from_row)?
        .map(|row| row.map(|(event, _)| event))
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let calendar_ids = tx
        .prepare(&format!("SELECT id FROM calendars WHERE deleted = 0 AND id IN {}", share::ACCESSIBLE))?
        .query_map([&user], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;

    let invites = tx
        .prepare(
            "SELECT m.calendar_id, c.name, c.owner, m.role, m.invited_by
             FROM calendar_members m JOIN calendars c ON c.id = m.calendar_id
             WHERE m.username = ?1 AND m.accepted = 0 AND c.deleted = 0 ORDER BY c.name",
        )?
        .query_map([&user], |r| {
            Ok(Invite {
                calendar_id: r.get(0)?,
                name: r.get(1)?,
                owner: r.get(2)?,
                role: r.get(3)?,
                invited_by: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(Json(SyncResponse { rev, calendars, events, calendar_ids, invites }))
}

async fn put_calendar(
    State(state): State<AppState>,
    Extension(User(user)): Extension<User>,
    Path(id): Path<String>,
    Json(mut calendar): Json<Calendar>,
) -> Result<Json<Calendar>, Error> {
    if calendar.name.trim().is_empty() || calendar.name.len() > 100 {
        return Err(Error::Invalid("name must be 1-100 characters".into()));
    }
    calendar.id = id;
    calendar.deleted = false;
    calendar.owner = user.clone();
    calendar.role = "owner".into();

    let mut db = state.db.lock().unwrap();
    let tx = db.transaction()?;
    match share::calendar_info(&tx, &calendar.id)? {
        None => {}
        Some((owner, _)) if owner == user => {}
        Some(_) => {
            return Err(share::denied(share::access(&tx, &user, &calendar.id)?, "only the owner can change a calendar"));
        }
    }
    // Calendars are last-write-wins: only name and color, conflicts are not worth a 409.
    calendar.rev = next_rev(&tx)?;
    tx.execute(
        "INSERT INTO calendars (id, owner, name, color, rev, deleted) VALUES (?1, ?2, ?3, ?4, ?5, 0)
         ON CONFLICT(id) DO UPDATE SET name = excluded.name, color = excluded.color,
                                       rev = excluded.rev, deleted = 0",
        params![calendar.id, user, calendar.name, calendar.color, calendar.rev],
    )?;
    let audience = share::users_of(&tx, &calendar.id)?;
    tx.commit()?;
    state.notify_all(&audience, calendar.rev);
    Ok(Json(calendar))
}

async fn delete_calendar(
    State(state): State<AppState>,
    Extension(User(user)): Extension<User>,
    Path(id): Path<String>,
) -> Result<StatusCode, Error> {
    let mut db = state.db.lock().unwrap();
    let tx = db.transaction()?;
    match share::calendar_info(&tx, &id)? {
        None => return Err(Error::NotFound("calendar")),
        Some((owner, _)) if owner == user => {}
        Some(_) => return Err(share::denied(share::access(&tx, &user, &id)?, "only the owner can delete a calendar")),
    }
    let rev = next_rev(&tx)?;
    tx.execute("UPDATE calendars SET deleted = 1, rev = ?1 WHERE id = ?2", params![rev, id])?;
    tx.execute(
        "UPDATE events SET deleted = 1, rev = ?1 WHERE calendar_id = ?2 AND deleted = 0",
        params![rev, id],
    )?;
    let audience = share::users_of(&tx, &id)?;
    tx.commit()?;
    state.notify_all(&audience, rev);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct BaseRev {
    base_rev: Option<i64>,
}

fn validate(event: &Event) -> Result<(), Error> {
    if event.title.trim().is_empty() || event.title.len() > 500 {
        return Err(Error::Invalid("title must be 1-500 characters".into()));
    }
    if event.location.len() > 500 || event.description.len() > 20_000 {
        return Err(Error::Invalid("location or description too long".into()));
    }
    if event.end < event.start {
        return Err(Error::Invalid("end must not be before start".into()));
    }
    if !STATUSES.contains(&event.status.as_str()) {
        return Err(Error::Invalid("status must be free, tentative, busy or oof".into()));
    }
    Ok(())
}

async fn put_event(
    State(state): State<AppState>,
    Extension(User(user)): Extension<User>,
    Path(id): Path<String>,
    Query(BaseRev { base_rev }): Query<BaseRev>,
    Json(mut event): Json<Event>,
) -> Result<Json<Event>, Error> {
    validate(&event)?;
    event.id = id;
    event.deleted = false;

    let mut db = state.db.lock().unwrap();
    let tx = db.transaction()?;

    let Some((calendar_owner, false)) = share::calendar_info(&tx, &event.calendar_id)? else {
        return Err(Error::NotFound("calendar"));
    };
    let level = share::access(&tx, &user, &event.calendar_id)?;
    if level < Access::Editor {
        return Err(share::denied(level, "this calendar is read-only for you"));
    }

    let mut audience = Vec::new();
    match load_event(&tx, &event.id)? {
        Some((current, _)) => {
            // Also needs write access to the calendar the event currently lives in.
            let current_level = share::access(&tx, &user, &current.calendar_id)?;
            if current_level < Access::Editor {
                return Err(Error::NotFound("event"));
            }
            if current.deleted {
                return Err(Error::Gone);
            }
            if base_rev != Some(current.rev) {
                return Err(Error::Conflict(current));
            }
            audience = share::users_of(&tx, &current.calendar_id)?;
        }
        None if base_rev.is_some() => return Err(Error::NotFound("event")),
        None => {}
    }

    event.rev = next_rev(&tx)?;
    tx.execute(
        "INSERT INTO events (id, owner, calendar_id, title, location, description, starts_at, ends_at, tz, rev, deleted, status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 0, ?11)
         ON CONFLICT(id) DO UPDATE SET calendar_id = excluded.calendar_id, title = excluded.title,
             location = excluded.location, description = excluded.description,
             starts_at = excluded.starts_at, ends_at = excluded.ends_at, tz = excluded.tz,
             status = excluded.status, rev = excluded.rev, deleted = 0",
        params![
            event.id,
            calendar_owner,
            event.calendar_id,
            event.title,
            event.location,
            event.description,
            event.start.format(DT_FORMAT).to_string(),
            event.end.format(DT_FORMAT).to_string(),
            event.tz,
            event.rev,
            event.status,
        ],
    )?;
    audience.extend(share::users_of(&tx, &event.calendar_id)?);
    tx.commit()?;
    state.notify_all(&audience, event.rev);
    Ok(Json(event))
}

async fn delete_event(
    State(state): State<AppState>,
    Extension(User(user)): Extension<User>,
    Path(id): Path<String>,
    Query(BaseRev { base_rev }): Query<BaseRev>,
) -> Result<StatusCode, Error> {
    let mut db = state.db.lock().unwrap();
    let tx = db.transaction()?;
    let Some((current, _)) = load_event(&tx, &id)? else {
        return Err(Error::NotFound("event"));
    };
    let level = share::access(&tx, &user, &current.calendar_id)?;
    if level < Access::Editor {
        return Err(if level == Access::None {
            Error::NotFound("event")
        } else {
            Error::Forbidden("this calendar is read-only for you")
        });
    }
    if current.deleted {
        return Ok(StatusCode::NO_CONTENT); // idempotent
    }
    if base_rev.is_some_and(|rev| rev != current.rev) {
        return Err(Error::Conflict(current));
    }
    let rev = next_rev(&tx)?;
    tx.execute("UPDATE events SET deleted = 1, rev = ?1 WHERE id = ?2", params![rev, id])?;
    let audience = share::users_of(&tx, &current.calendar_id)?;
    tx.commit()?;
    state.notify_all(&audience, rev);
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------
// Read-only iCalendar feed, so a phone can subscribe without any Kairo client:
// GET /ics/{calendar_id}?token=...
// ---------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct IcsQuery {
    token: String,
}

fn ics_escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace(';', "\\;")
        .replace(',', "\\,")
        .replace("\r\n", "\\n")
        .replace('\n', "\\n")
}

/// Folds a content line at 75 octets as RFC 5545 requires.
fn ics_fold(line: &str) -> String {
    let mut out = String::new();
    let mut len = 0;
    for c in line.chars() {
        let width = c.len_utf8();
        if len + width > 74 {
            out.push_str("\r\n ");
            len = 1;
        }
        out.push(c);
        len += width;
    }
    out
}

fn render_ics(calendar: &Calendar, events: &[Event]) -> String {
    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let mut lines = vec![
        "BEGIN:VCALENDAR".to_string(),
        "VERSION:2.0".into(),
        "PRODID:-//Kairo//Sync Server//DE".into(),
        format!("X-WR-CALNAME:{}", ics_escape(&calendar.name)),
    ];
    for event in events {
        lines.push("BEGIN:VEVENT".into());
        lines.push(format!("UID:{}@kairo", event.id));
        lines.push(format!("DTSTAMP:{stamp}"));
        lines.push(format!("SEQUENCE:{}", event.rev));
        lines.push(format!("DTSTART;TZID={}:{}", event.tz, event.start.format("%Y%m%dT%H%M%S")));
        lines.push(format!("DTEND;TZID={}:{}", event.tz, event.end.format("%Y%m%dT%H%M%S")));
        lines.push(format!("SUMMARY:{}", ics_escape(&event.title)));
        // Free time doesn't block the calendar; "tentative" is Outlook's "Mit Vorbehalt".
        lines.push(format!("TRANSP:{}", if event.status == "free" { "TRANSPARENT" } else { "OPAQUE" }));
        match event.status.as_str() {
            "tentative" => lines.push("STATUS:TENTATIVE".into()),
            "oof" => lines.push("X-MICROSOFT-CDO-BUSYSTATUS:OOF".into()),
            _ => {}
        }
        if !event.location.is_empty() {
            lines.push(format!("LOCATION:{}", ics_escape(&event.location)));
        }
        if !event.description.is_empty() {
            lines.push(format!("DESCRIPTION:{}", ics_escape(&event.description)));
        }
        lines.push("END:VEVENT".into());
    }
    lines.push("END:VCALENDAR".into());
    lines.iter().map(|l| ics_fold(l) + "\r\n").collect()
}

async fn ics(
    State(state): State<AppState>,
    Path(calendar_id): Path<String>,
    Query(query): Query<IcsQuery>,
) -> Result<Response, Error> {
    let db = state.db.lock().unwrap();
    // The feed uses its own read-only token, never a session token (it ends up in URLs).
    let Some(user) = auth::user_for_feed_token(&db, &query.token)? else {
        return Err(Error::NotFound("calendar")); // don't reveal whether the id exists
    };
    if share::access(&db, &user, &calendar_id)? == Access::None {
        return Err(Error::NotFound("calendar"));
    }
    let calendar = db
        .query_row(
            "SELECT id, name, color, rev, deleted, owner, 'viewer' FROM calendars WHERE id = ?1 AND deleted = 0",
            params![calendar_id],
            calendar_from_row,
        )
        .optional()?
        .ok_or(Error::NotFound("calendar"))?;
    let events = db
        .prepare(&format!(
            "SELECT {EVENT_COLUMNS} FROM events WHERE calendar_id = ?1 AND deleted = 0 ORDER BY starts_at"
        ))?
        .query_map([&calendar_id], event_from_row)?
        .map(|row| row.map(|(event, _)| event))
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(([(header::CONTENT_TYPE, "text/calendar; charset=utf-8")], render_ics(&calendar, &events)).into_response())
}

// ---------------------------------------------------------------------------------------
// Wiring
// ---------------------------------------------------------------------------------------

fn router(state: AppState) -> Router {
    let protected = Router::new()
        .route("/sync", get(sync))
        .route("/calendars/{id}", put(put_calendar).delete(delete_calendar))
        .route("/events/{id}", put(put_event).delete(delete_event))
        .route("/calendars/{id}/members", get(share::list_members))
        .route("/calendars/{id}/members/{username}", put(share::invite).delete(share::remove_member))
        .route("/calendars/{id}/accept", post(share::accept))
        .route("/ws", get(live::ws))
        .merge(auth::protected_routes())
        .layer(middleware::from_fn_with_state(state.clone(), auth::require_session));
    let api = auth::public_routes().merge(protected);

    Router::new()
        .nest("/api/v1", api)
        .route("/ics/{calendar_id}", get(ics))
        .route("/healthz", get(|| async { "ok" }))
        .layer(middleware::from_fn_with_state(state.clone(), auth::client_ip))
        .with_state(state)
}

/// `kairo-server healthcheck`: exits 0 if the local server answers `/healthz`. Used by
/// Docker, since the runtime image has no curl.
fn healthcheck() -> bool {
    use std::io::{Read, Write};
    let addr = std::env::var("KAIRO_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let port = addr.rsplit(':').next().unwrap_or("8080");
    let Ok(mut stream) = std::net::TcpStream::connect(format!("127.0.0.1:{port}")) else {
        return false;
    };
    let timeout = Some(std::time::Duration::from_secs(3));
    let _ = stream.set_read_timeout(timeout);
    let request = "GET /healthz HTTP/1.0\r\nHost: localhost\r\n\r\n";
    let mut response = String::new();
    stream.write_all(request.as_bytes()).is_ok()
        && stream.read_to_string(&mut response).is_ok()
        && (response.starts_with("HTTP/1.0 200") || response.starts_with("HTTP/1.1 200"))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        std::process::exit(if healthcheck() { 0 } else { 1 });
    }
    let addr = std::env::var("KAIRO_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let db_path = std::env::var("KAIRO_DB").unwrap_or_else(|_| "kairo.db".into());
    let config = auth::Config::from_env()?;

    let conn = Connection::open(&db_path)?;
    init_db(&conn)?;
    let state = AppState::new(conn, auth::AuthState::new(config));

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("kairo-server listening on http://{addr} (db: {db_path})");
    axum::serve(listener, router(state).into_make_service_with_connect_info::<SocketAddr>()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    fn app_with(config: auth::Config) -> Router {
        let conn = Connection::open_in_memory().unwrap();
        init_db(&conn).unwrap();
        router(AppState::new(conn, auth::AuthState::new(config)))
    }

    fn app() -> Router {
        app_with(auth::Config::test(true, None))
    }

    async fn call(app: &Router, method: &str, uri: &str, token: &str, body: Option<serde_json::Value>) -> (StatusCode, serde_json::Value) {
        let mut req = Request::builder().method(method).uri(uri);
        if !token.is_empty() {
            req = req.header("authorization", format!("Bearer {token}"));
        }
        let body = match body {
            Some(json) => {
                req = req.header("content-type", "application/json");
                Body::from(json.to_string())
            }
            None => Body::empty(),
        };
        let res = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
    }

    const PASSWORD: &str = "correct horse battery";

    /// Registers `name` and returns the session token.
    async fn register(app: &Router, name: &str) -> String {
        let (s, body) = call(app, "POST", "/api/v1/auth/register", "", Some(json!({ "username": name, "password": PASSWORD }))).await;
        assert_eq!(s, StatusCode::CREATED, "{body}");
        body["token"].as_str().unwrap().to_string()
    }

    fn event(title: &str) -> serde_json::Value {
        json!({ "calendar_id": "c1", "title": title, "start": "2026-10-05T09:00:00", "end": "2026-10-05T10:00:00" })
    }

    #[tokio::test]
    async fn requires_token() {
        let app = app();
        let (status, _) = call(&app, "GET", "/api/v1/sync", "wrong", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = call(&app, "GET", "/api/v1/sync", "", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn register_login_logout() {
        let app = app();
        let token = register(&app, "Alice").await; // usernames are case-insensitive
        let (s, me) = call(&app, "GET", "/api/v1/auth/me", &token, None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(me["username"], "alice");

        // duplicate (any casing), weak password and bad username are rejected
        let dup = |name: &str, pw: &str| json!({ "username": name, "password": pw });
        let (s, _) = call(&app, "POST", "/api/v1/auth/register", "", Some(dup("alice", PASSWORD))).await;
        assert_eq!(s, StatusCode::CONFLICT);
        let (s, _) = call(&app, "POST", "/api/v1/auth/register", "", Some(dup("bob", "short"))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let (s, _) = call(&app, "POST", "/api/v1/auth/register", "", Some(dup("b o b!", PASSWORD))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);

        // login gives an independent second session
        let (s, login) = call(&app, "POST", "/api/v1/auth/login", "", Some(json!({ "username": "alice", "password": PASSWORD, "device": "phone" }))).await;
        assert_eq!(s, StatusCode::OK);
        let phone = login["token"].as_str().unwrap().to_string();
        assert_ne!(phone, token);
        let (_, sessions) = call(&app, "GET", "/api/v1/auth/sessions", &phone, None).await;
        assert_eq!(sessions.as_array().unwrap().len(), 2);

        // logout revokes only that session
        let (s, _) = call(&app, "POST", "/api/v1/auth/logout", &phone, None).await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        assert_eq!(call(&app, "GET", "/api/v1/auth/me", &phone, None).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(call(&app, "GET", "/api/v1/auth/me", &token, None).await.0, StatusCode::OK);

        // wrong password / unknown user look the same
        let bad = |u: &str| json!({ "username": u, "password": "nope-nope-nope" });
        let (s1, b1) = call(&app, "POST", "/api/v1/auth/login", "", Some(bad("alice"))).await;
        let (s2, b2) = call(&app, "POST", "/api/v1/auth/login", "", Some(bad("ghost"))).await;
        assert_eq!((s1, s2), (StatusCode::UNAUTHORIZED, StatusCode::UNAUTHORIZED));
        assert_eq!(b1, b2);
    }

    #[tokio::test]
    async fn failed_logins_lock_out() {
        let app = app();
        register(&app, "alice").await;
        let bad = json!({ "username": "alice", "password": "wrong-wrong-wrong" });
        for _ in 0..8 {
            let (s, _) = call(&app, "POST", "/api/v1/auth/login", "", Some(bad.clone())).await;
            assert_eq!(s, StatusCode::UNAUTHORIZED);
        }
        // locked, even with the correct password
        let good = json!({ "username": "alice", "password": PASSWORD });
        let (s, _) = call(&app, "POST", "/api/v1/auth/login", "", Some(good)).await;
        assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn registration_can_be_closed_or_need_invite() {
        let closed = app_with(auth::Config::test(false, None));
        let body = json!({ "username": "alice", "password": PASSWORD });
        let (s, _) = call(&closed, "POST", "/api/v1/auth/register", "", Some(body.clone())).await;
        assert_eq!(s, StatusCode::FORBIDDEN);

        let invite = app_with(auth::Config::test(true, Some("sesame")));
        let (s, _) = call(&invite, "POST", "/api/v1/auth/register", "", Some(body.clone())).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let with_code = json!({ "username": "alice", "password": PASSWORD, "invite_code": "sesame" });
        let (s, _) = call(&invite, "POST", "/api/v1/auth/register", "", Some(with_code)).await;
        assert_eq!(s, StatusCode::CREATED);
    }

    #[tokio::test]
    async fn password_change_revokes_other_sessions() {
        let app = app();
        let laptop = register(&app, "alice").await;
        let (_, login) = call(&app, "POST", "/api/v1/auth/login", "", Some(json!({ "username": "alice", "password": PASSWORD }))).await;
        let phone = login["token"].as_str().unwrap().to_string();

        let change = |current: &str| json!({ "current_password": current, "new_password": "a brand new passphrase" });
        let (s, _) = call(&app, "POST", "/api/v1/auth/password", &laptop, Some(change("wrong password!"))).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        let (s, _) = call(&app, "POST", "/api/v1/auth/password", &laptop, Some(change(PASSWORD))).await;
        assert_eq!(s, StatusCode::NO_CONTENT);

        assert_eq!(call(&app, "GET", "/api/v1/auth/me", &laptop, None).await.0, StatusCode::OK);
        assert_eq!(call(&app, "GET", "/api/v1/auth/me", &phone, None).await.0, StatusCode::UNAUTHORIZED);
        let old = json!({ "username": "alice", "password": PASSWORD });
        assert_eq!(call(&app, "POST", "/api/v1/auth/login", "", Some(old)).await.0, StatusCode::UNAUTHORIZED);
        let new = json!({ "username": "alice", "password": "a brand new passphrase" });
        assert_eq!(call(&app, "POST", "/api/v1/auth/login", "", Some(new)).await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn feed_token_and_account_deletion() {
        let app = app();
        let token = register(&app, "alice").await;
        call(&app, "PUT", "/api/v1/calendars/c1", &token, Some(json!({ "name": "Team" }))).await;
        let (_, me) = call(&app, "GET", "/api/v1/auth/me", &token, None).await;
        let feed = me["feed_token"].as_str().unwrap().to_string();

        // the feed accepts the feed token but not a session token
        let uri = |t: &str| format!("/ics/c1?token={t}");
        let get = |app: &Router, uri: String| {
            let app = app.clone();
            async move {
                let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
                app.oneshot(req).await.unwrap().status()
            }
        };
        assert_eq!(get(&app, uri(&feed)).await, StatusCode::OK);
        assert_eq!(get(&app, uri(&token)).await, StatusCode::NOT_FOUND);

        // rotating invalidates the old URL
        let (_, rotated) = call(&app, "POST", "/api/v1/auth/feed-token", &token, None).await;
        assert_eq!(get(&app, uri(&feed)).await, StatusCode::NOT_FOUND);
        assert_eq!(get(&app, uri(rotated["feed_token"].as_str().unwrap())).await, StatusCode::OK);

        // deleting the account needs the password and removes everything
        let (s, _) = call(&app, "DELETE", "/api/v1/auth/account", &token, Some(json!({ "password": "wrong password!" }))).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        let (s, _) = call(&app, "DELETE", "/api/v1/auth/account", &token, Some(json!({ "password": PASSWORD }))).await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        assert_eq!(call(&app, "GET", "/api/v1/sync", &token, None).await.0, StatusCode::UNAUTHORIZED);
        let login = json!({ "username": "alice", "password": PASSWORD });
        assert_eq!(call(&app, "POST", "/api/v1/auth/login", "", Some(login)).await.0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn event_lifecycle_with_conflicts_and_delta_sync() {
        let app = app();
        let alice = &register(&app, "alice").await;
        let (s, _) = call(&app, "PUT", "/api/v1/calendars/c1", alice, Some(json!({ "name": "Team" }))).await;
        assert_eq!(s, StatusCode::OK);

        // create
        let (s, created) = call(&app, "PUT", "/api/v1/events/e1", alice, Some(event("Stand-up"))).await;
        assert_eq!(s, StatusCode::OK);
        let rev1 = created["rev"].as_i64().unwrap();

        // creating again without base_rev collides, updating with a stale one too
        let (s, _) = call(&app, "PUT", "/api/v1/events/e1", alice, Some(event("Other"))).await;
        assert_eq!(s, StatusCode::CONFLICT);

        // update with the right base_rev
        let (s, updated) = call(&app, "PUT", &format!("/api/v1/events/e1?base_rev={rev1}"), alice, Some(event("Stand-up 2"))).await;
        assert_eq!(s, StatusCode::OK);
        let rev2 = updated["rev"].as_i64().unwrap();
        assert!(rev2 > rev1);

        // stale base_rev now conflicts and returns the current version
        let (s, conflict) = call(&app, "PUT", &format!("/api/v1/events/e1?base_rev={rev1}"), alice, Some(event("Stale"))).await;
        assert_eq!(s, StatusCode::CONFLICT);
        assert_eq!(conflict["current"]["title"], "Stand-up 2");

        // delta sync: full, then nothing, then only the deletion
        let (_, full) = call(&app, "GET", "/api/v1/sync?since=0", alice, None).await;
        assert_eq!(full["events"].as_array().unwrap().len(), 1);
        let head = full["rev"].as_i64().unwrap();
        let (_, none) = call(&app, "GET", &format!("/api/v1/sync?since={head}"), alice, None).await;
        assert!(none["events"].as_array().unwrap().is_empty());

        let (s, _) = call(&app, "DELETE", "/api/v1/events/e1", alice, None).await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        let (_, delta) = call(&app, "GET", &format!("/api/v1/sync?since={head}"), alice, None).await;
        assert_eq!(delta["events"][0]["deleted"], true);
    }

    #[test]
    fn old_database_gets_status_column() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE events (
                 id TEXT PRIMARY KEY, owner TEXT NOT NULL, calendar_id TEXT NOT NULL,
                 title TEXT NOT NULL, location TEXT NOT NULL, description TEXT NOT NULL,
                 starts_at TEXT NOT NULL, ends_at TEXT NOT NULL, tz TEXT NOT NULL,
                 rev INTEGER NOT NULL, deleted INTEGER NOT NULL DEFAULT 0);
             INSERT INTO events VALUES ('e1','a','c1','t','','','2026-10-05T09:00:00','2026-10-05T10:00:00','Europe/Berlin',1,0);",
        )
        .unwrap();
        init_db(&conn).unwrap();
        init_db(&conn).unwrap(); // running it again is harmless
        let status: String = conn.query_row("SELECT status FROM events WHERE id = 'e1'", [], |r| r.get(0)).unwrap();
        assert_eq!(status, "busy");
    }

    #[tokio::test]
    async fn event_status_is_stored_and_validated() {
        let app = app();
        let token = &register(&app, "alice").await;
        call(&app, "PUT", "/api/v1/calendars/c1", token, Some(json!({ "name": "Team" }))).await;

        // defaults to busy
        let (_, created) = call(&app, "PUT", "/api/v1/events/e1", token, Some(event("Plain"))).await;
        assert_eq!(created["status"], "busy");

        let mut tentative = event("Maybe");
        tentative["status"] = "tentative".into();
        let (s, saved) = call(&app, "PUT", "/api/v1/events/e2", token, Some(tentative.clone())).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(saved["status"], "tentative");
        let (_, sync) = call(&app, "GET", "/api/v1/sync", token, None).await;
        assert!(sync["events"].as_array().unwrap().iter().any(|e| e["id"] == "e2" && e["status"] == "tentative"));

        tentative["status"] = "maybe-ish".into();
        let (s, _) = call(&app, "PUT", "/api/v1/events/e3", token, Some(tentative)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn sharing_invites_roles_and_leaving() {
        let app = app();
        let alice = &register(&app, "alice").await;
        let bob = &register(&app, "bob").await;
        let carol = &register(&app, "carol").await;
        let cal = |t: &str, m: &str, u: &str, b: Option<serde_json::Value>| {
            let (app, t, m, u) = (app.clone(), t.to_string(), m.to_string(), u.to_string());
            async move { call(&app, &m, &u, &t, b).await }
        };

        cal(alice, "PUT", "/api/v1/calendars/c1", Some(json!({ "name": "Team" }))).await;
        cal(alice, "PUT", "/api/v1/events/e1", Some(event("Planung"))).await;

        // inviting: unknown user, bad role and non-owners are refused
        let invite = |role: &str| Some(json!({ "role": role }));
        assert_eq!(cal(alice, "PUT", "/api/v1/calendars/c1/members/nobody", invite("editor")).await.0, StatusCode::NOT_FOUND);
        assert_eq!(cal(alice, "PUT", "/api/v1/calendars/c1/members/bob", invite("admin")).await.0, StatusCode::BAD_REQUEST);
        assert_eq!(cal(bob, "PUT", "/api/v1/calendars/c1/members/carol", invite("viewer")).await.0, StatusCode::NOT_FOUND);
        assert_eq!(cal(alice, "PUT", "/api/v1/calendars/c1/members/alice", invite("editor")).await.0, StatusCode::BAD_REQUEST);
        assert_eq!(cal(alice, "PUT", "/api/v1/calendars/c1/members/bob", invite("editor")).await.0, StatusCode::OK);
        assert_eq!(cal(alice, "PUT", "/api/v1/calendars/c1/members/carol", invite("viewer")).await.0, StatusCode::OK);

        // pending: Bob sees the invitation but neither the calendar nor its events
        let (_, sync) = cal(bob, "GET", "/api/v1/sync", None).await;
        assert!(sync["calendars"].as_array().unwrap().is_empty() && sync["events"].as_array().unwrap().is_empty());
        assert_eq!(sync["invites"][0]["calendar_id"], "c1");
        assert_eq!(sync["invites"][0]["owner"], "alice");
        assert_eq!(cal(bob, "PUT", "/api/v1/events/e2", Some(event("Early"))).await.0, StatusCode::NOT_FOUND);

        // only the invitee can accept
        assert_eq!(cal(alice, "POST", "/api/v1/calendars/c1/accept", None).await.0, StatusCode::NOT_FOUND);
        assert_eq!(cal(bob, "POST", "/api/v1/calendars/c1/accept", None).await.0, StatusCode::NO_CONTENT);
        assert_eq!(cal(carol, "POST", "/api/v1/calendars/c1/accept", None).await.0, StatusCode::NO_CONTENT);

        // a member who synced a long time ago (since far in the past) still gets everything
        let (_, sync) = cal(bob, "GET", "/api/v1/sync?since=0", None).await;
        assert_eq!(sync["calendars"][0]["role"], "editor");
        assert_eq!(sync["calendars"][0]["owner"], "alice");
        assert_eq!(sync["events"].as_array().unwrap().len(), 1);
        assert!(sync["invites"].as_array().unwrap().is_empty());
        let head = sync["rev"].as_i64().unwrap();

        // editor may change events, viewer may not; neither may rename or delete the calendar
        let current = sync["events"][0]["rev"].as_i64().unwrap();
        let (s, _) = cal(bob, "PUT", &format!("/api/v1/events/e1?base_rev={current}"), Some(event("Planung v2"))).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(cal(bob, "PUT", "/api/v1/events/e5", Some(event("Neu von Bob"))).await.0, StatusCode::OK);
        assert_eq!(cal(carol, "PUT", "/api/v1/events/e6", Some(event("Nope"))).await.0, StatusCode::FORBIDDEN);
        assert_eq!(cal(carol, "DELETE", "/api/v1/events/e1", None).await.0, StatusCode::FORBIDDEN);
        assert_eq!(cal(bob, "PUT", "/api/v1/calendars/c1", Some(json!({ "name": "Mine" }))).await.0, StatusCode::FORBIDDEN);
        assert_eq!(cal(bob, "DELETE", "/api/v1/calendars/c1", None).await.0, StatusCode::FORBIDDEN);

        // Carol (viewer) sees Bob's changes; so does the owner
        let (_, c) = cal(carol, "GET", "/api/v1/sync?since=0", None).await;
        let titles: Vec<_> = c["events"].as_array().unwrap().iter().map(|e| e["title"].as_str().unwrap().to_string()).collect();
        assert!(titles.contains(&"Planung v2".to_string()) && titles.contains(&"Neu von Bob".to_string()));
        assert!(head > 0);

        // members list: owner sees everybody, members only accepted ones
        let (_, members) = cal(alice, "GET", "/api/v1/calendars/c1/members", None).await;
        assert_eq!(members["members"].as_array().unwrap().len(), 2);
        assert_eq!(cal(&register(&app, "dave").await, "GET", "/api/v1/calendars/c1/members", None).await.0, StatusCode::NOT_FOUND);

        // owner removes Carol: her calendar list no longer contains it
        assert_eq!(cal(bob, "DELETE", "/api/v1/calendars/c1/members/carol", None).await.0, StatusCode::FORBIDDEN);
        assert_eq!(cal(alice, "DELETE", "/api/v1/calendars/c1/members/carol", None).await.0, StatusCode::NO_CONTENT);
        let (_, c) = cal(carol, "GET", "/api/v1/sync", None).await;
        assert!(c["calendar_ids"].as_array().unwrap().is_empty());
        assert_eq!(cal(carol, "PUT", "/api/v1/events/e6", Some(event("Nope"))).await.0, StatusCode::NOT_FOUND);

        // Bob leaves on his own; the owner can't leave
        assert_eq!(cal(alice, "DELETE", "/api/v1/calendars/c1/members/alice", None).await.0, StatusCode::BAD_REQUEST);
        assert_eq!(cal(bob, "DELETE", "/api/v1/calendars/c1/members/bob", None).await.0, StatusCode::NO_CONTENT);
        let (_, b) = cal(bob, "GET", "/api/v1/sync", None).await;
        assert!(b["calendar_ids"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn deleting_a_shared_calendar_reaches_members() {
        let app = app();
        let alice = &register(&app, "alice").await;
        let bob = &register(&app, "bob").await;
        call(&app, "PUT", "/api/v1/calendars/c1", alice, Some(json!({ "name": "Team" }))).await;
        call(&app, "PUT", "/api/v1/events/e1", alice, Some(event("x"))).await;
        call(&app, "PUT", "/api/v1/calendars/c1/members/bob", alice, Some(json!({ "role": "editor" }))).await;
        call(&app, "POST", "/api/v1/calendars/c1/accept", bob, None).await;
        let (_, first) = call(&app, "GET", "/api/v1/sync", bob, None).await;
        let head = first["rev"].as_i64().unwrap();

        // declining an invitation removes it
        call(&app, "PUT", "/api/v1/calendars/c2", alice, Some(json!({ "name": "Other" }))).await;
        call(&app, "PUT", "/api/v1/calendars/c2/members/bob", alice, Some(json!({ "role": "viewer" }))).await;
        assert_eq!(call(&app, "GET", "/api/v1/sync", bob, None).await.1["invites"].as_array().unwrap().len(), 1);
        call(&app, "DELETE", "/api/v1/calendars/c2/members/bob", bob, None).await;
        assert!(call(&app, "GET", "/api/v1/sync", bob, None).await.1["invites"].as_array().unwrap().is_empty());

        // the owner deletes the calendar; Bob gets the tombstone through his delta sync
        assert_eq!(call(&app, "DELETE", "/api/v1/calendars/c1", alice, None).await.0, StatusCode::NO_CONTENT);
        let (_, delta) = call(&app, "GET", &format!("/api/v1/sync?since={head}"), bob, None).await;
        assert_eq!(delta["calendars"][0]["deleted"], true);
        assert!(delta["calendar_ids"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn users_are_isolated() {
        let app = app();
        let alice = &register(&app, "alice").await;
        let bob = &register(&app, "bob").await;
        call(&app, "PUT", "/api/v1/calendars/c1", alice, Some(json!({ "name": "Team" }))).await;
        call(&app, "PUT", "/api/v1/events/e1", alice, Some(event("Secret"))).await;

        let (_, bobs) = call(&app, "GET", "/api/v1/sync", bob, None).await;
        assert!(bobs["events"].as_array().unwrap().is_empty());
        // Bob can neither write into Alice's calendar nor overwrite or delete her event.
        let (s, _) = call(&app, "PUT", "/api/v1/events/e2", bob, Some(event("Intruder"))).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let (s, _) = call(&app, "DELETE", "/api/v1/events/e1", bob, None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[test]
    fn ics_is_escaped_and_folded() {
        let calendar = Calendar { id: "c1".into(), name: "Team".into(), color: default_color(), rev: 1, deleted: false, owner: "a".into(), role: "owner".into() };
        let event = Event {
            id: "e1".into(),
            calendar_id: "c1".into(),
            title: "A, B; C".into(),
            location: String::new(),
            description: "x".repeat(200),
            start: "2026-10-05T09:00:00".parse().unwrap(),
            end: "2026-10-05T10:00:00".parse().unwrap(),
            tz: "Europe/Berlin".into(),
            status: "tentative".into(),
            rev: 3,
            deleted: false,
        };
        let ics = render_ics(&calendar, &[event]);
        assert!(ics.contains("SUMMARY:A\\, B\\; C\r\n"));
        assert!(ics.contains("DTSTART;TZID=Europe/Berlin:20261005T090000\r\n"));
        assert!(ics.contains("STATUS:TENTATIVE\r\n") && ics.contains("TRANSP:OPAQUE\r\n"));
        assert!(ics.lines().all(|line| line.len() <= 75));
    }
}
