//! iCalendar (`.ics`) files: reading them into Kairo events and writing a calendar out.
//!
//! Everything here is blocking; call it from `cx.background_executor()`.
//!
//! Like the Outlook import, times in UTC or in another zone are shown in Kairo's default time
//! zone; times with the default zone (or without any zone) are taken as they are.

use std::path::Path;

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Utc, Weekday};

use crate::model::{Event, Exception, convert_time, display_tz, weekday_bit};
use crate::import::{Imported, ImportedCalendar, ImportedEvent, split_days};

// Limits of the Kairo server (see `server/src/main.rs`).
const MAX_TITLE: usize = 500;
const MAX_LOCATION: usize = 500;
const MAX_DESCRIPTION: usize = 20_000;
const MAX_EXCEPTIONS: usize = 1000;
/// Counted series are followed this many days to find their last day.
const COUNT_HORIZON_DAYS: i64 = 366 * 30;

// ---------------------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct Property {
    name: String,
    params: Vec<(String, String)>,
    value: String,
}

impl Property {
    fn param(&self, key: &str) -> Option<&str> {
        self.params.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Default)]
struct Component {
    props: Vec<Property>,
    /// `TRIGGER` of the alarms inside.
    triggers: Vec<String>,
}

impl Component {
    fn get(&self, name: &str) -> Option<&Property> {
        self.props.iter().find(|p| p.name.eq_ignore_ascii_case(name))
    }

    fn all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Property> {
        self.props.iter().filter(move |p| p.name.eq_ignore_ascii_case(name))
    }

    fn text(&self, name: &str) -> String {
        self.get(name).map(|p| unescape(&p.value)).unwrap_or_default()
    }
}

pub fn read(path: &Path) -> Result<Imported, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("Die Datei ist nicht lesbar: {e}"))?;
    let text = String::from_utf8_lossy(&bytes);
    let fallback = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    parse(&text, &fallback)
}

fn parse(text: &str, fallback_name: &str) -> Result<Imported, String> {
    let mut name = None;
    let mut events: Vec<Component> = Vec::new();
    let mut current: Option<Component> = None;
    let mut alarm = false;
    let mut seen_calendar = false;

    for line in unfold(text) {
        let Some(prop) = parse_line(&line) else { continue };
        match (prop.name.to_ascii_uppercase().as_str(), prop.value.to_ascii_uppercase().as_str()) {
            ("BEGIN", "VCALENDAR") => seen_calendar = true,
            ("BEGIN", "VEVENT") => current = Some(Component::default()),
            ("END", "VEVENT") => events.extend(current.take()),
            ("BEGIN", "VALARM") => alarm = true,
            ("END", "VALARM") => alarm = false,
            _ => match &mut current {
                Some(event) if alarm => {
                    if prop.name.eq_ignore_ascii_case("TRIGGER") {
                        event.triggers.push(prop.value);
                    }
                }
                Some(event) => event.props.push(prop),
                None if prop.name.eq_ignore_ascii_case("X-WR-CALNAME") => name = Some(unescape(&prop.value)),
                None => {}
            },
        }
    }
    if !seen_calendar {
        return Err("Die Datei ist keine iCalendar-Datei (.ics).".into());
    }

    let mut out = Imported { calendars: Vec::new(), skipped: 0, simplified: 0 };
    let (overrides, masters): (Vec<_>, Vec<_>) = events.into_iter().partition(|e| e.get("RECURRENCE-ID").is_some());
    let mut imported: Vec<(String, ImportedEvent)> = Vec::new();
    for master in &masters {
        match convert(master) {
            Some((pieces, simplified)) => {
                out.simplified += usize::from(simplified);
                let uid = master.text("UID");
                imported.extend(pieces.into_iter().map(|e| (uid.clone(), e)));
            }
            None => out.skipped += 1,
        }
    }
    // Changed single repetitions belong to the series with the same UID.
    for change in &overrides {
        let uid = change.text("UID");
        let Some((pieces, _)) = convert(change) else {
            out.skipped += 1;
            continue;
        };
        let Some(single) = pieces.into_iter().next() else { continue };
        let Some((_, series)) = imported.iter_mut().find(|(id, e)| *id == uid && !e.repeat.is_empty()) else {
            out.skipped += 1;
            continue;
        };
        let Some(origin) = change.get("RECURRENCE-ID").and_then(|p| parse_time_in(p, &series.tz)) else {
            out.skipped += 1;
            continue;
        };
        // The changed times are written in the zone of the change itself.
        let (change_start, change_end) =
            (convert_time(single.start, &single.tz, &series.tz), convert_time(single.end, &single.tz, &series.tz));
        let cancelled = change.text("STATUS").eq_ignore_ascii_case("CANCELLED");
        series.exceptions.retain(|x| x.date != origin.date());
        series.exceptions.push(Exception {
            date: origin.date(),
            deleted: cancelled,
            start: (!cancelled).then_some(change_start),
            end: (!cancelled).then_some(change_end),
            title: (!cancelled && single.title != series.title).then(|| single.title.clone()),
            location: (!cancelled && single.location != series.location).then(|| single.location.clone()),
        });
    }

    let events: Vec<ImportedEvent> = imported
        .into_iter()
        .map(|(_, mut e)| {
            e.exceptions.sort_by_key(|x| x.date);
            e.exceptions.truncate(MAX_EXCEPTIONS);
            e
        })
        .collect();
    if events.is_empty() && out.skipped == 0 {
        return Err("Die Datei enthält keine Termine.".into());
    }
    let name = name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| {
        if fallback_name.trim().is_empty() { "Importierter Kalender".into() } else { fallback_name.trim().to_string() }
    });
    out.calendars.push(ImportedCalendar { name, events });
    Ok(out)
}

/// Joins continuation lines (starting with a blank or tab) to the line before.
fn unfold(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.trim_end_matches('\r');
        if let Some(rest) = raw.strip_prefix(' ').or_else(|| raw.strip_prefix('\t')) {
            if let Some(last) = lines.last_mut() {
                last.push_str(rest);
                continue;
            }
        }
        lines.push(raw.to_string());
    }
    lines
}

fn parse_line(line: &str) -> Option<Property> {
    // The first colon outside of quotes separates the name (and parameters) from the value.
    let mut quoted = false;
    let colon = line.char_indices().find_map(|(i, c)| {
        match c {
            '"' => quoted = !quoted,
            ':' if !quoted => return Some(i),
            _ => {}
        }
        None
    })?;
    let (head, value) = (&line[..colon], &line[colon + 1..]);

    let mut parts = Vec::new();
    let (mut start, mut quoted) = (0, false);
    for (i, c) in head.char_indices() {
        match c {
            '"' => quoted = !quoted,
            ';' if !quoted => {
                parts.push(&head[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&head[start..]);
    let name = parts[0].trim().to_string();
    if name.is_empty() {
        return None;
    }
    let params = parts[1..]
        .iter()
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().trim_matches('"').to_string()))
        .collect();
    Some(Property { name, params, value: value.to_string() })
}

fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n' | 'N') => out.push('\n'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

/// A time as written in the file (wall-clock), the zone it was written in (`None` = Kairo's
/// default or unknown) and whether it was a bare date.
fn parse_time_raw(prop: &Property) -> Option<(NaiveDateTime, bool, Option<String>)> {
    let value = prop.value.trim();
    if value.len() == 8 || prop.param("VALUE").is_some_and(|v| v.eq_ignore_ascii_case("DATE")) {
        let date = NaiveDate::parse_from_str(value.get(..8)?, "%Y%m%d").ok()?;
        return Some((date.and_time(NaiveTime::MIN), true, None));
    }
    let utc = value.ends_with('Z') || value.ends_with('z');
    let naive = NaiveDateTime::parse_from_str(value.trim_end_matches(['Z', 'z']), "%Y%m%dT%H%M%S").ok()?;
    let zone = if utc {
        Some("UTC".to_string())
    } else {
        prop.param("TZID").map(|tz| tz.trim_matches('/').to_string()).filter(|tz| tz.parse::<chrono_tz::Tz>().is_ok())
    };
    Some((naive, false, zone))
}

/// The time converted into the zone `tz` (values without a known zone are taken as they are).
fn parse_time_in(prop: &Property, tz: &str) -> Option<NaiveDateTime> {
    let (time, _, zone) = parse_time_raw(prop)?;
    Some(match zone {
        Some(zone) => convert_time(time, &zone, tz),
        None => time,
    })
}

/// `PT1H30M`, `P1D`, `-PT15M` as a duration.
fn parse_duration(text: &str) -> Option<Duration> {
    let text = text.trim();
    let (negative, text) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let rest = text.strip_prefix('P')?;
    let mut total = Duration::zero();
    let (mut number, mut in_time) = (String::new(), false);
    for c in rest.chars() {
        match c {
            'T' => in_time = true,
            '0'..='9' => number.push(c),
            unit => {
                let n: i64 = number.parse().ok()?;
                number.clear();
                total += match (unit, in_time) {
                    ('W', _) => Duration::weeks(n),
                    ('D', _) => Duration::days(n),
                    ('H', true) => Duration::hours(n),
                    ('M', true) => Duration::minutes(n),
                    ('S', true) => Duration::seconds(n),
                    _ => return None,
                };
            }
        }
    }
    Some(if negative { -total } else { total })
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

fn weekday_from_code(code: &str) -> Option<Weekday> {
    Some(match code.to_ascii_uppercase().as_str() {
        "MO" => Weekday::Mon,
        "TU" => Weekday::Tue,
        "WE" => Weekday::Wed,
        "TH" => Weekday::Thu,
        "FR" => Weekday::Fri,
        "SA" => Weekday::Sat,
        "SU" => Weekday::Sun,
        _ => return None,
    })
}

/// The items of a comma separated list (`\,` is a comma inside an item).
fn split_list(text: &str) -> Vec<String> {
    let mut items = vec![String::new()];
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(next) = chars.next() {
                    items.last_mut().map(|item| item.push(next));
                }
            }
            ',' => items.push(String::new()),
            other => items.last_mut().map(|item| item.push(other)).unwrap_or_default(),
        }
    }
    items
}

/// `#rrggbb` out of a color value (an alpha part such as `#rrggbbff` is dropped).
fn hex_color(text: &str) -> Option<String> {
    let text = text.trim();
    let digits = text.strip_prefix('#')?;
    (digits.len() >= 6 && digits[..6].chars().all(|c| c.is_ascii_hexdigit())).then(|| format!("#{}", digits[..6].to_ascii_lowercase()))
}

/// One `VEVENT` as Kairo events (a series, or one entry per day of a long event); the flag
/// says that its repeat rule had to be simplified.
fn convert(component: &Component) -> Option<(Vec<ImportedEvent>, bool)> {
    let (start, all_day, zone) = parse_time_raw(component.get("DTSTART")?)?;
    let tz = zone.unwrap_or_else(display_tz);
    let end = match (component.get("DTEND").and_then(|p| parse_time_in(p, &tz)), component.get("DURATION")) {
        (Some(end), _) => end,
        (None, Some(duration)) => start + parse_duration(&duration.value)?,
        (None, None) if all_day => start + Duration::days(1),
        (None, None) => start,
    };
    let title = truncate(component.text("SUMMARY").trim().to_string(), MAX_TITLE);
    let busy_status = component.text("X-MICROSOFT-CDO-BUSYSTATUS").to_ascii_uppercase();
    let status = match busy_status.as_str() {
        "OOF" => "oof",
        "WORKINGELSEWHERE" => "elsewhere",
        "FREE" => "free",
        "TENTATIVE" => "tentative",
        _ if component.text("TRANSP").eq_ignore_ascii_case("TRANSPARENT") => "free",
        _ if component.text("STATUS").eq_ignore_ascii_case("TENTATIVE") => "tentative",
        _ => "busy",
    };
    let private = matches!(component.text("CLASS").to_ascii_uppercase().as_str(), "PRIVATE" | "CONFIDENTIAL");
    let categories: Vec<String> = component
        .all("CATEGORIES")
        .flat_map(|p| split_list(&p.value))
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty() && c.len() <= 60 && !c.chars().any(char::is_control))
        .take(20)
        .collect();
    let remind = component
        .triggers
        .iter()
        .filter_map(|t| parse_duration(t))
        .find(|d| *d <= Duration::zero())
        .map(|d| u32::try_from((-d).num_minutes()).unwrap_or(0).min(40_320));
    let attendees = component
        .all("ATTENDEE")
        .filter_map(|a| {
            let address = a.value.trim();
            let address = address.get(..7).filter(|p| p.eq_ignore_ascii_case("mailto:")).map_or(address, |_| &address[7..]);
            (address.contains('@') && address.len() <= 254 && !address.chars().any(char::is_whitespace))
                .then(|| address.to_string())
        })
        .collect();

    // Kairo shows an event on the day it starts: longer ones are cut into one piece per day.
    let parts = split_days(start, end, all_day);
    let first = *parts.first()?;
    let mut event = ImportedEvent {
        title: if title.is_empty() { "(Ohne Titel)".into() } else { title },
        location: truncate(component.text("LOCATION").trim().to_string(), MAX_LOCATION),
        description: truncate(component.text("DESCRIPTION").replace("\r\n", "\n").trim().to_string(), MAX_DESCRIPTION),
        start: first.0,
        end: first.1,
        tz,
        status: status.into(),
        repeat: String::new(),
        repeat_until: None,
        repeat_interval: 1,
        repeat_days: 0,
        repeat_nth: 0,
        exceptions: Vec::new(),
        remind,
        attendees,
        color: ["COLOR", "X-APPLE-CALENDAR-COLOR"].iter().find_map(|name| hex_color(&component.text(name))),
        private,
        categories,
    };

    let mut simplified = false;
    if let Some(rule) = component.get("RRULE") {
        match apply_rule(&mut event, &rule.value) {
            Some(()) => {
                let deleted = component
                    .all("EXDATE")
                    .flat_map(|p| p.value.split(',').map(move |v| Property { value: v.to_string(), ..p.clone() }))
                    .filter_map(|p| parse_time_in(&p, &event.tz));
                event.exceptions = deleted
                    .map(|t| Exception { date: t.date(), deleted: true, start: None, end: None, title: None, location: None })
                    .collect();
            }
            None => simplified = true, // stays a single event
        }
    }
    if !event.repeat.is_empty() || parts.len() == 1 {
        return Some((vec![event], simplified));
    }
    let pieces = parts.into_iter().map(|(start, end)| ImportedEvent { start, end, ..event.clone() }).collect();
    Some((pieces, simplified))
}

/// Turns an `RRULE` into the repeat fields of `event`; `None` if Kairo can't express it.
fn apply_rule(event: &mut ImportedEvent, rule: &str) -> Option<()> {
    let mut freq = "";
    let (mut interval, mut count) = (1u32, None::<u32>);
    let (mut until, mut by_day, mut by_month_day, mut by_set_pos, mut by_month) = (None, Vec::new(), None, None, None);
    for part in rule.split(';') {
        let (key, value) = part.split_once('=')?;
        match key.to_ascii_uppercase().as_str() {
            "FREQ" => {
                freq = match value.to_ascii_uppercase().as_str() {
                    "DAILY" => "daily",
                    "WEEKLY" => "weekly",
                    "MONTHLY" => "monthly",
                    "YEARLY" => "yearly",
                    _ => return None,
                }
            }
            "INTERVAL" => interval = value.parse().ok().filter(|n| (1..=999).contains(n))?,
            "COUNT" => count = Some(value.parse().ok()?),
            "UNTIL" => {
                let prop = Property { value: value.to_string(), ..Default::default() };
                let time = parse_time_in(&prop, &event.tz)?;
                // A time before the event's own time of day ends the series the day before.
                until = Some(if value.len() > 8 && time.time() < event.start.time() {
                    time.date().pred_opt()?
                } else {
                    time.date()
                });
            }
            "BYDAY" => by_day = value.split(',').map(str::to_string).collect(),
            "BYMONTHDAY" => by_month_day = Some(value.parse::<i32>().ok()?),
            "BYSETPOS" => by_set_pos = Some(value.parse::<i32>().ok()?),
            "BYMONTH" => by_month = Some(value.parse::<u32>().ok()?),
            "WKST" => {}
            _ => return None,
        }
    }
    if freq.is_empty() {
        return None;
    }
    let first = event.start.date();

    let (mut days, mut nth) = (0u8, 0i8);
    match freq {
        "daily" => {
            if !by_day.is_empty() || by_month_day.is_some() || by_month.is_some() {
                return None;
            }
        }
        "weekly" => {
            for code in &by_day {
                days |= weekday_bit(weekday_from_code(code)?);
            }
            if by_month_day.is_some() || by_month.is_some() {
                return None;
            }
        }
        _ => {
            if by_month.is_some_and(|m| m != first.month()) || by_month_day.is_some_and(|d| d != first.day() as i32) {
                return None;
            }
            match by_day.as_slice() {
                [] => {}
                [one] => {
                    // "2TU", "-1FR", or "TU" together with BYSETPOS
                    let split = one.find(|c: char| c.is_ascii_alphabetic())?;
                    let (ordinal, code) = one.split_at(split);
                    if weekday_from_code(code)? != first.weekday() {
                        return None;
                    }
                    let position = if ordinal.is_empty() { by_set_pos? } else { ordinal.parse::<i32>().ok()? };
                    nth = match position {
                        1..=4 => position as i8,
                        -1 | 5 => -1,
                        _ => return None,
                    };
                }
                _ => return None,
            }
        }
    }

    event.repeat = freq.to_string();
    event.repeat_interval = interval;
    event.repeat_days = days;
    event.repeat_nth = nth;
    event.repeat_until = until;
    if let Some(count) = count {
        // The last day of a counted series.
        let probe = probe_event(event);
        let (mut found, mut last) = (0, None);
        for offset in 0..COUNT_HORIZON_DAYS {
            let day = first + Duration::days(offset);
            if !probe.occurrences_on(day).is_empty() {
                found += 1;
                last = Some(day);
                if found >= count {
                    break;
                }
            }
        }
        event.repeat_until = last;
    }
    Some(())
}

#[cfg(test)]
fn probe_with(source: &ImportedEvent, id: &str) -> Event {
    let mut event = probe_event(source);
    event.id = id.into();
    event
}

fn probe_event(event: &ImportedEvent) -> Event {
    Event {
        id: String::new(),
        calendar: String::new(),
        title: String::new(),
        location: String::new(),
        description: String::new(),
        start: event.start,
        end: event.end,
        tz: event.tz.clone(),
        status: String::new(),
        repeat: event.repeat.clone(),
        repeat_until: event.repeat_until,
        repeat_interval: event.repeat_interval,
        repeat_days: event.repeat_days,
        repeat_nth: event.repeat_nth,
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

// ---------------------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------------------

fn escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace(';', "\\;").replace(',', "\\,").replace("\r\n", "\\n").replace('\n', "\\n")
}

/// Lines are at most 75 bytes; continuation lines start with a blank.
fn fold(line: &str) -> String {
    let mut out = String::new();
    let mut length = 0;
    for c in line.chars() {
        if length + c.len_utf8() > 75 {
            out.push_str("\r\n ");
            length = 1;
        }
        out.push(c);
        length += c.len_utf8();
    }
    out
}

const WEEKDAY_CODES: [&str; 7] = ["MO", "TU", "WE", "TH", "FR", "SA", "SU"];

fn local(event: &Event, time: NaiveDateTime, all_day: bool) -> String {
    if all_day {
        format!(";VALUE=DATE:{}", time.format("%Y%m%d"))
    } else {
        format!(";TZID={}:{}", event.tz, time.format("%Y%m%dT%H%M%S"))
    }
}

fn rrule(event: &Event) -> Option<String> {
    let freq = match event.repeat.as_str() {
        "daily" => "DAILY",
        "weekly" => "WEEKLY",
        "monthly" => "MONTHLY",
        "yearly" => "YEARLY",
        _ => return None,
    };
    let mut rule = format!("FREQ={freq}");
    if event.repeat_interval > 1 {
        rule += &format!(";INTERVAL={}", event.repeat_interval);
    }
    if freq == "WEEKLY" && event.repeat_days != 0 {
        let days: Vec<&str> = (0..7).filter(|i| event.repeat_days >> i & 1 == 1).map(|i| WEEKDAY_CODES[i]).collect();
        rule += &format!(";BYDAY={}", days.join(","));
    }
    if matches!(freq, "MONTHLY" | "YEARLY") && event.repeat_nth != 0 {
        rule += &format!(";BYDAY={}{}", event.repeat_nth, WEEKDAY_CODES[event.start.weekday().num_days_from_monday() as usize]);
    }
    if let Some(until) = event.repeat_until {
        rule += &format!(";UNTIL={}T235959Z", until.format("%Y%m%d"));
    }
    Some(rule)
}

/// A whole calendar as `.ics` text.
pub fn export(name: &str, events: &[&Event]) -> String {
    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let mut lines = vec![
        "BEGIN:VCALENDAR".to_string(),
        "VERSION:2.0".into(),
        "PRODID:-//Kairo//Desktop//DE".into(),
        format!("X-WR-CALNAME:{}", escape(name)),
    ];
    for event in events.iter().filter(|e| !e.draft) {
        push_event(&mut lines, &stamp, event, None);
        if event.repeats() {
            for change in event.exceptions.iter().filter(|x| !x.deleted) {
                push_event(&mut lines, &stamp, event, Some(change));
            }
        }
    }
    lines.push("END:VCALENDAR".into());
    lines.iter().map(|l| fold(l) + "\r\n").collect()
}

fn push_event(lines: &mut Vec<String>, stamp: &str, event: &Event, changed: Option<&Exception>) {
    let all_day = event.is_all_day() && changed.is_none_or(|x| x.start.is_none() && x.end.is_none());
    let start = changed.and_then(|x| x.start).unwrap_or(event.start);
    let mut end = changed.and_then(|x| x.end).unwrap_or(event.end);
    if all_day {
        // iCalendar's all-day end is the day after.
        let last = start.date() + Duration::days(event.extra_days());
        end = last.succ_opt().unwrap_or(last).and_time(NaiveTime::MIN);
    }
    let title = changed.and_then(|x| x.title.as_deref()).unwrap_or(&event.title);
    let location = changed.and_then(|x| x.location.as_deref()).unwrap_or(&event.location);

    lines.push("BEGIN:VEVENT".into());
    lines.push(format!("UID:{}@kairo", event.id));
    lines.push(format!("DTSTAMP:{stamp}"));
    if let Some(x) = changed {
        let origin = x.date.and_time(event.start.time());
        lines.push(format!("RECURRENCE-ID{}", local(event, origin, event.is_all_day())));
    }
    lines.push(format!("DTSTART{}", local(event, start, all_day)));
    lines.push(format!("DTEND{}", local(event, end, all_day)));
    if changed.is_none() {
        if let Some(rule) = rrule(event) {
            lines.push(format!("RRULE:{rule}"));
            let dates: Vec<String> = event
                .exceptions
                .iter()
                .map(|x| {
                    let time = x.date.and_time(event.start.time());
                    if event.is_all_day() { time.format("%Y%m%d").to_string() } else { time.format("%Y%m%dT%H%M%S").to_string() }
                })
                .collect();
            if !dates.is_empty() {
                let head = if event.is_all_day() { ";VALUE=DATE".to_string() } else { format!(";TZID={}", event.tz) };
                lines.push(format!("EXDATE{head}:{}", dates.join(",")));
            }
        }
    }
    lines.push(format!("SUMMARY:{}", escape(title)));
    lines.push(format!("TRANSP:{}", if matches!(event.status.as_str(), "free" | "elsewhere") { "TRANSPARENT" } else { "OPAQUE" }));
    match event.status.as_str() {
        "tentative" => lines.push("STATUS:TENTATIVE".into()),
        "oof" => lines.push("X-MICROSOFT-CDO-BUSYSTATUS:OOF".into()),
        "elsewhere" => lines.push("X-MICROSOFT-CDO-BUSYSTATUS:WORKINGELSEWHERE".into()),
        _ => {}
    }
    if !location.is_empty() {
        lines.push(format!("LOCATION:{}", escape(location)));
    }
    if !event.description.is_empty() {
        lines.push(format!("DESCRIPTION:{}", escape(&event.description)));
    }
    if let Some(color) = &event.color {
        lines.push(format!("COLOR:{color}"));
    }
    if event.private {
        lines.push("CLASS:PRIVATE".into());
    }
    if !event.categories.is_empty() {
        let names: Vec<String> = event.categories.iter().map(|c| escape(c)).collect();
        lines.push(format!("CATEGORIES:{}", names.join(",")));
    }
    for attendee in &event.attendees {
        lines.push(format!("ATTENDEE;ROLE=REQ-PARTICIPANT:mailto:{attendee}"));
    }
    if let Some(minutes) = event.remind {
        lines.push("BEGIN:VALARM".into());
        lines.push("ACTION:DISPLAY".into());
        lines.push(format!("DESCRIPTION:{}", escape(title)));
        lines.push(format!("TRIGGER:-PT{minutes}M"));
        lines.push("END:VALARM".into());
    }
    lines.push("END:VEVENT".into());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    const SAMPLE: &str = "BEGIN:VCALENDAR\r
VERSION:2.0\r
X-WR-CALNAME:Team\\, Nord\r
BEGIN:VEVENT\r
UID:a1\r
DTSTART;TZID=Europe/Berlin:20261005T090000\r
DTEND;TZID=Europe/Berlin:20261005T100000\r
RRULE:FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE;COUNT=4\r
EXDATE;TZID=Europe/Berlin:20261007T090000\r
SUMMARY:Planung\\; Montag\r
LOCATION:Raum\r
  1\r
ATTENDEE;CN=Bob:mailto:bob@example.com\r
BEGIN:VALARM\r
TRIGGER:-PT15M\r
ACTION:DISPLAY\r
END:VALARM\r
END:VEVENT\r
BEGIN:VEVENT\r
UID:a1\r
RECURRENCE-ID;TZID=Europe/Berlin:20261019T090000\r
DTSTART;TZID=Europe/Berlin:20261019T110000\r
DTEND;TZID=Europe/Berlin:20261019T120000\r
SUMMARY:Planung\\; Montag\r
END:VEVENT\r
BEGIN:VEVENT\r
UID:b2\r
DTSTART;VALUE=DATE:20261024\r
DTEND;VALUE=DATE:20261025\r
SUMMARY:Ganztags\r
END:VEVENT\r
BEGIN:VEVENT\r
UID:c3\r
DTSTART:20261005T070000Z\r
DTEND:20261005T080000Z\r
SUMMARY:UTC\r
END:VEVENT\r
END:VCALENDAR\r
";

    #[test]
    fn reads_series_exceptions_and_alarms() {
        let imported = parse(SAMPLE, "datei").unwrap();
        let calendar = &imported.calendars[0];
        assert_eq!(calendar.name, "Team, Nord");
        let series = &calendar.events[0];
        assert_eq!(series.title, "Planung; Montag");
        assert_eq!(series.location, "Raum 1");
        assert_eq!((series.repeat.as_str(), series.repeat_interval), ("weekly", 2));
        assert_eq!(series.repeat_days, weekday_bit(Weekday::Mon) | weekday_bit(Weekday::Wed));
        // 4 occurrences: Mo 5., Mi 7., Mo 19., Mi 21.
        assert_eq!(series.repeat_until, Some(day(2026, 10, 21)));
        assert_eq!(series.remind, Some(15));
        assert_eq!(series.attendees, vec!["bob@example.com".to_string()]);
        // the deleted Wednesday and the moved Monday
        assert_eq!(series.exceptions.len(), 2);
        assert!(series.exceptions[0].deleted && series.exceptions[0].date == day(2026, 10, 7));
        assert_eq!(series.exceptions[1].start, Some(day(2026, 10, 19).and_hms_opt(11, 0, 0).unwrap()));
        assert_eq!(series.exceptions[1].title, None);
    }

    #[test]
    fn dates_and_utc_times() {
        let imported = parse(SAMPLE, "datei").unwrap();
        let events = &imported.calendars[0].events;
        // all-day: one whole day
        assert_eq!(events[1].start, day(2026, 10, 24).and_hms_opt(0, 0, 0).unwrap());
        assert_eq!(events[1].end, day(2026, 10, 24).and_hms_opt(23, 59, 0).unwrap());
        // UTC stays UTC; Kairo shows it as 09:00 in Berlin (summer time)
        assert_eq!((events[2].tz.as_str(), events[2].start.to_string().as_str()), ("UTC", "2026-10-05 07:00:00"));
        assert_eq!(events[0].tz, "Europe/Berlin");
    }

    #[test]
    fn nth_weekday_rules_and_unsupported_ones() {
        let mut event = ImportedEvent {
            title: String::new(),
            location: String::new(),
            description: String::new(),
            start: day(2026, 10, 13).and_hms_opt(9, 0, 0).unwrap(), // 2nd Tuesday
            end: day(2026, 10, 13).and_hms_opt(10, 0, 0).unwrap(),
            tz: "Europe/Berlin".into(),
            status: "busy".into(),
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
        };
        assert!(apply_rule(&mut event, "FREQ=MONTHLY;BYDAY=2TU").is_some());
        assert_eq!((event.repeat.as_str(), event.repeat_nth), ("monthly", 2));
        assert!(apply_rule(&mut event, "FREQ=MONTHLY;BYDAY=TU;BYSETPOS=-1").is_some());
        assert_eq!(event.repeat_nth, -1);
        assert!(apply_rule(&mut event, "FREQ=MONTHLY;BYDAY=2WE").is_none());
        assert!(apply_rule(&mut event, "FREQ=HOURLY").is_none());
        assert!(apply_rule(&mut event, "FREQ=MONTHLY;BYMONTHDAY=3").is_none());
    }

    #[test]
    fn exports_what_it_reads() {
        let imported = parse(SAMPLE, "datei").unwrap();
        let source = &imported.calendars[0].events[0];
        let event = Event {
            id: "e1".into(),
            calendar: "c".into(),
            title: source.title.clone(),
            location: source.location.clone(),
            description: String::new(),
            start: source.start,
            end: source.end,
            tz: "Europe/Berlin".into(),
            status: "busy".into(),
            repeat: source.repeat.clone(),
            repeat_until: source.repeat_until,
            repeat_interval: source.repeat_interval,
            repeat_days: source.repeat_days,
            repeat_nth: source.repeat_nth,
            exceptions: source.exceptions.clone(),
            remind: source.remind,
            attendees: source.attendees.clone(),
            color: None,
            private: false,
            categories: Vec::new(),
            rev: 0,
            dirty: false,
            draft: false,
        };
        let text = export("Team, Nord", &[&event]);
        assert!(text.contains("RRULE:FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE;UNTIL=20261021T235959Z\r\n"));
        assert!(text.contains("EXDATE;TZID=Europe/Berlin:20261007T090000,20261019T090000\r\n"));
        assert!(text.contains("RECURRENCE-ID;TZID=Europe/Berlin:20261019T090000\r\n"));
        assert!(text.lines().all(|l| l.len() <= 75));

        let again = parse(&text, "x").unwrap();
        let back = &again.calendars[0].events[0];
        assert_eq!((back.repeat.as_str(), back.repeat_interval, back.repeat_days), ("weekly", 2, source.repeat_days));
        assert_eq!(back.repeat_until, source.repeat_until);
        assert_eq!(back.exceptions.len(), 2);
        assert_eq!(back.remind, Some(15));
    }

    #[test]
    fn colors_and_zones_survive_a_round_trip() {
        assert_eq!(hex_color("#C42B1C"), Some("#c42b1c".to_string()));
        assert_eq!(hex_color("#c42b1cff"), Some("#c42b1c".to_string()));
        assert_eq!(hex_color("red"), None);

        let mut event = probe_event(&ImportedEvent {
            title: "T".into(),
            location: String::new(),
            description: String::new(),
            start: day(2026, 10, 5).and_hms_opt(9, 0, 0).unwrap(),
            end: day(2026, 10, 5).and_hms_opt(10, 0, 0).unwrap(),
            tz: "America/New_York".into(),
            status: "busy".into(),
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
        });
        event.id = "z1".into();
        event.color = Some("#0f6cbd".into());
        let text = export("Zonen", &[&event]);
        assert!(text.contains("DTSTART;TZID=America/New_York:20261005T090000\r\n"));
        let back = &parse(&text, "x").unwrap().calendars[0].events[0];
        assert_eq!((back.tz.as_str(), back.color.as_deref()), ("America/New_York", Some("#0f6cbd")));
        assert_eq!(back.start, event.start);
    }

    #[test]
    fn multi_day_events_stay_in_one_piece() {
        let text = "BEGIN:VCALENDAR
BEGIN:VEVENT
UID:m1
DTSTART;VALUE=DATE:20261005
DTEND;VALUE=DATE:20261008
SUMMARY:Urlaub
END:VEVENT
BEGIN:VEVENT
UID:m2
DTSTART;TZID=Europe/Berlin:20261005T220000
DTEND;TZID=Europe/Berlin:20261006T020000
SUMMARY:Nachtschicht
END:VEVENT
END:VCALENDAR
";
        let imported = parse(text, "x").unwrap();
        let events = &imported.calendars[0].events;
        assert_eq!(events.len(), 2);
        assert_eq!((events[0].start, events[0].end), (day(2026, 10, 5).and_hms_opt(0, 0, 0).unwrap(), day(2026, 10, 7).and_hms_opt(23, 59, 0).unwrap()));
        assert_eq!((events[1].start, events[1].end), (day(2026, 10, 5).and_hms_opt(22, 0, 0).unwrap(), day(2026, 10, 6).and_hms_opt(2, 0, 0).unwrap()));
        let out = export("x", &[&probe_with(&events[0], "m1")]);
        assert!(out.contains("DTSTART;VALUE=DATE:20261005\r\n") && out.contains("DTEND;VALUE=DATE:20261008\r\n"));
    }

    #[test]
    fn status_privacy_and_labels_survive_a_round_trip() {
        let text = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:p1\r\nDTSTART:20261005T090000\r\nDTEND:20261005T100000\r\nSUMMARY:Arzt\r\n\
                    X-MICROSOFT-CDO-BUSYSTATUS:WORKINGELSEWHERE\r\nCLASS:PRIVATE\r\nCATEGORIES:Vor Ort,Homeoffice\\, teils,\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let imported = parse(text, "x").unwrap();
        let event = &imported.calendars[0].events[0];
        assert_eq!(event.status, "elsewhere");
        assert!(event.private);
        assert_eq!(event.categories, ["Vor Ort", "Homeoffice, teils"]);

        let mut out = probe_event(event);
        out.id = "p1".into();
        out.status = event.status.clone();
        out.private = true;
        out.categories = event.categories.clone();
        let written = export("x", &[&out]);
        assert!(written.contains("X-MICROSOFT-CDO-BUSYSTATUS:WORKINGELSEWHERE\r\n"));
        assert!(written.contains("CLASS:PRIVATE\r\n"));
        assert!(written.contains("CATEGORIES:Vor Ort,Homeoffice\\, teils\r\n"));
        let again = &parse(&written, "x").unwrap().calendars[0].events[0];
        assert_eq!((again.status.as_str(), again.private, again.categories.len()), ("elsewhere", true, 2));
    }

    #[test]
    fn rejects_other_files() {
        assert!(parse("hello", "x").is_err());
        assert!(parse("BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n", "x").is_err());
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("PT1H30M"), Some(Duration::minutes(90)));
        assert_eq!(parse_duration("-P1D"), Some(Duration::days(-1)));
        assert_eq!(parse_duration("-PT15M"), Some(Duration::minutes(-15)));
        assert_eq!(parse_duration("nonsense"), None);
    }
}
