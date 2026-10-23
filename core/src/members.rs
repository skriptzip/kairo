//! Sharing a calendar with other people: members, invitations (blocking calls, run them on a
//! background thread).

use serde::Deserialize;
use serde_json::json;

use crate::api::{self, Session};

#[derive(Clone, Debug, Deserialize)]
pub struct Member {
    pub email: String,
    pub role: String,
    pub accepted: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct MemberList {
    pub owner: String,
    pub members: Vec<Member>,
}

#[derive(Deserialize)]
struct ErrorBody {
    #[serde(default)]
    error: String,
}

fn bearer(session: &Session) -> String {
    format!("Bearer {}", session.token)
}

fn url(session: &Session, calendar_id: &str, rest: &str) -> String {
    format!("{}/api/v1/calendars/{calendar_id}{rest}", session.server)
}

fn failure(status: u16, body: Option<ErrorBody>, not_found: &str) -> String {
    match status {
        401 => "Deine Sitzung ist abgelaufen. Bitte neu anmelden.".into(),
        403 => "Das darf nur der Besitzer des Kalenders.".into(),
        404 => not_found.into(),
        429 => "Zu viele Anfragen. Bitte später erneut versuchen.".into(),
        400 => match body.map(|b| b.error).unwrap_or_default().as_str() {
            e if e.contains("already own") => "Du bist bereits Besitzer dieses Kalenders.".into(),
            e if e.contains("can't leave") => "Der Besitzer kann nicht austreten – lösche stattdessen den Kalender.".into(),
            _ => "Ungültige Eingabe.".into(),
        },
        _ => format!("Serverfehler (HTTP {status})."),
    }
}

fn unreachable(err: ureq::Error) -> String {
    format!("Server nicht erreichbar: {err}")
}

pub fn list(session: &Session, calendar_id: &str) -> Result<MemberList, String> {
    let mut res = api::agent()
        .get(&url(session, calendar_id, "/members"))
        .header("Authorization", &bearer(session))
        .call()
        .map_err(unreachable)?;
    let status = res.status().as_u16();
    if status != 200 {
        return Err(failure(status, res.body_mut().read_json().ok(), "Kalender nicht gefunden."));
    }
    res.body_mut().read_json().map_err(|_| "Unerwartete Antwort des Servers.".to_string())
}

/// Invites `email`, or changes the role of somebody who already joined.
pub fn invite(session: &Session, calendar_id: &str, email: &str, role: &str) -> Result<(), String> {
    let email = email.trim().to_lowercase();
    if email.is_empty() {
        return Err("Bitte eine E-Mail-Adresse eingeben.".into());
    }
    let mut res = api::agent()
        .put(&url(session, calendar_id, &format!("/members/{email}")))
        .header("Authorization", &bearer(session))
        .send_json(&json!({ "role": role }))
        .map_err(unreachable)?;
    let status = res.status().as_u16();
    if status == 200 {
        return Ok(());
    }
    Err(failure(status, res.body_mut().read_json().ok(), "Diesen Benutzer gibt es nicht."))
}

/// Removes a member or withdraws an invitation (owner), or leaves / declines (yourself).
pub fn remove(session: &Session, calendar_id: &str, email: &str) -> Result<(), String> {
    let mut res = api::agent()
        .delete(&url(session, calendar_id, &format!("/members/{email}")))
        .header("Authorization", &bearer(session))
        .call()
        .map_err(unreachable)?;
    let status = res.status().as_u16();
    if status == 204 {
        return Ok(());
    }
    Err(failure(status, res.body_mut().read_json().ok(), "Mitglied nicht gefunden."))
}

pub fn accept(session: &Session, calendar_id: &str) -> Result<(), String> {
    let mut res = api::agent()
        .post(&url(session, calendar_id, "/accept"))
        .header("Authorization", &bearer(session))
        .send_empty()
        .map_err(unreachable)?;
    let status = res.status().as_u16();
    if status == 204 {
        return Ok(());
    }
    Err(failure(status, res.body_mut().read_json().ok(), "Die Einladung gibt es nicht mehr."))
}

pub fn role_label(role: &str) -> &'static str {
    match role {
        "owner" => "Besitzer",
        "editor" => "Bearbeiten",
        _ => "Ansehen",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{Event, Store, default_status, default_tz, new_id},
        sync,
    };

    fn round(store: &mut Store, session: &Session) {
        let out = sync::run(store.job(session));
        assert!(out.failure.is_none(), "{:?}", out.failure);
        store.apply(out);
    }

    /// Needs a running server: `KAIRO_TEST_SERVER=http://127.0.0.1:8099 cargo test -- --ignored`
    #[test]
    #[ignore]
    fn share_a_calendar_between_two_accounts() {
        let server = std::env::var("KAIRO_TEST_SERVER").expect("KAIRO_TEST_SERVER");
        let id = std::process::id();
        let pw = "correct horse battery";
        let alice = api::authenticate(&server, &format!("al{id}@example.com"), pw, true, "").unwrap();
        let bob = api::authenticate(&server, &format!("bo{id}@example.com"), pw, true, "").unwrap();

        // Alice has a calendar with one event.
        let mut a = Store::default();
        round(&mut a, &alice);
        round(&mut a, &alice);
        let cal = a.calendars[0].id.clone();
        let event_id = new_id();
        let day = chrono::NaiveDate::from_ymd_opt(2026, 10, 5).unwrap();
        a.events.push(Event {
            id: event_id.clone(),
            calendar: cal.clone(),
            title: "Planung".into(),
            location: String::new(),
            description: String::new(),
            start: day.and_hms_opt(9, 0, 0).unwrap(),
            end: day.and_hms_opt(10, 0, 0).unwrap(),
            tz: default_tz(),
            status: default_status(),
            repeat: String::new(),
            repeat_until: None,
            repeat_interval: 1,
            repeat_days: 0,
            repeat_nth: 0,
            exceptions: Vec::new(),
            remind: None,
            attendees: Vec::new(),
            color: None,
            private: false,
            categories: Vec::new(),
            rev: 0,
            dirty: true,
            draft: false,
        });
        round(&mut a, &alice);

        // Unknown users are rejected; Bob gets an invitation and nothing else.
        assert!(invite(&alice, &cal, "nobody-here@example.com", "editor").is_err());
        invite(&alice, &cal, &bob.email, "editor").unwrap();
        let mut b = Store::default();
        round(&mut b, &bob);
        assert_eq!(b.invites.len(), 1);
        assert!(b.events.is_empty());

        // Accepting brings the calendar and its existing event over.
        accept(&bob, &cal).unwrap();
        round(&mut b, &bob);
        assert!(b.invites.is_empty());
        let shared = b.calendars.iter().find(|c| c.id == cal).unwrap();
        assert_eq!(shared.role, "editor");
        assert_eq!(b.events.len(), 1);

        // Bob edits it, Alice sees the change.
        b.events[0].title = "Planung (Bob)".into();
        b.events[0].dirty = true;
        round(&mut b, &bob);
        round(&mut a, &alice);
        assert_eq!(a.events[0].title, "Planung (Bob)");

        // The owner sees both people; downgrading Bob to viewer reaches his client.
        let members = list(&alice, &cal).unwrap();
        assert_eq!(members.members.len(), 1);
        invite(&alice, &cal, &bob.email, "viewer").unwrap();
        round(&mut b, &bob);
        assert_eq!(b.calendars.iter().find(|c| c.id == cal).unwrap().role, "viewer");

        // Bob leaves: the calendar is gone for him, still there for Alice.
        b.remove_calendar(&cal);
        round(&mut b, &bob);
        round(&mut b, &bob);
        assert!(b.calendars.iter().all(|c| c.id != cal) && b.pending_calendar_removals.is_empty());
        assert!(list(&alice, &cal).unwrap().members.is_empty());
        round(&mut a, &alice);
        assert_eq!(a.events.len(), 1);

        // Alice deletes the calendar for everybody.
        a.remove_calendar(&cal);
        round(&mut a, &alice);
        assert!(a.pending_calendar_removals.is_empty());
    }
}
