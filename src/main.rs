use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use gpui_kit::component::{ActiveTheme, Root};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::calendar::{Calendar, CalendarEvent, CalendarState};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

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

struct CalendarInfo {
    name: &'static str,
    color: u32,
    visible: bool,
}

struct Event {
    id: u32,
    title: String,
    location: String,
    start: NaiveDateTime,
    end: NaiveDateTime,
    calendar: usize,
}

struct CalendarApp {
    mini_calendar: Entity<CalendarState>,
    scroll: ScrollHandle,
    details_window: Option<AnyWindowHandle>,
    mode: ViewMode,
    date: NaiveDate,
    calendars: Vec<CalendarInfo>,
    events: Vec<Event>,
    selected: Option<u32>,
    next_id: u32,
    _subscriptions: Vec<Subscription>,
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
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
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

        let mut app = Self {
            mini_calendar,
            scroll,
            details_window: None,
            mode: ViewMode::WorkWeek,
            date: today,
            calendars: vec![
                CalendarInfo { name: "Team", color: 0x0f6cbd, visible: true },
                CalendarInfo { name: "Kunden", color: 0x107c10, visible: true },
                CalendarInfo { name: "Persönlich", color: 0x8764b8, visible: true },
            ],
            events: Vec::new(),
            selected: None,
            next_id: 1,
            _subscriptions: subscriptions,
        };
        app.seed_demo_events(today);
        app
    }

    fn seed_demo_events(&mut self, today: NaiveDate) {
        let monday = monday_of(today);
        let d = |offset: i64| monday + Duration::days(offset);
        let mut add = |day: NaiveDate, from: (u32, u32), to: (u32, u32), cal, title: &str, loc: &str| {
            self.add_raw(title, loc, at(day, from.0, from.1), at(day, to.0, to.1), cal);
        };
        for i in 0..5 {
            add(d(i), (9, 0), (9, 15), 0, "Daily Stand-up", "Teams");
        }
        add(d(0), (10, 0), (12, 0), 0, "Sprint Planning", "Raum Berlin");
        add(d(0), (14, 0), (15, 0), 2, "Zahnarzt", "");
        add(d(1), (14, 0), (15, 30), 1, "Kundentermin Müller GmbH", "Raum Hamburg");
        add(d(1), (14, 30), (15, 30), 0, "Design Review", "Teams");
        add(d(2), (12, 0), (13, 0), 2, "Mittagessen mit HR", "Kantine");
        add(d(2), (15, 0), (16, 0), 1, "Angebot Schmidt AG", "Telefon");
        add(d(3), (9, 30), (11, 0), 0, "Quartalsbericht", "Raum München");
        add(d(3), (13, 0), (17, 0), 0, "Workshop IT-Sicherheit", "Schulungsraum");
        add(d(4), (11, 0), (11, 30), 0, "1:1 mit Teamleitung", "Teams");
        add(d(4), (15, 0), (16, 30), 1, "Projekt-Abnahme Nord", "Kunde vor Ort");
    }

    fn add_raw(
        &mut self,
        title: &str,
        location: &str,
        start: NaiveDateTime,
        end: NaiveDateTime,
        calendar: usize,
    ) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.events.push(Event {
            id,
            title: title.to_string(),
            location: location.to_string(),
            start,
            end,
            calendar,
        });
        id
    }

    fn select_event(&mut self, id: Option<u32>, cx: &mut Context<Self>) {
        self.selected = id;
        cx.notify();
    }

    fn create_event(&mut self, day: NaiveDate, hour: u32, cx: &mut Context<Self>) {
        let calendar = self.calendars.iter().position(|c| c.visible).unwrap_or(0);
        let id = self.add_raw(
            "Neuer Termin",
            "",
            at(day, hour, 0),
            at(day, hour, 0) + Duration::hours(1),
            calendar,
        );
        self.select_event(Some(id), cx);
    }

    /// Opens the selected event in its own OS window, replacing a previously opened one.
    fn open_details(&mut self, cx: &mut Context<Self>) {
        let Some(ev) = self.selected.and_then(|id| self.events.iter().find(|e| e.id == id)) else {
            return;
        };
        let calendar = &self.calendars[ev.calendar];
        let snapshot = EventSnapshot {
            id: ev.id,
            title: ev.title.clone(),
            location: ev.location.clone(),
            when: format!(
                "{}, {}. {} {} · {} – {} Uhr",
                WEEKDAYS[ev.start.weekday().num_days_from_monday() as usize],
                ev.start.day(),
                MONTHS[ev.start.month0() as usize],
                ev.start.year(),
                ev.start.format("%H:%M"),
                ev.end.format("%H:%M"),
            ),
            calendar_name: calendar.name,
            calendar_color: calendar.color,
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
                size(px(480.), px(380.)),
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

    fn delete_event(&mut self, id: u32, cx: &mut Context<Self>) {
        self.events.retain(|e| e.id != id);
        if self.selected == Some(id) {
            self.selected = None;
        }
        cx.notify();
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
                    .on_click(cx.listener(|this, _, _, cx| {
                        let now = chrono::Local::now().naive_local();
                        let hour = if this.date == now.date() {
                            (now.hour() + 1).clamp(8, 22)
                        } else {
                            9
                        };
                        this.create_event(this.date, hour, cx);
                        this.open_details(cx);
                    })),
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
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let calendars = self.calendars.iter().enumerate().map(|(i, info)| {
            div()
                .id(("calendar-toggle", i))
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .cursor_pointer()
                .hover(|s| s.bg(cx.theme().list_hover))
                .child(
                    div()
                        .size_3()
                        .rounded_sm()
                        .border_1()
                        .border_color(rgb(info.color))
                        .when(info.visible, |d| d.bg(rgb(info.color))),
                )
                .child(info.name)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.calendars[i].visible = !this.calendars[i].visible;
                    cx.notify();
                }))
        });

        let details = self.selected.and_then(|id| self.events.iter().find(|e| e.id == id)).map(|ev| {
            div()
                .flex()
                .flex_col()
                .gap_2()
                .p_3()
                .rounded_md()
                .border_1()
                .border_color(cx.theme().border)
                .child(div().font_weight(FontWeight::SEMIBOLD).child("Termin"))
                .child(ev.title.clone())
                .child(format!(
                    "{}, {} – {}",
                    ev.start.format("%d.%m.%Y"),
                    ev.start.format("%H:%M"),
                    ev.end.format("%H:%M")
                ))
                .child(format!("Kalender: {}", self.calendars[ev.calendar].name))
                .when(!ev.location.is_empty(), |d| d.child(format!("Ort: {}", ev.location)))
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            Button::new("details")
                                .primary()
                                .label("Details…")
                                .on_click(cx.listener(|this, _, _, cx| this.open_details(cx))),
                        )
                        .child(Button::new("delete").label("Löschen").on_click(cx.listener(
                            |this, _, _, cx| {
                                if let Some(id) = this.selected {
                                    this.delete_event(id, cx);
                                }
                            },
                        ))),
                )
        });

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
                    .child(div().px_2().font_weight(FontWeight::SEMIBOLD).child("Meine Kalender"))
                    .children(calendars),
            )
            .children(details)
    }

    fn render_day_column(&self, day: NaiveDate, today: NaiveDateTime, cx: &mut Context<Self>) -> impl IntoElement {
        let day_events: Vec<&Event> = self
            .events
            .iter()
            .filter(|e| e.start.date() == day && self.calendars[e.calendar].visible)
            .collect();
        let placed = Self::layout_day(&day_events);

        let slots = (0..24u32).map(|hour| {
            div()
                .id(("slot", day.num_days_from_ce() as usize * 24 + hour as usize))
                .h(px(HOUR_H))
                .border_b_1()
                .border_color(cx.theme().border.opacity(0.5))
                .hover(|s| s.bg(cx.theme().list_hover))
                .on_click(cx.listener(move |this, _, _, cx| this.create_event(day, hour, cx)))
        });

        let blocks = placed.into_iter().map(|(i, col, cols)| {
            let ev = day_events[i];
            let minutes = |t: NaiveDateTime| (t.hour() * 60 + t.minute()) as f32;
            let top = minutes(ev.start) / 60.0 * HOUR_H;
            let height = ((minutes(ev.end) - minutes(ev.start)) / 60.0 * HOUR_H).max(20.0);
            let color = rgb(self.calendars[ev.calendar].color);
            let id = ev.id;
            let selected = self.selected == Some(id);

            div()
                .id(("event", id as usize))
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
                        .border_l_4()
                        .border_color(color)
                        .bg(color.opacity(if selected { 0.45 } else { 0.22 }))
                        .text_xs()
                        .cursor_pointer()
                        .child(div().font_weight(FontWeight::SEMIBOLD).child(ev.title.clone()))
                        .child(format!("{}–{}", ev.start.format("%H:%M"), ev.end.format("%H:%M")))
                        .when(!ev.location.is_empty() && height > 52.0, |d| d.child(ev.location.clone())),
                )
                .on_click(cx.listener(move |this, ev: &ClickEvent, _, cx| {
                    this.select_event(Some(id), cx);
                    if ev.click_count() >= 2 {
                        this.open_details(cx);
                    }
                }))
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

struct EventSnapshot {
    id: u32,
    title: String,
    location: String,
    when: String,
    calendar_name: &'static str,
    calendar_color: u32,
}

/// Content of the separate "Termindetails" OS window. Edits are written straight back to the app.
struct EventWindow {
    app: Entity<CalendarApp>,
    event_id: u32,
    title_input: Entity<InputState>,
    location_input: Entity<InputState>,
    when: String,
    calendar_name: &'static str,
    calendar_color: u32,
    _subscriptions: Vec<Subscription>,
}

impl EventWindow {
    /// Takes a snapshot of the event instead of reading `app`: this runs while `app` is
    /// still being updated (the window is opened from one of its click handlers).
    fn new(app: Entity<CalendarApp>, snapshot: EventSnapshot, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let EventSnapshot { id: event_id, title, location, when, calendar_name, calendar_color } = snapshot;

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

        let subscriptions = vec![
            cx.subscribe(&title_input, |this, input, ev: &InputEvent, cx| {
                if let InputEvent::Change = ev {
                    let title = input.read(cx).value().to_string();
                    this.update_event(cx, |event| event.title = title);
                }
            }),
            cx.subscribe(&location_input, |this, input, ev: &InputEvent, cx| {
                if let InputEvent::Change = ev {
                    let location = input.read(cx).value().to_string();
                    this.update_event(cx, |event| event.location = location);
                }
            }),
        ];

        Self {
            app,
            event_id,
            title_input,
            location_input,
            when,
            calendar_name,
            calendar_color,
            _subscriptions: subscriptions,
        }
    }

    fn update_event(&self, cx: &mut Context<Self>, change: impl FnOnce(&mut Event)) {
        let id = self.event_id;
        self.app.update(cx, |app, cx| {
            if let Some(event) = app.events.iter_mut().find(|e| e.id == id) {
                change(event);
            }
            cx.notify();
        });
    }
}

impl Render for EventWindow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let field = |label: &'static str, input: &Entity<InputState>| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_xs().text_color(cx.theme().muted_foreground).child(label))
                .child(Input::new(input))
        };
        let id = self.event_id;
        let app = self.app.clone();

        div()
            .flex()
            .flex_col()
            .size_full()
            .gap_3()
            .p_4()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(field("Titel", &self.title_input))
            .child(field("Ort", &self.location_input))
            .child(self.when.clone())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().size_3().rounded_sm().bg(rgb(self.calendar_color)))
                    .child(self.calendar_name),
            )
            .child(div().flex_1())
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(Button::new("delete").label("Löschen").on_click(
                        move |_, window, cx| {
                            app.update(cx, |app, cx| app.delete_event(id, cx));
                            window.remove_window();
                        },
                    ))
                    .child(
                        Button::new("close")
                            .primary()
                            .label("Schließen")
                            .on_click(|_, window, _| window.remove_window()),
                    ),
            )
    }
}

impl Render for CalendarApp {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
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

fn main() {
    application()
        .with_assets(assets::Assets)
        .run(|cx| {
            init(cx);

            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1280.), px(800.)),
                    cx,
                ))),
                ..Default::default()
            };
            cx.open_window(options, |window, cx| {
                let view = cx.new(|cx| CalendarApp::new(window, cx));
                cx.new(|cx| Root::new(view, window, cx))
            })
                .expect("Failed to open window");
        });
}
