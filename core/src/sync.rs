//! Two-way sync with the Kairo server.
//!
//! A sync round (`run`, blocking, meant for a background thread) first pushes local
//! changes and then pulls everything newer than `Store::last_rev`. The result (`Outcome`)
//! is merged back into the live `Store` on the UI thread with `Store::apply`, so edits made
//! while the round was in flight are never lost.
//!
//! Conflicts: if someone else changed an event since we last saw it, the server answers
//! `409` with its current version. The local edit wins and is re-sent on top of that version.

use std::time::Duration;

use chrono::{NaiveDate, NaiveDateTime};
use serde::{Deserialize, Serialize};

use crate::{
    api::Session,
    model::{
        CalendarInfo, Event, Exception, Invite, PendingCalendarRemoval, PendingDelete, Store, default_status, default_tz, new_id,
    },
};

#[derive(Serialize, Deserialize)]
pub struct WireCalendar {
    pub id: String,
    pub name: String,
    pub color: String,
    #[serde(default)]
    pub rev: i64,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub owner: String,
    #[serde(default = "owner_role")]
    pub role: String,
}

fn owner_role() -> String {
    "owner".into()
}

#[derive(Serialize, Deserialize)]
pub struct WireEvent {
    pub id: String,
    pub calendar_id: String,
    pub title: String,
    #[serde(default)]
    pub location: String,
    #[serde(default)]
    pub description: String,
    pub start: NaiveDateTime,
    pub end: NaiveDateTime,
    #[serde(default = "default_tz")]
    pub tz: String,
    #[serde(default = "default_status")]
    pub status: String,
    #[serde(default)]
    pub repeat: String,
    #[serde(default)]
    pub repeat_until: Option<NaiveDate>,
    #[serde(default = "crate::model::one")]
    pub repeat_interval: u32,
    #[serde(default)]
    pub repeat_days: u8,
    #[serde(default)]
    pub repeat_nth: i8,
    #[serde(default)]
    pub exceptions: Vec<Exception>,
    #[serde(default)]
    pub remind: Option<u32>,
    #[serde(default)]
    pub attendees: Vec<String>,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub private: bool,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub rev: i64,
    #[serde(default)]
    pub deleted: bool,
}

#[derive(Deserialize)]
struct SyncResponse {
    rev: i64,
    calendars: Vec<WireCalendar>,
    events: Vec<WireEvent>,
    #[serde(default)]
    calendar_ids: Vec<String>,
    #[serde(default)]
    invites: Vec<Invite>,
}

#[derive(Deserialize)]
struct Conflict {
    current: WireEvent,
}

impl From<&Event> for WireEvent {
    fn from(e: &Event) -> Self {
        WireEvent {
            id: e.id.clone(),
            calendar_id: e.calendar.clone(),
            title: e.title.clone(),
            location: e.location.clone(),
            description: e.description.clone(),
            start: e.start,
            end: e.end,
            tz: e.tz.clone(),
            status: e.status.clone(),
            repeat: e.repeat.clone(),
            repeat_until: e.repeat_until,
            repeat_interval: e.repeat_interval,
            repeat_days: e.repeat_days,
            repeat_nth: e.repeat_nth,
            exceptions: e.exceptions.clone(),
            remind: e.remind,
            attendees: e.attendees.clone(),
            color: e.color.clone(),
            private: e.private,
            categories: e.categories.clone(),
            rev: e.rev,
            deleted: false,
        }
    }
}

pub struct Job {
    pub session: Session,
    pub since: i64,
    pub calendars: Vec<CalendarInfo>,
    pub events: Vec<Event>,
    pub deletes: Vec<PendingDelete>,
    pub calendar_removals: Vec<PendingCalendarRemoval>,
}

#[derive(Debug, Clone)]
pub enum Failure {
    /// Server unreachable; everything stays queued.
    Offline,
    /// The token is no longer valid.
    Unauthorized,
    Server(String),
}

pub struct Pulled {
    pub rev: i64,
    pub calendars: Vec<WireCalendar>,
    pub events: Vec<WireEvent>,
    /// All calendars the user can see right now.
    pub calendar_ids: Vec<String>,
    pub invites: Vec<Invite>,
}

#[derive(Default)]
pub struct Outcome {
    /// What was uploaded (as it was when the round started) and the revision the server gave it.
    pub pushed_calendars: Vec<(CalendarInfo, i64)>,
    pub pushed_events: Vec<(Event, i64)>,
    pub done_deletes: Vec<String>,
    pub done_calendar_removals: Vec<String>,
    /// Uploads the server refused (we lost access meanwhile).
    pub rejected_calendars: Vec<String>,
    pub rejected_events: Vec<String>,
    pub pulled: Option<Pulled>,
    pub failure: Option<Failure>,
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .http_status_as_error(false)
        .build()
        .into()
}

fn transport(_: ureq::Error) -> Failure {
    Failure::Offline
}

fn status_failure(status: u16) -> Failure {
    if status == 401 { Failure::Unauthorized } else { Failure::Server(format!("Serverfehler (HTTP {status})")) }
}

fn bearer(session: &Session) -> String {
    format!("Bearer {}", session.token)
}

/// `None` means the server refused because we have no write access (any more).
fn push_calendar(agent: &ureq::Agent, session: &Session, calendar: &CalendarInfo) -> Result<Option<i64>, Failure> {
    let body = serde_json::json!({ "name": calendar.name, "color": calendar.color });
    let mut res = agent
        .put(&format!("{}/api/v1/calendars/{}", session.server, calendar.id))
        .header("Authorization", &bearer(session))
        .send_json(&body)
        .map_err(transport)?;
    match res.status().as_u16() {
        200 => {
            let saved: WireCalendar = res.body_mut().read_json().map_err(|e| Failure::Server(e.to_string()))?;
            Ok(Some(saved.rev))
        }
        403 | 404 => Ok(None),
        status => Err(status_failure(status)),
    }
}

fn push_event(agent: &ureq::Agent, session: &Session, event: &Event) -> Result<Option<i64>, Failure> {
    let mut base = (event.rev > 0).then_some(event.rev);
    let body = WireEvent::from(event);
    for _ in 0..3 {
        let query = base.map(|b| format!("?base_rev={b}")).unwrap_or_default();
        let mut res = agent
            .put(&format!("{}/api/v1/events/{}{query}", session.server, event.id))
            .header("Authorization", &bearer(session))
            .send_json(&body)
            .map_err(transport)?;
        match res.status().as_u16() {
            200 => {
                let saved: WireEvent = res.body_mut().read_json().map_err(|e| Failure::Server(e.to_string()))?;
                return Ok(Some(saved.rev));
            }
            // Changed elsewhere (or a retry of an upload whose answer got lost): local wins.
            409 => {
                let conflict: Conflict = res.body_mut().read_json().map_err(|e| Failure::Server(e.to_string()))?;
                base = Some(conflict.current.rev);
            }
            // Read-only calendar, or no access any more.
            403 | 404 => return Ok(None),
            status => return Err(status_failure(status)),
        }
    }
    Err(Failure::Server("Konflikt konnte nicht aufgelöst werden".into()))
}

fn push_delete(agent: &ureq::Agent, session: &Session, delete: &PendingDelete) -> Result<(), Failure> {
    let mut base = delete.rev;
    for _ in 0..3 {
        let mut res = agent
            .delete(&format!("{}/api/v1/events/{}?base_rev={base}", session.server, delete.id))
            .header("Authorization", &bearer(session))
            .call()
            .map_err(transport)?;
        match res.status().as_u16() {
            // Already gone on the server counts as deleted.
            204 | 404 | 410 => return Ok(()),
            409 => {
                let conflict: Conflict = res.body_mut().read_json().map_err(|e| Failure::Server(e.to_string()))?;
                base = conflict.current.rev;
            }
            status => return Err(status_failure(status)),
        }
    }
    Err(Failure::Server("Konflikt konnte nicht aufgelöst werden".into()))
}

fn push_calendar_removal(
    agent: &ureq::Agent,
    session: &Session,
    removal: &PendingCalendarRemoval,
) -> Result<(), Failure> {
    let url = if removal.leave {
        format!("{}/api/v1/calendars/{}/members/{}", session.server, removal.id, session.email)
    } else {
        format!("{}/api/v1/calendars/{}", session.server, removal.id)
    };
    let res = agent.delete(&url).header("Authorization", &bearer(session)).call().map_err(transport)?;
    match res.status().as_u16() {
        // Already gone, or not ours any more: nothing left to do.
        204 | 403 | 404 => Ok(()),
        status => Err(status_failure(status)),
    }
}

fn pull(agent: &ureq::Agent, session: &Session, since: i64) -> Result<Pulled, Failure> {
    let mut res = agent
        .get(&format!("{}/api/v1/sync?since={since}", session.server))
        .header("Authorization", &bearer(session))
        .call()
        .map_err(transport)?;
    let status = res.status().as_u16();
    if status != 200 {
        return Err(status_failure(status));
    }
    let body: SyncResponse = res.body_mut().read_json().map_err(|e| Failure::Server(e.to_string()))?;
    Ok(Pulled {
        rev: body.rev,
        calendars: body.calendars,
        events: body.events,
        calendar_ids: body.calendar_ids,
        invites: body.invites,
    })
}

/// One full round. Stops at the first failure but still reports what succeeded before it.
pub fn run(job: Job) -> Outcome {
    let agent = agent();
    let session = &job.session;
    let mut out = Outcome::default();

    let result = (|| -> Result<(), Failure> {
        // Calendars first: events refer to them.
        for calendar in &job.calendars {
            match push_calendar(&agent, session, calendar)? {
                Some(rev) => out.pushed_calendars.push((calendar.clone(), rev)),
                None => out.rejected_calendars.push(calendar.id.clone()),
            }
        }
        for event in &job.events {
            match push_event(&agent, session, event)? {
                Some(rev) => out.pushed_events.push((event.clone(), rev)),
                None => out.rejected_events.push(event.id.clone()),
            }
        }
        for delete in &job.deletes {
            push_delete(&agent, session, delete)?;
            out.done_deletes.push(delete.id.clone());
        }
        for removal in &job.calendar_removals {
            push_calendar_removal(&agent, session, removal)?;
            out.done_calendar_removals.push(removal.id.clone());
        }
        out.pulled = Some(pull(&agent, session, job.since)?);
        Ok(())
    })();
    out.failure = result.err();
    out
}

const DEFAULT_CALENDARS: [(&str, &str); 1] = [("Persönlich", "#8764b8")];

impl Store {
    pub fn has_pending(&self) -> bool {
        !self.pending_deletes.is_empty()
            || !self.pending_calendar_removals.is_empty()
            || self.calendars.iter().any(|c| c.dirty && !c.draft)
            || self.events.iter().any(|e| e.dirty && !e.draft)
    }

    /// Everything that still has to be uploaded, as a snapshot for a background round.
    pub fn job(&self, session: &Session) -> Job {
        Job {
            session: session.clone(),
            since: self.last_rev,
            calendars: self.calendars.iter().filter(|c| c.dirty && !c.draft && c.is_owner()).cloned().collect(),
            events: self.events.iter().filter(|e| e.dirty && !e.draft).cloned().collect(),
            deletes: self.pending_deletes.clone(),
            calendar_removals: self.pending_calendar_removals.clone(),
        }
    }

    /// Deletes a calendar you own, or leaves one that is shared with you.
    pub fn remove_calendar(&mut self, id: &str) {
        let Some(i) = self.calendars.iter().position(|c| c.id == id) else { return };
        let calendar = self.calendars.remove(i);
        self.events.retain(|e| e.calendar != id);
        if calendar.rev > 0 {
            let leave = !calendar.is_owner();
            self.pending_calendar_removals.push(PendingCalendarRemoval { id: calendar.id, leave });
        }
    }

    /// Merges a finished round into the current state. Returns true if another round should
    /// run right away (something was refused and the local view must be rebuilt from scratch).
    pub fn apply(&mut self, out: Outcome) -> bool {
        // 1. Uploads: remember the new revision; the item is clean only if it wasn't edited meanwhile.
        for (sent, rev) in out.pushed_calendars {
            if let Some(local) = self.calendars.iter_mut().find(|c| c.id == sent.id) {
                local.rev = rev;
                local.dirty = !local.same_content(&sent);
            }
        }
        for (sent, rev) in out.pushed_events {
            if let Some(local) = self.events.iter_mut().find(|e| e.id == sent.id) {
                local.rev = rev;
                local.dirty = !local.same_content(&sent);
            }
        }
        self.pending_deletes.retain(|d| !out.done_deletes.contains(&d.id));
        self.pending_calendar_removals.retain(|r| !out.done_calendar_removals.contains(&r.id));

        // Refused uploads: drop our version and pull everything again to see the server's.
        let refused = !out.rejected_calendars.is_empty() || !out.rejected_events.is_empty();
        for id in &out.rejected_calendars {
            if let Some(local) = self.calendars.iter_mut().find(|c| &c.id == id) {
                local.dirty = false;
            }
        }
        self.events.retain(|e| !out.rejected_events.contains(&e.id));

        // 2. Downloads. Local edits that are still waiting for upload win over server content,
        //    but adopt the server revision so the next upload replaces that version.
        if let Some(pulled) = out.pulled {
            self.apply_pull(pulled);
        }
        if refused {
            self.last_rev = 0;
        }
        refused
    }

    fn apply_pull(&mut self, pulled: Pulled) {
        let removing = |removals: &[PendingCalendarRemoval], id: &str| removals.iter().any(|r| r.id == id);
        for wire in pulled.calendars {
            if removing(&self.pending_calendar_removals, &wire.id) {
                continue;
            }
            let position = self.calendars.iter().position(|c| c.id == wire.id);
            if wire.deleted {
                self.calendars.retain(|c| c.id != wire.id);
                self.events.retain(|e| e.calendar != wire.id);
                continue;
            }
            match position {
                Some(i) => {
                    let local = &mut self.calendars[i];
                    local.rev = wire.rev;
                    local.owner = wire.owner;
                    local.role = wire.role;
                    if !local.dirty {
                        local.name = wire.name;
                        local.color = wire.color;
                    }
                }
                None => self.calendars.push(CalendarInfo {
                    id: wire.id,
                    name: wire.name,
                    color: wire.color,
                    rev: wire.rev,
                    dirty: false,
                    visible: true,
                    owner: wire.owner,
                    role: wire.role,
                    draft: false,
                }),
            }
        }
        for wire in pulled.events {
            let position = self.events.iter().position(|e| e.id == wire.id);
            if wire.deleted {
                // A local edit of an event deleted elsewhere brings it back on the next upload.
                if let Some(i) = position {
                    if self.events[i].dirty {
                        self.events[i].rev = wire.rev;
                    } else {
                        self.events.remove(i);
                    }
                }
                continue;
            }
            if self.pending_deletes.iter().any(|d| d.id == wire.id)
                || removing(&self.pending_calendar_removals, &wire.calendar_id)
            {
                continue;
            }
            match position {
                Some(i) => {
                    let local = &mut self.events[i];
                    local.rev = wire.rev;
                    if !local.dirty {
                        local.calendar = wire.calendar_id;
                        local.title = wire.title;
                        local.location = wire.location;
                        local.description = wire.description;
                        local.start = wire.start;
                        local.end = wire.end;
                        local.tz = wire.tz;
                        local.status = wire.status;
                        local.repeat = wire.repeat;
                        local.repeat_until = wire.repeat_until;
                        local.repeat_interval = wire.repeat_interval;
                        local.repeat_days = wire.repeat_days;
                        local.repeat_nth = wire.repeat_nth;
                        local.exceptions = wire.exceptions;
                        local.remind = wire.remind;
                        local.attendees = wire.attendees;
                        local.color = wire.color;
                        local.private = wire.private;
                        local.categories = wire.categories;
                    }
                }
                None => self.events.push(Event {
                    id: wire.id,
                    calendar: wire.calendar_id,
                    title: wire.title,
                    location: wire.location,
                    description: wire.description,
                    start: wire.start,
                    end: wire.end,
                    tz: wire.tz,
                    status: wire.status,
                    repeat: wire.repeat,
                    repeat_until: wire.repeat_until,
                    repeat_interval: wire.repeat_interval,
                    repeat_days: wire.repeat_days,
                    repeat_nth: wire.repeat_nth,
                    exceptions: wire.exceptions,
                    remind: wire.remind,
                    attendees: wire.attendees,
                    color: wire.color,
                    private: wire.private,
                    categories: wire.categories,
                    rev: wire.rev,
                    dirty: false,
                    draft: false,
                }),
            }
        }
        self.last_rev = pulled.rev;

        // Calendars that vanished from the server's list were unshared with us (or removed).
        // Ones we never uploaded (rev 0) or have open work on are ours to keep.
        let unshared: Vec<String> = self
            .calendars
            .iter()
            .filter(|c| c.rev > 0 && !c.draft && !pulled.calendar_ids.contains(&c.id))
            .map(|c| c.id.clone())
            .collect();
        self.calendars.retain(|c| !unshared.contains(&c.id));
        self.events.retain(|e| !unshared.contains(&e.calendar));
        self.invites = pulled.invites;

        // A brand-new account has no calendars yet: start with one to put events into.
        if self.calendars.is_empty() && self.invites.is_empty() {
            for (name, color) in DEFAULT_CALENDARS {
                self.calendars.push(CalendarInfo {
                    id: new_id(),
                    name: name.into(),
                    color: color.into(),
                    rev: 0,
                    dirty: true,
                    visible: true,
                    owner: String::new(),
                    role: "owner".into(),
                    draft: false,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(h: u32) -> NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2026, 10, 5).unwrap().and_hms_opt(h, 0, 0).unwrap()
    }

    fn event(id: &str, title: &str, rev: i64, dirty: bool) -> Event {
        Event {
            id: id.into(),
            calendar: "c1".into(),
            title: title.into(),
            location: String::new(),
            description: String::new(),
            start: dt(9),
            end: dt(10),
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
            rev,
            dirty,
            draft: false,
        }
    }

    fn wire(id: &str, title: &str, rev: i64, deleted: bool) -> WireEvent {
        WireEvent {
            id: id.into(),
            calendar_id: "c1".into(),
            title: title.into(),
            location: String::new(),
            description: String::new(),
            start: dt(9),
            end: dt(10),
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
            rev,
            deleted,
        }
    }

    fn pulled(rev: i64, events: Vec<WireEvent>) -> Option<Pulled> {
        Some(Pulled { rev, calendars: Vec::new(), events, calendar_ids: vec!["c1".into()], invites: Vec::new() })
    }

    fn store_with(events: Vec<Event>) -> Store {
        let calendar = CalendarInfo {
            id: "c1".into(),
            name: "Team".into(),
            color: "#0f6cbd".into(),
            rev: 1,
            dirty: false,
            visible: true,
            owner: String::new(),
            role: "owner".into(),
            draft: false,
        };
        Store { calendars: vec![calendar], events, ..Store::default() }
    }

    fn wire_calendar(id: &str, rev: i64, role: &str) -> WireCalendar {
        WireCalendar {
            id: id.into(),
            name: "Shared".into(),
            color: "#107c10".into(),
            rev,
            deleted: false,
            owner: "alice".into(),
            role: role.into(),
        }
    }

    #[test]
    fn shared_calendars_arrive_with_role_and_vanish_when_unshared() {
        let mut store = store_with(vec![event("a", "mine", 1, false)]);
        let mut shared_event = wire("s", "theirs", 5, false);
        shared_event.calendar_id = "c2".into();
        store.apply(Outcome {
            pulled: Some(Pulled {
                rev: 6,
                calendars: vec![wire_calendar("c2", 4, "viewer")],
                events: vec![shared_event],
                calendar_ids: vec!["c1".into(), "c2".into()],
                invites: Vec::new(),
            }),
            ..Outcome::default()
        });
        let shared = store.calendars.iter().find(|c| c.id == "c2").unwrap();
        assert_eq!((shared.role.as_str(), shared.owner.as_str()), ("viewer", "alice"));
        assert!(!shared.can_edit() && !shared.is_owner());
        assert_eq!(store.events.len(), 2);

        // the owner removes us: c2 is no longer listed, so it and its events go away
        store.apply(Outcome {
            pulled: Some(Pulled { rev: 7, calendars: vec![], events: vec![], calendar_ids: vec!["c1".into()], invites: Vec::new() }),
            ..Outcome::default()
        });
        assert!(store.calendars.iter().all(|c| c.id != "c2"));
        assert_eq!(store.events.len(), 1);
    }

    #[test]
    fn invites_replace_the_default_calendar() {
        let invite = Invite { calendar_id: "c9".into(), name: "Team".into(), owner: "alice".into(), role: "editor".into(), invited_by: "alice".into() };
        let mut store = Store::default();
        store.apply(Outcome {
            pulled: Some(Pulled { rev: 1, calendars: vec![], events: vec![], calendar_ids: vec![], invites: vec![invite] }),
            ..Outcome::default()
        });
        assert_eq!(store.invites.len(), 1);
        assert!(store.calendars.is_empty()); // nothing is invented while an invitation is waiting
    }

    #[test]
    fn removing_a_calendar_queues_a_server_request_and_blocks_resurrection() {
        let mut store = store_with(vec![event("a", "x", 1, false)]);
        store.remove_calendar("c1");
        assert!(store.calendars.is_empty() && store.events.is_empty());
        assert_eq!(store.pending_calendar_removals.len(), 1);
        assert!(!store.pending_calendar_removals[0].leave); // owner → delete for everyone
        assert!(store.has_pending());

        // a pull that still lists it must not bring it back
        let mut pulled = pulled(5, vec![wire("a", "x", 1, false)]).unwrap();
        pulled.calendars.push(wire_calendar("c1", 3, "owner"));
        store.apply(Outcome { pulled: Some(pulled), ..Outcome::default() });
        assert!(store.calendars.iter().all(|c| c.id != "c1"));
        assert!(store.events.is_empty());

        store.apply(Outcome { done_calendar_removals: vec!["c1".into()], ..Outcome::default() });
        assert!(store.pending_calendar_removals.is_empty());
    }

    #[test]
    fn a_never_uploaded_calendar_just_disappears() {
        let mut store = store_with(vec![]);
        store.calendars[0].rev = 0;
        store.remove_calendar("c1");
        assert!(store.pending_calendar_removals.is_empty());
    }

    #[test]
    fn refused_uploads_trigger_a_fresh_full_pull() {
        let mut store = store_with(vec![event("a", "edited as viewer", 1, true)]);
        store.last_rev = 40;
        let again = store.apply(Outcome { rejected_events: vec!["a".into()], failure: Some(Failure::Offline), ..Outcome::default() });
        assert!(again);
        assert!(store.events.is_empty());
        assert_eq!(store.last_rev, 0);
    }

    #[test]
    fn drafts_are_not_synced() {
        let mut draft = event("d", "unsaved", 0, true);
        draft.draft = true;
        let store = store_with(vec![draft, event("a", "saved", 0, true)]);
        let job = store.job(&Session { server: String::new(), email: String::new(), token: String::new(), feed_token: String::new() });
        assert_eq!(job.events.len(), 1);
        assert_eq!(job.events[0].id, "a");

        let mut only_draft = event("d", "unsaved", 0, true);
        only_draft.draft = true;
        assert!(!store_with(vec![only_draft]).has_pending());
    }

    #[test]
    fn status_travels_both_ways() {
        let mut tentative = wire("a", "maybe", 2, false);
        tentative.status = "tentative".into();
        let mut store = store_with(vec![]);
        store.apply(Outcome { pulled: pulled(2, vec![tentative]), ..Outcome::default() });
        assert_eq!(store.events[0].status, "tentative");

        // …and a local change of the status counts as an edit that must be uploaded
        let mut edited = store.events[0].clone();
        edited.status = "busy".into();
        assert!(!store.events[0].same_content(&edited));
        assert_eq!(WireEvent::from(&store.events[0]).status, "tentative");
    }

    #[test]
    fn pull_adds_updates_and_removes() {
        let mut store = store_with(vec![event("a", "old", 1, false), event("b", "gone", 2, false)]);
        store.apply(Outcome {
            pulled: pulled(9, vec![wire("a", "new", 5, false), wire("b", "gone", 6, true), wire("c", "fresh", 7, false)]),
            ..Outcome::default()
        });
        assert_eq!(store.last_rev, 9);
        let titles: Vec<_> = store.events.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, ["new", "fresh"]);
    }

    #[test]
    fn local_edit_wins_but_adopts_server_revision() {
        let mut store = store_with(vec![event("a", "mine", 1, true)]);
        store.apply(Outcome { pulled: pulled(5, vec![wire("a", "theirs", 4, false)]), ..Outcome::default() });
        assert_eq!(store.events[0].title, "mine");
        assert_eq!(store.events[0].rev, 4);
        assert!(store.events[0].dirty);
    }

    #[test]
    fn edit_during_sync_stays_dirty() {
        let sent = event("a", "v1", 0, true);
        let mut store = store_with(vec![event("a", "v2", 0, true)]); // edited while uploading v1
        store.apply(Outcome { pushed_events: vec![(sent, 3)], ..Outcome::default() });
        assert_eq!(store.events[0].rev, 3);
        assert!(store.events[0].dirty);

        let mut store = store_with(vec![event("a", "v1", 0, true)]);
        store.apply(Outcome { pushed_events: vec![(event("a", "v1", 0, true), 3)], ..Outcome::default() });
        assert!(!store.events[0].dirty);
    }

    #[test]
    fn pending_delete_is_not_resurrected_by_pull() {
        let mut store = store_with(vec![]);
        store.pending_deletes.push(PendingDelete { id: "a".into(), rev: 1 });
        store.apply(Outcome { pulled: pulled(5, vec![wire("a", "x", 1, false)]), ..Outcome::default() });
        assert!(store.events.is_empty());
        store.apply(Outcome { done_deletes: vec!["a".into()], ..Outcome::default() });
        assert!(store.pending_deletes.is_empty());
    }

    #[test]
    fn empty_account_gets_default_calendars() {
        let mut store = Store::default();
        store.apply(Outcome { pulled: pulled(0, vec![]), ..Outcome::default() });
        assert_eq!(store.calendars.len(), 1);
        assert_eq!(store.calendars[0].name, "Persönlich");
        assert!(store.calendars.iter().all(|c| c.dirty));
        // …but not after a failed round, where we don't know what the server has
        let mut store = Store::default();
        store.apply(Outcome { failure: Some(Failure::Offline), ..Outcome::default() });
        assert!(store.calendars.is_empty());
    }

    /// Needs a running server: `KAIRO_TEST_SERVER=http://127.0.0.1:8099 cargo test -- --ignored`
    #[test]
    #[ignore]
    fn two_devices_converge() {
        let server = std::env::var("KAIRO_TEST_SERVER").expect("KAIRO_TEST_SERVER");
        let name = format!("s{}", std::process::id());
        let session = crate::api::authenticate(&server, &name, "correct horse battery", true, "").unwrap();

        let round = |store: &mut Store| {
            let out = run(store.job(&session));
            assert!(out.failure.is_none(), "{:?}", out.failure);
            store.apply(out);
        };

        // Device A: first sync creates default calendars, second uploads them.
        let mut a = Store::default();
        round(&mut a);
        round(&mut a);
        assert_eq!(a.calendars.len(), 1);
        assert!(!a.has_pending());

        // A creates an event; B (fresh) receives it.
        let cal = a.calendars[0].id.clone();
        a.events.push(Event { calendar: cal.clone(), ..event(&new_id(), "Stand-up", 0, true) });
        round(&mut a);
        let mut b = Store::default();
        round(&mut b);
        assert_eq!(b.events.len(), 1);
        assert_eq!(b.events[0].title, "Stand-up");
        assert!(!a.has_pending());

        // Both edit the same event: B uploads first, A's later edit wins.
        b.events[0].title = "B edit".into();
        b.events[0].dirty = true;
        round(&mut b);
        a.events[0].title = "A edit".into();
        a.events[0].dirty = true;
        round(&mut a);
        round(&mut b);
        assert_eq!(a.events[0].title, "A edit");
        assert_eq!(b.events[0].title, "A edit");

        // A deletes it; B sees the deletion.
        let gone = a.events.remove(0);
        a.pending_deletes.push(PendingDelete { id: gone.id, rev: gone.rev });
        round(&mut a);
        round(&mut b);
        assert!(a.events.is_empty() && b.events.is_empty());
        assert!(!a.has_pending());
    }
}
