//! Kairo sync server.
//!
//! Every change (calendar or event, including deletions as tombstones) gets a global,
//! strictly increasing `rev`. A client syncs with `GET /api/v1/sync?since=<last rev>` and
//! writes with optimistic concurrency: `PUT /events/{id}?base_rev=<rev it last saw>`
//! answers `409` with the current server version if someone else changed the event first.
//!
//! Configuration (environment):
//! - `KAIRO_TOKENS`  required, `user:token,user2:token2`
//! - `KAIRO_ADDR`    listen address, default `127.0.0.1:8080`
//! - `KAIRO_DB`      SQLite file, default `kairo.db`

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, Request, State},
    http::{StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, put},
};
use chrono::{NaiveDateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::json;

const DT_FORMAT: &str = "%Y-%m-%dT%H:%M:%S";

// ---------------------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------------------

fn default_tz() -> String {
    "Europe/Berlin".into()
}

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
    /// token -> user
    tokens: Arc<HashMap<String, String>>,
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
             rev INTEGER NOT NULL, deleted INTEGER NOT NULL DEFAULT 0);
         CREATE INDEX IF NOT EXISTS events_owner_rev ON events (owner, rev);
         CREATE INDEX IF NOT EXISTS events_calendar ON events (calendar_id);",
    )
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
    "id, calendar_id, title, location, description, starts_at, ends_at, tz, rev, deleted, owner";

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
// Auth
// ---------------------------------------------------------------------------------------

async fn auth(State(state): State<AppState>, mut req: Request, next: middleware::Next) -> Result<Response, StatusCode> {
    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let user = token
        .and_then(|t| state.tokens.get(t))
        .ok_or(StatusCode::UNAUTHORIZED)?
        .clone();
    req.extensions_mut().insert(User(user));
    Ok(next.run(req).await)
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
struct SyncResponse {
    /// Store this and send it as `since` next time.
    rev: i64,
    calendars: Vec<Calendar>,
    events: Vec<Event>,
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
        .prepare("SELECT id, name, color, rev, deleted FROM calendars WHERE owner = ?1 AND rev > ?2 ORDER BY rev")?
        .query_map(params![user, query.since], |r| {
            Ok(Calendar {
                id: r.get(0)?,
                name: r.get(1)?,
                color: r.get(2)?,
                rev: r.get(3)?,
                deleted: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let events = tx
        .prepare(&format!(
            "SELECT {EVENT_COLUMNS} FROM events WHERE owner = ?1 AND rev > ?2 ORDER BY rev"
        ))?
        .query_map(params![user, query.since], event_from_row)?
        .map(|row| row.map(|(event, _)| event))
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(Json(SyncResponse { rev, calendars, events }))
}

async fn put_calendar(
    State(state): State<AppState>,
    Extension(User(user)): Extension<User>,
    Path(id): Path<String>,
    Json(mut calendar): Json<Calendar>,
) -> Result<Json<Calendar>, Error> {
    if calendar.name.trim().is_empty() {
        return Err(Error::Invalid("name must not be empty".into()));
    }
    calendar.id = id;
    calendar.deleted = false;

    let mut db = state.db.lock().unwrap();
    let tx = db.transaction()?;
    let owner: Option<String> = tx
        .query_row("SELECT owner FROM calendars WHERE id = ?1", [&calendar.id], |r| r.get(0))
        .optional()?;
    if owner.is_some_and(|owner| owner != user) {
        return Err(Error::NotFound("calendar"));
    }
    // Calendars are last-write-wins: only name and color, conflicts are not worth a 409.
    calendar.rev = next_rev(&tx)?;
    tx.execute(
        "INSERT INTO calendars (id, owner, name, color, rev, deleted) VALUES (?1, ?2, ?3, ?4, ?5, 0)
         ON CONFLICT(id) DO UPDATE SET name = excluded.name, color = excluded.color,
                                       rev = excluded.rev, deleted = 0",
        params![calendar.id, user, calendar.name, calendar.color, calendar.rev],
    )?;
    tx.commit()?;
    Ok(Json(calendar))
}

async fn delete_calendar(
    State(state): State<AppState>,
    Extension(User(user)): Extension<User>,
    Path(id): Path<String>,
) -> Result<StatusCode, Error> {
    let mut db = state.db.lock().unwrap();
    let tx = db.transaction()?;
    let exists = tx
        .query_row("SELECT 1 FROM calendars WHERE id = ?1 AND owner = ?2", params![id, user], |_| Ok(()))
        .optional()?
        .is_some();
    if !exists {
        return Err(Error::NotFound("calendar"));
    }
    let rev = next_rev(&tx)?;
    tx.execute("UPDATE calendars SET deleted = 1, rev = ?1 WHERE id = ?2", params![rev, id])?;
    tx.execute(
        "UPDATE events SET deleted = 1, rev = ?1 WHERE calendar_id = ?2 AND deleted = 0",
        params![rev, id],
    )?;
    tx.commit()?;
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

    let calendar_ok = tx
        .query_row(
            "SELECT 1 FROM calendars WHERE id = ?1 AND owner = ?2 AND deleted = 0",
            params![event.calendar_id, user],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !calendar_ok {
        return Err(Error::NotFound("calendar"));
    }

    match load_event(&tx, &event.id)? {
        Some((current, owner)) => {
            if owner != user {
                return Err(Error::NotFound("event"));
            }
            if current.deleted {
                return Err(Error::Gone);
            }
            if base_rev != Some(current.rev) {
                return Err(Error::Conflict(current));
            }
        }
        None if base_rev.is_some() => return Err(Error::NotFound("event")),
        None => {}
    }

    event.rev = next_rev(&tx)?;
    tx.execute(
        "INSERT INTO events (id, owner, calendar_id, title, location, description, starts_at, ends_at, tz, rev, deleted)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 0)
         ON CONFLICT(id) DO UPDATE SET calendar_id = excluded.calendar_id, title = excluded.title,
             location = excluded.location, description = excluded.description,
             starts_at = excluded.starts_at, ends_at = excluded.ends_at, tz = excluded.tz,
             rev = excluded.rev, deleted = 0",
        params![
            event.id,
            user,
            event.calendar_id,
            event.title,
            event.location,
            event.description,
            event.start.format(DT_FORMAT).to_string(),
            event.end.format(DT_FORMAT).to_string(),
            event.tz,
            event.rev,
        ],
    )?;
    tx.commit()?;
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
    let Some((current, owner)) = load_event(&tx, &id)? else {
        return Err(Error::NotFound("event"));
    };
    if owner != user {
        return Err(Error::NotFound("event"));
    }
    if current.deleted {
        return Ok(StatusCode::NO_CONTENT); // idempotent
    }
    if base_rev.is_some_and(|rev| rev != current.rev) {
        return Err(Error::Conflict(current));
    }
    let rev = next_rev(&tx)?;
    tx.execute("UPDATE events SET deleted = 1, rev = ?1 WHERE id = ?2", params![rev, id])?;
    tx.commit()?;
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
    let Some(user) = state.tokens.get(&query.token) else {
        return Err(Error::NotFound("calendar")); // don't reveal whether the id exists
    };
    let db = state.db.lock().unwrap();
    let calendar = db
        .query_row(
            "SELECT id, name, color, rev, deleted FROM calendars WHERE id = ?1 AND owner = ?2 AND deleted = 0",
            params![calendar_id, user],
            |r| {
                Ok(Calendar {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    color: r.get(2)?,
                    rev: r.get(3)?,
                    deleted: r.get(4)?,
                })
            },
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
    let api = Router::new()
        .route("/sync", get(sync))
        .route("/calendars/{id}", put(put_calendar).delete(delete_calendar))
        .route("/events/{id}", put(put_event).delete(delete_event))
        .layer(middleware::from_fn_with_state(state.clone(), auth));

    Router::new()
        .nest("/api/v1", api)
        .route("/ics/{calendar_id}", get(ics))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(state)
}

fn parse_tokens(spec: &str) -> Result<HashMap<String, String>, String> {
    spec.split(',')
        .filter(|entry| !entry.trim().is_empty())
        .map(|entry| {
            let (user, token) = entry
                .trim()
                .split_once(':')
                .ok_or_else(|| format!("invalid KAIRO_TOKENS entry {entry:?}, expected user:token"))?;
            if user.is_empty() || token.len() < 16 {
                return Err(format!("token for {user:?} must be at least 16 characters"));
            }
            Ok((token.to_string(), user.to_string()))
        })
        .collect()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let tokens = parse_tokens(&std::env::var("KAIRO_TOKENS").map_err(|_| "KAIRO_TOKENS is not set (user:token,...)")?)?;
    if tokens.is_empty() {
        return Err("KAIRO_TOKENS contains no users".into());
    }
    let addr = std::env::var("KAIRO_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let db_path = std::env::var("KAIRO_DB").unwrap_or_else(|_| "kairo.db".into());

    let conn = Connection::open(&db_path)?;
    init_db(&conn)?;
    let state = AppState { db: Arc::new(Mutex::new(conn)), tokens: Arc::new(tokens) };

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("kairo-server listening on http://{addr} (db: {db_path})");
    axum::serve(listener, router(state)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    fn app() -> Router {
        let conn = Connection::open_in_memory().unwrap();
        init_db(&conn).unwrap();
        let tokens = HashMap::from([
            ("alice-token-0123456789".to_string(), "alice".to_string()),
            ("bob-token-0123456789".to_string(), "bob".to_string()),
        ]);
        router(AppState { db: Arc::new(Mutex::new(conn)), tokens: Arc::new(tokens) })
    }

    async fn call(app: &Router, method: &str, uri: &str, token: &str, body: Option<serde_json::Value>) -> (StatusCode, serde_json::Value) {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {token}"));
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

    const ALICE: &str = "alice-token-0123456789";
    const BOB: &str = "bob-token-0123456789";

    fn event(title: &str) -> serde_json::Value {
        json!({ "calendar_id": "c1", "title": title, "start": "2026-10-05T09:00:00", "end": "2026-10-05T10:00:00" })
    }

    #[tokio::test]
    async fn requires_token() {
        let app = app();
        let (status, _) = call(&app, "GET", "/api/v1/sync", "wrong", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn event_lifecycle_with_conflicts_and_delta_sync() {
        let app = app();
        let (s, _) = call(&app, "PUT", "/api/v1/calendars/c1", ALICE, Some(json!({ "name": "Team" }))).await;
        assert_eq!(s, StatusCode::OK);

        // create
        let (s, created) = call(&app, "PUT", "/api/v1/events/e1", ALICE, Some(event("Stand-up"))).await;
        assert_eq!(s, StatusCode::OK);
        let rev1 = created["rev"].as_i64().unwrap();

        // creating again without base_rev collides, updating with a stale one too
        let (s, _) = call(&app, "PUT", "/api/v1/events/e1", ALICE, Some(event("Other"))).await;
        assert_eq!(s, StatusCode::CONFLICT);

        // update with the right base_rev
        let (s, updated) = call(&app, "PUT", &format!("/api/v1/events/e1?base_rev={rev1}"), ALICE, Some(event("Stand-up 2"))).await;
        assert_eq!(s, StatusCode::OK);
        let rev2 = updated["rev"].as_i64().unwrap();
        assert!(rev2 > rev1);

        // stale base_rev now conflicts and returns the current version
        let (s, conflict) = call(&app, "PUT", &format!("/api/v1/events/e1?base_rev={rev1}"), ALICE, Some(event("Stale"))).await;
        assert_eq!(s, StatusCode::CONFLICT);
        assert_eq!(conflict["current"]["title"], "Stand-up 2");

        // delta sync: full, then nothing, then only the deletion
        let (_, full) = call(&app, "GET", "/api/v1/sync?since=0", ALICE, None).await;
        assert_eq!(full["events"].as_array().unwrap().len(), 1);
        let head = full["rev"].as_i64().unwrap();
        let (_, none) = call(&app, "GET", &format!("/api/v1/sync?since={head}"), ALICE, None).await;
        assert!(none["events"].as_array().unwrap().is_empty());

        let (s, _) = call(&app, "DELETE", "/api/v1/events/e1", ALICE, None).await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        let (_, delta) = call(&app, "GET", &format!("/api/v1/sync?since={head}"), ALICE, None).await;
        assert_eq!(delta["events"][0]["deleted"], true);
    }

    #[tokio::test]
    async fn users_are_isolated() {
        let app = app();
        call(&app, "PUT", "/api/v1/calendars/c1", ALICE, Some(json!({ "name": "Team" }))).await;
        call(&app, "PUT", "/api/v1/events/e1", ALICE, Some(event("Secret"))).await;

        let (_, bobs) = call(&app, "GET", "/api/v1/sync", BOB, None).await;
        assert!(bobs["events"].as_array().unwrap().is_empty());
        // Bob can neither write into Alice's calendar nor overwrite or delete her event.
        let (s, _) = call(&app, "PUT", "/api/v1/events/e2", BOB, Some(event("Intruder"))).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let (s, _) = call(&app, "DELETE", "/api/v1/events/e1", BOB, None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[test]
    fn ics_is_escaped_and_folded() {
        let calendar = Calendar { id: "c1".into(), name: "Team".into(), color: default_color(), rev: 1, deleted: false };
        let event = Event {
            id: "e1".into(),
            calendar_id: "c1".into(),
            title: "A, B; C".into(),
            location: String::new(),
            description: "x".repeat(200),
            start: "2026-10-05T09:00:00".parse().unwrap(),
            end: "2026-10-05T10:00:00".parse().unwrap(),
            tz: "Europe/Berlin".into(),
            rev: 3,
            deleted: false,
        };
        let ics = render_ics(&calendar, &[event]);
        assert!(ics.contains("SUMMARY:A\\, B\\; C\r\n"));
        assert!(ics.contains("DTSTART;TZID=Europe/Berlin:20261005T090000\r\n"));
        assert!(ics.lines().all(|line| line.len() <= 75));
    }
}
