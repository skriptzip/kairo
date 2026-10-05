#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod api;
mod calendar_window;
mod live;
mod login;
mod members;
mod model;
mod sync;

use api::Session;
use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use gpui_kit::component::{ActiveTheme, Root};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::calendar::{Calendar, CalendarEvent, CalendarState};
use gpui_kit::component::date_picker::{DatePicker, DatePickerEvent, DatePickerState};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::{Disableable, IndexPath, Sizable};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt, PopupMenuItem};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use calendar_window::{CalendarSnapshot, CalendarWindow};
use login::{LoginEvent, LoginView};
use model::{CalendarInfo, Event, Store, new_id, parse_color};

actions!(
    kairo,
    [
        NewEvent,
        OpenEvent,
        DeleteEvent,
        DuplicateEvent,
        ClearSelection,
        GoToday,
        PrevPeriod,
        NextPeriod,
        ViewDay,
        ViewWorkWeek,
        ViewWeek,
        SyncNow,
        SaveEvent,
        CloseWindow,
    ]
);

/// Registers the keyboard shortcuts; the context names match `key_context` in the views.
fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-n", NewEvent, Some("CalendarApp")),
        KeyBinding::new("enter", OpenEvent, Some("CalendarApp")),
        KeyBinding::new("f2", OpenEvent, Some("CalendarApp")),
        KeyBinding::new("delete", DeleteEvent, Some("CalendarApp")),
        KeyBinding::new("backspace", DeleteEvent, Some("CalendarApp")),
        KeyBinding::new("ctrl-d", DuplicateEvent, Some("CalendarApp")),
        KeyBinding::new("escape", ClearSelection, Some("CalendarApp")),
        KeyBinding::new("ctrl-t", GoToday, Some("CalendarApp")),
        KeyBinding::new("ctrl-left", PrevPeriod, Some("CalendarApp")),
        KeyBinding::new("ctrl-right", NextPeriod, Some("CalendarApp")),
        KeyBinding::new("ctrl-1", ViewDay, Some("CalendarApp")),
        KeyBinding::new("ctrl-2", ViewWorkWeek, Some("CalendarApp")),
        KeyBinding::new("ctrl-3", ViewWeek, Some("CalendarApp")),
        KeyBinding::new("f5", SyncNow, Some("CalendarApp")),
        KeyBinding::new("ctrl-r", SyncNow, Some("CalendarApp")),
        KeyBinding::new("ctrl-s", SaveEvent, Some("EventWindow")),
        KeyBinding::new("ctrl-enter", SaveEvent, Some("EventWindow")),
        KeyBinding::new("escape", CloseWindow, Some("EventWindow")),
        KeyBinding::new("escape", CloseWindow, Some("CalendarWindow")),
    ]);
}

/// Colors offered for calendars.
const PALETTE: [&str; 8] =
    ["#0f6cbd", "#107c10", "#8764b8", "#d13438", "#ca5010", "#038387", "#c239b3", "#69797e"];

const HOUR_H: f32 = 52.0;
const GUTTER_W: f32 = 56.0;
const SIDEBAR_W: f32 = 280.0;

const WEEKDAYS: [&str; 7] = [
    "Montag",
    "Dienstag",
    "Mittwoch",
    "Donnerstag",
    "Freitag",
    "Samstag",
    "Sonntag",
];
const MONTHS: [&str; 12] = [
    "Januar",
    "Februar",
    "März",
    "April",
    "Mai",
    "Juni",
    "Juli",
    "August",
    "September",
    "Oktober",
    "November",
    "Dezember",
];

#[derive(Clone, Copy, PartialEq)]
enum ViewMode {
    Day,
    WorkWeek,
    Week,
}

#[derive(Clone)]
enum SyncStatus {
    Idle,
    Syncing,
    Synced(String),
    Offline,
    Error(String),
}

struct CalendarApp {
    session: Session,
    store: Store,
    mini_calendar: Entity<CalendarState>,
    scroll: ScrollHandle,
    focus: FocusHandle,
    details_window: Option<AnyWindowHandle>,
    calendar_window: Option<AnyWindowHandle>,
    /// Last problem answering an invitation, shown under the invitations.
    notice: Option<String>,
    mode: ViewMode,
    date: NaiveDate,
    selected: Option<String>,
    /// Empty time slot (day, hour) marked by a single click.
    selected_slot: Option<(NaiveDate, u32)>,
    status: SyncStatus,
    syncing: bool,
    /// Another change arrived while a round was running.
    resync: bool,
    debounce: Option<Task<()>>,
    sync_task: Option<Task<()>>,
    _periodic: Option<Task<()>>,
    _live: live::Live,
    _live_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

enum AppEvent {
    Logout,
    /// The server rejected our token.
    SessionExpired,
}

impl EventEmitter<AppEvent> for CalendarApp {}

fn monday_of(date: NaiveDate) -> NaiveDate {
    date - Duration::days(date.weekday().num_days_from_monday() as i64)
}

fn at(day: NaiveDate, hour: u32, minute: u32) -> NaiveDateTime {
    day.and_time(NaiveTime::from_hms_opt(hour, minute, 0).unwrap())
}

fn short_date(date: NaiveDate) -> String {
    format!("{}. {}", date.day(), &MONTHS[date.month0() as usize][..3])
}

impl CalendarApp {
    fn new(session: Session, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let today = chrono::Local::now().date_naive();
        let mini_calendar = cx.new(|cx| CalendarState::new(window, cx));
        mini_calendar.update(cx, |state, cx| state.set_date(today, window, cx));

        let subscriptions = vec![cx.subscribe(&mini_calendar, |this, _, ev: &CalendarEvent, cx| {
            let CalendarEvent::Selected(date) = ev;
            if let Some(day) = date.start() {
                this.date = day;
                cx.notify();
            }
        })];

        let scroll = ScrollHandle::new();
        scroll.set_offset(point(px(0.), px(-8.0 * HOUR_H)));

        let store = Store::load(&session);
        let periodic = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(std::time::Duration::from_secs(60)).await;
                if this.update(cx, |this, cx| this.sync_now(cx)).is_err() {
                    break;
                }
            }
        });

        // Hints from the server's WebSocket trigger an immediate sync.
        let (live, hints) = live::start(session.clone());
        let live_task = cx.spawn(async move |this, cx| {
            while hints.recv().await.is_ok() {
                if this.update(cx, |this, cx| this.sync_now(cx)).is_err() {
                    break;
                }
            }
        });

        let mut app = Self {
            session,
            store,
            mini_calendar,
            scroll,
            focus: cx.focus_handle(),
            details_window: None,
            calendar_window: None,
            notice: None,
            mode: ViewMode::WorkWeek,
            date: today,
            selected: None,
            selected_slot: None,
            status: SyncStatus::Idle,
            syncing: false,
            resync: false,
            debounce: None,
            sync_task: None,
            _periodic: Some(periodic),
            _live: live,
            _live_task: live_task,
            _subscriptions: subscriptions,
        };
        app.sync_now(cx);
        window.focus(&app.focus, cx);
        app
    }

    fn calendar(&self, id: &str) -> Option<&CalendarInfo> {
        self.store.calendars.iter().find(|c| c.id == id)
    }

    fn calendar_color(&self, id: &str) -> u32 {
        self.calendar(id).map_or(0x808080, |c| parse_color(&c.color))
    }

    /// Schedules a sync shortly after the last local change.
    fn mark_changed(&mut self, cx: &mut Context<Self>) {
        self.debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_millis(1500)).await;
            this.update(cx, |this, cx| this.sync_now(cx)).ok();
        }));
        cx.notify();
    }

    fn sync_now(&mut self, cx: &mut Context<Self>) {
        if self.syncing {
            self.resync = true;
            return;
        }
        self.syncing = true;
        self.status = SyncStatus::Syncing;
        self.store.save(&self.session);
        let job = self.store.job(&self.session);
        let task = cx.background_executor().spawn(async move { sync::run(job) });
        self.sync_task = Some(cx.spawn(async move |this, cx| {
            let outcome = task.await;
            this.update(cx, |this, cx| this.finish_sync(outcome, cx)).ok();
        }));
        cx.notify();
    }

    fn finish_sync(&mut self, outcome: sync::Outcome, cx: &mut Context<Self>) {
        self.syncing = false;
        let failure = outcome.failure.clone();
        let refetch = self.store.apply(outcome);
        self.store.save(&self.session);
        if let Some(id) = &self.selected {
            if !self.store.events.iter().any(|e| &e.id == id) {
                self.selected = None;
            }
        }
        match failure {
            None => {
                self.status = SyncStatus::Synced(chrono::Local::now().format("%H:%M").to_string());
                // Edits made during the round, or calendars created for a new account.
                if refetch || self.resync || self.store.has_pending() {
                    self.resync = false;
                    self.sync_now(cx);
                    return;
                }
            }
            Some(sync::Failure::Unauthorized) => {
                cx.emit(AppEvent::SessionExpired);
                return;
            }
            Some(sync::Failure::Offline) => self.status = SyncStatus::Offline,
            Some(sync::Failure::Server(message)) => self.status = SyncStatus::Error(message),
        }
        self.resync = false;
        cx.notify();
    }

    /// Creates a calendar and opens its settings; it only becomes real once saved there.
    fn new_calendar(&mut self, cx: &mut Context<Self>) {
        let color = PALETTE[self.store.calendars.len() % PALETTE.len()].to_string();
        let id = new_id();
        self.store.calendars.push(CalendarInfo {
            id: id.clone(),
            name: "Neuer Kalender".into(),
            color,
            rev: 0,
            dirty: true,
            visible: true,
            owner: String::new(),
            role: "owner".into(),
            draft: true,
        });
        self.open_calendar_window(&id, cx);
        cx.notify();
    }

    fn open_calendar_window(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(calendar) = self.calendar(id) else { return };
        let owner = if calendar.owner.is_empty() { self.session.email.clone() } else { calendar.owner.clone() };
        let snapshot = CalendarSnapshot {
            id: calendar.id.clone(),
            name: calendar.name.clone(),
            color: calendar.color.clone(),
            owner,
            is_owner: calendar.is_owner(),
            is_draft: calendar.draft,
        };
        if let Some(old) = self.calendar_window.take() {
            old.update(cx, |_, window, _| window.remove_window()).ok();
        }
        let (app, session) = (cx.entity(), self.session.clone());
        let options = WindowOptions {
            titlebar: Some(TitlebarOptions { title: Some("Kalender verwalten".into()), ..Default::default() }),
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(520.), px(560.)), cx))),
            is_resizable: false,
            ..Default::default()
        };
        self.calendar_window = cx
            .open_window(options, |window, cx| {
                let view = cx.new(|cx| CalendarWindow::new(app, session, snapshot, window, cx));
                cx.new(|cx| Root::new(view, window, cx))
            })
            .ok()
            .map(Into::into);
    }

    fn save_calendar(&mut self, id: &str, name: String, color: String, cx: &mut Context<Self>) {
        if let Some(calendar) = self.store.calendars.iter_mut().find(|c| c.id == id) {
            calendar.name = name;
            calendar.color = color;
            calendar.dirty = true;
            calendar.draft = false;
        }
        self.mark_changed(cx);
    }

    /// Deletes a calendar you own (with all its events), or leaves one shared with you.
    fn remove_calendar(&mut self, id: &str, cx: &mut Context<Self>) {
        self.store.remove_calendar(id);
        if let Some(selected) = &self.selected {
            if !self.store.events.iter().any(|e| &e.id == selected) {
                self.selected = None;
            }
        }
        self.mark_changed(cx);
    }

    fn discard_calendar_draft(&mut self, id: &str, cx: &mut Context<Self>) {
        let before = self.store.calendars.len();
        self.store.calendars.retain(|c| !(c.id == id && c.draft));
        if self.store.calendars.len() != before {
            cx.notify();
        }
    }

    fn respond_to_invite(&mut self, calendar_id: &str, accept: bool, cx: &mut Context<Self>) {
        self.store.invites.retain(|i| i.calendar_id != calendar_id);
        self.notice = None;
        let (session, id) = (self.session.clone(), calendar_id.to_string());
        let task = cx.background_executor().spawn(async move {
            if accept { members::accept(&session, &id) } else { members::remove(&session, &id, &session.email) }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if let Err(message) = result {
                    this.notice = Some(message);
                }
                this.sync_now(cx); // pulls the calendar in (or the corrected invitation list)
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn status_text(&self) -> String {
        let pending = self.store.has_pending();
        match &self.status {
            SyncStatus::Idle => String::new(),
            SyncStatus::Syncing => "Synchronisiere …".into(),
            SyncStatus::Synced(time) => format!("Synchronisiert {time}"),
            SyncStatus::Offline if pending => "Offline – Änderungen werden später gesendet".into(),
            SyncStatus::Offline => "Offline".into(),
            SyncStatus::Error(message) => format!("Sync-Fehler: {message}"),
        }
    }

    fn select_event(&mut self, id: Option<String>, cx: &mut Context<Self>) {
        self.selected = id;
        self.selected_slot = None;
        cx.notify();
    }

    fn create_event(&mut self, day: NaiveDate, hour: u32, cx: &mut Context<Self>) {
        let writable = || self.store.calendars.iter().filter(|c| c.can_edit() && !c.draft);
        let Some(calendar) = writable().find(|c| c.visible).or(writable().next()) else {
            return; // no calendar to put it into (yet)
        };
        let id = new_id();
        self.store.events.push(Event {
            id: id.clone(),
            calendar: calendar.id.clone(),
            title: "Neuer Termin".into(),
            location: String::new(),
            description: String::new(),
            start: at(day, hour, 0),
            end: at(day, hour, 0) + Duration::hours(1),
            tz: model::default_tz(),
            status: model::default_status(),
            rev: 0,
            dirty: true,
            // Only becomes a real event (and syncs) once "Speichern" is pressed.
            draft: true,
        });
        self.select_event(Some(id), cx);
    }

    /// Removes an unsaved draft again; saved events are left alone.
    fn discard_draft(&mut self, id: &str, cx: &mut Context<Self>) {
        let before = self.store.events.len();
        self.store.events.retain(|e| !(e.id == id && e.draft));
        if self.store.events.len() != before {
            if self.selected.as_deref() == Some(id) {
                self.selected = None;
            }
            cx.notify();
        }
    }

    /// Opens the selected event in its own OS window, replacing a previously opened one.
    fn open_details(&mut self, cx: &mut Context<Self>) {
        let Some(ev) = self.selected.as_ref().and_then(|id| self.store.events.iter().find(|e| &e.id == id)) else {
            return;
        };
        let snapshot = EventSnapshot {
            id: ev.id.clone(),
            title: ev.title.clone(),
            location: ev.location.clone(),
            start: ev.start,
            end: ev.end,
            // Only calendars you can write to (plus the event's own, so it is shown correctly).
            calendars: self
                .store
                .calendars
                .iter()
                .filter(|c| (c.can_edit() && !c.draft) || c.id == ev.calendar)
                .map(|c| CalendarChoice { id: c.id.clone(), name: c.name.clone(), color: parse_color(&c.color) })
                .collect(),
            calendar_id: ev.calendar.clone(),
            status: ev.status.clone(),
            read_only: self.calendar(&ev.calendar).is_some_and(|c| !c.can_edit()),
        };
        if let Some(old) = self.details_window.take() {
            old.update(cx, |_, window, _| window.remove_window()).ok();
        }
        let app = cx.entity();
        let options = WindowOptions {
            titlebar: Some(TitlebarOptions {
                title: Some("Termindetails".into()),
                ..Default::default()
            }),
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(480.), px(590.)),
                cx,
            ))),
            is_resizable: false,
            ..Default::default()
        };
        self.details_window = cx
            .open_window(options, |window, cx| {
                let view = cx.new(|cx| EventWindow::new(app, snapshot, window, cx));
                cx.new(|cx| Root::new(view, window, cx))
            })
            .ok()
            .map(Into::into);
    }

    fn delete_event(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(i) = self.store.events.iter().position(|e| e.id == id) {
            let removed = self.store.events.remove(i);
            // Only events the server knows about need a deletion request.
            if removed.rev > 0 {
                self.store.pending_deletes.push(model::PendingDelete { id: removed.id, rev: removed.rev });
            }
        }
        if self.selected.as_deref() == Some(id) {
            self.selected = None;
        }
        self.mark_changed(cx);
    }

    /// Copies an event right after itself in the same calendar.
    fn duplicate_event(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(original) = self.store.events.iter().find(|e| e.id == id && !e.draft) else { return };
        if !self.calendar(&original.calendar).is_some_and(|c| c.can_edit()) {
            return;
        }
        let copy = Event { id: new_id(), rev: 0, dirty: true, draft: false, ..original.clone() };
        let copy_id = copy.id.clone();
        self.store.events.push(copy);
        self.select_event(Some(copy_id), cx);
        self.mark_changed(cx);
    }

    fn can_edit_event(&self, id: &str) -> bool {
        self.store
            .events
            .iter()
            .find(|e| e.id == id)
            .is_some_and(|e| self.calendar(&e.calendar).is_some_and(|c| c.can_edit()))
    }

    /// "Neuer Termin" at the given slot, opened for editing.
    fn create_and_open(&mut self, day: NaiveDate, hour: u32, cx: &mut Context<Self>) {
        self.create_event(day, hour, cx);
        self.open_details(cx);
    }

    fn new_event_now(&mut self, cx: &mut Context<Self>) {
        let now = chrono::Local::now().naive_local();
        let hour = if self.date == now.date() { (now.hour() + 1).clamp(8, 22) } else { 9 };
        self.create_and_open(self.date, hour, cx);
    }

    fn go_to(&mut self, date: NaiveDate, window: &mut Window, cx: &mut Context<Self>) {
        self.date = date;
        self.mini_calendar
            .update(cx, |state, cx| state.set_date(date, window, cx));
        cx.notify();
    }

    fn step(&mut self, direction: i64, window: &mut Window, cx: &mut Context<Self>) {
        let days = if self.mode == ViewMode::Day { 1 } else { 7 };
        self.go_to(self.date + Duration::days(days * direction), window, cx);
    }

    fn visible_days(&self) -> Vec<NaiveDate> {
        match self.mode {
            ViewMode::Day => vec![self.date],
            ViewMode::WorkWeek => (0..5).map(|i| monday_of(self.date) + Duration::days(i)).collect(),
            ViewMode::Week => (0..7).map(|i| monday_of(self.date) + Duration::days(i)).collect(),
        }
    }

    fn title(&self) -> String {
        let days = self.visible_days();
        let (first, last) = (days[0], days[days.len() - 1]);
        if self.mode == ViewMode::Day {
            return format!(
                "{}, {}. {} {}",
                WEEKDAYS[first.weekday().num_days_from_monday() as usize],
                first.day(),
                MONTHS[first.month0() as usize],
                first.year()
            );
        }
        format!(
            "KW {} · {} – {} {}",
            first.iso_week().week(),
            short_date(first),
            short_date(last),
            last.year()
        )
    }

    /// Assigns each event of one day a column so overlapping events sit side by side.
    fn layout_day(events: &[&Event]) -> Vec<(usize, usize, usize)> {
        let mut order: Vec<usize> = (0..events.len()).collect();
        order.sort_by_key(|&i| (events[i].start, events[i].end));

        let mut result = Vec::new();
        let mut cluster: Vec<(usize, usize)> = Vec::new();
        let mut column_ends: Vec<NaiveDateTime> = Vec::new();
        let mut cluster_end: Option<NaiveDateTime> = None;

        let flush = |cluster: &mut Vec<(usize, usize)>, columns: usize, out: &mut Vec<(usize, usize, usize)>| {
            out.extend(cluster.drain(..).map(|(i, col)| (i, col, columns)));
        };

        for i in order {
            let ev = events[i];
            if cluster_end.is_some_and(|end| ev.start >= end) {
                flush(&mut cluster, column_ends.len(), &mut result);
                column_ends.clear();
                cluster_end = None;
            }
            let col = match column_ends.iter().position(|&end| end <= ev.start) {
                Some(col) => {
                    column_ends[col] = ev.end;
                    col
                }
                None => {
                    column_ends.push(ev.end);
                    column_ends.len() - 1
                }
            };
            cluster.push((i, col));
            cluster_end = Some(cluster_end.map_or(ev.end, |end| end.max(ev.end)));
        }
        flush(&mut cluster, column_ends.len(), &mut result);
        result
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mode_button = |id: &'static str, label: &'static str, mode: ViewMode, cx: &mut Context<Self>| {
            let button = Button::new(id).label(label);
            let button = if self.mode == mode { button.primary() } else { button };
            button.on_click(cx.listener(move |this, _, _, cx| {
                this.mode = mode;
                cx.notify();
            }))
        };

        div()
            .flex()
            .items_center()
            .gap_2()
            .px_4()
            .py_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                Button::new("new-event")
                    .primary()
                    .label("Neuer Termin")
                    .on_click(cx.listener(|this, _, _, cx| this.new_event_now(cx))),
            )
            .child(Button::new("today").label("Heute").on_click(cx.listener(
                |this, _, window, cx| this.go_to(chrono::Local::now().date_naive(), window, cx),
            )))
            .child(Button::new("prev").label("‹").on_click(
                cx.listener(|this, _, window, cx| this.step(-1, window, cx)),
            ))
            .child(Button::new("next").label("›").on_click(
                cx.listener(|this, _, window, cx| this.step(1, window, cx)),
            ))
            .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child(self.title()))
            .child(div().flex_1())
            .child(mode_button("mode-day", "Tag", ViewMode::Day, cx))
            .child(mode_button("mode-work", "Arbeitswoche", ViewMode::WorkWeek, cx))
            .child(mode_button("mode-week", "Woche", ViewMode::Week, cx))
            .child(div().w_2())
            .child(div().text_sm().text_color(cx.theme().muted_foreground).child(self.status_text()))
            .child(
                Button::new("sync")
                    .label("Sync")
                    .on_click(cx.listener(|this, _, _, cx| this.sync_now(cx))),
            )
            .child(div().text_sm().text_color(cx.theme().muted_foreground).child(self.session.email.clone()))
            .child(
                Button::new("logout")
                    .label("Abmelden")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(AppEvent::Logout))),
            )
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let app = cx.entity();
        let calendars = self.store.calendars.iter().enumerate().filter(|(_, c)| !c.draft).map(|(i, info)| {
            let color = rgb(parse_color(&info.color));
            let shared = (!info.is_owner()).then(|| format!("von {}", info.owner));
            let (menu_app, menu_id, visible) = (app.clone(), info.id.clone(), info.visible);
            div()
                .id(("calendar-row", i))
                .flex()
                .items_center()
                .child(
                    div()
                        .id(("calendar-toggle", i))
                        .flex()
                        .flex_1()
                        .min_w_0()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .py_1()
                        .cursor_pointer()
                        .hover(|s| s.bg(cx.theme().list_hover))
                        .child(
                            div()
                                .flex_none()
                                .size_3()
                                .rounded_sm()
                                .border_1()
                                .border_color(color)
                                .when(info.visible, |d| d.bg(color)),
                        )
                        .child(div().truncate().child(info.name.clone()))
                        .when_some(shared, |d, text| d.child(div().text_xs().text_color(muted).child(text)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.store.calendars[i].visible = !this.store.calendars[i].visible;
                            cx.notify();
                        })),
                )
                .child(
                    Button::new(("manage-calendar", i))
                        .ghost()
                        .xsmall()
                        .label("···")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let id = this.store.calendars[i].id.clone();
                            this.open_calendar_window(&id, cx);
                        })),
                )
                .context_menu(move |menu, _, _| {
                    let (toggle_app, edit_app, only_app) = (menu_app.clone(), menu_app.clone(), menu_app.clone());
                    let (toggle_id, edit_id, only_id) = (menu_id.clone(), menu_id.clone(), menu_id.clone());
                    menu.item(PopupMenuItem::new(if visible { "Ausblenden" } else { "Einblenden" }).on_click(
                        move |_, _, cx| {
                            toggle_app.update(cx, |app, cx| {
                                if let Some(c) = app.store.calendars.iter_mut().find(|c| c.id == toggle_id) {
                                    c.visible = !c.visible;
                                }
                                cx.notify();
                            })
                        },
                    ))
                    .item(PopupMenuItem::new("Nur diesen anzeigen").on_click(move |_, _, cx| {
                        only_app.update(cx, |app, cx| {
                            for c in app.store.calendars.iter_mut() {
                                c.visible = c.id == only_id;
                            }
                            cx.notify();
                        })
                    }))
                    .separator()
                    .item(PopupMenuItem::new("Einstellungen …").on_click(move |_, _, cx| {
                        edit_app.update(cx, |app, cx| app.open_calendar_window(&edit_id, cx))
                    }))
                })
        });

        let invites = self.store.invites.iter().enumerate().map(|(i, invite)| {
            let (accept_id, decline_id) = (invite.calendar_id.clone(), invite.calendar_id.clone());
            let (menu_app, menu_id) = (app.clone(), invite.calendar_id.clone());
            div()
                .id(("invite", i))
                .flex()
                .flex_col()
                .gap_1()
                .p_2()
                .rounded_md()
                .border_1()
                .border_color(cx.theme().border)
                .child(div().font_weight(FontWeight::SEMIBOLD).child(invite.name.clone()))
                .child(div().text_xs().text_color(muted).child(format!(
                    "Einladung von {} · {}",
                    invite.invited_by,
                    members::role_label(&invite.role)
                )))
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(Button::new(("accept", i)).primary().small().label("Annehmen").on_click(
                            cx.listener(move |this, _, _, cx| this.respond_to_invite(&accept_id, true, cx)),
                        ))
                        .child(Button::new(("decline", i)).small().label("Ablehnen").on_click(
                            cx.listener(move |this, _, _, cx| this.respond_to_invite(&decline_id, false, cx)),
                        )),
                )
                .context_menu(move |menu, _, _| {
                    let (accept_app, decline_app) = (menu_app.clone(), menu_app.clone());
                    let (accept_id, decline_id) = (menu_id.clone(), menu_id.clone());
                    menu.item(PopupMenuItem::new("Annehmen").on_click(move |_, _, cx| {
                        accept_app.update(cx, |app, cx| app.respond_to_invite(&accept_id, true, cx))
                    }))
                    .item(PopupMenuItem::new("Ablehnen").on_click(move |_, _, cx| {
                        decline_app.update(cx, |app, cx| app.respond_to_invite(&decline_id, false, cx))
                    }))
                })
        });
        let has_invites = !self.store.invites.is_empty();

        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(SIDEBAR_W))
            .h_full()
            .gap_4()
            .p_3()
            .border_r_1()
            .border_color(cx.theme().border)
            .child(Calendar::new(&self.mini_calendar))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .child(div().flex_1().px_2().font_weight(FontWeight::SEMIBOLD).child("Meine Kalender"))
                            .child(
                                Button::new("new-calendar")
                                    .ghost()
                                    .xsmall()
                                    .label("+")
                                    .on_click(cx.listener(|this, _, _, cx| this.new_calendar(cx))),
                            ),
                    )
                    .children(calendars),
            )
            .when(has_invites, |d| {
                d.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(div().px_2().font_weight(FontWeight::SEMIBOLD).child("Einladungen"))
                        .children(invites),
                )
            })
            .when_some(self.notice.clone(), |d, message| {
                d.child(div().px_2().text_sm().text_color(rgb(0xc42b1c)).child(message))
            })
    }

    fn render_day_column(&self, day: NaiveDate, today: NaiveDateTime, cx: &mut Context<Self>) -> impl IntoElement {
        let day_events: Vec<&Event> = self
            .store
            .events
            .iter()
            .filter(|e| e.start.date() == day && self.calendar(&e.calendar).is_some_and(|c| c.visible))
            .collect();
        let placed = Self::layout_day(&day_events);

        let app = cx.entity();
        let can_create = self.store.calendars.iter().any(|c| c.can_edit() && !c.draft);
        let slots = (0..24u32).map(|hour| {
            let marked = self.selected_slot == Some((day, hour));
            let app = app.clone();
            div()
                .id(("slot", day.num_days_from_ce() as usize * 24 + hour as usize))
                .h(px(HOUR_H))
                .border_b_1()
                .border_color(cx.theme().border.opacity(0.5))
                .hover(|s| s.bg(cx.theme().list_hover))
                .when(marked, |d| d.bg(cx.theme().primary.opacity(0.18)))
                .on_click(cx.listener(move |this, ev: &ClickEvent, _, cx| {
                    if ev.click_count() >= 2 {
                        // Double click: create the event and open its settings right away.
                        this.create_and_open(day, hour, cx);
                    } else {
                        this.selected = None;
                        this.selected_slot = Some((day, hour));
                        cx.notify();
                    }
                }))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, _, _, cx| {
                        this.selected = None;
                        this.selected_slot = Some((day, hour));
                        cx.notify();
                    }),
                )
                .context_menu(move |menu, _, _| {
                    let (create_app, today_app) = (app.clone(), app.clone());
                    menu.item(
                        PopupMenuItem::new(format!("Neuer Termin um {hour:02}:00"))
                            .disabled(!can_create)
                            .on_click(move |_, _, cx| {
                                create_app.update(cx, |app, cx| app.create_and_open(day, hour, cx))
                            }),
                    )
                    .item(PopupMenuItem::new("Heute anzeigen  (Strg+T)").on_click(move |_, window, cx| {
                        today_app.update(cx, |app, cx| app.go_to(chrono::Local::now().date_naive(), window, cx))
                    }))
                })
        });

        let blocks = placed.into_iter().map(|(i, col, cols)| {
            let ev = day_events[i];
            let minutes = |t: NaiveDateTime| (t.hour() * 60 + t.minute()) as f32;
            let top = minutes(ev.start) / 60.0 * HOUR_H;
            let height = ((minutes(ev.end) - minutes(ev.start)) / 60.0 * HOUR_H).max(20.0);
            let color = rgb(self.calendar_color(&ev.calendar));
            let id = ev.id.clone();
            let selected = self.selected.as_ref() == Some(&id);
            let editable = self.can_edit_event(&id);
            let (menu_app, menu_id, select_id) = (app.clone(), id.clone(), id.clone());

            // Outlook-style "Anzeigen als": free is faint, tentative dashed, out of office purple.
            let status = ev.status.as_str();
            let tint = if status == "oof" { rgb(0xb146c2) } else { color };
            let alpha = match (selected, status) {
                (true, _) => 0.45,
                (_, "free") => 0.06,
                (_, "tentative") => 0.10,
                (_, "oof") => 0.35,
                _ => 0.22,
            };
            let tentative = status == "tentative";
            div()
                .id(SharedString::from(format!("event-{id}")))
                .occlude()
                .absolute()
                .top(px(top))
                .h(px(height - 1.0))
                .left(relative(col as f32 / cols as f32))
                .w(relative(1.0 / cols as f32))
                .p_0p5()
                .child(
                    div()
                        .size_full()
                        .overflow_hidden()
                        .px_1p5()
                        .py_0p5()
                        .rounded_sm()
                        .when(tentative, |d| d.border_1().border_dashed())
                        .border_l_4()
                        .border_color(tint)
                        .bg(tint.opacity(alpha))
                        .text_xs()
                        .cursor_pointer()
                        .child(div().font_weight(FontWeight::SEMIBOLD).child(ev.title.clone()))
                        .child(format!("{}–{}", ev.start.format("%H:%M"), ev.end.format("%H:%M")))
                        .when(!ev.location.is_empty() && height > 52.0, |d| d.child(ev.location.clone())),
                )
                .on_click(cx.listener(move |this, ev: &ClickEvent, _, cx| {
                    this.select_event(Some(id.clone()), cx);
                    if ev.click_count() >= 2 {
                        this.open_details(cx);
                    }
                }))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, _, _, cx| this.select_event(Some(select_id.clone()), cx)),
                )
                .context_menu(move |menu, _, _| {
                    let (open_app, copy_app, delete_app) = (menu_app.clone(), menu_app.clone(), menu_app.clone());
                    let (open_id, copy_id, delete_id) = (menu_id.clone(), menu_id.clone(), menu_id.clone());
                    menu.item(PopupMenuItem::new("Öffnen  (Enter)").on_click(move |_, _, cx| {
                        open_app.update(cx, |app, cx| {
                            app.select_event(Some(open_id.clone()), cx);
                            app.open_details(cx);
                        })
                    }))
                    .item(PopupMenuItem::new("Duplizieren  (Strg+D)").disabled(!editable).on_click(
                        move |_, _, cx| copy_app.update(cx, |app, cx| app.duplicate_event(&copy_id, cx)),
                    ))
                    .separator()
                    .item(PopupMenuItem::new("Löschen  (Entf)").disabled(!editable).on_click(
                        move |_, _, cx| delete_app.update(cx, |app, cx| app.delete_event(&delete_id, cx)),
                    ))
                })
        });

        let now_line = (today.date() == day).then(|| {
            let y = (today.hour() * 60 + today.minute()) as f32 / 60.0 * HOUR_H;
            div()
                .absolute()
                .top(px(y))
                .left_0()
                .right_0()
                .h(px(2.))
                .bg(rgb(0xd13438))
        });

        div()
            .relative()
            .flex_1()
            .min_w_0()
            .border_l_1()
            .border_color(cx.theme().border)
            .child(div().flex().flex_col().children(slots))
            .children(blocks)
            .children(now_line)
    }

    fn render_grid(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let days = self.visible_days();
        let now = chrono::Local::now().naive_local();

        let header = days.iter().map(|&day| {
            let is_today = day == now.date();
            div()
                .flex_1()
                .min_w_0()
                .py_2()
                .flex()
                .flex_col()
                .items_center()
                .border_l_1()
                .border_color(cx.theme().border)
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(WEEKDAYS[day.weekday().num_days_from_monday() as usize]),
                )
                .child(
                    div()
                        .px_2()
                        .rounded_full()
                        .text_lg()
                        .when(is_today, |d| d.bg(cx.theme().primary).text_color(white()))
                        .child(day.day().to_string()),
                )
        }).collect::<Vec<_>>();

        let hours = (0..24u32).map(|hour| {
            div()
                .h(px(HOUR_H))
                .pr_2()
                .text_xs()
                .text_right()
                .text_color(cx.theme().muted_foreground)
                .child(if hour == 0 { String::new() } else { format!("{hour:02}:00") })
        }).collect::<Vec<_>>();

        let columns = days
            .iter()
            .map(|&day| self.render_day_column(day, now, cx).into_any_element())
            .collect::<Vec<_>>();

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(
                div()
                    .flex()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(div().flex_none().w(px(GUTTER_W)))
                    .children(header),
            )
            .child(
                div()
                    .id("grid-scroll")
                    .flex_1()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(
                        div()
                            .flex()
                            .child(
                                div()
                                    .flex_none()
                                    .w(px(GUTTER_W))
                                    .child(div().h(px(0.)))
                                    .children(hours),
                            )
                            .children(columns),
                    ),
            )
    }
}

struct CalendarChoice {
    id: String,
    name: String,
    color: u32,
}

struct EventSnapshot {
    id: String,
    title: String,
    location: String,
    start: NaiveDateTime,
    end: NaiveDateTime,
    calendars: Vec<CalendarChoice>,
    calendar_id: String,
    status: String,
    /// In a calendar shared with you as "Ansehen": looking is fine, changing is not.
    read_only: bool,
}

/// "Anzeigen als" choices, in dropdown order: (stored value, label).
const STATUS_CHOICES: [(&str, &str); 4] =
    [("free", "Frei"), ("tentative", "Mit Vorbehalt"), ("busy", "Gebucht"), ("oof", "Abwesend")];

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
struct EventWindow {
    app: Entity<CalendarApp>,
    event_id: String,
    title_input: Entity<InputState>,
    location_input: Entity<InputState>,
    date_picker: Entity<DatePickerState>,
    start_select: Entity<ChoiceState>,
    end_select: Entity<ChoiceState>,
    calendar_select: Entity<ChoiceState>,
    status_select: Entity<ChoiceState>,
    status_index: usize,
    read_only: bool,
    calendars: Vec<CalendarChoice>,
    calendar_names: Vec<String>,
    calendar_index: usize,
    start: NaiveDateTime,
    end: NaiveDateTime,
    _subscriptions: Vec<Subscription>,
}

impl EventWindow {
    /// Takes a snapshot of the event instead of reading `app`: this runs while `app` is
    /// still being updated (the window is opened from one of its click handlers).
    fn new(app: Entity<CalendarApp>, snapshot: EventSnapshot, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let EventSnapshot { id: event_id, title, location, start, end, calendars, calendar_id, status, read_only } = snapshot;

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

        let subscriptions = vec![
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
                        let end = this.start.date().and_time(time);
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
        let title_focus = title_input.read(cx).focus_handle(cx);
        window.focus(&title_focus, cx);

        Self {
            app,
            event_id,
            title_input,
            location_input,
            date_picker,
            start_select,
            end_select,
            calendar_select,
            status_select,
            status_index,
            read_only,
            calendars,
            calendar_names,
            calendar_index,
            start,
            end,
            _subscriptions: subscriptions,
        }
    }

    /// Sets the event to `start` with the given duration (kept within the start's day, since the
    /// calendar grid shows single-day events) and brings all the pickers in line.
    fn set_times(&mut self, start: NaiveDateTime, duration: Duration, window: &mut Window, cx: &mut Context<Self>) {
        let last_minute = start.date().and_hms_opt(23, 59, 0).unwrap();
        let mut end = start + duration;
        if end > last_minute {
            end = last_minute.max(start + Duration::minutes(1));
        }
        if (start, end) == (self.start, self.end) {
            return; // also stops the pickers' own updates from echoing back
        }
        self.start = start;
        self.end = end;

        let day = start.date();
        self.date_picker.update(cx, |picker, cx| picker.set_date(day, window, cx));
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
        self.app.update(cx, |app, cx| {
            if let Some(event) = app.store.events.iter_mut().find(|e| e.id == id) {
                event.status = status;
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
        let label = |text: &'static str| div().text_xs().text_color(cx.theme().muted_foreground).child(text);
        let field = |text: &'static str, content: AnyElement| {
            div().flex().flex_col().gap_1().child(label(text)).child(content)
        };
        let id = self.event_id.clone();
        let app = self.app.clone();
        let read_only = self.read_only;
        let color = self.calendars.get(self.calendar_index).map_or(0x808080, |c| c.color);

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
            .gap_3()
            .p_4()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(field("Titel", Input::new(&self.title_input).into_any_element()))
            .child(field("Ort", Input::new(&self.location_input).into_any_element()))
            .child(field("Datum", DatePicker::new(&self.date_picker).into_any_element()))
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(div().flex_1().child(field("Beginn", Select::new(&self.start_select).into_any_element())))
                    .child(div().flex_1().child(field("Ende", Select::new(&self.end_select).into_any_element()))),
            )
            .child(field(
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
            .when(read_only, |d| {
                d.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("Dieser Kalender ist für dich schreibgeschützt."),
                )
            })
            .child(div().flex_1())
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(Button::new("delete").label("Löschen").disabled(read_only).on_click(
                        move |_, window, cx| {
                            app.update(cx, |app, cx| app.delete_event(&id, cx));
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

impl Render for CalendarApp {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("CalendarApp")
            .track_focus(&self.focus)
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| window.focus(&this.focus, cx)))
            .on_mouse_down(MouseButton::Right, cx.listener(|this, _, window, cx| window.focus(&this.focus, cx)))
            .on_action(cx.listener(|this, _: &NewEvent, _, cx| this.new_event_now(cx)))
            .on_action(cx.listener(|this, _: &OpenEvent, _, cx| this.open_details(cx)))
            .on_action(cx.listener(|this, _: &DeleteEvent, _, cx| {
                if let Some(id) = this.selected.clone() {
                    if this.can_edit_event(&id) {
                        this.delete_event(&id, cx);
                    }
                }
            }))
            .on_action(cx.listener(|this, _: &DuplicateEvent, _, cx| {
                if let Some(id) = this.selected.clone() {
                    this.duplicate_event(&id, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ClearSelection, _, cx| this.select_event(None, cx)))
            .on_action(cx.listener(|this, _: &GoToday, window, cx| {
                this.go_to(chrono::Local::now().date_naive(), window, cx)
            }))
            .on_action(cx.listener(|this, _: &PrevPeriod, window, cx| this.step(-1, window, cx)))
            .on_action(cx.listener(|this, _: &NextPeriod, window, cx| this.step(1, window, cx)))
            .on_action(cx.listener(|this, _: &ViewDay, _, cx| {
                this.mode = ViewMode::Day;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ViewWorkWeek, _, cx| {
                this.mode = ViewMode::WorkWeek;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ViewWeek, _, cx| {
                this.mode = ViewMode::Week;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &SyncNow, _, cx| this.sync_now(cx)))
            .flex()
            .flex_col()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_toolbar(cx))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_sidebar(cx))
                    .child(self.render_grid(cx)),
            )
    }
}

enum Screen {
    /// Checking a stored session against the server.
    Checking,
    Login(Entity<LoginView>),
    Calendar(Entity<CalendarApp>),
}

/// Top-level view: sign-in screen or calendar, depending on the session.
struct Shell {
    screen: Screen,
    session: Option<Session>,
    _subscriptions: Vec<Subscription>,
}

impl Shell {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut shell = Self { screen: Screen::Checking, session: None, _subscriptions: Vec::new() };
        match api::load() {
            None => shell.show_login(None, window, cx),
            Some(session) => {
                let checked = session.clone();
                let task = cx.background_executor().spawn(async move { api::validate(&checked) });
                cx.spawn_in(window, async move |this, cx| {
                    let validity = task.await;
                    this.update_in(cx, |this, window, cx| match validity {
                        api::Validity::Rejected => {
                            api::forget_token(&session);
                            this.show_login(Some("Deine Sitzung ist abgelaufen. Bitte neu anmelden.".into()), window, cx);
                        }
                        // Offline: keep working with the stored session.
                        api::Validity::Valid | api::Validity::Offline => this.show_calendar(session, window, cx),
                    })
                    .ok();
                })
                .detach();
            }
        }
        shell
    }

    fn show_login(&mut self, notice: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let login = cx.new(|cx| LoginView::new(notice, window, cx));
        self._subscriptions = vec![cx.subscribe_in(&login, window, |this, _, ev: &LoginEvent, window, cx| {
            let LoginEvent::Authenticated(session) = ev;
            this.show_calendar(session.clone(), window, cx);
        })];
        self.session = None;
        self.screen = Screen::Login(login);
        cx.notify();
    }

    fn show_calendar(&mut self, session: Session, window: &mut Window, cx: &mut Context<Self>) {
        let app = cx.new(|cx| CalendarApp::new(session.clone(), window, cx));
        self._subscriptions = vec![cx.subscribe_in(&app, window, |this, _, ev: &AppEvent, window, cx| {
            let Some(session) = this.session.take() else { return };
            api::forget_token(&session);
            match ev {
                AppEvent::Logout => {
                    cx.background_executor().spawn(async move { api::logout(&session) }).detach();
                    this.show_login(None, window, cx);
                }
                AppEvent::SessionExpired => {
                    this.show_login(Some("Deine Sitzung ist abgelaufen. Bitte neu anmelden.".into()), window, cx);
                }
            }
        })];
        self.session = Some(session);
        self.screen = Screen::Calendar(app);
        cx.notify();
    }
}

impl Render for Shell {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match &self.screen {
            Screen::Checking => div()
                .flex()
                .items_center()
                .justify_center()
                .size_full()
                .bg(cx.theme().background)
                .text_color(cx.theme().muted_foreground)
                .child("Verbinde mit Server …")
                .into_any_element(),
            Screen::Login(view) => view.clone().into_any_element(),
            Screen::Calendar(view) => view.clone().into_any_element(),
        }
    }
}

fn main() {
    application()
        .with_assets(assets::Assets)
        .run(|cx| {
            init(cx);
            bind_keys(cx);
            cx.set_app_identity("de.kairo.app", "Kairo");

            let options = WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("Kairo".into()),
                    ..Default::default()
                }),
                // Matches packaging/kairo.desktop (StartupWMClass) so Linux shows the icon.
                app_id: Some("kairo".into()),
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1280.), px(800.)),
                    cx,
                ))),
                ..Default::default()
            };
            cx.open_window(options, |window, cx| {
                let view = cx.new(|cx| Shell::new(window, cx));
                cx.new(|cx| Root::new(view, window, cx))
            })
                .expect("Failed to open window");
        });
}

#[cfg(test)]
mod tests {
    use super::{CalendarChoice, NaiveTime, parse_time, time_options, unique_names};

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
        let choice = |name: &str| CalendarChoice { id: name.into(), name: name.into(), color: 0 };
        let names = unique_names(&[choice("Team"), choice("Team"), choice("Privat")]);
        assert_eq!(names, ["Team", "Team (2)", "Privat"]);
    }
}
