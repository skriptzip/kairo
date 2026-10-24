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
use serde::Deserialize;
use serde_json::json;

use crate::{
    AppState, Error, User,
    db::{self, Conn, n, next_rev, query, s},
};

/// Ids of all calendars a user may see: owned or accepted membership. Binds the email twice.
pub const ACCESSIBLE: &str = "(SELECT id FROM calendars WHERE owner = ? \
     UNION SELECT calendar_id FROM calendar_members WHERE email = ? AND accepted = 1)";

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Access {
    None,
    Viewer,
    Editor,
    Owner,
}

pub async fn init_db(conn: &mut Conn) -> db::Result<()> {
    db::create_table(
        conn,
        "calendar_members",
        "calendar_id VARCHAR(255) NOT NULL, email VARCHAR(255) NOT NULL, role VARCHAR(32) NOT NULL,
         accepted BIGINT NOT NULL DEFAULT 0, invited_by VARCHAR(255) NOT NULL,
         PRIMARY KEY (calendar_id, email)",
    )
    .await?;
    db::create_index(conn, "calendar_members_user", "calendar_members", "email").await
}

/// `(owner, deleted)` of a calendar, if it exists.
pub async fn calendar_info(conn: &mut Conn, id: &str) -> db::Result<Option<(String, bool)>> {
    query("SELECT owner, deleted FROM calendars WHERE id = ?")
        .bind(id)
        .fetch_optional(conn)
        .await?
        .map(|r| Ok((s(&r, 0)?, n(&r, 1)? != 0)))
        .transpose()
}

pub async fn access(conn: &mut Conn, user: &str, calendar_id: &str) -> db::Result<Access> {
    let Some((owner, _)) = calendar_info(conn, calendar_id).await? else { return Ok(Access::None) };
    if owner == user {
        return Ok(Access::Owner);
    }
    let role = query("SELECT role FROM calendar_members WHERE calendar_id = ? AND email = ? AND accepted = 1")
        .bind(calendar_id)
        .bind(user)
        .fetch_optional(conn)
        .await?
        .map(|r| s(&r, 0))
        .transpose()?;
    Ok(match role.as_deref() {
        Some("editor") => Access::Editor,
        Some("viewer") => Access::Viewer,
        _ => Access::None,
    })
}

/// Owner plus accepted members: everybody who should hear about a change.
pub async fn users_of(conn: &mut Conn, calendar_id: &str) -> db::Result<Vec<String>> {
    let rows = query(
        "SELECT owner FROM calendars WHERE id = ?
         UNION SELECT email FROM calendar_members WHERE calendar_id = ? AND accepted = 1",
    )
    .bind(calendar_id)
    .bind(calendar_id)
    .fetch_all(conn)
    .await?;
    let mut users = rows.iter().map(|r| s(r, 0)).collect::<db::Result<Vec<String>>>()?;
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
async fn touch(conn: &mut Conn, calendar_id: &str, rev: i64) -> db::Result<()> {
    query("UPDATE calendars SET rev = ? WHERE id = ?").bind(rev).bind(calendar_id).execute(&mut *conn).await?;
    query("UPDATE events SET rev = ? WHERE calendar_id = ?").bind(rev).bind(calendar_id).execute(conn).await?;
    Ok(())
}

async fn require_owner(conn: &mut Conn, user: &str, calendar_id: &str) -> Result<(), Error> {
    match calendar_info(conn, calendar_id).await? {
        Some((owner, false)) if owner == user => Ok(()),
        Some((_, false)) => Err(denied(access(conn, user, calendar_id).await?, "only the owner can do this")),
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
    let mut conn = state.db.conn().await?;
    let level = access(&mut conn, &user, &id).await?;
    if level == Access::None {
        return Err(Error::NotFound("calendar"));
    }
    let (owner, _) = calendar_info(&mut conn, &id).await?.ok_or(Error::NotFound("calendar"))?;
    let rows = query(
        "SELECT email, role, accepted FROM calendar_members WHERE calendar_id = ?
         AND (accepted = 1 OR ? = 1) ORDER BY email",
    )
    .bind(&id)
    .bind(i64::from(level == Access::Owner))
    .fetch_all(&mut *conn)
    .await?;
    let members = rows
        .iter()
        .map(|r| Ok::<_, sqlx::Error>(json!({ "email": s(r, 0)?, "role": s(r, 1)?, "accepted": n(r, 2)? != 0 })))
        .collect::<Result<Vec<_>, _>>()?;
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
    let mut tx = state.db.begin().await?;
    let rev = next_rev(&mut tx).await?; // also serializes concurrent writers
    require_owner(&mut tx, &user, &id).await?;
    let exists = query("SELECT email FROM users WHERE email = ?").bind(&invitee).fetch_optional(&mut *tx).await?.is_some();
    if !exists {
        return Err(Error::NotFound("user"));
    }
    let was_accepted = query("SELECT accepted FROM calendar_members WHERE calendar_id = ? AND email = ?")
        .bind(&id)
        .bind(&invitee)
        .fetch_optional(&mut *tx)
        .await?
        .map(|r| n(&r, 0))
        .transpose()?
        .is_some_and(|accepted| accepted != 0);
    query(&db::upsert(
        "calendar_members",
        &["calendar_id", "email", "role", "accepted", "invited_by"],
        &["calendar_id", "email"],
        &["role"],
    ))
    .bind(&id)
    .bind(&invitee)
    .bind(&body.role)
    .bind(0_i64)
    .bind(&user)
    .execute(&mut *tx)
    .await?;
    if was_accepted {
        // The member's role changed: make it reach their client.
        query("UPDATE calendars SET rev = ? WHERE id = ?").bind(rev).bind(&id).execute(&mut *tx).await?;
    }
    let mut audience = users_of(&mut tx, &id).await?;
    audience.push(invitee.clone());
    tx.commit().await?;
    state.notify_all(&audience, rev);
    Ok(Json(json!({ "email": invitee, "role": body.role, "accepted": was_accepted })).into_response())
}

/// The invited user joins the calendar.
pub async fn accept(
    State(state): State<AppState>,
    Extension(User(user)): Extension<User>,
    Path(id): Path<String>,
) -> Result<StatusCode, Error> {
    let mut tx = state.db.begin().await?;
    let rev = next_rev(&mut tx).await?;
    if !matches!(calendar_info(&mut tx, &id).await?, Some((_, false))) {
        return Err(Error::NotFound("invitation"));
    }
    let done = query("UPDATE calendar_members SET accepted = 1 WHERE calendar_id = ? AND email = ? AND accepted = 0")
        .bind(&id)
        .bind(&user)
        .execute(&mut *tx)
        .await?;
    if done.rows_affected() == 0 {
        return Err(Error::NotFound("invitation"));
    }
    touch(&mut tx, &id, rev).await?;
    let audience = users_of(&mut tx, &id).await?;
    tx.commit().await?;
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
    let mut tx = state.db.begin().await?;
    let rev = next_rev(&mut tx).await?;
    let Some((owner, _)) = calendar_info(&mut tx, &id).await? else { return Err(Error::NotFound("calendar")) };
    if target == owner {
        return Err(Error::Invalid("the owner can't leave; delete the calendar instead".into()));
    }
    if user != owner && user != target {
        return Err(denied(access(&mut tx, &user, &id).await?, "only the owner can remove other members"));
    }
    let done = query("DELETE FROM calendar_members WHERE calendar_id = ? AND email = ?")
        .bind(&id)
        .bind(&target)
        .execute(&mut *tx)
        .await?;
    if done.rows_affected() == 0 {
        return Err(Error::NotFound("member"));
    }
    let mut audience = users_of(&mut tx, &id).await?;
    audience.push(target);
    tx.commit().await?;
    state.notify_all(&audience, rev);
    Ok(StatusCode::NO_CONTENT)
}
