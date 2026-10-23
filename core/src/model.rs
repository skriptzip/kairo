//! Calendar data as the app holds it, plus its on-disk cache.
//!
//! Every item remembers the server revision it was last in sync with (`rev`, 0 = never
//! uploaded) and whether it has local changes that still have to be pushed (`dirty`).

use std::path::PathBuf;

use chrono::{Datelike, NaiveDate, NaiveDateTime};
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

/// One repetition of a series that was deleted or changed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Exception {
    /// The day the repetition would normally take place.
    pub date: NaiveDate,
    #[serde(default)]
    pub deleted: bool,
    /// What differs from the series; empty fields stay as in the series.
    #[serde(default)]
    pub start: Option<NaiveDateTime>,
    #[serde(default)]
    pub end: Option<NaiveDateTime>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub location: Option<String>,
}

pub fn one() -> u32 {
    1
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
    /// `daily`, `weekly`, `monthly`, `yearly` or empty for a single event. The event then
    /// is the first of a series that repeats on the same time of day.
    #[serde(default)]
    pub repeat: String,
    /// Last day of the series (inclusive); none = forever.
    #[serde(default)]
    pub repeat_until: Option<NaiveDate>,
    /// Every n-th day / week / month / year.
    #[serde(default = "one")]
    pub repeat_interval: u32,
    /// Weekly: bit mask of the weekdays (Monday = 1 ... Sunday = 64); 0 = the start's weekday.
    #[serde(default)]
    pub repeat_days: u8,
    /// Monthly / yearly: 0 = same day of the month, 1-4 = n-th weekday of the month (the start's
    /// weekday), -1 = last such weekday.
    #[serde(default)]
    pub repeat_nth: i8,
    /// Repetitions that were deleted or changed on their own.
    #[serde(default)]
    pub exceptions: Vec<Exception>,
    /// Minutes before the start to remind (every repetition of a series); none = no reminder.
    #[serde(default)]
    pub remind: Option<u32>,
    /// E-mail addresses of the people invited to the event.
    #[serde(default)]
    pub attendees: Vec<String>,
    /// Own color as `#rrggbb`; none = the calendar's color.
    #[serde(default)]
    pub color: Option<String>,
    /// Only the calendar's owner sees the details; others just see when.
    #[serde(default)]
    pub private: bool,
    /// Labels such as "Vor Ort" or "Homeoffice".
    #[serde(default)]
    pub categories: Vec<String>,
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
    /// Reminders already shown (event id, start of the repetition), so a restart doesn't repeat them.
    #[serde(default)]
    pub reminded: Vec<(String, NaiveDateTime)>,
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

/// Zones offered in the dropdowns (IANA names).
pub const TIME_ZONES: [&str; 20] = [
    "Europe/Berlin",
    "Europe/London",
    "Europe/Lisbon",
    "Europe/Athens",
    "Europe/Istanbul",
    "Europe/Moscow",
    "UTC",
    "Africa/Cairo",
    "Africa/Johannesburg",
    "Asia/Dubai",
    "Asia/Kolkata",
    "Asia/Singapore",
    "Asia/Shanghai",
    "Asia/Tokyo",
    "Australia/Sydney",
    "Pacific/Auckland",
    "America/Sao_Paulo",
    "America/New_York",
    "America/Chicago",
    "America/Los_Angeles",
];

/// The zone all times are shown in (chosen in the sidebar, remembered between runs).
static DISPLAY_TZ: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);

fn settings_path() -> Option<PathBuf> {
    crate::platform::config_dir().map(|dir| dir.join("settings.json"))
}

pub fn display_tz() -> String {
    DISPLAY_TZ.read().ok().and_then(|zone| zone.clone()).unwrap_or_else(default_tz)
}

/// Switches the display zone and remembers it; unknown names are ignored.
pub fn set_display_tz(name: &str) {
    if name.parse::<chrono_tz::Tz>().is_err() {
        return;
    }
    if let Ok(mut zone) = DISPLAY_TZ.write() {
        *zone = Some(name.to_string());
    }
    if let Some(path) = settings_path() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, serde_json::json!({ "display_tz": name }).to_string());
    }
}

/// Reads the remembered display zone (at startup).
pub fn load_display_tz() {
    let saved = settings_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|json| json["display_tz"].as_str().map(str::to_string));
    if let (Some(name), Ok(mut zone)) = (saved, DISPLAY_TZ.write()) {
        if name.parse::<chrono_tz::Tz>().is_ok() {
            *zone = Some(name);
        }
    }
}

/// The current time in the display zone.
pub fn now() -> NaiveDateTime {
    convert_time(chrono::Utc::now().naive_utc(), "UTC", &display_tz())
}

pub fn today() -> NaiveDate {
    now().date()
}

/// The zone called `name` (Kairo's default if unknown).
pub fn zone(name: &str) -> chrono_tz::Tz {
    name.parse().unwrap_or(chrono_tz::Europe::Berlin)
}

/// The wall-clock time `time` in zone `from` as a wall-clock time in zone `to`.
pub fn convert_time(time: NaiveDateTime, from: &str, to: &str) -> NaiveDateTime {
    use chrono::TimeZone;
    if from == to {
        return time;
    }
    let source = zone(from);
    // A time that doesn't exist (clocks going forward) counts an hour later.
    let exact = source
        .from_local_datetime(&time)
        .earliest()
        .or_else(|| source.from_local_datetime(&(time + chrono::Duration::hours(1))).earliest());
    exact.map_or(time, |t| t.with_timezone(&zone(to)).naive_local())
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

/// Bit of a weekday in `Event::repeat_days` (Monday = 1 ... Sunday = 64).
pub fn weekday_bit(day: chrono::Weekday) -> u8 {
    1 << day.num_days_from_monday()
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let (next_year, next_month) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
    NaiveDate::from_ymd_opt(next_year, next_month, 1).and_then(|d| d.pred_opt()).map_or(28, |d| d.day())
}

/// From 00:00 to the end of a day (23:59, or the midnight after it): one or more whole days.
pub fn spans_whole_day(start: NaiveDateTime, end: NaiveDateTime) -> bool {
    let last_minute = chrono::NaiveTime::from_hms_opt(23, 59, 0);
    start.time() == chrono::NaiveTime::MIN
        && end > start
        && (Some(end.time()) == last_minute || end.time() == chrono::NaiveTime::MIN)
}

impl Event {
    /// Lasts one or more whole days (00:00 to the end of a day), which the grid shows in the
    /// all-day row on top instead of as a 24-hour block.
    pub fn is_all_day(&self) -> bool {
        spans_whole_day(self.start, self.end)
    }

    pub fn same_content(&self, other: &Self) -> bool {
        self.calendar == other.calendar
            && self.title == other.title
            && self.location == other.location
            && self.description == other.description
            && self.start == other.start
            && self.end == other.end
            && self.tz == other.tz
            && self.status == other.status
            && self.repeat == other.repeat
            && self.repeat_until == other.repeat_until
            && self.repeat_interval == other.repeat_interval
            && self.repeat_days == other.repeat_days
            && self.repeat_nth == other.repeat_nth
            && self.exceptions == other.exceptions
            && self.remind == other.remind
            && self.attendees == other.attendees
            && self.color == other.color
            && self.private == other.private
            && self.categories == other.categories
    }

    pub fn repeats(&self) -> bool {
        !self.repeat.is_empty()
    }

    /// Does the series rule (ignoring exceptions) hit `day`?
    fn rule_hits(&self, day: NaiveDate) -> bool {
        let first = self.start.date();
        if day < first || self.repeat_until.is_some_and(|until| day > until) {
            return false;
        }
        let interval = i64::from(self.repeat_interval.max(1));
        let months_apart = i64::from(day.year() - first.year()) * 12 + i64::from(day.month()) - i64::from(first.month());
        let monday = |d: NaiveDate| d - chrono::Duration::days(i64::from(d.weekday().num_days_from_monday()));
        match self.repeat.as_str() {
            "daily" => (day - first).num_days() % interval == 0,
            "weekly" => {
                let days = if self.repeat_days == 0 { weekday_bit(first.weekday()) } else { self.repeat_days };
                days & weekday_bit(day.weekday()) != 0 && ((monday(day) - monday(first)).num_days() / 7) % interval == 0
            }
            "monthly" => months_apart % interval == 0 && self.month_day_hits(day),
            "yearly" => months_apart % (12 * interval) == 0 && self.month_day_hits(day),
            _ => day == first,
        }
    }

    /// Monthly / yearly: is `day` the right day within its month?
    fn month_day_hits(&self, day: NaiveDate) -> bool {
        let first = self.start.date();
        if self.repeat_nth == 0 {
            // The 31st also repeats on a shorter month's last day.
            return day.day() == first.day().min(days_in_month(day.year(), day.month()));
        }
        day.weekday() == first.weekday()
            && match self.repeat_nth {
                -1 => day.day() + 7 > days_in_month(day.year(), day.month()),
                n => (day.day() - 1) / 7 + 1 == n as u32,
            }
    }

    fn exception(&self, origin: NaiveDate) -> Option<&Exception> {
        self.exceptions.iter().find(|x| x.date == origin)
    }

    /// The repetition that regularly takes place on `origin`, with its own changes applied.
    pub fn occurrence(&self, origin: NaiveDate) -> Event {
        let mut occurrence = self.clone();
        occurrence.start = origin.and_time(self.start.time());
        occurrence.end = occurrence.start + (self.end - self.start);
        if let Some(x) = self.exception(origin).filter(|x| !x.deleted) {
            occurrence.start = x.start.unwrap_or(occurrence.start);
            occurrence.end = x.end.unwrap_or(occurrence.end);
            if let Some(title) = &x.title {
                occurrence.title = title.clone();
            }
            if let Some(location) = &x.location {
                occurrence.location = location.clone();
            }
        }
        occurrence
    }

    /// Event time (in the event's own zone) as shown in Kairo's default zone.
    pub fn to_display(&self, time: NaiveDateTime) -> NaiveDateTime {
        convert_time(time, &self.tz, &display_tz())
    }

    /// The inverse of `to_display`.
    pub fn from_display(&self, time: NaiveDateTime) -> NaiveDateTime {
        convert_time(time, &display_tz(), &self.tz)
    }

    /// The last day the event covers (a midnight end belongs to the day before).
    pub fn last_day(&self) -> NaiveDate {
        if self.end.time() == chrono::NaiveTime::MIN && self.end > self.start {
            self.end.date() - chrono::Duration::days(1)
        } else {
            self.end.date()
        }
    }

    /// How many days the event reaches beyond its first one (0 for an ordinary event).
    pub fn extra_days(&self) -> i64 {
        (self.last_day() - self.start.date()).num_days().max(0)
    }

    /// Does the event sit in another zone than the one it is shown in? (All-day events don't.)
    pub fn is_foreign(&self) -> bool {
        self.tz != display_tz() && !self.is_all_day()
    }

    /// Everything of this event (or series) that is shown on `day` (a day in the display
    /// zone), with the day each repetition regularly belongs to. A longer event also shows up on
    /// the days after its first: all-day ones as they are, timed ones cut to that day.
    pub fn occurrences_on(&self, day: NaiveDate) -> Vec<(Event, NaiveDate)> {
        // The longest this event (or one of its changed repetitions) reaches.
        let reach = self
            .exceptions
            .iter()
            .filter_map(|x| x.start.zip(x.end))
            .map(|(start, end)| (end.date() - start.date()).num_days())
            .fold(self.extra_days(), i64::max)
            .max(0);
        if reach == 0 {
            return self.starting_on(day);
        }
        let midnight = chrono::NaiveTime::MIN;
        (0..=reach + 1)
            .flat_map(|back| self.starting_on(day - chrono::Duration::days(back)))
            .filter_map(|(occurrence, origin)| {
                let (first, last) = (occurrence.start.date(), occurrence.last_day());
                if first > day || last < day {
                    return None;
                }
                if first == day && last == day || occurrence.is_all_day() {
                    return Some((occurrence, origin));
                }
                // A timed event on a day it neither starts nor ends: cut to the day. The cut end
                // is 23:59:59, so it doesn't look like an all-day event.
                let mut piece = occurrence;
                piece.start = piece.start.max(day.and_time(midnight));
                if piece.end.date() > day {
                    piece.end = day.and_hms_opt(23, 59, 59).unwrap_or(piece.end);
                }
                Some((piece, origin))
            })
            .collect()
    }

    /// The occurrences that *begin* on `day` (a day in the display zone), uncut. For an event
    /// in another time zone the day in its own zone may differ.
    pub fn starting_on(&self, day: NaiveDate) -> Vec<(Event, NaiveDate)> {
        if !self.is_foreign() {
            return self.occurrences_in_own_zone(day);
        }
        // The same moment can fall on a neighboring day in the event's own zone.
        [-1, 0, 1]
            .into_iter()
            .flat_map(|offset| self.occurrences_in_own_zone(day + chrono::Duration::days(offset)))
            .filter_map(|(mut occurrence, origin)| {
                occurrence.start = self.to_display(occurrence.start);
                occurrence.end = self.to_display(occurrence.end);
                occurrence.tz = display_tz();
                (occurrence.start.date() == day).then_some((occurrence, origin))
            })
            .collect()
    }

    /// Like `occurrences_on`, but `day` and the result are in the event's own zone.
    fn occurrences_in_own_zone(&self, day: NaiveDate) -> Vec<(Event, NaiveDate)> {
        if !self.repeats() {
            return if self.start.date() == day { vec![(self.clone(), day)] } else { Vec::new() };
        }
        let mut out = Vec::new();
        if self.rule_hits(day) && self.exception(day).is_none() {
            out.push((self.occurrence(day), day));
        }
        for x in self.exceptions.iter().filter(|x| !x.deleted && self.rule_hits(x.date)) {
            let moved = self.occurrence(x.date);
            if moved.start.date() == day {
                out.push((moved, x.date));
            }
        }
        out
    }

    /// Changes the single repetition from `origin`; values equal to the series are not stored.
    pub fn change_occurrence(
        &mut self,
        origin: NaiveDate,
        start: NaiveDateTime,
        end: NaiveDateTime,
        title: &str,
        location: &str,
    ) {
        let regular_start = origin.and_time(self.start.time());
        let regular_end = regular_start + (self.end - self.start);
        let exception = Exception {
            date: origin,
            deleted: false,
            start: (start != regular_start).then_some(start),
            end: (end != regular_end).then_some(end),
            title: (title != self.title).then(|| title.to_string()),
            location: (location != self.location).then(|| location.to_string()),
        };
        self.exceptions.retain(|x| x.date != origin);
        if exception.start.is_some() || exception.end.is_some() || exception.title.is_some() || exception.location.is_some() {
            self.exceptions.push(exception);
            self.exceptions.sort_by_key(|x| x.date);
        }
    }

    /// Deletes the single repetition from `origin`.
    pub fn delete_occurrence(&mut self, origin: NaiveDate) {
        self.exceptions.retain(|x| x.date != origin);
        self.exceptions.push(Exception { date: origin, deleted: true, start: None, end: None, title: None, location: None });
        self.exceptions.sort_by_key(|x| x.date);
    }
}


impl Store {
    /// One cache file per account and server, so switching accounts never mixes data.
    fn path(session: &Session) -> Option<PathBuf> {
        let key: String = format!("{}@{}", session.email, session.server)
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' })
            .collect();
        crate::platform::data_dir().map(|dir| dir.join(format!("{key}.json")))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn event(start: &str, end: &str) -> Event {
        let parse = |s: &str| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap();
        Event {
            id: "e".into(),
            calendar: "c".into(),
            title: "t".into(),
            location: String::new(),
            description: String::new(),
            start: parse(start),
            end: parse(end),
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
            dirty: false,
            draft: false,
        }
    }

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn hits(e: &Event, d: NaiveDate) -> bool {
        !e.occurrences_on(d).is_empty()
    }

    #[test]
    fn repetitions_follow_their_rule() {
        let mut e = event("2026-01-31 09:00", "2026-01-31 10:00");
        assert!(hits(&e, day(2026, 1, 31)) && !hits(&e, day(2026, 2, 28)));

        e.repeat = "weekly".into();
        assert!(hits(&e, day(2026, 2, 7)) && !hits(&e, day(2026, 2, 8)) && !hits(&e, day(2026, 1, 24)));

        e.repeat = "monthly".into();
        assert!(hits(&e, day(2026, 2, 28)) && hits(&e, day(2026, 3, 31)) && !hits(&e, day(2026, 3, 30)));

        e.repeat = "yearly".into();
        assert!(hits(&e, day(2027, 1, 31)) && !hits(&e, day(2027, 2, 28)));

        e.repeat = "daily".into();
        e.repeat_until = Some(day(2026, 2, 2));
        assert!(hits(&e, day(2026, 2, 2)) && !hits(&e, day(2026, 2, 3)));
    }

    #[test]
    fn intervals_and_weekday_sets() {
        // Monday 2026-10-05; every 2nd week on Monday and Wednesday
        let mut e = event("2026-10-05 09:00", "2026-10-05 10:00");
        e.repeat = "weekly".into();
        e.repeat_interval = 2;
        e.repeat_days = weekday_bit(chrono::Weekday::Mon) | weekday_bit(chrono::Weekday::Wed);
        assert!(hits(&e, day(2026, 10, 5)) && hits(&e, day(2026, 10, 7)));
        assert!(!hits(&e, day(2026, 10, 12)) && !hits(&e, day(2026, 10, 14)));
        assert!(hits(&e, day(2026, 10, 19)) && hits(&e, day(2026, 10, 21)));
        assert!(!hits(&e, day(2026, 10, 6)));

        e.repeat = "daily".into();
        e.repeat_interval = 3;
        assert!(hits(&e, day(2026, 10, 8)) && !hits(&e, day(2026, 10, 9)));

        e.repeat = "monthly".into();
        e.repeat_interval = 3;
        assert!(hits(&e, day(2027, 1, 5)) && !hits(&e, day(2026, 11, 5)));
    }

    #[test]
    fn nth_weekday_of_the_month() {
        // 2026-10-13 is the 2nd Tuesday
        let mut e = event("2026-10-13 09:00", "2026-10-13 10:00");
        e.repeat = "monthly".into();
        e.repeat_nth = 2;
        assert!(hits(&e, day(2026, 11, 10)) && !hits(&e, day(2026, 11, 17)) && !hits(&e, day(2026, 11, 3)));

        // 2026-10-27 is the last Tuesday
        let mut last = event("2026-10-27 09:00", "2026-10-27 10:00");
        last.repeat = "monthly".into();
        last.repeat_nth = -1;
        assert!(hits(&last, day(2026, 11, 24)) && !hits(&last, day(2026, 11, 17)));

        last.repeat = "yearly".into();
        assert!(hits(&last, day(2027, 10, 26)) && !hits(&last, day(2027, 11, 30)));
    }

    #[test]
    fn a_repetition_keeps_time_and_length() {
        let mut e = event("2026-10-05 09:30", "2026-10-05 10:15");
        e.repeat = "weekly".into();
        let later = e.occurrence(day(2026, 10, 12));
        assert_eq!(later.start.to_string(), "2026-10-12 09:30:00");
        assert_eq!(later.end.to_string(), "2026-10-12 10:15:00");
    }

    #[test]
    fn single_repetitions_can_be_deleted_and_changed() {
        let mut e = event("2026-10-05 09:00", "2026-10-05 10:00");
        e.repeat = "daily".into();

        e.delete_occurrence(day(2026, 10, 6));
        assert!(!hits(&e, day(2026, 10, 6)) && hits(&e, day(2026, 10, 7)));

        // 7th moves to the 8th, later and with a new title
        let (start, end) = (day(2026, 10, 8).and_hms_opt(14, 0, 0).unwrap(), day(2026, 10, 8).and_hms_opt(15, 0, 0).unwrap());
        e.change_occurrence(day(2026, 10, 7), start, end, "Anders", "");
        let moved = e.occurrences_on(day(2026, 10, 8));
        assert_eq!(moved.len(), 2); // the regular one of the 8th and the moved one of the 7th
        assert!(moved.iter().any(|(o, origin)| *origin == day(2026, 10, 7) && o.title == "Anders" && o.start == start));
        assert!(!hits(&e, day(2026, 10, 7)));

        // changing it back removes the exception again
        let regular = day(2026, 10, 7).and_hms_opt(9, 0, 0).unwrap();
        e.change_occurrence(day(2026, 10, 7), regular, regular + chrono::Duration::hours(1), "t", "");
        assert!(hits(&e, day(2026, 10, 7)));
        assert_eq!(e.exceptions.len(), 1);
    }

    #[test]
    fn events_in_other_zones_are_shown_converted() {
        let mut e = event("2026-10-05 09:00", "2026-10-05 10:00");
        e.tz = "America/New_York".into(); // 09:00 there is 15:00 in Berlin (both on summer time)
        let shown = e.occurrences_on(day(2026, 10, 5));
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].0.start.to_string(), "2026-10-05 15:00:00");
        assert_eq!(shown[0].1, day(2026, 10, 5));

        // 20:00 in New York is 02:00 the next day in Berlin
        let mut late = event("2026-10-05 20:00", "2026-10-05 21:00");
        late.tz = "America/New_York".into();
        assert!(late.occurrences_on(day(2026, 10, 5)).is_empty());
        let next = late.occurrences_on(day(2026, 10, 6));
        assert_eq!(next[0].0.start.to_string(), "2026-10-06 02:00:00");
        assert_eq!(next[0].1, day(2026, 10, 5));

        // a series moves along with its zone: Mondays 20:00 New York are Tuesdays 02:00 in Berlin
        late.repeat = "weekly".into();
        assert!(!late.occurrences_on(day(2026, 10, 13)).is_empty() && late.occurrences_on(day(2026, 10, 12)).is_empty());

        // going back and forth
        assert_eq!(late.from_display(late.to_display(late.start)), late.start);
    }

    #[test]
    fn long_events_show_on_every_day_they_cover() {
        // Monday 22:00 to Wednesday 08:00
        let e = event("2026-10-05 22:00", "2026-10-07 08:00");
        let on = |d: u32| e.occurrences_on(day(2026, 10, d));
        assert_eq!(on(4).len(), 0);
        assert_eq!((on(5)[0].0.start.to_string(), on(5)[0].0.end.to_string()), ("2026-10-05 22:00:00".into(), "2026-10-05 23:59:59".into()));
        assert_eq!((on(6)[0].0.start.to_string(), on(6)[0].0.end.to_string()), ("2026-10-06 00:00:00".into(), "2026-10-06 23:59:59".into()));
        assert!(!on(6)[0].0.is_all_day(), "a cut piece is not an all-day event");
        assert_eq!((on(7)[0].0.start.to_string(), on(7)[0].0.end.to_string()), ("2026-10-07 00:00:00".into(), "2026-10-07 08:00:00".into()));
        assert_eq!(on(8).len(), 0);
        // the series reaches over midnight every week
        let mut weekly = e.clone();
        weekly.repeat = "weekly".into();
        assert_eq!(weekly.occurrences_on(day(2026, 10, 14)).len(), 1);
        assert_eq!(weekly.starting_on(day(2026, 10, 14)).len(), 0);
    }

    #[test]
    fn multi_day_all_day_events_keep_their_length() {
        // three whole days: Oct 5 to Oct 7
        let e = event("2026-10-05 00:00", "2026-10-07 23:59");
        assert!(e.is_all_day());
        assert_eq!((e.start.date(), e.last_day(), e.extra_days()), (day(2026, 10, 5), day(2026, 10, 7), 2));
        for d in 5..=7 {
            let shown = e.occurrences_on(day(2026, 10, d));
            assert_eq!(shown.len(), 1);
            assert_eq!(shown[0].0.start, e.start, "all-day events are not cut");
        }
        assert!(e.occurrences_on(day(2026, 10, 8)).is_empty());
        // a midnight end is exclusive
        assert_eq!(event("2026-10-05 00:00", "2026-10-07 00:00").last_day(), day(2026, 10, 6));
        assert!(!event("2026-10-05 00:00", "2026-10-05 00:00").is_all_day());
    }

    #[test]
    fn all_day_events_ignore_zones() {
        let mut e = event("2026-10-05 00:00", "2026-10-05 23:59");
        e.tz = "Asia/Tokyo".into();
        assert_eq!(e.occurrences_on(day(2026, 10, 5)).len(), 1);
        assert!(e.occurrences_on(day(2026, 10, 4)).is_empty());
    }

    #[test]
    fn whole_day_events_are_all_day() {
        assert!(event("2026-10-05 00:00", "2026-10-05 23:59").is_all_day());
        assert!(event("2026-10-05 00:00", "2026-10-06 00:00").is_all_day());
        assert!(!event("2026-10-05 00:00", "2026-10-05 12:00").is_all_day());
        assert!(!event("2026-10-05 08:00", "2026-10-05 23:59").is_all_day());
    }
}
