#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod agenda_view;
mod calendar_window;
mod event_window;
mod history;
mod login;
mod month_view;
mod notify;
mod print;
mod pst_import;
mod sidebar;
mod transfer;
mod week_view;
mod widgets;

use std::{cell::Cell, rc::Rc};

use kairo_core::{api, ics, live, members, model, sync};

use api::Session;
use chrono::{Datelike, Duration, Months, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use gpui_kit::component::{ActiveTheme, Root, Theme, ThemeMode};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::calendar::{Calendar, CalendarEvent, CalendarState};
use gpui_kit::component::date_picker::{DatePicker, DatePickerEvent, DatePickerState};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::{Disableable, IndexPath, Sizable};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::{ContextMenuExt, PopupMenu, PopupMenuItem};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use calendar_window::{CalendarSnapshot, CalendarWindow};
use event_window::{CalendarChoice, EventSnapshot, EventWindow};
use widgets::{Drag, DragPreview, event_menu};
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
        ViewMonth,
        ViewAgenda,
        PrintView,
        SyncNow,
        ImportPst,
        Undo,
        Redo,
        FocusSearch,
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
        KeyBinding::new("ctrl-4", ViewMonth, Some("CalendarApp")),
        KeyBinding::new("ctrl-5", ViewAgenda, Some("CalendarApp")),
        KeyBinding::new("ctrl-p", PrintView, Some("CalendarApp")),
        KeyBinding::new("f5", SyncNow, Some("CalendarApp")),
        KeyBinding::new("ctrl-r", SyncNow, Some("CalendarApp")),
        KeyBinding::new("ctrl-i", ImportPst, Some("CalendarApp")),
        KeyBinding::new("ctrl-z", Undo, Some("CalendarApp")),
        KeyBinding::new("ctrl-y", Redo, Some("CalendarApp")),
        KeyBinding::new("ctrl-shift-z", Redo, Some("CalendarApp")),
        KeyBinding::new("ctrl-f", FocusSearch, Some("CalendarApp")),
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
    Month,
    Agenda,
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
    /// Result of the last `.pst` import, shown under the calendars.
    import_note: Option<String>,
    /// The events as they were after the last change or sync: what the next change starts from.
    baseline: Vec<Event>,
    undo_stack: Vec<Vec<Event>>,
    redo_stack: Vec<Vec<Event>>,
    search_input: Entity<InputState>,
    /// Dropdown for the zone times are shown in.
    zone_select: Entity<SelectState<Vec<String>>>,
    /// What is typed into the search box.
    search: String,
    importing: bool,
    /// An import waiting for the choice of calendars.
    pending_import: Option<transfer::PendingImport>,
    mode: ViewMode,
    date: NaiveDate,
    selected: Option<String>,
    /// The regular day of the selected repetition (for repeating events).
    selected_origin: Option<NaiveDate>,
    /// Empty time slot (day, hour) marked by a single click.
    selected_slot: Option<(NaiveDate, u32)>,
    status: SyncStatus,
    syncing: bool,
    /// Another change arrived while a round was running.
    resync: bool,
    debounce: Option<Task<()>>,
    sync_task: Option<Task<()>>,
    _periodic: Option<Task<()>>,
    _reminders: Option<Task<()>>,
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

/// A label always gets the same color (one of the calendar colors, picked by its name).
fn category_color(name: &str) -> u32 {
    let sum = name.to_lowercase().bytes().fold(7u32, |acc, b| acc.wrapping_mul(31).wrapping_add(u32::from(b)));
    parse_color(PALETTE[sum as usize % PALETTE.len()])
}

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
        let today = model::today();
        let mini_calendar = cx.new(|cx| CalendarState::new(window, cx));
        mini_calendar.update(cx, |state, cx| state.set_date(today, window, cx));

        let search_input = cx.new(|cx| InputState::new(window, cx).placeholder("Termine suchen (Strg+F)"));
        let zone_select = cx.new(|cx| {
            let current = model::display_tz();
            let mut zones: Vec<String> = model::TIME_ZONES.iter().map(|z| z.to_string()).collect();
            if !zones.contains(&current) {
                zones.push(current.clone());
            }
            let index = zones.iter().position(|z| *z == current).unwrap_or(0);
            SelectState::new(zones, Some(IndexPath::new(index)), window, cx).searchable(true)
        });
        let subscriptions = vec![
            cx.subscribe(&zone_select, |this, _, ev: &SelectEvent<Vec<String>>, cx| {
                if let SelectEvent::Confirm(Some(name)) = ev {
                    model::set_display_tz(name);
                    this.baseline = this.snapshot();
                    cx.notify();
                }
            }),
            cx.subscribe(&search_input, |this, input, ev: &InputEvent, cx| {
                if let InputEvent::Change = ev {
                    this.search = input.read(cx).value().to_string();
                    cx.notify();
                }
            }),
            cx.subscribe(&mini_calendar, |this, _, ev: &CalendarEvent, cx| {
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

        let reminders = cx.spawn(async move |this, cx| {
            loop {
                if this.update(cx, |this, cx| this.check_reminders(cx)).is_err() {
                    break;
                }
                cx.background_executor().timer(std::time::Duration::from_secs(20)).await;
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
            import_note: None,
            baseline: Vec::new(),
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            search_input,
            zone_select,
            search: String::new(),
            importing: false,
            pending_import: None,
            mode: ViewMode::WorkWeek,
            date: today,
            selected: None,
            selected_origin: None,
            selected_slot: None,
            status: SyncStatus::Idle,
            syncing: false,
            resync: false,
            debounce: None,
            sync_task: None,
            _periodic: Some(periodic),
            _reminders: Some(reminders),
            _live: live,
            _live_task: live_task,
            _subscriptions: subscriptions,
        };
        app.baseline = app.snapshot();
        app.sync_now(cx);
        window.focus(&app.focus, cx);
        app
    }

    fn calendar(&self, id: &str) -> Option<&CalendarInfo> {
        self.store.calendars.iter().find(|c| c.id == id)
    }

    /// The event's own color, or its calendar's.
    fn event_color(&self, event: &Event) -> u32 {
        event.color.as_deref().map(parse_color).unwrap_or_else(|| self.calendar_color(&event.calendar))
    }

    fn calendar_color(&self, id: &str) -> u32 {
        self.calendar(id).map_or(0x808080, |c| parse_color(&c.color))
    }

    /// Schedules a sync shortly after the last local change.
    fn schedule_sync(&mut self, cx: &mut Context<Self>) {
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
        self.baseline = self.snapshot();
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
        self.selected_origin = None;
        self.selected_slot = None;
        cx.notify();
    }

    /// Selects one repetition (`origin` = the day it regularly belongs to).
    fn select_occurrence(&mut self, id: String, origin: NaiveDate, cx: &mut Context<Self>) {
        self.select_event(Some(id), cx);
        self.selected_origin = Some(origin);
    }

    fn is_selected(&self, id: &str, origin: NaiveDate) -> bool {
        self.selected.as_deref() == Some(id) && self.selected_origin.is_none_or(|o| o == origin)
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
            tz: model::display_tz(),
            status: model::default_status(),
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

    /// Opens the selected event (for a series: the selected repetition) in its own window.
    fn open_details(&mut self, cx: &mut Context<Self>) {
        self.open_event_window(false, cx);
    }

    /// Opens the whole series of the selected event.
    fn open_series(&mut self, cx: &mut Context<Self>) {
        self.open_event_window(true, cx);
    }

    /// Replaces a previously opened details window.
    fn open_event_window(&mut self, whole_series: bool, cx: &mut Context<Self>) {
        let Some(ev) = self.selected.as_ref().and_then(|id| self.store.events.iter().find(|e| &e.id == id)) else {
            return;
        };
        let occurrence = (!whole_series && ev.repeats()).then(|| self.selected_origin.unwrap_or(ev.start.date()));
        let shown = occurrence.map_or_else(|| ev.clone(), |origin| ev.occurrence(origin));
        let snapshot = EventSnapshot {
            id: ev.id.clone(),
            occurrence,
            title: shown.title.clone(),
            location: shown.location.clone(),
            start: shown.start,
            end: shown.end,
            repeat_interval: ev.repeat_interval,
            repeat_days: ev.repeat_days,
            repeat_nth: ev.repeat_nth,
            remind: ev.remind,
            attendees: ev.attendees.clone(),
            color: ev.color.clone(),
            tz: ev.tz.clone(),
            description: ev.description.clone(),
            private: ev.private,
            categories: ev.categories.clone(),
            start_editing: ev.draft,
            // Only calendars you can write to (plus the event's own, so it is shown correctly).
            calendars: self
                .store
                .calendars
                .iter()
                .filter(|c| (c.can_edit() && !c.draft) || c.id == ev.calendar)
                .map(|c| CalendarChoice { id: c.id.clone(), name: c.name.clone(), color: parse_color(&c.color), owned: c.is_owner() })
                .collect(),
            calendar_id: ev.calendar.clone(),
            status: ev.status.clone(),
            repeat: ev.repeat.clone(),
            repeat_until: ev.repeat_until,
            read_only: !self.can_edit_event(&ev.id),
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
                size(px(480.), px(780.)),
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

    /// Deletes one repetition of a series.
    fn delete_occurrence(&mut self, id: &str, origin: NaiveDate, cx: &mut Context<Self>) {
        if let Some(event) = self.store.events.iter_mut().find(|e| e.id == id) {
            event.delete_occurrence(origin);
            event.dirty = true;
        }
        if self.selected.as_deref() == Some(id) {
            self.selected = None;
        }
        self.mark_changed(cx);
    }

    /// Saves the changes of one repetition from its details window.
    fn save_occurrence(
        &mut self,
        id: &str,
        origin: NaiveDate,
        (start, end): (NaiveDateTime, NaiveDateTime),
        title: &str,
        location: &str,
        cx: &mut Context<Self>,
    ) {
        if let Some(event) = self.store.events.iter_mut().find(|e| e.id == id) {
            event.change_occurrence(origin, start, end, title, location);
            event.dirty = true;
        }
        self.mark_changed(cx);
    }

    fn can_edit_event(&self, id: &str) -> bool {
        self.store
            .events
            .iter()
            .find(|e| e.id == id)
            // Others' private events show only when; their details are not ours to change.
            .is_some_and(|e| self.calendar(&e.calendar).is_some_and(|c| c.can_edit() && (!e.private || c.is_owner())))
    }

    /// "Neuer Termin" at the given slot, opened for editing.
    fn create_and_open(&mut self, day: NaiveDate, hour: u32, cx: &mut Context<Self>) {
        self.create_event(day, hour, cx);
        self.open_details(cx);
    }

    fn new_event_now(&mut self, cx: &mut Context<Self>) {
        let now = model::now();
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
        if self.mode == ViewMode::Agenda {
            self.go_to(self.date + Duration::days(14 * direction), window, cx);
            return;
        }
        if self.mode == ViewMode::Month {
            let months = Months::new(1);
            let date = if direction < 0 { self.date.checked_sub_months(months) } else { self.date.checked_add_months(months) };
            self.go_to(date.unwrap_or(self.date), window, cx);
            return;
        }
        let days = if self.mode == ViewMode::Day { 1 } else { 7 };
        self.go_to(self.date + Duration::days(days * direction), window, cx);
    }

    fn visible_days(&self) -> Vec<NaiveDate> {
        match self.mode {
            ViewMode::Day => vec![self.date],
            ViewMode::WorkWeek => (0..5).map(|i| monday_of(self.date) + Duration::days(i)).collect(),
            ViewMode::Week => (0..7).map(|i| monday_of(self.date) + Duration::days(i)).collect(),
            ViewMode::Agenda => (0..agenda_view::AGENDA_DAYS).map(|i| self.date + Duration::days(i)).collect(),
            ViewMode::Month => {
                let first = self.date.with_day(1).unwrap_or(self.date);
                (0..42).map(|i| monday_of(first) + Duration::days(i)).collect()
            }
        }
    }

    fn title(&self) -> String {
        if self.mode == ViewMode::Month {
            return format!("{} {}", MONTHS[self.date.month0() as usize], self.date.year());
        }
        if self.mode == ViewMode::Agenda {
            return format!("Ab {}. {} {}", self.date.day(), MONTHS[self.date.month0() as usize], self.date.year());
        }
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
                |this, _, window, cx| this.go_to(model::today(), window, cx),
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
            .child(mode_button("mode-month", "Monat", ViewMode::Month, cx))
            .child(mode_button("mode-agenda", "Liste", ViewMode::Agenda, cx))
            .child(
                Button::new("print")
                    .ghost()
                    .label("Drucken")
                    .on_click(cx.listener(|this, _, _, cx| this.print_view(cx))),
            )
            .child(
                Button::new("undo")
                    .ghost()
                    .label("↶")
                    .disabled(self.undo_stack.is_empty())
                    .on_click(cx.listener(|this, _, _, cx| this.step_history(false, cx))),
            )
            .child(
                Button::new("redo")
                    .ghost()
                    .label("↷")
                    .disabled(self.redo_stack.is_empty())
                    .on_click(cx.listener(|this, _, _, cx| this.step_history(true, cx))),
            )
            .child(
                Button::new("theme")
                    .ghost()
                    .label(if cx.theme().mode.is_dark() { "Hell" } else { "Dunkel" })
                    .on_click(cx.listener(|_, _, window, cx| {
                        let mode = if cx.theme().mode.is_dark() { ThemeMode::Light } else { ThemeMode::Dark };
                        Theme::change(mode, Some(window), cx);
                        cx.refresh_windows(); // the details windows share the theme
                    })),
            )
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

}

impl Render for CalendarApp {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let import_choice = self.render_import_choice(cx);
        div()
            .key_context("CalendarApp")
            .relative()
            .track_focus(&self.focus)
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| window.focus(&this.focus, cx)))
            .on_mouse_down(MouseButton::Right, cx.listener(|this, _, window, cx| window.focus(&this.focus, cx)))
            .on_action(cx.listener(|this, _: &NewEvent, _, cx| this.new_event_now(cx)))
            .on_action(cx.listener(|this, _: &OpenEvent, _, cx| this.open_details(cx)))
            .on_action(cx.listener(|this, _: &DeleteEvent, _, cx| {
                if let Some(id) = this.selected.clone() {
                    if this.can_edit_event(&id) {
                        let series = this.store.events.iter().any(|e| e.id == id && e.repeats());
                        match this.selected_origin {
                            Some(origin) if series => this.delete_occurrence(&id, origin, cx),
                            _ => this.delete_event(&id, cx),
                        }
                    }
                }
            }))
            .on_action(cx.listener(|this, _: &DuplicateEvent, _, cx| {
                if let Some(id) = this.selected.clone() {
                    this.duplicate_event(&id, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ClearSelection, window, cx| {
                if !this.search.is_empty() {
                    this.search_input.update(cx, |input, cx| input.set_value("", window, cx));
                }
                this.select_event(None, cx);
            }))
            .on_action(cx.listener(|this, _: &GoToday, window, cx| {
                this.go_to(model::today(), window, cx)
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
            .on_action(cx.listener(|this, _: &ViewAgenda, _, cx| {
                this.mode = ViewMode::Agenda;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &PrintView, _, cx| this.print_view(cx)))
            .on_action(cx.listener(|this, _: &ViewMonth, _, cx| {
                this.mode = ViewMode::Month;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ViewWeek, _, cx| {
                this.mode = ViewMode::Week;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &SyncNow, _, cx| this.sync_now(cx)))
            .on_action(cx.listener(|this, _: &ImportPst, _, cx| this.import_pst(cx)))
            .on_action(cx.listener(|this, _: &Undo, _, cx| this.step_history(false, cx)))
            .on_action(cx.listener(|this, _: &Redo, _, cx| this.step_history(true, cx)))
            .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
                let handle = this.search_input.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
            }))
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
                    .child(match self.mode {
                        ViewMode::Month => self.render_month(cx).into_any_element(),
                        ViewMode::Agenda => self.render_agenda(cx).into_any_element(),
                        _ => self.render_grid(cx).into_any_element(),
                    }),
            )
            .children(import_choice)
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
    model::load_display_tz();
    #[cfg(windows)]
    notify::register_app_id();
    application()
        .with_assets(assets::Assets)
        .run(|cx| {
            init(cx);
            Theme::sync_system_appearance(None, cx);
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
                // Follow the system's light/dark setting while the app runs.
                window
                    .observe_window_appearance(|window, cx| {
                        Theme::sync_system_appearance(Some(window), cx);
                        cx.refresh_windows();
                    })
                    .detach();
                let view = cx.new(|cx| Shell::new(window, cx));
                cx.new(|cx| Root::new(view, window, cx))
            })
                .expect("Failed to open window");
        });
}

