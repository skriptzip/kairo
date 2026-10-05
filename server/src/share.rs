//! Shared calendars: membership, roles and invitations.
//!
//! A calendar has one owner and any number of members with the role `editor` (may change
//! events) or `viewer` (read-only). The owner invites a user by name; the invitation stays
//! `accepted = 0` (invisible apart from the invite itself) until that user accepts it.
//!
//! Delta sync works per user: when somebody joins, the calendar and all its events get a new
//! revision so the newcomer's `since` catches them; when somebody leaves or is removed the
//! calendar disappears from `calendar_ids` in their next sync response.

use axum::{
    Extension, Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::json;

use crate::{AppState, Error, User, next_rev};

/// Ids of all calendars `?1` (an email address) may see: owned or accepted membership.
pub const ACCESSIBLE: &str = "(SELECT id FROM calendars WHERE owner = ?1 \
     UNION SELECT calendar_id FROM calendar_members WHERE email = ?1 AND accepted = 1)";

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Access {
    None,
    Viewer,
    Editor,
    Owner,
}

pub fn init_db(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS calendar_members (
             calendar_id TEXT NOT NULL, email TEXT NOT NULL, role TEXT NOT NULL,
             accepted INTEGER NOT NULL DEFAULT 0, invited_by TEXT NOT NULL,
             PRIMARY KEY (calendar_id, email));
         CREATE INDEX IF NOT EXISTS calendar_members_user ON calendar_members (email);",
    )
}

/// `(owner, deleted)` of a calendar, if it exists.
pub fn calendar_info(conn: &Connection, id: &str) -> rusqlite::Result<Option<(String, bool)>> {
    conn.query_row("SELECT owner, deleted FROM calendars WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()
}

pub fn access(conn: &Connection, user: &str, calendar_id: &str) -> rusqlite::Result<Access> {
    let Some((owner, _)) = calendar_info(conn, calendar_id)? else { return Ok(Access::None) };
    if owner == user {
        return Ok(Access::Owner);
    }
    let role: Option<String> = conn
        .query_row(
            "SELECT role FROM calendar_members WHERE calendar_id = ?1 AND email = ?2 AND accepted = 1",
            params![calendar_id, user],
            |r| r.get(0),
        )
        .optional()?;
    Ok(match role.as_deref() {
        Some("editor") => Access::Editor,
        Some("viewer") => Access::Viewer,
        _ => Access::None,
    })
}

/// Owner plus accepted members: everybody who should hear about a change.
pub fn users_of(conn: &Connection, calendar_id: &str) -> rusqlite::Result<Vec<String>> {
    let mut users: Vec<String> = conn
        .prepare("SELECT owner FROM calendars WHERE id = ?1 UNION SELECT email FROM calendar_members WHERE calendar_id = ?1 AND accepted = 1")?
        .query_map([calendar_id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    users.sort();
    users.dedup();
    Ok(users)
}

/// Error for a user who wants to change `calendar_id` but isn't allowed to: members learn
/// that they lack permission, strangers learn nothing.
pub fn denied(access: Access, what: &'static str) -> Error {
    if access >= Access::Viewer { Error::Forbidden(what) } else { Error::NotFound("calendar") }
}

/// Gives the calendar and all its events a fresh revision so every member's next delta sync
/// includes them again.
fn touch(conn: &Connection, calendar_id: &str, rev: i64) -> rusqlite::Result<()> {
    conn.execute("UPDATE calendars SET rev = ?1 WHERE id = ?2", params![rev, calendar_id])?;
    conn.execute("UPDATE events SET rev = ?1 WHERE calendar_id = ?2", params![rev, calendar_id])?;
    Ok(())
}

fn require_owner(conn: &Connection, user: &str, calendar_id: &str) -> Result<(), Error> {
    match calendar_info(conn, calendar_id)? {
        Some((owner, false)) if owner == user => Ok(()),
        Some((_, false)) => Err(denied(access(conn, user, calendar_id)?, "only the owner can do this")),
        _ => Err(Error::NotFound("calendar")),
    }
}

// ---------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------

pub async fn list_members(
    State(state): State<AppState>,
    Extension(User(user)): Extension<User>,
    Path(id): Path<String>,
) -> Result<Response, Error> {
    let db = state.db.lock().unwrap();
    let level = access(&db, &user, &id)?;
    if level == Access::None {
        return Err(Error::NotFound("calendar"));
    }
    let (owner, _) = calendar_info(&db, &id)?.ok_or(Error::NotFound("calendar"))?;
    let members = db
        .prepare(
            "SELECT email, role, accepted FROM calendar_members WHERE calendar_id = ?1
             AND (accepted = 1 OR ?2) ORDER BY email",
        )?
        .query_map(params![id, level == Access::Owner], |r| {
            Ok(json!({ "email": r.get::<_, String>(0)?, "role": r.get::<_, String>(1)?, "accepted": r.get::<_, bool>(2)? }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Json(json!({ "owner": owner, "members": members })).into_response())
}

#[derive(Deserialize)]
pub struct InviteBody {
    role: String,
}

/// Invites a user (or changes the role of an existing member). Owner only.
pub async fn invite(
    State(state): State<AppState>,
    Extension(User(user)): Extension<User>,
    Path((id, invitee)): Path<(String, String)>,
    Json(body): Json<InviteBody>,
) -> Result<Response, Error> {
    let invitee = invitee.trim().to_lowercase();
    if body.role != "editor" && body.role != "viewer" {
        return Err(Error::Invalid("role must be editor or viewer".into()));
    }
    if invitee == user {
        return Err(Error::Invalid("you already own this calendar".into()));
    }
    let mut db = state.db.lock().unwrap();
    let tx = db.transaction()?;
    require_owner(&tx, &user, &id)?;
    let exists = tx
        .query_row("SELECT 1 FROM users WHERE email = ?1", [&invitee], |_| Ok(()))
        .optional()?
        .is_some();
    if !exists {
        return Err(Error::NotFound("user"));
    }
    let was_accepted: bool = tx
        .query_row(
            "SELECT accepted FROM calendar_members WHERE calendar_id = ?1 AND email = ?2",
            params![id, invitee],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(false);
    let rev = next_rev(&tx)?;
    tx.execute(
        "INSERT INTO calendar_members (calendar_id, email, role, accepted, invited_by)
         VALUES (?1, ?2, ?3, 0, ?4)
         ON CONFLICT(calendar_id, email) DO UPDATE SET role = excluded.role",
        params![id, invitee, body.role, user],
    )?;
    if was_accepted {
        // The member's role changed: make it reach their client.
        tx.execute("UPDATE calendars SET rev = ?1 WHERE id = ?2", params![rev, id])?;
    }
    let mut audience = users_of(&tx, &id)?;
    audience.push(invitee.clone());
    tx.commit()?;
    state.notify_all(&audience, rev);
    Ok(Json(json!({ "email": invitee, "role": body.role, "accepted": was_accepted })).into_response())
}

/// The invited user joins the calendar.
pub async fn accept(
    State(state): State<AppState>,
    Extension(User(user)): Extension<User>,
    Path(id): Path<String>,
) -> Result<StatusCode, Error> {
    let mut db = state.db.lock().unwrap();
    let tx = db.transaction()?;
    if !matches!(calendar_info(&tx, &id)?, Some((_, false))) {
        return Err(Error::NotFound("invitation"));
    }
    let n = tx.execute(
        "UPDATE calendar_members SET accepted = 1 WHERE calendar_id = ?1 AND email = ?2 AND accepted = 0",
        params![id, user],
    )?;
    if n == 0 {
        return Err(Error::NotFound("invitation"));
    }
    let rev = next_rev(&tx)?;
    touch(&tx, &id, rev)?;
    let audience = users_of(&tx, &id)?;
    tx.commit()?;
    state.notify_all(&audience, rev);
    Ok(StatusCode::NO_CONTENT)
}

/// Owner removes a member / withdraws an invitation, or a member leaves / declines.
pub async fn remove_member(
    State(state): State<AppState>,
    Extension(User(user)): Extension<User>,
    Path((id, target)): Path<(String, String)>,
) -> Result<StatusCode, Error> {
    let target = target.trim().to_lowercase();
    let mut db = state.db.lock().unwrap();
    let tx = db.transaction()?;
    let Some((owner, _)) = calendar_info(&tx, &id)? else { return Err(Error::NotFound("calendar")) };
    if target == owner {
        return Err(Error::Invalid("the owner can't leave; delete the calendar instead".into()));
    }
    if user != owner && user != target {
        return Err(denied(access(&tx, &user, &id)?, "only the owner can remove other members"));
    }
    let n = tx.execute("DELETE FROM calendar_members WHERE calendar_id = ?1 AND email = ?2", params![id, target])?;
    if n == 0 {
        return Err(Error::NotFound("member"));
    }
    let rev = next_rev(&tx)?;
    let mut audience = users_of(&tx, &id)?;
    audience.push(target);
    tx.commit()?;
    state.notify_all(&audience, rev);
    Ok(StatusCode::NO_CONTENT)
}
