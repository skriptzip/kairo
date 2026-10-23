//! The day and week views: hour grid, events as blocks, the all-day row and dropping events.

use super::*;

impl CalendarApp {
    /// Assigns each event of one day a column so overlapping events sit side by side.
    pub(crate) fn layout_day(events: &[&Event]) -> Vec<(usize, usize, usize)> {
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

    pub(crate) fn render_day_column(&self, day: NaiveDate, today: NaiveDateTime, cx: &mut Context<Self>) -> impl IntoElement {
        let day_events: Vec<(Event, NaiveDate)> = self
            .store
            .events
            .iter()
            .filter(|e| self.calendar(&e.calendar).is_some_and(|c| c.visible))
            .flat_map(|e| e.occurrences_on(day))
            .filter(|(e, _)| !e.is_all_day())
            .collect();
        let placed = Self::layout_day(&day_events.iter().map(|(e, _)| e).collect::<Vec<_>>());

        let app = cx.entity();
        let can_create = self.store.calendars.iter().any(|c| c.can_edit() && !c.draft);
        let highlight = cx.theme().primary.opacity(0.25);
        let slots = (0..24u32).map(|hour| {
            let marked = self.selected_slot == Some((day, hour));
            let app = app.clone();
            div()
                .id(("slot", day.num_days_from_ce() as usize * 24 + hour as usize))
                .relative()
                .h(px(HOUR_H))
                .border_b_1()
                .border_color(cx.theme().border.opacity(0.5))
                .children((0..4i64).map(|i| {
                    let quarter = i64::from(hour) * 4 + i;
                    div()
                        .id(("quarter", day.num_days_from_ce() as usize * 96 + quarter as usize))
                        .absolute()
                        .left_0()
                        .right_0()
                        .top(px(HOUR_H / 4.0 * i as f32))
                        .h(px(HOUR_H / 4.0))
                        .drag_over::<Drag>(move |style, _, _, _| style.bg(highlight))
                        .on_drop(cx.listener(move |this, drag: &Drag, _, cx| {
                            this.drop_event(drag, day, Some(quarter), cx)
                        }))
                }))
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
                        today_app.update(cx, |app, cx| app.go_to(model::today(), window, cx))
                    }))
                })
        });

        let blocks = placed.into_iter().map(|(i, col, cols)| {
            let (ev, origin) = &day_events[i];
            let (origin, repeats) = (*origin, ev.repeats());
            let minutes = |t: NaiveDateTime| (t.hour() * 60 + t.minute()) as f32;
            let top = minutes(ev.start) / 60.0 * HOUR_H;
            let height = ((minutes(ev.end) - minutes(ev.start)) / 60.0 * HOUR_H).max(20.0);
            let color = rgb(self.event_color(&ev));
            let id = ev.id.clone();
            let selected = self.is_selected(&id, origin);
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
                (_, "elsewhere") => 0.12,
                _ => 0.22,
            };
            let tentative = status == "tentative";
            let elsewhere = status == "elsewhere";
            let (drag_title, drag_color) = (ev.title.clone(), self.event_color(&ev));
            let grab = Rc::new(Cell::new(0i64));
            let (move_id, resize_id, resize_title) = (id.clone(), id.clone(), ev.title.clone());
            div()
                .id(SharedString::from(format!("event-{id}")))
                .occlude()
                .when(editable, |d| {
                    d.on_drag(Drag::Move { id: move_id, grab: grab.clone(), from: origin, days: (day - origin).num_days().max(0) }, move |drag, offset, _, cx| {
                        if let Drag::Move { grab, .. } = drag {
                            grab.set((f32::from(offset.y) / (HOUR_H / 4.0)).floor() as i64);
                        }
                        cx.new(|_| DragPreview { title: drag_title.clone(), color: drag_color })
                    })
                })
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
                        .when(elsewhere, |d| d.border_1())
                        .border_l_4()
                        .border_color(tint)
                        .bg(tint.opacity(alpha))
                        .text_xs()
                        .cursor_pointer()
                        .child(div().font_weight(FontWeight::SEMIBOLD).child(ev.title.clone()))
                        .child(format!("{}–{}", ev.start.format("%H:%M"), ev.end.format("%H:%M")))
                        .when(!ev.location.is_empty() && height > 52.0, |d| d.child(ev.location.clone())),
                )
                .when(editable && ev.end.second() != 59, |d| {
                    d.child(
                        div()
                            .id(SharedString::from(format!("resize-{resize_id}")))
                            .absolute()
                            .bottom_0()
                            .left_0()
                            .right_0()
                            .h(px(6.))
                            .cursor_row_resize()
                            .on_drag(Drag::Resize { id: resize_id.clone(), from: origin }, move |_, _, _, cx| {
                                cx.new(|_| DragPreview { title: resize_title.clone(), color: drag_color })
                            }),
                    )
                })
                .on_click(cx.listener(move |this, ev: &ClickEvent, _, cx| {
                    this.select_occurrence(id.clone(), origin, cx);
                    if ev.click_count() >= 2 {
                        this.open_details(cx);
                    }
                }))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, _, _, cx| this.select_occurrence(select_id.clone(), origin, cx)),
                )
                .context_menu(move |menu, _, _| event_menu(menu, &menu_app, &menu_id, origin, repeats, editable))
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

        // Like Outlook: an all-day "Abwesend" / "An anderem Ort tätig" shades the whole column.
        let shade = self.all_day_events(day).iter().find_map(|(e, _)| match e.status.as_str() {
            "oof" => Some(rgb(0xb146c2)),
            "elsewhere" => Some(rgb(self.event_color(e))),
            _ => None,
        });

        div()
            .relative()
            .flex_1()
            .min_w_0()
            .border_l_1()
            .border_color(cx.theme().border)
            .when_some(shade, |d, color| {
                // Diagonal stripes over a faint tint, like Outlook's hatching.
                d.bg(color.opacity(0.08)).child(div().absolute().size_full().bg(pattern_slash(color.opacity(0.22), 1.5, 9.0)))
            })
            .child(div().flex().flex_col().children(slots))
            .children(blocks)
            .children(now_line)
    }

    pub(crate) fn all_day_events(&self, day: NaiveDate) -> Vec<(Event, NaiveDate)> {
        self.store
            .events
            .iter()
            .filter(|e| self.calendar(&e.calendar).is_some_and(|c| c.visible))
            .flat_map(|e| e.occurrences_on(day))
            .filter(|(e, _)| e.is_all_day())
            .collect()
    }

    /// One day's cell of the all-day row: a chip per all-day event.
    pub(crate) fn render_all_day_cell(&self, day: NaiveDate, cx: &mut Context<Self>) -> impl IntoElement {
        let app = cx.entity();
        let visible = self.visible_days();
        let (first_visible, last_visible) = (visible[0], visible[visible.len() - 1]);
        let chips = self.all_day_events(day).into_iter().map(|(ev, origin)| {
            let (id, repeats) = (ev.id.clone(), ev.repeats());
            let color = rgb(self.event_color(&ev));
            let selected = self.is_selected(&id, origin);
            let editable = self.can_edit_event(&id);
            let (menu_app, menu_id, select_id, move_id) = (app.clone(), id.clone(), id.clone(), id.clone());
            let (drag_title, drag_color) = (ev.title.clone(), self.event_color(&ev));
            let alpha = match (selected, ev.status.as_str()) {
                (true, _) => 0.45,
                (_, "free") => 0.10,
                _ => 0.25,
            };
            // A longer event is one bar over several days: round only at its ends.
            let starts_here = ev.start.date() == day;
            let (first, last) = (starts_here || day == first_visible, ev.last_day() == day || day == last_visible);
            div()
                .id(SharedString::from(format!("all-day-{id}-{day}")))
                .h(px(20.))
                .px_1p5()
                .py_0p5()
                .when(first, |d| d.ml_0p5())
                .when(last, |d| d.mr_0p5())
                .when(first && last, |d| d.rounded_sm())
                .when(first && !last, |d| d.rounded_l_sm())
                .when(!first && last, |d| d.rounded_r_sm())
                .when(starts_here, |d| d.border_l_4())
                .border_color(color)
                .bg(color.opacity(alpha))
                .text_xs()
                .truncate()
                .cursor_pointer()
                .font_weight(FontWeight::SEMIBOLD)
                .child(if first { ev.title.clone() } else { String::new() })
                .when(editable, |d| {
                    d.on_drag(Drag::Move { id: move_id, grab: Rc::new(Cell::new(0)), from: origin, days: (day - origin).num_days().max(0) }, move |_, _, _, cx| {
                        cx.new(|_| DragPreview { title: drag_title.clone(), color: drag_color })
                    })
                })
                .on_click(cx.listener(move |this, ev: &ClickEvent, _, cx| {
                    this.select_occurrence(id.clone(), origin, cx);
                    if ev.click_count() >= 2 {
                        this.open_details(cx);
                    }
                }))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, _, _, cx| this.select_occurrence(select_id.clone(), origin, cx)),
                )
                .context_menu(move |menu, _, _| event_menu(menu, &menu_app, &menu_id, origin, repeats, editable))
        });
        let highlight = cx.theme().primary.opacity(0.25);
        div()
            .id(("all-day-cell", day.num_days_from_ce() as usize))
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h(px(24.))
            .gap_0p5()
            .py_0p5()
            .border_l_1()
            .border_color(cx.theme().border)
            .drag_over::<Drag>(move |style, _, _, _| style.bg(highlight))
            .on_drop(cx.listener(move |this, drag: &Drag, _, cx| this.drop_event(drag, day, None, cx)))
            .children(chips)
    }

    pub(crate) fn render_grid(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let days = self.visible_days();
        let now = model::now();

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

        let all_day = days.iter().map(|&day| self.render_all_day_cell(day, cx).into_any_element()).collect::<Vec<_>>();
        let has_all_day = days.iter().any(|&day| !self.all_day_events(day).is_empty());

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
            .when(has_all_day, |d| {
                d.child(
                    div()
                        .flex()
                        .flex_none()
                        .border_b_1()
                        .border_color(cx.theme().border)
                        .child(div().flex_none().w(px(GUTTER_W)))
                        .children(all_day),
                )
            })
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

    /// Applies a drop onto `day`: `quarter` is the quarter hour of the day (0-95), `None` the
    /// all-day row. For a series only the dragged repetition changes.
    pub(crate) fn drop_event(&mut self, drag: &Drag, day: NaiveDate, quarter: Option<i64>, cx: &mut Context<Self>) {
        let (id, origin, grabbed_days) = match drag {
            Drag::Move { id, from, days, .. } => (id.clone(), *from, *days),
            Drag::Resize { id, from } => (id.clone(), *from, 0),
        };
        let day = day - Duration::days(grabbed_days);
        if !self.can_edit_event(&id) {
            return;
        }
        let Some(event) = self.store.events.iter_mut().find(|e| e.id == id) else { return };
        let current = event.occurrence(origin);
        // The grid shows event times in Kairo's zone; `current` is in the event's own zone.
        let in_other_zone = event.tz != model::display_tz();
        let foreign = in_other_zone && !current.is_all_day();
        let (cur_start, cur_end) =
            if foreign { (event.to_display(current.start), event.to_display(current.end)) } else { (current.start, current.end) };
        let day_start = |day: NaiveDate| day.and_time(NaiveTime::MIN);
        let last_minute = |day: NaiveDate| day.and_hms_opt(23, 59, 0).unwrap();
        let (start, end) = match (drag, quarter) {
            (Drag::Move { .. }, None) => {
                let span = Duration::days(current.extra_days());
                (day_start(day), last_minute(day) + span)
            }
            (Drag::Move { grab, .. }, Some(quarter)) => {
                let duration = if current.is_all_day() { Duration::hours(1) } else { cur_end - cur_start };
                let start = day_start(day) + Duration::minutes(((quarter - grab.get()).max(0) * 15).min(95 * 15));
                if current.extra_days() > 0 { (start, start + duration) } else { (start, (start + duration).min(last_minute(day)).max(start)) }
            }
            (Drag::Resize { .. }, Some(quarter)) => {
                let date = cur_end.date();
                let end = (day_start(date) + Duration::minutes((quarter + 1) * 15)).min(last_minute(date));
                (cur_start, end.max(cur_start + Duration::minutes(15)).min(last_minute(date)))
            }
            (Drag::Resize { .. }, None) => return,
        };
        // Timed results are in Kairo's zone: back to the event's.
        let (start, end) =
            if in_other_zone && quarter.is_some() { (event.from_display(start), event.from_display(end)) } else { (start, end) };
        if (start, end) == (current.start, current.end) {
            return;
        }
        if event.repeats() {
            event.change_occurrence(origin, start, end, &current.title, &current.location);
        } else {
            event.start = start;
            event.end = end;
        }
        event.dirty = true;
        self.mark_changed(cx);
    }

}
