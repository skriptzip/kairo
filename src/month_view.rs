//! The month view.

use super::*;

impl CalendarApp {
    /// The month view: six weeks with the events of every day as short lines.
    pub(crate) fn render_month(&self, cx: &mut Context<Self>) -> impl IntoElement {
        const MAX_LINES: usize = 4;
        let app = cx.entity();
        let days = self.visible_days();
        let today = model::today();
        let muted = cx.theme().muted_foreground;
        let highlight = cx.theme().primary.opacity(0.25);

        let header = (0..7).map(|i| {
            div()
                .flex_1()
                .min_w_0()
                .py_1()
                .text_xs()
                .text_center()
                .text_color(muted)
                .border_l_1()
                .border_color(cx.theme().border)
                .child(WEEKDAYS[i])
        });

        let weeks = days.chunks(7).map(|week| {
            let cells = week.iter().map(|&day| {
                let mut events: Vec<(Event, NaiveDate)> = self
                    .store
                    .events
                    .iter()
                    .filter(|e| self.calendar(&e.calendar).is_some_and(|c| c.visible))
                    .flat_map(|e| e.occurrences_on(day))
                    .collect();
                events.sort_by_key(|(e, _)| (!e.is_all_day(), e.start));
                let more = events.len().saturating_sub(MAX_LINES);

                let lines = events.into_iter().take(MAX_LINES).map(|(ev, origin)| {
                    let id = ev.id.clone();
                    let color = rgb(self.event_color(&ev));
                    let selected = self.is_selected(&id, origin);
                    let editable = self.can_edit_event(&id);
                    let repeats = ev.repeats();
                    let (menu_app, menu_id, select_id, move_id) = (app.clone(), id.clone(), id.clone(), id.clone());
                    let (drag_title, drag_color) = (ev.title.clone(), self.event_color(&ev));
                    let label = if ev.is_all_day() { ev.title.clone() } else { format!("{} {}", ev.start.format("%H:%M"), ev.title) };
                    div()
                        .id(SharedString::from(format!("month-{id}-{}", ev.start)))
                        .occlude()
                        .px_1()
                        .rounded_sm()
                        .border_l_2()
                        .border_color(color)
                        .bg(color.opacity(if selected { 0.45 } else if ev.status == "free" { 0.08 } else { 0.22 }))
                        .text_xs()
                        .truncate()
                        .cursor_pointer()
                        .child(label)
                        .when(editable, |d| {
                            d.on_drag(
                                Drag::Move { id: move_id, grab: Rc::new(Cell::new(0)), from: origin, days: (day - origin).num_days().max(0) },
                                move |_, _, _, cx| cx.new(|_| DragPreview { title: drag_title.clone(), color: drag_color }),
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

                let other_month = day.month() != self.date.month();
                div()
                    .id(("month-day", day.num_days_from_ce() as usize))
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .p_0p5()
                    .overflow_hidden()
                    .border_l_1()
                    .border_color(cx.theme().border)
                    .when(other_month, |d| d.bg(cx.theme().muted.opacity(0.35)))
                    .drag_over::<Drag>(move |style, _, _, _| style.bg(highlight))
                    .on_drop(cx.listener(move |this, drag: &Drag, _, cx| this.drop_on_month_day(drag, day, cx)))
                    .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                        this.go_to(day, window, cx);
                        this.selected = None;
                        if ev.click_count() >= 2 {
                            this.create_and_open(day, 9, cx);
                        }
                    }))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .child(
                                div()
                                    .px_1p5()
                                    .rounded_full()
                                    .text_xs()
                                    .when(day == today, |d| d.bg(cx.theme().primary).text_color(white()))
                                    .when(other_month && day != today, |d| d.text_color(muted))
                                    .child(if day.day() == 1 {
                                        format!("{}. {}", day.day(), &MONTHS[day.month0() as usize][..3])
                                    } else {
                                        day.day().to_string()
                                    }),
                            ),
                    )
                    .children(lines)
                    .when(more > 0, |d| d.child(div().px_1().text_xs().text_color(muted).child(format!("+{more} weitere"))))
            });
            div().flex().flex_1().min_h_0().border_t_1().border_color(cx.theme().border).children(cells)
        });

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(div().flex().child(div().flex_none().w(px(0.))).children(header))
            .children(weeks)
    }

    /// Applies a drop onto a day of the month view: the event keeps its time of day.
    pub(crate) fn drop_on_month_day(&mut self, drag: &Drag, day: NaiveDate, cx: &mut Context<Self>) {
        let Drag::Move { id, from, days, .. } = drag else { return };
        let day = day - Duration::days(*days);
        if !self.can_edit_event(id) {
            return;
        }
        let Some(event) = self.store.events.iter_mut().find(|e| &e.id == id) else { return };
        let current = event.occurrence(*from);
        let foreign = event.tz != model::display_tz() && !current.is_all_day();
        let shown = if foreign { event.to_display(current.start) } else { current.start };
        let target = day.and_time(shown.time());
        let start = if foreign { event.from_display(target) } else { target };
        let end = start + (current.end - current.start);
        if start == current.start {
            return;
        }
        if event.repeats() {
            event.change_occurrence(*from, start, end, &current.title, &current.location);
        } else {
            event.start = start;
            event.end = end;
        }
        event.dirty = true;
        self.mark_changed(cx);
    }

}
