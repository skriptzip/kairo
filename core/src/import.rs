//! The result of reading a calendar file (`.ics`, `.pst`), before it becomes `model::Event`s.

use chrono::{Duration, NaiveDate, NaiveDateTime, NaiveTime};

use crate::model::Exception as SeriesException;

/// One appointment (or one occurrence of a series) with Kairo's event fields.
#[derive(Clone)]
pub struct ImportedEvent {
    pub title: String,
    pub location: String,
    pub description: String,
    pub start: NaiveDateTime,
    pub end: NaiveDateTime,
    /// `free`, `tentative`, `busy` or `oof`
    pub status: String,
    /// The time zone the start and end are written in (an IANA name).
    pub tz: String,
    /// Kairo repeat rule (`daily`, `weekly`, `monthly`, `yearly`) of a series, empty otherwise.
    pub repeat: String,
    pub repeat_until: Option<NaiveDate>,
    pub repeat_interval: u32,
    pub repeat_days: u8,
    pub repeat_nth: i8,
    pub exceptions: Vec<SeriesException>,
    pub remind: Option<u32>,
    pub attendees: Vec<String>,
    /// Own color as `#rrggbb`.
    pub color: Option<String>,
    pub private: bool,
    pub categories: Vec<String>,
}

/// All appointments of one folder / file.
pub struct ImportedCalendar {
    pub name: String,
    pub events: Vec<ImportedEvent>,
}

pub struct Imported {
    pub calendars: Vec<ImportedCalendar>,
    /// Items that could not be read.
    pub skipped: usize,
    /// Repeat rules Kairo can't express: the item was imported as a single event.
    pub simplified: usize,
}

/// An event's start and end as Kairo keeps them: all-day items become whole days (the end is
/// the last minute of the last day), others keep their length, even over midnight.
pub fn split_days(start: NaiveDateTime, end: NaiveDateTime, all_day: bool) -> Vec<(NaiveDateTime, NaiveDateTime)> {
    let last_minute = NaiveTime::from_hms_opt(23, 59, 0).unwrap();
    let (mut start, mut end) = (start, end);
    if all_day {
        // The file's end is the (exclusive) midnight after the last day.
        let round = |t: NaiveDateTime| {
            let day = if t.time() >= NaiveTime::from_hms_opt(12, 0, 0).unwrap() { t.date().succ_opt().unwrap() } else { t.date() };
            day.and_time(NaiveTime::MIN)
        };
        start = round(start);
        let last_day = (round(end) - Duration::days(1)).date().max(start.date());
        end = last_day.and_time(last_minute);
    } else if end <= start {
        end = (start + Duration::minutes(30)).min(start.date().and_time(last_minute)).max(start);
    }
    vec![(start, end)]
}
