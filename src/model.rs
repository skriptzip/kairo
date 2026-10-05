//! Calendar data as the app holds it, plus its on-disk cache.
//!
//! Every item remembers the server revision it was last in sync with (`rev`, 0 = never
//! uploaded) and whether it has local changes that still have to be pushed (`dirty`).

use std::path::PathBuf;

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

use crate::api::Session;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CalendarInfo {
    pub id: String,
    pub name: String,
    /// `#rrggbb`
    pub color: String,
    #[serde(default)]
    pub rev: i64,
    #[serde(default)]
    pub dirty: bool,
    /// Local display preference, not synced.
    #[serde(default = "yes")]
    pub visible: bool,
    /// Who owns the calendar (empty = you, for data from before sharing existed).
    #[serde(default)]
    pub owner: String,
    /// What you may do here: `owner`, `editor` or `viewer`.
    #[serde(default = "owner_role")]
    pub role: String,
    /// New calendar whose settings window hasn't been saved yet: invisible to sync.
    #[serde(default)]
    pub draft: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    pub calendar: String,
    pub title: String,
    #[serde(default)]
    pub location: String,
    #[serde(default)]
    pub description: String,
    pub start: NaiveDateTime,
    pub end: NaiveDateTime,
    #[serde(default = "default_tz")]
    pub tz: String,
    /// `free`, `tentative` ("Mit Vorbehalt"), `busy` or `oof` ("Abwesend").
    #[serde(default = "default_status")]
    pub status: String,
    #[serde(default)]
    pub rev: i64,
    #[serde(default)]
    pub dirty: bool,
    /// Created in the details window but not saved yet: invisible to sync, dropped on restart.
    #[serde(default)]
    pub draft: bool,
}

/// An invitation to a calendar somebody else owns.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Invite {
    pub calendar_id: String,
    pub name: String,
    pub owner: String,
    pub role: String,
    pub invited_by: String,
}

/// A calendar removed locally whose removal the server doesn't know yet.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingCalendarRemoval {
    pub id: String,
    /// `true`: just leave a calendar somebody else owns; `false`: delete it for everyone.
    pub leave: bool,
}

/// An event deleted locally whose deletion the server doesn't know yet.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingDelete {
    pub id: String,
    pub rev: i64,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Store {
    /// Last server revision pulled (`GET /sync?since=`).
    #[serde(default)]
    pub last_rev: i64,
    #[serde(default)]
    pub calendars: Vec<CalendarInfo>,
    #[serde(default)]
    pub events: Vec<Event>,
    #[serde(default)]
    pub pending_deletes: Vec<PendingDelete>,
    #[serde(default)]
    pub pending_calendar_removals: Vec<PendingCalendarRemoval>,
    #[serde(default)]
    pub invites: Vec<Invite>,
}

fn yes() -> bool {
    true
}

fn owner_role() -> String {
    "owner".into()
}

pub fn default_status() -> String {
    "busy".into()
}

pub fn default_tz() -> String {
    "Europe/Berlin".into()
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

pub fn parse_color(hex: &str) -> u32 {
    u32::from_str_radix(hex.trim_start_matches('#'), 16).unwrap_or(0x0f6cbd)
}

impl CalendarInfo {
    pub fn can_edit(&self) -> bool {
        self.role == "owner" || self.role == "editor"
    }

    pub fn is_owner(&self) -> bool {
        self.role == "owner"
    }

    pub fn same_content(&self, other: &Self) -> bool {
        self.name == other.name && self.color == other.color
    }
}

impl Event {
    pub fn same_content(&self, other: &Self) -> bool {
        self.calendar == other.calendar
            && self.title == other.title
            && self.location == other.location
            && self.description == other.description
            && self.start == other.start
            && self.end == other.end
            && self.tz == other.tz
            && self.status == other.status
    }
}

impl Store {
    /// One cache file per account and server, so switching accounts never mixes data.
    fn path(session: &Session) -> Option<PathBuf> {
        let key: String = format!("{}@{}", session.email, session.server)
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' })
            .collect();
        dirs::data_dir().map(|dir| dir.join("kairo").join(format!("{key}.json")))
    }

    pub fn load(session: &Session) -> Self {
        Self::path(session)
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|json| serde_json::from_str::<Store>(&json).ok())
            .map(|mut store| {
                store.events.retain(|e| !e.draft);
                let drafts: Vec<String> = store.calendars.iter().filter(|c| c.draft).map(|c| c.id.clone()).collect();
                store.calendars.retain(|c| !c.draft);
                store.events.retain(|e| !drafts.contains(&e.calendar));
                store
            })
            .unwrap_or_default()
    }

    pub fn save(&self, session: &Session) {
        let Some(path) = Self::path(session) else { return };
        let Ok(json) = serde_json::to_string(self) else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // Write-then-rename so a crash can't leave a half-written cache behind.
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(tmp, path);
        }
    }
}
