//! The list view: the next two months as a plain list, grouped by day.

use super::*;

/// How many days the list covers.
pub(crate) const AGENDA_DAYS: i64 = 60;

impl CalendarApp {
    /// What happens on `day` in the visible calendars: all-day events first, then by start time.
    pub(crate) fn day_events(&self, day: NaiveDate) -> Vec<(Event, NaiveDate)> {
        let mut events: Vec<(Event, NaiveDate)> = self
            .store
            .events
            .iter()
            .filter(|e| !e.draft && self.calendar(&e.calendar).is_some_and(|c| c.visible))
            .flat_map(|e| e.occurrences_on(day))
            .collect();
        events.sort_by_key(|(e, _)| (!e.is_all_day(), e.start, e.title.clone()));
        events
    }

    pub(crate) fn render_agenda(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let app = cx.entity();
        let today = model::today();
        let muted = cx.theme().muted_foreground;

        let mut sections = Vec::new();
        for (n, day) in self.visible_days().into_iter().enumerate() {
            let events = self.day_events(day);
            if events.is_empty() {
                continue;
            }
            let rows = events.into_iter().enumerate().map(|(i, (ev, origin))| {
                let id = ev.id.clone();
                let color = rgb(self.event_color(&ev));
                let selected = self.is_selected(&id, origin);
                let editable = self.can_edit_event(&id);
                let repeats = ev.repeats();
                let (menu_app, menu_id, select_id) = (app.clone(), id.clone(), id.clone());
                let when = if ev.is_all_day() {
                    "ganztägig".to_string()
                } else {
                    format!("{}–{}", ev.start.format("%H:%M"), ev.end.format("%H:%M"))
                };
                div()
                    .id(("agenda-row", n * 1000 + i))
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().list_hover))
                    .when(selected, |d| d.bg(color.opacity(0.25)))
                    .child(div().flex_none().w(px(110.)).text_sm().text_color(muted).child(when))
                    .child(div().flex_none().w(px(4.)).h(px(18.)).rounded_sm().bg(color))
                    .child(div().flex_none().font_weight(FontWeight::SEMIBOLD).child(ev.title.clone()))
                    .when(!ev.location.is_empty(), |d| d.child(div().truncate().text_sm().text_color(muted).child(ev.location.clone())))
                    .children(ev.categories.iter().map(|c| {
                        let tint = rgb(category_color(c));
                        div()
                            .flex_none()
                            .px_1p5()
                            .rounded_full()
                            .text_xs()
                            .bg(tint.opacity(0.2))
                            .child(c.clone())
                    }))
                    .when(repeats, |d| d.child(div().flex_none().text_xs().text_color(muted).child("↻")))
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
            sections.push(
                div()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .child(
                        div()
                            .px_2()
                            .pt_3()
                            .pb_1()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .when(day == today, |d| d.text_color(cx.theme().primary))
                            .border_b_1()
                            .border_color(cx.theme().border)
                            .child(format!(
                                "{}, {}. {} {}{}",
                                WEEKDAYS[day.weekday().num_days_from_monday() as usize],
                                day.day(),
                                MONTHS[day.month0() as usize],
                                day.year(),
                                if day == today { " · heute" } else { "" }
                            )),
                    )
                    .children(rows),
            );
        }

        let empty = sections.is_empty();
        div()
            .id("agenda")
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .px_4()
            .pb_4()
            .children(sections)
            .when(empty, |d| {
                d.child(div().p_4().text_color(muted).child(format!("Keine Termine in den nächsten {AGENDA_DAYS} Tagen.")))
            })
    }
}
