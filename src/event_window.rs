//! The "Termindetails" window: edit one event, one repetition of a series, or a whole series.

use super::*;

pub(crate) struct CalendarChoice {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) color: u32,
    /// You own the calendar (only owners can make events private).
    pub(crate) owned: bool,
}

pub(crate) struct EventSnapshot {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) location: String,
    pub(crate) start: NaiveDateTime,
    pub(crate) end: NaiveDateTime,
    pub(crate) calendars: Vec<CalendarChoice>,
    pub(crate) calendar_id: String,
    pub(crate) status: String,
    pub(crate) repeat: String,
    pub(crate) repeat_until: Option<NaiveDate>,
    pub(crate) repeat_interval: u32,
    pub(crate) repeat_days: u8,
    pub(crate) repeat_nth: i8,
    pub(crate) remind: Option<u32>,
    pub(crate) attendees: Vec<String>,
    pub(crate) color: Option<String>,
    pub(crate) tz: String,
    pub(crate) description: String,
    pub(crate) private: bool,
    pub(crate) categories: Vec<String>,
    /// Open the form right away (a new event) instead of the read-only details.
    pub(crate) start_editing: bool,
    /// Editing only the repetition from this day instead of the whole series.
    pub(crate) occurrence: Option<NaiveDate>,
    /// In a calendar shared with you as "Ansehen": looking is fine, changing is not.
    pub(crate) read_only: bool,
}

/// "Anzeigen als" choices, in dropdown order: (stored value, label).
const STATUS_CHOICES: [(&str, &str); 5] = [
    ("free", "Frei"),
    ("tentative", "Mit Vorbehalt"),
    ("busy", "Beschäftigt"),
    ("oof", "Abwesend"),
    ("elsewhere", "An anderem Ort tätig"),
];

/// "Wiederholung" choices, in dropdown order: (stored value, label).
const REPEAT_CHOICES: [(&str, &str); 5] = [
    ("", "Keine"),
    ("daily", "Täglich"),
    ("weekly", "Wöchentlich"),
    ("monthly", "Monatlich"),
    ("yearly", "Jährlich"),
];

/// How a monthly / yearly series picks its day.
const MONTH_CHOICES: [&str; 3] = [
    "Am gleichen Datum",
    "Am gleichen Wochentag (z. B. jeder 2. Dienstag)",
    "Am letzten gleichen Wochentag",
];

/// The e-mail addresses in a typed list (separated by commas, semicolons or blanks), without duplicates.
fn parse_attendees(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for part in text.split(|c: char| c == ',' || c == ';' || c.is_whitespace()) {
        let part = part.trim().trim_matches(|c| c == '<' || c == '>');
        if part.contains('@') && part.len() <= 254 && !out.iter().any(|a| a.eq_ignore_ascii_case(part)) {
            out.push(part.to_string());
        }
    }
    out
}

/// The day an event ends on (a midnight end belongs to the day before).
fn last_day_of(start: NaiveDateTime, end: NaiveDateTime) -> NaiveDate {
    if end.time() == NaiveTime::MIN && end > start { end.date() - Duration::days(1) } else { end.date() }
}

/// Labels in a typed list (separated by commas or semicolons), without duplicates.
fn parse_categories(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for part in text.split([',', ';']) {
        let part = part.trim();
        if !part.is_empty() && part.len() <= 60 && !part.chars().any(char::is_control)
            && !out.iter().any(|c| c.eq_ignore_ascii_case(part))
            && out.len() < 20
        {
            out.push(part.to_string());
        }
    }
    out
}

/// Labels offered with one click.
const CATEGORY_PRESETS: [&str; 6] = ["Vor Ort", "Homeoffice", "Dienstreise", "Wichtig", "Besprechung", "Privat"];

/// Percent-encodes text for a `mailto:` link.
fn url_encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Reminder choices in dropdown order: minutes before the start.
const REMIND_CHOICES: [Option<u32>; 9] =
    [None, Some(0), Some(5), Some(10), Some(15), Some(30), Some(60), Some(120), Some(1440)];

fn remind_label(minutes: Option<u32>) -> String {
    match minutes {
        None => "Keine".into(),
        Some(0) => "Zum Beginn".into(),
        Some(1440) => "1 Tag vorher".into(),
        Some(m) if m >= 60 && m % 60 == 0 => format!("{} Std. vorher", m / 60),
        Some(m) => format!("{m} Min. vorher"),
    }
}

const WEEKDAY_LABELS: [&str; 7] = ["Mo", "Di", "Mi", "Do", "Fr", "Sa", "So"];

type ChoiceState = SelectState<Vec<String>>;

/// Quarter-hour choices for a time dropdown, plus `current` if it is off that grid.
fn time_options(current: NaiveTime) -> Vec<String> {
    let mut minutes: Vec<u32> = (0..96).map(|i| i * 15).collect();
    let now = current.hour() * 60 + current.minute();
    if !minutes.contains(&now) {
        minutes.push(now);
        minutes.sort_unstable();
    }
    minutes.iter().map(|m| format!("{:02}:{:02}", m / 60, m % 60)).collect()
}

fn time_label(time: NaiveTime) -> String {
    time.format("%H:%M").to_string()
}

fn parse_time(label: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(label, "%H:%M").ok()
}

/// Names for the calendar dropdown; duplicates get a number so each choice is unique.
fn unique_names(calendars: &[CalendarChoice]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for calendar in calendars {
        let mut name = calendar.name.clone();
        let mut n = 2;
        while names.contains(&name) {
            name = format!("{} ({n})", calendar.name);
            n += 1;
        }
        names.push(name);
    }
    names
}

/// Content of the separate "Termindetails" OS window. Edits are written straight back to the app.
pub(crate) struct EventWindow {
    app: Entity<CalendarApp>,
    event_id: String,
    title_input: Entity<InputState>,
    location_input: Entity<InputState>,
    date_picker: Entity<DatePickerState>,
    end_date_picker: Entity<DatePickerState>,
    start_select: Entity<ChoiceState>,
    end_select: Entity<ChoiceState>,
    calendar_select: Entity<ChoiceState>,
    status_select: Entity<ChoiceState>,
    status_index: usize,
    repeat_select: Entity<ChoiceState>,
    repeat_index: usize,
    remind_select: Entity<ChoiceState>,
    attendees_input: Entity<InputState>,
    /// Own color of the event (`#rrggbb`); none = the calendar's.
    color: Option<String>,
    tz_select: Entity<ChoiceState>,
    description_input: Entity<TextareaState>,
    private: bool,
    categories_input: Entity<InputState>,
    /// The form is shown (otherwise the read-only details).
    editing: bool,
    focus: FocusHandle,
    /// Choices of the time zone dropdown (IANA names).
    tz_choices: Vec<String>,
    tz_index: usize,
    /// Choices of the reminder dropdown (minutes before the start).
    remind_choices: Vec<Option<u32>>,
    remind_index: usize,
    occurrence: Option<NaiveDate>,
    interval_input: Entity<InputState>,
    /// Weekly: the chosen weekdays (`model::weekday_bit`).
    weekdays: u8,
    month_select: Entity<ChoiceState>,
    /// Monthly / yearly: 0 = same date, 1 = same n-th weekday, 2 = last such weekday.
    month_index: usize,
    /// Whether the series stops at `until` (otherwise it goes on forever).
    until_enabled: bool,
    until: NaiveDate,
    until_picker: Entity<DatePickerState>,
    read_only: bool,
    calendars: Vec<CalendarChoice>,
    calendar_names: Vec<String>,
    calendar_index: usize,
    start: NaiveDateTime,
    end: NaiveDateTime,
    /// Times to go back to when "Ganztägig" is switched off again.
    timed: (NaiveTime, NaiveTime),
    _subscriptions: Vec<Subscription>,
}

impl EventWindow {
    /// Takes a snapshot of the event instead of reading `app`: this runs while `app` is
    /// still being updated (the window is opened from one of its click handlers).
    pub(crate) fn new(app: Entity<CalendarApp>, snapshot: EventSnapshot, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let EventSnapshot {
            id: event_id,
            title,
            location,
            start,
            end,
            calendars,
            calendar_id,
            status,
            repeat,
            repeat_until,
            repeat_interval,
            repeat_days,
            repeat_nth,
            remind,
            attendees,
            color,
            tz,
            description,
            private,
            categories,
            start_editing,
            occurrence,
            read_only,
        } = snapshot;

        let title_input = cx.new(|cx| {
            let mut state = InputState::new(window, cx).placeholder("Titel");
            state.set_value(title, window, cx);
            state
        });
        let location_input = cx.new(|cx| {
            let mut state = InputState::new(window, cx).placeholder("Ort");
            state.set_value(location, window, cx);
            state
        });
        let date_picker = cx.new(|cx| {
            let mut state = DatePickerState::new(window, cx).date_format("%d.%m.%Y");
            state.set_date(start.date(), window, cx);
            state
        });
        let end_date_picker = cx.new(|cx| {
            let mut state = DatePickerState::new(window, cx).date_format("%d.%m.%Y");
            state.set_date(last_day_of(start, end), window, cx);
            state
        });
        let time_select = |time: NaiveTime, window: &mut Window, cx: &mut Context<Self>| {
            let options = time_options(time);
            let index = options.iter().position(|o| *o == time_label(time));
            cx.new(|cx| {
                SelectState::new(options, index.map(IndexPath::new), window, cx).searchable(true)
            })
        };
        let start_select = time_select(start.time(), window, cx);
        let end_select = time_select(end.time(), window, cx);

        let calendar_names = unique_names(&calendars);
        let calendar_index = calendars.iter().position(|c| c.id == calendar_id).unwrap_or(0);
        let calendar_select = cx.new(|cx| {
            SelectState::new(calendar_names.clone(), Some(IndexPath::new(calendar_index)), window, cx)
        });

        let status_index = STATUS_CHOICES.iter().position(|(key, _)| *key == status).unwrap_or(2);
        let status_select = cx.new(|cx| {
            let labels: Vec<String> = STATUS_CHOICES.iter().map(|(_, label)| label.to_string()).collect();
            SelectState::new(labels, Some(IndexPath::new(status_index)), window, cx)
        });

        let repeat_index = REPEAT_CHOICES.iter().position(|(key, _)| *key == repeat).unwrap_or(0);
        let repeat_select = cx.new(|cx| {
            let labels: Vec<String> = REPEAT_CHOICES.iter().map(|(_, label)| label.to_string()).collect();
            SelectState::new(labels, Some(IndexPath::new(repeat_index)), window, cx)
        });
        // A reminder time that isn't on the list (set elsewhere) stays selectable.
        let mut remind_choices: Vec<Option<u32>> = REMIND_CHOICES.to_vec();
        if !remind_choices.contains(&remind) {
            remind_choices.push(remind);
        }
        let remind_index = remind_choices.iter().position(|c| *c == remind).unwrap_or(0);
        let remind_select = cx.new(|cx| {
            let labels: Vec<String> = remind_choices.iter().map(|c| remind_label(*c)).collect();
            SelectState::new(labels, Some(IndexPath::new(remind_index)), window, cx)
        });
        // The usual zones, plus the event's own if it is another one.
        let mut tz_choices: Vec<String> = model::TIME_ZONES.iter().map(|z| z.to_string()).collect();
        if !tz_choices.contains(&tz) {
            tz_choices.push(tz.clone());
        }
        let tz_index = tz_choices.iter().position(|z| *z == tz).unwrap_or(0);
        let tz_select = cx.new(|cx| {
            SelectState::new(tz_choices.clone(), Some(IndexPath::new(tz_index)), window, cx).searchable(true)
        });
        let description_input = cx.new(|cx| {
            let mut state = TextareaState::new(window, cx).placeholder("Notizen");
            state.set_value(description, window, cx);
            state
        });
        let categories_input = cx.new(|cx| {
            let mut state = InputState::new(window, cx).placeholder("z. B. Vor Ort, Homeoffice");
            state.set_value(categories.join(", "), window, cx);
            state
        });
        let focus = cx.focus_handle();
        let attendees_input = cx.new(|cx| {
            let mut state = InputState::new(window, cx).placeholder("E-Mail-Adressen, durch Komma getrennt");
            state.set_value(attendees.join(", "), window, cx);
            state
        });
        let interval_input = cx.new(|cx| {
            let mut state = InputState::new(window, cx).placeholder("1");
            state.set_value(repeat_interval.max(1).to_string(), window, cx);
            state
        });
        let weekdays = if repeat_days == 0 { model::weekday_bit(start.weekday()) } else { repeat_days };
        let month_index = match repeat_nth {
            0 => 0,
            -1 => 2,
            _ => 1,
        };
        let month_select = cx.new(|cx| {
            let labels = MONTH_CHOICES.iter().map(|label| label.to_string()).collect::<Vec<_>>();
            SelectState::new(labels, Some(IndexPath::new(month_index)), window, cx)
        });
        let until = repeat_until.unwrap_or(start.date() + Duration::days(90));
        let until_picker = cx.new(|cx| {
            let mut state = DatePickerState::new(window, cx).date_format("%d.%m.%Y");
            state.set_date(until, window, cx);
            state
        });

        let subscriptions = vec![
            cx.subscribe(&tz_select, |this, _, ev: &SelectEvent<Vec<String>>, cx| {
                if let SelectEvent::Confirm(Some(name)) = ev {
                    if let Some(index) = this.tz_choices.iter().position(|z| z == name) {
                        this.tz_index = index;
                        cx.notify();
                    }
                }
            }),
            cx.subscribe(&remind_select, |this, _, ev: &SelectEvent<Vec<String>>, cx| {
                if let SelectEvent::Confirm(Some(label)) = ev {
                    if let Some(index) = this.remind_choices.iter().position(|c| remind_label(*c) == *label) {
                        this.remind_index = index;
                        cx.notify();
                    }
                }
            }),
            cx.subscribe(&month_select, |this, _, ev: &SelectEvent<Vec<String>>, cx| {
                if let SelectEvent::Confirm(Some(label)) = ev {
                    if let Some(index) = MONTH_CHOICES.iter().position(|l| l == label) {
                        this.month_index = index;
                        cx.notify();
                    }
                }
            }),
            cx.subscribe(&repeat_select, |this, _, ev: &SelectEvent<Vec<String>>, cx| {
                if let SelectEvent::Confirm(Some(label)) = ev {
                    if let Some(index) = REPEAT_CHOICES.iter().position(|(_, l)| l == label) {
                        this.repeat_index = index;
                        cx.notify();
                    }
                }
            }),
            cx.subscribe(&until_picker, |this, _, ev: &DatePickerEvent, cx| {
                let DatePickerEvent::Change(date) = ev;
                if let Some(day) = date.start() {
                    this.until = day;
                    cx.notify();
                }
            }),
            cx.subscribe(&status_select, |this, _, ev: &SelectEvent<Vec<String>>, cx| {
                if let SelectEvent::Confirm(Some(label)) = ev {
                    if let Some(index) = STATUS_CHOICES.iter().position(|(_, l)| l == label) {
                        this.status_index = index;
                        cx.notify();
                    }
                }
            }),
            cx.subscribe_in(&title_input, window, |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.save(window, cx);
                }
            }),
            cx.subscribe_in(&date_picker, window, |this, _, ev: &DatePickerEvent, window, cx| {
                let DatePickerEvent::Change(date) = ev;
                if let Some(day) = date.start() {
                    let duration = this.end - this.start;
                    this.set_times(day.and_time(this.start.time()), duration, window, cx);
                }
            }),
            cx.subscribe_in(&end_date_picker, window, |this, _, ev: &DatePickerEvent, window, cx| {
                let DatePickerEvent::Change(date) = ev;
                if let Some(day) = date.start() {
                    let day = day.max(this.start.date());
                    let end = if this.is_all_day() { day.and_hms_opt(23, 59, 0).unwrap_or(this.end) } else { day.and_time(this.end.time()) };
                    // An end at or before the start is bumped to a quarter hour after it.
                    let duration = (end - this.start).max(Duration::minutes(15));
                    this.set_times(this.start, duration, window, cx);
                }
            }),
            cx.subscribe_in(&start_select, window, |this, _, ev: &SelectEvent<Vec<String>>, window, cx| {
                if let SelectEvent::Confirm(Some(time)) = ev {
                    if let Some(time) = parse_time(time) {
                        // The end stays where it is, unless the new start reaches it.
                        let start = this.start.date().and_time(time);
                        let duration = if start < this.end { this.end - start } else { Duration::minutes(15) };
                        this.set_times(start, duration, window, cx);
                    }
                }
            }),
            cx.subscribe_in(&end_select, window, |this, _, ev: &SelectEvent<Vec<String>>, window, cx| {
                if let SelectEvent::Confirm(Some(time)) = ev {
                    if let Some(time) = parse_time(time) {
                        let end = this.end.date().and_time(time);
                        // An end at or before the start is bumped to a quarter hour after it.
                        let duration = (end - this.start).max(Duration::minutes(15));
                        this.set_times(this.start, duration, window, cx);
                    }
                }
            }),
            cx.subscribe(&calendar_select, |this, _, ev: &SelectEvent<Vec<String>>, cx| {
                if let SelectEvent::Confirm(Some(name)) = ev {
                    if let Some(index) = this.calendar_names.iter().position(|n| n == name) {
                        this.calendar_index = index;
                        cx.notify();
                    }
                }
            }),
        ];

        // However the window goes away (cancel, close button, replaced), an unsaved new event goes too.
        let (draft_app, draft_id) = (app.clone(), event_id.clone());
        let release = cx.on_release(move |_, cx| {
            let (app, id) = (draft_app.clone(), draft_id.clone());
            cx.defer(move |cx| app.update(cx, |app, cx| app.discard_draft(&id, cx)));
        });
        let mut subscriptions = subscriptions;
        subscriptions.push(release);
        // So the shortcuts (Strg+S, Esc) work right away.
        if start_editing {
            let title_focus = title_input.read(cx).focus_handle(cx);
            window.focus(&title_focus, cx);
        } else {
            window.focus(&focus, cx);
        }

        Self {
            app,
            event_id,
            title_input,
            location_input,
            date_picker,
            end_date_picker,
            start_select,
            end_select,
            calendar_select,
            status_select,
            status_index,
            repeat_select,
            repeat_index,
            remind_select,
            attendees_input,
            color,
            tz_select,
            description_input,
            private,
            categories_input,
            editing: start_editing,
            focus,
            tz_choices,
            tz_index,
            remind_choices,
            remind_index,
            occurrence,
            interval_input,
            weekdays,
            month_select,
            month_index,
            until_enabled: repeat_until.is_some(),
            until,
            until_picker,
            read_only,
            calendars,
            calendar_names,
            calendar_index,
            start,
            end,
            timed: if model::spans_whole_day(start, end) {
                (NaiveTime::from_hms_opt(9, 0, 0).unwrap(), NaiveTime::from_hms_opt(10, 0, 0).unwrap())
            } else {
                (start.time(), end.time())
            },
            _subscriptions: subscriptions,
        }
    }

    fn is_all_day(&self) -> bool {
        model::spans_whole_day(self.start, self.end)
    }

    /// Opens the mail program with an invitation to everybody on the list.
    fn send_invitation(&self, cx: &mut Context<Self>) {
        let attendees = parse_attendees(self.attendees_input.read(cx).value().as_ref());
        if attendees.is_empty() {
            return;
        }
        let title = self.title_input.read(cx).value().to_string();
        let location = self.location_input.read(cx).value().to_string();
        let when = if self.is_all_day() {
            self.start.format("%d.%m.%Y").to_string() + " (ganztägig)"
        } else {
            format!("{} {}–{}", self.start.format("%d.%m.%Y"), self.start.format("%H:%M"), self.end.format("%H:%M"))
        };
        let mut body = format!("Hallo,\n\nhiermit lade ich dich ein:\n\n{title}\nWann: {when}\n");
        if !location.trim().is_empty() {
            body += &format!("Wo: {}\n", location.trim());
        }
        let url = format!(
            "mailto:{}?subject={}&body={}",
            attendees.join(","),
            url_encode(&format!("Einladung: {title}")),
            url_encode(&body)
        );
        cx.open_url(&url);
    }

    fn set_all_day(&mut self, all_day: bool, window: &mut Window, cx: &mut Context<Self>) {
        let day = self.start.date();
        if all_day {
            if !self.is_all_day() {
                self.timed = (self.start.time(), self.end.time());
            }
            // Whole days from the start day to the end day.
            let last = self.end.date().max(day).and_hms_opt(23, 59, 0).unwrap_or(self.end);
            self.set_times(day.and_time(NaiveTime::MIN), last - day.and_time(NaiveTime::MIN), window, cx);
        } else {
            let (start, end) = self.timed;
            let duration = (day.and_time(end) - day.and_time(start)).max(Duration::minutes(15));
            self.set_times(day.and_time(start), duration, window, cx);
        }
    }

    /// Sets the event to `start` with the given duration and brings all the pickers in line.
    fn set_times(&mut self, start: NaiveDateTime, duration: Duration, window: &mut Window, cx: &mut Context<Self>) {
        let end = start + duration;
        if (start, end) == (self.start, self.end) {
            return; // also stops the pickers' own updates from echoing back
        }
        self.start = start;
        self.end = end;

        let day = start.date();
        self.date_picker.update(cx, |picker, cx| picker.set_date(day, window, cx));
        let last = last_day_of(start, end);
        self.end_date_picker.update(cx, |picker, cx| picker.set_date(last, window, cx));
        for (select, time) in [(&self.start_select, start.time()), (&self.end_select, end.time())] {
            let options = time_options(time);
            let label = time_label(time);
            select.update(cx, |select, cx| {
                select.set_items(options, window, cx);
                select.set_selected_value(&label, window, cx);
            });
        }
        cx.notify();
    }

    /// Writes the draft into the calendar, queues it for sync and closes the window.
    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let title = self.title_input.read(cx).value().to_string();
        let location = self.location_input.read(cx).value().to_string();
        let (start, end) = (self.start, self.end);
        let calendar = self.calendars.get(self.calendar_index).map(|c| c.id.clone());
        let status = STATUS_CHOICES[self.status_index].0.to_string();
        let id = self.event_id.clone();
        if let Some(origin) = self.occurrence {
            self.app.update(cx, |app, cx| app.save_occurrence(&id, origin, (start, end), &title, &location, cx));
            window.remove_window();
            return;
        }
        let repeat = REPEAT_CHOICES[self.repeat_index].0.to_string();
        let repeat_until = (!repeat.is_empty() && self.until_enabled).then(|| self.until.max(start.date()));
        let remind = self.remind_choices[self.remind_index];
        let attendees = parse_attendees(self.attendees_input.read(cx).value().as_ref());
        let color = self.color.clone();
        let tz = self.tz_choices[self.tz_index].clone();
        let categories = parse_categories(self.categories_input.read(cx).value().as_ref());
        let private = self.private && self.calendars.get(self.calendar_index).is_some_and(|c| c.owned);
        let mut description = self.description_input.read(cx).value().to_string();
        let mut cut = description.len().min(20_000);
        while !description.is_char_boundary(cut) {
            cut -= 1;
        }
        description.truncate(cut);
        let repeat_interval = if repeat.is_empty() {
            1
        } else {
            self.interval_input.read(cx).value().trim().parse::<u32>().unwrap_or(1).clamp(1, 999)
        };
        // Just the start's weekday is the default and needs no mask.
        let repeat_days = if repeat == "weekly" && self.weekdays != model::weekday_bit(start.weekday()) {
            self.weekdays
        } else {
            0
        };
        let repeat_nth: i8 = if matches!(repeat.as_str(), "monthly" | "yearly") {
            let nth = (start.day() - 1) / 7 + 1;
            match self.month_index {
                1 if nth <= 4 => nth as i8,
                1 | 2 => -1,
                _ => 0,
            }
        } else {
            0
        };
        self.app.update(cx, |app, cx| {
            if let Some(event) = app.store.events.iter_mut().find(|e| e.id == id) {
                event.status = status;
                if repeat.is_empty() {
                    event.exceptions.clear();
                }
                event.repeat = repeat;
                event.repeat_until = repeat_until;
                event.repeat_interval = repeat_interval;
                event.repeat_days = repeat_days;
                event.repeat_nth = repeat_nth;
                event.remind = remind;
                event.attendees = attendees;
                event.color = color;
                event.tz = tz;
                event.description = description;
                event.categories = categories;
                event.private = private;
                event.title = title;
                event.location = location;
                event.start = start;
                event.end = end;
                if let Some(calendar) = calendar {
                    event.calendar = calendar;
                }
                event.dirty = true;
                event.draft = false;
            }
            app.mark_changed(cx);
        });
        window.remove_window();
    }
}

impl Render for EventWindow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.editing {
            self.render_form(cx).into_any_element()
        } else {
            self.render_details(cx).into_any_element()
        }
    }
}

/// "Every 2 weeks on Monday, Wednesday, until 21.12.2026" in words.
fn describe_repeat(
    repeat: &str,
    interval: u32,
    days: u8,
    nth: i8,
    until: Option<NaiveDate>,
    start: NaiveDate,
) -> String {
    let every = |one: &str, many: &str| if interval <= 1 { one.to_string() } else { format!("Alle {interval} {many}") };
    let weekday = WEEKDAYS[start.weekday().num_days_from_monday() as usize];
    let month = MONTHS[start.month0() as usize];
    let which = match nth {
        0 => String::new(),
        -1 => format!("letzten {weekday}"),
        n => format!("{n}. {weekday}"),
    };
    let mut text = match repeat {
        "daily" => every("Jeden Tag", "Tage"),
        "weekly" => {
            let names: Vec<&str> = (0..7).filter(|i| days >> i & 1 == 1).map(|i| WEEKDAYS[i]).collect();
            let on = if names.is_empty() { weekday.to_string() } else { names.join(", ") };
            format!("{} am {on}", every("Jede Woche", "Wochen"))
        }
        "monthly" if nth == 0 => format!("{} am {}.", every("Jeden Monat", "Monate"), start.day()),
        "monthly" => format!("{} am {which}", every("Jeden Monat", "Monate")),
        "yearly" if nth == 0 => format!("{} am {}. {month}", every("Jedes Jahr", "Jahre"), start.day()),
        "yearly" => format!("{} am {which} im {month}", every("Jedes Jahr", "Jahre")),
        _ => return String::new(),
    };
    if let Some(until) = until {
        text += &format!(", bis {}", until.format("%d.%m.%Y"));
    }
    text
}

/// "Mittwoch, 21. Oktober 2026" (with the year only when `year`).
fn long_date(date: NaiveDate, year: bool) -> String {
    let base = format!("{}, {}. {}", WEEKDAYS[date.weekday().num_days_from_monday() as usize], date.day(), MONTHS[date.month0() as usize]);
    if year { format!("{base} {}", date.year()) } else { base }
}

/// When an event takes place, as lines for the details.
fn describe_when(start: NaiveDateTime, end: NaiveDateTime) -> Vec<String> {
    let all_day = model::spans_whole_day(start, end);
    let last = last_day_of(start, end);
    if all_day {
        let range = if last == start.date() {
            long_date(start.date(), true)
        } else {
            format!("{} – {}", long_date(start.date(), start.year() != last.year()), long_date(last, true))
        };
        return vec![range, "ganztägig".into()];
    }
    if end.date() == start.date() {
        return vec![long_date(start.date(), true), format!("{} – {} Uhr", start.format("%H:%M"), end.format("%H:%M"))];
    }
    vec![
        format!("{}, {} Uhr", long_date(start.date(), start.year() != end.year()), start.format("%H:%M")),
        format!("bis {}, {} Uhr", long_date(end.date(), true), end.format("%H:%M")),
    ]
}

impl EventWindow {
    /// The read-only details: everything about the event in one place, like Outlook's reading view.
    fn render_details(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let row = |name: &'static str, content: AnyElement| {
            div()
                .flex()
                .gap_3()
                .child(div().flex_none().w(px(96.)).text_sm().text_color(muted).child(name))
                .child(div().flex_1().min_w_0().text_sm().child(content))
        };
        let text = |s: String| div().child(s).into_any_element();

        let calendar = self.calendars.get(self.calendar_index);
        let calendar_color = calendar.map_or(0x808080, |c| c.color);
        let color = self.color.as_deref().map_or(calendar_color, parse_color);
        let title = self.title_input.read(cx).value().to_string();
        let location = self.location_input.read(cx).value().to_string();
        let notes = self.description_input.read(cx).value().to_string();
        let categories = parse_categories(self.categories_input.read(cx).value().as_ref());
        let private = self.private;
        let zone = self.tz_choices[self.tz_index].clone();
        let when = describe_when(self.start, self.end);
        let attendees = parse_attendees(self.attendees_input.read(cx).value().as_ref());
        let interval = self.interval_input.read(cx).value().trim().parse::<u32>().unwrap_or(1).max(1);
        let repeat = REPEAT_CHOICES[self.repeat_index].0;
        let nth = match (self.month_index, (self.start.day() - 1) / 7 + 1) {
            (1, n) if n <= 4 => n as i8,
            (1, _) | (2, _) => -1,
            _ => 0,
        };
        let days = if self.weekdays == model::weekday_bit(self.start.weekday()) { 0 } else { self.weekdays };
        let until = (self.until_enabled && !repeat.is_empty()).then_some(self.until);
        let series = describe_repeat(repeat, interval, days, nth, until, self.start.date());
        let single = self.occurrence.is_some();
        let read_only = self.read_only;
        let (id, app, occurrence) = (self.event_id.clone(), self.app.clone(), self.occurrence);
        let has_zone_note = zone != model::display_tz() && !self.is_all_day();

        div()
            .key_context("EventWindow")
            .track_focus(&self.focus)
            .on_action(|_: &CloseWindow, window, _| window.remove_window())
            .flex()
            .flex_col()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                div()
                    .id("event-details")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .gap_3()
                    .p_4()
                    .child(
                        div()
                            .flex()
                            .gap_3()
                            .child(div().flex_none().w(px(6.)).rounded_full().bg(rgb(color)))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .min_w_0()
                                    .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).child(title))
                                    .when(private, |d| d.child(div().text_xs().text_color(muted).child("Privat – andere sehen nur die Zeit")))
                                    .children(when.iter().enumerate().map(|(i, line)| {
                                        div().text_sm().when(i > 0, |d| d.text_color(muted)).child(line.clone())
                                    }))
                                    .when(has_zone_note, |d| {
                                        let shown = (
                                            model::convert_time(self.start, &zone, &model::display_tz()),
                                            model::convert_time(self.end, &zone, &model::display_tz()),
                                        );
                                        d.child(div().text_xs().text_color(muted).child(format!(
                                            "{zone} · bei dir {} – {} Uhr",
                                            shown.0.format("%d.%m. %H:%M"),
                                            shown.1.format("%d.%m. %H:%M")
                                        )))
                                    }),
                            ),
                    )
                    .when(single, |d| {
                        d.child(div().text_xs().text_color(muted).child("Ein einzelner Termin einer Serie"))
                    })
                    .when(!series.is_empty(), |d| d.child(row("Wiederholung", text(series.clone()))))
                    .when(!location.trim().is_empty(), |d| d.child(row("Ort", text(location.trim().to_string()))))
                    .child(row(
                        "Kalender",
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().flex_none().size_3().rounded_sm().bg(rgb(calendar_color)))
                            .child(calendar.map_or(String::new(), |c| c.name.clone()))
                            .into_any_element(),
                    ))
                    .child(row("Anzeigen als", text(STATUS_CHOICES[self.status_index].1.to_string())))
                    .when(self.remind_choices[self.remind_index].is_some(), |d| {
                        d.child(row("Erinnerung", text(remind_label(self.remind_choices[self.remind_index]))))
                    })
                    .when(!attendees.is_empty(), |d| {
                        d.child(row(
                            "Teilnehmer",
                            div().flex().flex_col().children(attendees.iter().map(|a| div().child(a.clone()))).into_any_element(),
                        ))
                    })
                    .when(!categories.is_empty(), |d| {
                        d.child(row(
                            "Kategorien",
                            div()
                                .flex()
                                .flex_wrap()
                                .gap_1()
                                .children(categories.iter().map(|c| {
                                    let tint = rgb(category_color(c));
                                    div().px_2().rounded_full().text_xs().bg(tint.opacity(0.2)).border_1().border_color(tint).child(c.clone())
                                }))
                                .into_any_element(),
                        ))
                    })
                    .when(!notes.trim().is_empty(), |d| {
                        d.child(
                            div()
                                .mt_2()
                                .pt_3()
                                .border_t_1()
                                .border_color(cx.theme().border)
                                .text_sm()
                                .child(notes.trim().to_string()),
                        )
                    })
                    .when(read_only, |d| {
                        d.child(div().text_xs().text_color(muted).child("Dieser Kalender ist für dich schreibgeschützt."))
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .p_4()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(Button::new("delete").label("Löschen").disabled(read_only).on_click(move |_, window, cx| {
                        app.update(cx, |app, cx| match occurrence {
                            Some(origin) => app.delete_occurrence(&id, origin, cx),
                            None => app.delete_event(&id, cx),
                        });
                        window.remove_window();
                    }))
                    .child(div().flex_1())
                    .child(Button::new("close").label("Schließen").on_click(|_, window, _| window.remove_window()))
                    .child(Button::new("edit").primary().label("Bearbeiten").disabled(read_only).on_click(
                        cx.listener(|this, _, window, cx| {
                            this.editing = true;
                            let title_focus = this.title_input.read(cx).focus_handle(cx);
                            window.focus(&title_focus, cx);
                            cx.notify();
                        }),
                    )),
            )
    }

    /// The form to change the event.
    fn render_form(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let label = |text: &'static str| div().text_xs().text_color(cx.theme().muted_foreground).child(text);
        let field = |text: &'static str, content: AnyElement| {
            div().flex().flex_col().gap_1().child(label(text)).child(content)
        };
        let id = self.event_id.clone();
        let app = self.app.clone();
        let occurrence = self.occurrence;
        let read_only = self.read_only;
        let all_day = self.is_all_day();
        let repeats = self.repeat_index > 0;
        let single = self.occurrence.is_some();
        let unit = ["", "Tage", "Wochen", "Monate", "Jahre"][self.repeat_index];
        let color = self.calendars.get(self.calendar_index).map_or(0x808080, |c| c.color);
        let owns_calendar = self.calendars.get(self.calendar_index).is_some_and(|c| c.owned);

        div()
            .key_context("EventWindow")
            .on_action(cx.listener(|this, _: &SaveEvent, window, cx| {
                if !this.read_only {
                    this.save(window, cx);
                }
            }))
            .on_action(|_: &CloseWindow, window, _| window.remove_window())
            .flex()
            .flex_col()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                div()
                    .id("event-body")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .gap_3()
                    .p_4()
            .child(field("Titel", Input::new(&self.title_input).into_any_element()))
            .child(field("Ort", Input::new(&self.location_input).into_any_element()))
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(div().flex_1().child(field("Von", DatePicker::new(&self.date_picker).into_any_element())))
                    .child(div().flex_1().child(field("Bis", DatePicker::new(&self.end_date_picker).into_any_element()))),
            )
            .child(
                Checkbox::new("all-day")
                    .label("Ganztägig")
                    .checked(all_day)
                    .disabled(read_only)
                    .on_click(cx.listener(|this, checked: &bool, window, cx| this.set_all_day(*checked, window, cx))),
            )
            .when(!all_day, |d| {
                d.child(
                div()
                    .flex()
                    .gap_3()
                    .child(div().flex_1().child(field("Beginn", Select::new(&self.start_select).into_any_element())))
                    .child(div().flex_1().child(field("Ende", Select::new(&self.end_select).into_any_element()))),
            )
            })
            .when(!single && !all_day, |d| {
                d.child(field("Zeitzone", Select::new(&self.tz_select).into_any_element())).when(
                    self.tz_choices[self.tz_index] != model::display_tz(),
                    |d| {
                        let zone = &self.tz_choices[self.tz_index];
                        let (start, end) = (
                            model::convert_time(self.start, zone, &model::display_tz()),
                            model::convert_time(self.end, zone, &model::display_tz()),
                        );
                        d.child(div().text_xs().text_color(cx.theme().muted_foreground).child(format!(
                            "Bei dir: {} {}–{} Uhr ({})",
                            start.format("%d.%m."),
                            start.format("%H:%M"),
                            end.format("%H:%M"),
                            model::display_tz()
                        )))
                    },
                )
            })
            .when(!single, |d| {
                d.child(field(
                    "Kalender",
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().flex_none().size_3().rounded_sm().bg(rgb(color)))
                        .child(div().flex_1().child(Select::new(&self.calendar_select)))
                        .into_any_element(),
                ))
                .child(field("Anzeigen als", Select::new(&self.status_select).into_any_element()))
                .child(field(
                    "Farbe",
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .children(PALETTE.iter().enumerate().map(|(i, hex)| {
                            let selected = self.color.as_deref() == Some(*hex);
                            div()
                                .id(("event-color", i))
                                .size_5()
                                .rounded_full()
                                .cursor_pointer()
                                .bg(rgb(parse_color(hex)))
                                .when(selected, |d| d.border_2().border_color(cx.theme().foreground))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.color = Some(PALETTE[i].to_string());
                                    cx.notify();
                                }))
                        }))
                        .child(Button::new("event-color-reset").small().label("Kalenderfarbe").on_click(cx.listener(
                            |this, _, _, cx| {
                                this.color = None;
                                cx.notify();
                            },
                        )))
                        .into_any_element(),
                ))
                .child(field("Notizen", Textarea::new(&self.description_input).h(px(110.)).into_any_element()))
                .child(field(
                    "Kategorien",
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(Input::new(&self.categories_input))
                        .child(div().flex().flex_wrap().gap_1().children(CATEGORY_PRESETS.iter().enumerate().map(|(i, name)| {
                            let on = parse_categories(self.categories_input.read(cx).value().as_ref())
                                .iter()
                                .any(|c| c.eq_ignore_ascii_case(name));
                            let button = Button::new(("category", i)).xsmall().label(*name);
                            let button = if on { button.primary() } else { button };
                            button.on_click(cx.listener(move |this, _, window, cx| {
                                let mut chosen = parse_categories(this.categories_input.read(cx).value().as_ref());
                                match chosen.iter().position(|c| c.eq_ignore_ascii_case(CATEGORY_PRESETS[i])) {
                                    Some(at) => {
                                        chosen.remove(at);
                                    }
                                    None => chosen.push(CATEGORY_PRESETS[i].to_string()),
                                }
                                this.categories_input.update(cx, |input, cx| input.set_value(chosen.join(", "), window, cx));
                                cx.notify();
                            }))
                        })))
                        .into_any_element(),
                ))
                .when(owns_calendar, |d| {
                    d.child(
                        Checkbox::new("private")
                            .label("Privat (andere sehen nur die Zeit)")
                            .checked(self.private)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.private = *checked;
                                cx.notify();
                            })),
                    )
                })
                .child(field("Erinnerung", Select::new(&self.remind_select).into_any_element()))
                .child(field(
                    "Teilnehmer",
                    div()
                        .flex()
                        .gap_2()
                        .child(div().flex_1().child(Input::new(&self.attendees_input)))
                        .child(Button::new("invite").label("Einladen").on_click(cx.listener(
                            |this, _, _, cx| this.send_invitation(cx),
                        )))
                        .into_any_element(),
                ))
                .child(field("Wiederholung", Select::new(&self.repeat_select).into_any_element()))
            })
            .when(single, |d| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("Nur dieser Termin der Serie. Kalender, Status und Wiederholung gelten für die ganze Serie."),
                        )
                        .child(Button::new("series").label("Ganze Serie").on_click(cx.listener(
                            |this, _, window, cx| {
                                let (app, id) = (this.app.clone(), this.event_id.clone());
                                let origin = this.occurrence.unwrap_or_default();
                                window.remove_window();
                                cx.defer(move |cx| {
                                    app.update(cx, |app, cx| {
                                        app.select_occurrence(id, origin, cx);
                                        app.open_series(cx);
                                    })
                                });
                            },
                        ))),
                )
            })
            .when(repeats && !single, |d| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child("Alle")
                        .child(div().w(px(64.)).child(Input::new(&self.interval_input)))
                        .child(unit),
                )
                .when(self.repeat_index == 2, |d| {
                    d.child(div().flex().gap_1().children(WEEKDAY_LABELS.iter().enumerate().map(|(i, label)| {
                        let on = self.weekdays >> i & 1 == 1;
                        let button = Button::new(("weekday", i)).small().label(*label);
                        let button = if on { button.primary() } else { button };
                        button.disabled(read_only).on_click(cx.listener(move |this, _, _, cx| {
                            // At least one day stays selected.
                            let toggled = this.weekdays ^ (1 << i);
                            if toggled != 0 {
                                this.weekdays = toggled;
                                cx.notify();
                            }
                        }))
                    })))
                })
                .when(self.repeat_index >= 3, |d| d.child(Select::new(&self.month_select)))
                .child(
                    Checkbox::new("until")
                        .label("Serie endet am")
                        .checked(self.until_enabled)
                        .disabled(read_only)
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.until_enabled = *checked;
                            cx.notify();
                        })),
                )
                .when(self.until_enabled, |d| d.child(DatePicker::new(&self.until_picker)))
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child("Änderungen gelten für die ganze Serie."),
                )
            })
            .when(read_only, |d| {
                d.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("Dieser Kalender ist für dich schreibgeschützt."),
                )
            })
            )
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .p_4()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(Button::new("delete").label("Löschen").disabled(read_only).on_click(
                        move |_, window, cx| {
                            app.update(cx, |app, cx| match occurrence {
                                Some(origin) => app.delete_occurrence(&id, origin, cx),
                                None => app.delete_event(&id, cx),
                            });
                            window.remove_window();
                        },
                    ))
                    .child(
                        Button::new("cancel")
                            .label("Abbrechen")
                            .on_click(|_, window, _| window.remove_window()),
                    )
                    .child(
                        Button::new("save")
                            .primary()
                            .label("Speichern")
                            .disabled(read_only)
                            .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{CalendarChoice, NaiveTime, describe_repeat, describe_when, parse_time, time_options, unique_names};
    use chrono::NaiveDate;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn repeats_are_described_in_words() {
        let monday = d(2026, 10, 5);
        assert_eq!(describe_repeat("daily", 1, 0, 0, None, monday), "Jeden Tag");
        assert_eq!(describe_repeat("daily", 3, 0, 0, None, monday), "Alle 3 Tage");
        assert_eq!(describe_repeat("weekly", 1, 0, 0, None, monday), "Jede Woche am Montag");
        assert_eq!(describe_repeat("weekly", 2, 0b0000101, 0, Some(d(2026, 12, 21)), monday), "Alle 2 Wochen am Montag, Mittwoch, bis 21.12.2026");
        assert_eq!(describe_repeat("monthly", 1, 0, 0, None, monday), "Jeden Monat am 5.");
        assert_eq!(describe_repeat("monthly", 2, 0, 1, None, monday), "Alle 2 Monate am 1. Montag");
        assert_eq!(describe_repeat("monthly", 1, 0, -1, None, d(2026, 10, 26)), "Jeden Monat am letzten Montag");
        assert_eq!(describe_repeat("yearly", 1, 0, 0, None, monday), "Jedes Jahr am 5. Oktober");
        assert_eq!(describe_repeat("yearly", 1, 0, 2, None, d(2026, 10, 13)), "Jedes Jahr am 2. Dienstag im Oktober");
        assert_eq!(describe_repeat("", 1, 0, 0, None, monday), "");
    }

    #[test]
    fn times_are_described_in_words() {
        let at = |day: u32, h: u32, m: u32| d(2026, 10, day).and_hms_opt(h, m, 0).unwrap();
        assert_eq!(describe_when(at(21, 9, 0), at(21, 10, 30)), ["Mittwoch, 21. Oktober 2026", "09:00 – 10:30 Uhr"]);
        assert_eq!(describe_when(at(21, 0, 0), at(21, 23, 59)), ["Mittwoch, 21. Oktober 2026", "ganztägig"]);
        assert_eq!(describe_when(at(5, 0, 0), at(7, 23, 59)), ["Montag, 5. Oktober – Mittwoch, 7. Oktober 2026", "ganztägig"]);
        assert_eq!(describe_when(at(5, 22, 0), at(6, 2, 0)), ["Montag, 5. Oktober, 22:00 Uhr", "bis Dienstag, 6. Oktober 2026, 02:00 Uhr"]);
    }

    #[test]
    fn time_options_are_quarter_hours_plus_current() {
        let on_grid = time_options(NaiveTime::from_hms_opt(9, 15, 0).unwrap());
        assert_eq!(on_grid.len(), 96);
        assert_eq!((on_grid[0].as_str(), on_grid[95].as_str()), ("00:00", "23:45"));

        let off_grid = time_options(NaiveTime::from_hms_opt(9, 7, 0).unwrap());
        assert_eq!(off_grid.len(), 97);
        let at = off_grid.iter().position(|t| t == "09:07").unwrap();
        assert_eq!((off_grid[at - 1].as_str(), off_grid[at + 1].as_str()), ("09:00", "09:15"));
        assert_eq!(parse_time("09:07"), NaiveTime::from_hms_opt(9, 7, 0));
    }

    #[test]
    fn duplicate_calendar_names_stay_selectable() {
        let choice = |name: &str| CalendarChoice { id: name.into(), name: name.into(), color: 0, owned: true };
        let names = unique_names(&[choice("Team"), choice("Team"), choice("Privat")]);
        assert_eq!(names, ["Team", "Team (2)", "Privat"]);
    }
}
