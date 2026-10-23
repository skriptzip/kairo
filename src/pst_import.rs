//! Reads the calendar items out of an Outlook `.pst` data file.
//!
//! Everything here is blocking; call it from `cx.background_executor()`.
//!
//! Kairo has no recurrence rules, so recurring appointments are expanded into single events
//! (up to two years ahead). Times of single appointments are stored in UTC by Outlook and are
//! shown in Kairo's default time zone; times of recurring series are wall-clock times already.

use std::{collections::HashSet, path::Path};

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Weekday};
use kairo_core::import::split_days;
pub use kairo_core::import::{Imported, ImportedCalendar, ImportedEvent};
use kairo_core::model::Exception as SeriesException;
use outlook_pst::{
    ltp::prop_context::PropertyValue,
    messaging::{
        message::Message,
        named_prop::{NamedPropertyGuid, NamedPropertyId},
        store::Store,
    },
    ndb::node_id::NodeId,
};


// Limits of the Kairo server (see `server/src/main.rs`).
const MAX_TITLE: usize = 500;
const MAX_LOCATION: usize = 500;
const MAX_DESCRIPTION: usize = 20_000;

/// Recurring series are expanded up to this many days after today ...
const HORIZON_DAYS: i64 = 2 * 365;
/// ... and never into more than this many events.
const MAX_OCCURRENCES: usize = 2000;

// PSETID_Appointment {00062002-0000-0000-C000-000000000046}
const APPOINTMENT_GUID: (u32, u16, u16, [u8; 8]) = (0x0006_2002, 0, 0, [0xC0, 0, 0, 0, 0, 0, 0, 0x46]);

// PSETID_Common {00062008-0000-0000-C000-000000000046}
const COMMON_GUID: (u32, u16, u16, [u8; 8]) = (0x0006_2008, 0, 0, [0xC0, 0, 0, 0, 0, 0, 0, 0x46]);

const PID_SUBJECT: u16 = 0x0037;
const PID_BODY: u16 = 0x1000;

// Named property ids in PSETID_Appointment.
const LID_BUSY_STATUS: u32 = 0x8205;
const LID_LOCATION: u32 = 0x8208;
const LID_START_WHOLE: u32 = 0x820D;
const LID_END_WHOLE: u32 = 0x820E;
const LID_ALL_DAY: u32 = 0x8215;
const LID_RECURRENCE_BLOB: u32 = 0x8216;
const LID_RECURRING: u32 = 0x8223;
// ... and in PSETID_Common.
const LID_REMINDER_DELTA: u32 = 0x8501;
const LID_REMINDER_SET: u32 = 0x8503;

#[derive(Default)]
struct PropIds {
    busy_status: u16,
    location: u16,
    start: u16,
    end: u16,
    all_day: u16,
    recurrence: u16,
    recurring: u16,
    reminder_delta: u16,
    reminder_set: u16,
}

pub fn read(path: &Path) -> Result<Imported, String> {
    let store = outlook_pst::open_store(path)
        .map_err(|e| format!("Die Datei ist keine lesbare Outlook-Datendatei (.pst): {e}"))?;
    let ids = named_ids(&*store).map_err(|e| format!("Die Eigenschaften der PST-Datei sind nicht lesbar: {e}"))?;
    let root = store
        .properties()
        .ipm_sub_tree_entry_id()
        .map_err(|e| format!("Die Ordnerstruktur der PST-Datei ist nicht lesbar: {e}"))?;
    let trash = store.properties().ipm_wastebasket_entry_id().ok().map(|e| e.node_id());

    let mut out = Imported { calendars: Vec::new(), skipped: 0, simplified: 0 };
    let mut seen = HashSet::new();
    let mut todo = vec![(root.node_id(), String::new())];
    while let Some((node, parent_name)) = todo.pop() {
        if !seen.insert(u32::from(node)) || Some(node) == trash {
            continue;
        }
        let Ok(entry_id) = store.properties().make_entry_id(node) else { continue };
        let Ok(folder) = store.open_folder(&entry_id) else {
            out.skipped += 1;
            continue;
        };
        let name = folder.properties().display_name().unwrap_or_default();

        if let Some(table) = folder.hierarchy_table() {
            for row in table.rows_matrix() {
                todo.push((NodeId::from(u32::from(row.id())), name.clone()));
            }
        }

        let mut events = Vec::new();
        if let Some(table) = folder.contents_table() {
            for row in table.rows_matrix() {
                let Ok(entry_id) = store.properties().make_entry_id(NodeId::from(u32::from(row.id()))) else {
                    continue;
                };
                let Ok(message) = store.open_message(&entry_id, None) else {
                    out.skipped += 1;
                    continue;
                };
                let class = message.properties().message_class().unwrap_or_default();
                if !class.to_ascii_uppercase().starts_with("IPM.APPOINTMENT") {
                    continue;
                }
                match read_appointment(&*message, &ids) {
                    Some(items) => events.extend(items),
                    None => out.skipped += 1,
                }
            }
        }
        if !events.is_empty() {
            let name = match (name.trim(), parent_name.trim()) {
                ("", "") => "Importierter Kalender".to_string(),
                ("", parent) => parent.to_string(),
                (name, _) => name.to_string(),
            };
            out.calendars.push(ImportedCalendar { name, events });
        }
    }
    out.calendars.reverse(); // `todo` is a stack: restore the order of the file
    Ok(out)
}

/// Named properties get file-specific ids; look up the ones used by appointments.
fn named_ids(store: &dyn Store) -> std::io::Result<PropIds> {
    let map = store.named_property_map()?;
    let props = map.properties();
    let guids = props.stream_guid()?;
    let mut ids = PropIds::default();
    for entry in props.stream_entry()? {
        let (NamedPropertyGuid::GuidIndex(index), NamedPropertyId::Number(number)) = (entry.guid(), entry.id()) else {
            continue;
        };
        let Some(guid) = guids.get(index as usize) else { continue };
        let key = (guid.data1(), guid.data2(), guid.data3(), *guid.data4());
        if key == COMMON_GUID {
            match number {
                LID_REMINDER_DELTA => ids.reminder_delta = entry.prop_id(),
                LID_REMINDER_SET => ids.reminder_set = entry.prop_id(),
                _ => {}
            }
            continue;
        }
        if key != APPOINTMENT_GUID {
            continue;
        }
        let slot = match number {
            LID_BUSY_STATUS => &mut ids.busy_status,
            LID_LOCATION => &mut ids.location,
            LID_START_WHOLE => &mut ids.start,
            LID_END_WHOLE => &mut ids.end,
            LID_ALL_DAY => &mut ids.all_day,
            LID_RECURRENCE_BLOB => &mut ids.recurrence,
            LID_RECURRING => &mut ids.recurring,
            _ => continue,
        };
        *slot = entry.prop_id();
    }
    Ok(ids)
}

fn text(message: &dyn Message, id: u16) -> String {
    if id == 0 {
        return String::new();
    }
    match message.properties().get(id) {
        Some(PropertyValue::Unicode(v)) => v.to_string(),
        Some(PropertyValue::String8(v)) => v.to_string(),
        _ => String::new(),
    }
}

fn truncate(mut text: String, max: usize) -> String {
    if text.len() > max {
        let mut cut = max;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
    }
    text
}

/// Outlook marks some subjects with a leading control prefix (`\u{1}` + a code character).
fn clean_subject(subject: String) -> String {
    let trimmed = subject.trim_start_matches('\u{1}');
    let trimmed = if trimmed.len() < subject.len() && trimmed.chars().next().is_some_and(|c| (c as u32) < 0x20) {
        &trimmed[trimmed.chars().next().map_or(0, char::len_utf8)..]
    } else {
        trimmed
    };
    trimmed.trim().to_string()
}

fn filetime_to_local(ticks: i64) -> Option<NaiveDateTime> {
    let secs = ticks / 10_000_000 - 11_644_473_600;
    let utc = chrono::DateTime::from_timestamp(secs, 0)?;
    Some(crate::model::convert_time(utc.naive_utc(), "UTC", &crate::model::display_tz()))
}

fn read_appointment(message: &dyn Message, ids: &PropIds) -> Option<Vec<ImportedEvent>> {
    let props = message.properties();
    let time = |id: u16| match props.get(id) {
        Some(PropertyValue::Time(t)) if id != 0 => filetime_to_local(*t),
        _ => None,
    };
    let flag = |id: u16| matches!(props.get(id), Some(PropertyValue::Boolean(true)));

    let title = truncate(clean_subject(text(message, PID_SUBJECT)), MAX_TITLE);
    let title = if title.is_empty() { "(Ohne Titel)".to_string() } else { title };
    let location = truncate(text(message, ids.location).trim().to_string(), MAX_LOCATION);
    let description = truncate(text(message, PID_BODY).replace("\r\n", "\n").trim().to_string(), MAX_DESCRIPTION);
    let remind = flag(ids.reminder_set).then(|| match props.get(ids.reminder_delta) {
        Some(PropertyValue::Integer32(minutes)) => u32::try_from(*minutes).unwrap_or(0).min(40_320),
        _ => 15,
    });
    // PidTagSensitivity: 2 = private, 3 = confidential
    let private = matches!(props.get(0x0036), Some(PropertyValue::Integer32(2 | 3)));
    let status = match props.get(ids.busy_status) {
        Some(PropertyValue::Integer32(0)) => "free",
        Some(PropertyValue::Integer32(1)) => "tentative",
        Some(PropertyValue::Integer32(3)) => "oof",
        Some(PropertyValue::Integer32(4)) => "elsewhere",
        _ => "busy",
    }
    .to_string();
    let all_day = flag(ids.all_day);

    let make = |start: NaiveDateTime, end: NaiveDateTime, title: &str, location: &str| {
        split_days(start, end, all_day)
            .into_iter()
            .map(|(start, end)| ImportedEvent {
                title: title.to_string(),
                location: location.to_string(),
                description: description.clone(),
                start,
                end,
                status: status.clone(),
                tz: crate::model::display_tz(),
                repeat: String::new(),
                repeat_until: None,
                repeat_interval: 1,
                repeat_days: 0,
                repeat_nth: 0,
                exceptions: Vec::new(),
                remind,
                attendees: Vec::new(),
                color: None,
                private,
                categories: Vec::new(),
            })
            .collect::<Vec<_>>()
    };

    if flag(ids.recurring) {
        if let Some(PropertyValue::Binary(blob)) = props.get(ids.recurrence) {
            if let Some(series) = Recurrence::parse(blob.buffer()) {
                let today = crate::model::today();
                // A series Kairo can express stays a series (with its deleted / changed days).
                if let Some(rule) = series.kairo_rule(today + Duration::days(HORIZON_DAYS)) {
                    let start = rule.first.and_time(NaiveTime::MIN) + Duration::minutes(series.start_offset);
                    let end = start + Duration::minutes((series.end_offset - series.start_offset).max(0));
                    let mut first = make(start, end, &title, &location);
                    if first.len() == 1 {
                        first[0].repeat = rule.repeat.to_string();
                        first[0].repeat_until = rule.until;
                        first[0].repeat_interval = rule.interval;
                        first[0].repeat_days = rule.days;
                        first[0].repeat_nth = rule.nth;
                        first[0].exceptions = rule.exceptions;
                        return Some(first);
                    }
                }
                let mut events = Vec::new();

                for (start, end, subject, place) in series.occurrences(today + Duration::days(HORIZON_DAYS)) {
                    let subject = subject.map(|s| truncate(s.trim().to_string(), MAX_TITLE)).filter(|s| !s.is_empty());
                    let place = place.map(|s| truncate(s.trim().to_string(), MAX_LOCATION));
                    events.extend(make(
                        start,
                        end,
                        subject.as_deref().unwrap_or(&title),
                        place.as_deref().unwrap_or(&location),
                    ));
                }
                if !events.is_empty() {
                    return Some(events);
                }
            }
        }
        // Unreadable series: keep at least the first occurrence below.
    }

    let start = time(ids.start)?;
    let end = time(ids.end).unwrap_or(start);
    Some(make(start, end, &title, &location))
}

// ---------------------------------------------------------------------------------------
// Recurrence: MS-OXOCAL "AppointmentRecurrencePattern"
// ---------------------------------------------------------------------------------------

/// A recurrence in the terms of `model::Event`.
struct KairoRule {
    repeat: &'static str,
    interval: u32,
    days: u8,
    nth: i8,
    until: Option<NaiveDate>,
    /// The first day the series really takes place.
    first: NaiveDate,
    exceptions: Vec<SeriesException>,
}

/// Outlook's weekday bits (Sunday = 1 ... Saturday = 0x40) in Kairo's (Monday = 1 ... Sunday = 64).
fn kairo_weekdays(outlook: u32) -> u8 {
    ((outlook >> 1 & 0x3F) | (outlook & 1) << 6) as u8
}

#[derive(Debug, PartialEq)]
enum Pattern {
    Daily { days: u32 },
    Weekly { weeks: u32, weekdays: u32 },
    /// `day` 31 or more means "last day of the month".
    Monthly { months: u32, day: u32 },
    /// The `nth` (1-4, 5 = last) day matching `weekdays` of the month.
    MonthlyNth { months: u32, weekdays: u32, nth: u32 },
}

#[derive(Debug, PartialEq)]
enum End {
    Never,
    On(NaiveDate),
    After(u32),
}

#[derive(Debug, PartialEq)]
struct Exception {
    start: NaiveDateTime,
    end: NaiveDateTime,
    original: NaiveDate,
    subject: Option<String>,
    location: Option<String>,
}

#[derive(Debug, PartialEq)]
struct Recurrence {
    pattern: Pattern,
    end: End,
    first_day_of_week: Weekday,
    first: NaiveDate,
    deleted: Vec<NaiveDate>,
    start_offset: i64,
    end_offset: i64,
    exceptions: Vec<Exception>,
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let bytes = self.data.get(self.pos..self.pos.checked_add(n)?)?;
        self.pos += n;
        Some(bytes)
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    /// Length-prefixed ANSI string (`len + 1`, `len`, bytes).
    fn ansi(&mut self) -> Option<String> {
        let _with_terminator = self.u16()?;
        let len = self.u16()? as usize;
        Some(self.take(len)?.iter().map(|&b| b as char).collect())
    }
}

/// Minutes since 1601-01-01 as used throughout the recurrence blob.
fn minutes_to_time(minutes: u32) -> Option<NaiveDateTime> {
    NaiveDate::from_ymd_opt(1601, 1, 1)?.and_time(NaiveTime::MIN).checked_add_signed(Duration::minutes(minutes.into()))
}

fn weekday_bit(day: Weekday) -> u32 {
    1 << day.num_days_from_sunday()
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let (next_year, next_month) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
    NaiveDate::from_ymd_opt(next_year, next_month, 1)
        .and_then(|d| d.pred_opt())
        .map_or(28, |d| d.day())
}

impl Recurrence {
    fn parse(data: &[u8]) -> Option<Self> {
        let mut r = Reader { data, pos: 0 };
        let (_reader_version, _writer_version) = (r.u16()?, r.u16()?);
        let _frequency = r.u16()?;
        let pattern_type = r.u16()?;
        let _calendar_type = r.u16()?;
        let _first_date_time = r.u32()?;
        let period = r.u32()?.max(1);
        let _sliding_flag = r.u32()?;
        let pattern = match pattern_type {
            0x0000 => Pattern::Daily { days: (period / 1440).max(1) },
            0x0001 => Pattern::Weekly { weeks: period, weekdays: r.u32()? },
            0x0002 | 0x000A => {
                let day = r.u32()?;
                Pattern::Monthly { months: period, day: if pattern_type == 0x000A { 31 } else { day } }
            }
            0x0003 => {
                let weekdays = r.u32()?;
                Pattern::MonthlyNth { months: period, weekdays, nth: r.u32()? }
            }
            _ => return None, // lunar calendars
        };
        let end_type = r.u32()?;
        let count = r.u32()?;
        let first_day_of_week = match r.u32()? {
            0 => Weekday::Sun,
            1 => Weekday::Mon,
            2 => Weekday::Tue,
            3 => Weekday::Wed,
            4 => Weekday::Thu,
            5 => Weekday::Fri,
            _ => Weekday::Sat,
        };
        let deleted_count = r.u32()?;
        let deleted = (0..deleted_count.min(10_000))
            .map(|_| minutes_to_time(r.u32()?).map(|t| t.date()))
            .collect::<Option<Vec<_>>>()?;
        let modified_count = r.u32()?;
        for _ in 0..modified_count.min(10_000) {
            r.u32()?;
        }
        let first = minutes_to_time(r.u32()?)?.date();
        let end_date = minutes_to_time(r.u32()?).map(|t| t.date());
        let end = match end_type {
            0x2021 => End::On(end_date?),
            0x2022 => End::After(count),
            _ => End::Never,
        };
        let (_reader_version2, _writer_version2) = (r.u32()?, r.u32()?);
        let start_offset = r.u32()?.into();
        let end_offset = r.u32()?.into();

        // Exceptions are optional: a damaged tail only costs the exceptions.
        let exceptions = Self::parse_exceptions(&mut r).unwrap_or_default();
        Some(Self { pattern, end, first_day_of_week, first, deleted, start_offset, end_offset, exceptions })
    }

    fn parse_exceptions(r: &mut Reader) -> Option<Vec<Exception>> {
        let count = r.u16()?;
        let mut out = Vec::new();
        for _ in 0..count {
            let start = minutes_to_time(r.u32()?)?;
            let end = minutes_to_time(r.u32()?)?;
            let original = minutes_to_time(r.u32()?)?.date();
            let flags = r.u16()?;
            let subject = if flags & 0x0001 != 0 { Some(r.ansi()?) } else { None };
            if flags & 0x0002 != 0 {
                r.u32()?;
            }
            if flags & 0x0004 != 0 {
                r.u32()?;
            }
            if flags & 0x0008 != 0 {
                r.u32()?;
            }
            let location = if flags & 0x0010 != 0 { Some(r.ansi()?) } else { None };
            out.push(Exception { start, end, original, subject, location });
            // BusyStatus, Attachment, SubType and Color follow; they don't matter here and
            // the rest of the structure is only needed for further exceptions.
            for bit in [0x0020, 0x0040, 0x0080, 0x0100] {
                if flags & bit != 0 {
                    r.u32()?;
                }
            }
        }
        Some(out)
    }

    /// This series as a Kairo repeat rule, if Kairo can show exactly the same days; otherwise
    /// `None` (the caller then lists every occurrence).
    fn kairo_rule(&self, limit: NaiveDate) -> Option<KairoRule> {
        let (repeat, interval, days, nth) = match self.pattern {
            Pattern::Daily { days } => ("daily", days, 0, 0),
            Pattern::Weekly { weeks, weekdays } => ("weekly", weeks, kairo_weekdays(weekdays), 0),
            Pattern::Monthly { months, .. } if months % 12 == 0 => ("yearly", months / 12, 0, 0),
            Pattern::Monthly { months, .. } => ("monthly", months, 0, 0),
            Pattern::MonthlyNth { months, weekdays, nth } => {
                let nth = match nth {
                    1..=4 => nth as i8,
                    5 => -1,
                    _ => return None,
                };
                if weekdays.count_ones() != 1 {
                    return None; // "every weekday" and the like
                }
                if months % 12 == 0 { ("yearly", months / 12, 0, nth) } else { ("monthly", months, 0, nth) }
            }
        };
        let counted = matches!(self.end, End::After(_));
        let dates = self.dates(if counted { self.first + Duration::days(366 * 50) } else { limit });
        let first = *dates.first()?;
        let until = match self.end {
            End::Never => None,
            End::On(date) => Some(date),
            End::After(_) => dates.last().copied(),
        };

        // Only take the rule if it hits exactly the same days.
        let probe = crate::model::Event {
            id: String::new(),
            calendar: String::new(),
            title: String::new(),
            location: String::new(),
            description: String::new(),
            start: first.and_time(NaiveTime::MIN),
            end: first.and_time(NaiveTime::MIN),
            tz: String::new(),
            status: String::new(),
            repeat: repeat.to_string(),
            repeat_until: until,
            repeat_interval: interval,
            repeat_days: days,
            repeat_nth: nth,
            exceptions: Vec::new(),
            remind: None,
            attendees: Vec::new(),
            color: None,
            private: false,
            categories: Vec::new(),
            rev: 0,
            dirty: false,
            draft: false,
        };
        let mut day = first;
        let last = until.unwrap_or(limit).min(limit.max(dates.last().copied().unwrap_or(first)));
        while day <= last {
            if probe.occurrences_on(day).is_empty() == dates.contains(&day) {
                return None;
            }
            day += Duration::days(1);
        }

        let changed: HashSet<NaiveDate> = self.exceptions.iter().map(|e| e.original).collect();
        let mut exceptions: Vec<SeriesException> = self
            .deleted
            .iter()
            .filter(|d| !changed.contains(d) && **d >= first)
            .map(|&date| SeriesException { date, deleted: true, start: None, end: None, title: None, location: None })
            .collect();
        exceptions.extend(self.exceptions.iter().map(|e| SeriesException {
            date: e.original,
            deleted: false,
            start: Some(e.start),
            end: Some(e.end),
            title: e.subject.as_ref().map(|s| truncate(s.trim().to_string(), MAX_TITLE)).filter(|s| !s.is_empty()),
            location: e.location.as_ref().map(|s| truncate(s.trim().to_string(), MAX_LOCATION)),
        }));
        exceptions.sort_by_key(|e| e.date);
        exceptions.dedup_by_key(|e| e.date);
        if exceptions.len() > 1000 {
            return None;
        }
        Some(KairoRule { repeat, interval, days, nth, until, first, exceptions })
    }

    /// Dates of all occurrences up to `limit`, before deletions and exceptions.
    fn dates(&self, limit: NaiveDate) -> Vec<NaiveDate> {
        let until = match self.end {
            End::On(date) => date.min(limit),
            _ => limit,
        };
        let max = match self.end {
            End::After(n) => (n as usize).min(MAX_OCCURRENCES),
            _ => MAX_OCCURRENCES,
        };
        let mut out = Vec::new();
        let first = self.first;
        match self.pattern {
            Pattern::Daily { days } => {
                let mut day = first;
                while day <= until && out.len() < max {
                    out.push(day);
                    day += Duration::days(days.into());
                }
            }
            Pattern::Weekly { weeks, weekdays } => {
                let back = (first.weekday().num_days_from_sunday() + 7 - self.first_day_of_week.num_days_from_sunday()) % 7;
                let week_start = first - Duration::days(back.into());
                let mut day = first;
                while day <= until && out.len() < max {
                    let week = (day - week_start).num_days() / 7;
                    if week % i64::from(weeks) == 0 && weekdays & weekday_bit(day.weekday()) != 0 {
                        out.push(day);
                    }
                    day += Duration::days(1);
                }
            }
            Pattern::Monthly { months, day } => {
                let (mut year, mut month) = (first.year(), first.month());
                'months: loop {
                    let date = NaiveDate::from_ymd_opt(year, month, day.min(days_in_month(year, month)).max(1));
                    if let Some(date) = date {
                        if date > until || out.len() >= max {
                            break 'months;
                        }
                        if date >= first {
                            out.push(date);
                        }
                    }
                    (year, month) = add_months(year, month, months);
                }
            }
            Pattern::MonthlyNth { months, weekdays, nth } => {
                let (mut year, mut month) = (first.year(), first.month());
                loop {
                    let matching: Vec<NaiveDate> = (1..=days_in_month(year, month))
                        .filter_map(|d| NaiveDate::from_ymd_opt(year, month, d))
                        .filter(|d| weekdays & weekday_bit(d.weekday()) != 0)
                        .collect();
                    let pick = if nth >= 5 { matching.last() } else { matching.get(nth.saturating_sub(1) as usize) };
                    if NaiveDate::from_ymd_opt(year, month, 1).is_none_or(|d| d > until) || out.len() >= max {
                        break;
                    }
                    if let Some(&date) = pick {
                        if date >= first && date <= until {
                            out.push(date);
                        }
                    }
                    (year, month) = add_months(year, month, months);
                }
            }
        }
        out
    }

    /// Start, end, and the subject / location if the occurrence overrides them.
    fn occurrences(
        &self,
        limit: NaiveDate,
    ) -> Vec<(NaiveDateTime, NaiveDateTime, Option<String>, Option<String>)> {
        let duration = Duration::minutes((self.end_offset - self.start_offset).max(0));
        let mut out: Vec<_> = self
            .dates(limit)
            .into_iter()
            .filter(|d| !self.deleted.contains(d))
            .map(|date| {
                let start = date.and_time(NaiveTime::MIN) + Duration::minutes(self.start_offset);
                (start, start + duration, None, None)
            })
            .collect();
        out.extend(
            self.exceptions
                .iter()
                .map(|e| (e.start, e.end, e.subject.clone(), e.location.clone())),
        );
        out.sort_by_key(|o| o.0);
        out
    }
}

fn add_months(year: i32, month: u32, months: u32) -> (i32, u32) {
    let total = year * 12 + month as i32 - 1 + months as i32;
    (total.div_euclid(12), total.rem_euclid(12) as u32 + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn series(pattern: Pattern, end: End, first: NaiveDate) -> Recurrence {
        Recurrence {
            pattern,
            end,
            first_day_of_week: Weekday::Mon,
            first,
            deleted: vec![],
            start_offset: 9 * 60,
            end_offset: 10 * 60,
            exceptions: vec![],
        }
    }

    #[test]
    fn weekly_every_second_week_on_two_days() {
        // Monday and Wednesday, every 2nd week, 4 occurrences
        let rec = series(Pattern::Weekly { weeks: 2, weekdays: 0b0001010 }, End::After(4), d(2026, 10, 5));
        assert_eq!(rec.dates(d(2030, 1, 1)), vec![d(2026, 10, 5), d(2026, 10, 7), d(2026, 10, 19), d(2026, 10, 21)]);
    }

    #[test]
    fn series_become_kairo_series() {
        let limit = d(2030, 1, 1);
        let weekly = series(Pattern::Weekly { weeks: 1, weekdays: weekday_bit(Weekday::Mon) }, End::After(3), d(2026, 10, 5));
        let rule = weekly.kairo_rule(limit).unwrap();
        assert_eq!((rule.repeat, rule.interval, rule.until), ("weekly", 1, Some(d(2026, 10, 19))));

        let open = series(Pattern::Daily { days: 1 }, End::Never, d(2026, 10, 5));
        assert!(open.kairo_rule(limit).is_some_and(|r| r.repeat == "daily" && r.until.is_none()));

        // every 2nd week on Monday and Wednesday (Outlook bits: Mon 2, Wed 8)
        let two_days = series(Pattern::Weekly { weeks: 2, weekdays: 0b0001010 }, End::Never, d(2026, 10, 5));
        let rule = two_days.kairo_rule(limit).unwrap();
        assert_eq!((rule.interval, rule.days), (2, 0b0000101));

        // every 2nd Tuesday of the month, last Friday of the year's October
        let nth = series(Pattern::MonthlyNth { months: 1, weekdays: 0b0000100, nth: 2 }, End::Never, d(2026, 10, 1));
        let rule = nth.kairo_rule(limit).unwrap();
        assert_eq!((rule.repeat, rule.nth, rule.first), ("monthly", 2, d(2026, 10, 13)));
        let last = series(Pattern::MonthlyNth { months: 12, weekdays: 0b0100000, nth: 5 }, End::Never, d(2026, 10, 1));
        assert!(last.kairo_rule(limit).is_some_and(|r| r.repeat == "yearly" && r.nth == -1));
        assert!(series(Pattern::MonthlyNth { months: 1, weekdays: 0b0111110, nth: 1 }, End::Never, d(2026, 10, 1))
            .kairo_rule(limit)
            .is_none());
    }

    #[test]
    fn deleted_and_changed_days_travel_with_the_series() {
        let mut rec = series(Pattern::Daily { days: 1 }, End::On(d(2026, 10, 9)), d(2026, 10, 5));
        rec.deleted = vec![d(2026, 10, 6), d(2026, 10, 7)];
        rec.exceptions = vec![Exception {
            start: d(2026, 10, 7).and_hms_opt(14, 0, 0).unwrap(),
            end: d(2026, 10, 7).and_hms_opt(15, 0, 0).unwrap(),
            original: d(2026, 10, 7),
            subject: Some("Verschoben".into()),
            location: None,
        }];
        let rule = rec.kairo_rule(d(2030, 1, 1)).unwrap();
        assert_eq!(rule.exceptions.len(), 2);
        assert!(rule.exceptions[0].deleted && rule.exceptions[0].date == d(2026, 10, 6));
        assert_eq!(rule.exceptions[1].title.as_deref(), Some("Verschoben"));
    }

    #[test]
    fn monthly_clamps_to_the_last_day() {
        let rec = series(Pattern::Monthly { months: 1, day: 31 }, End::After(3), d(2026, 1, 31));
        assert_eq!(rec.dates(d(2030, 1, 1)), vec![d(2026, 1, 31), d(2026, 2, 28), d(2026, 3, 31)]);
    }

    #[test]
    fn nth_weekday_of_the_month() {
        // Second Tuesday and last Friday
        let second_tuesday = series(Pattern::MonthlyNth { months: 1, weekdays: 0b0000100, nth: 2 }, End::After(2), d(2026, 10, 1));
        assert_eq!(second_tuesday.dates(d(2030, 1, 1)), vec![d(2026, 10, 13), d(2026, 11, 10)]);
        let last_friday = series(Pattern::MonthlyNth { months: 1, weekdays: 0b0100000, nth: 5 }, End::After(2), d(2026, 10, 1));
        assert_eq!(last_friday.dates(d(2030, 1, 1)), vec![d(2026, 10, 30), d(2026, 11, 27)]);
    }

    #[test]
    fn deletions_and_exceptions_are_applied() {
        let mut rec = series(Pattern::Daily { days: 1 }, End::On(d(2026, 10, 7)), d(2026, 10, 5));
        rec.deleted = vec![d(2026, 10, 6)];
        rec.exceptions = vec![Exception {
            start: d(2026, 10, 7).and_hms_opt(14, 0, 0).unwrap(),
            end: d(2026, 10, 7).and_hms_opt(15, 0, 0).unwrap(),
            original: d(2026, 10, 7),
            subject: Some("Verschoben".into()),
            location: None,
        }];
        rec.deleted.push(d(2026, 10, 7));
        let starts: Vec<_> = rec.occurrences(d(2030, 1, 1)).into_iter().map(|o| (o.0.date(), o.0.hour_minute())).collect();
        assert_eq!(starts, vec![(d(2026, 10, 5), (9, 0)), (d(2026, 10, 7), (14, 0))]);
    }

    trait HourMinute {
        fn hour_minute(&self) -> (u32, u32);
    }

    impl HourMinute for NaiveDateTime {
        fn hour_minute(&self) -> (u32, u32) {
            use chrono::Timelike;
            (self.hour(), self.minute())
        }
    }

    #[test]
    fn midnight_crossing_items_keep_their_length() {
        let start = d(2026, 10, 5).and_hms_opt(22, 0, 0).unwrap();
        let end = d(2026, 10, 6).and_hms_opt(2, 0, 0).unwrap();
        assert_eq!(split_days(start, end, false), vec![(start, end)]);
    }

    #[test]
    fn all_day_items_cover_whole_days() {
        let start = d(2026, 10, 5).and_hms_opt(0, 0, 0).unwrap();
        let parts = split_days(start, d(2026, 10, 7).and_hms_opt(0, 0, 0).unwrap(), true);
        assert_eq!(parts, vec![(start, d(2026, 10, 6).and_hms_opt(23, 59, 0).unwrap())]);
        // a single day
        let one = split_days(start, d(2026, 10, 6).and_hms_opt(0, 0, 0).unwrap(), true);
        assert_eq!(one, vec![(start, d(2026, 10, 5).and_hms_opt(23, 59, 0).unwrap())]);
    }

    #[test]
    fn subjects_lose_their_control_prefix() {
        assert_eq!(clean_subject("\u{1}\u{1}Zahnarzt".into()), "Zahnarzt");
        assert_eq!(clean_subject("  Zahnarzt ".into()), "Zahnarzt");
    }

    /// `KAIRO_TEST_PST=path/to/file.pst cargo test pst -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn reads_a_real_file() {
        let path = std::env::var("KAIRO_TEST_PST").expect("KAIRO_TEST_PST");
        let imported = read(Path::new(&path)).unwrap();
        println!("skipped: {}", imported.skipped);
        for calendar in &imported.calendars {
            println!("{}: {} events", calendar.name, calendar.events.len());
            for e in calendar.events.iter().take(15) {
                println!("  {} - {}  {} [{}] @{} remind={:?}", e.start, e.end, e.title, e.status, e.location, e.remind);
            }
        }
        assert!(!imported.calendars.is_empty());
    }
}
