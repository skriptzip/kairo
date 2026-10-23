//! The sidebar: mini calendar, search, calendar list and invitations.

use super::*;

/// One search result: an event (for a series its next repetition).
struct Hit {
    id: String,
    title: String,
    /// The day shown in the grid and the day the repetition regularly belongs to.
    date: NaiveDate,
    origin: NaiveDate,
    color: u32,
}

impl CalendarApp {
    /// Events matching the search text in title, place or notes: upcoming ones first (nearest
    /// first), then past ones (latest first).
    fn search_hits(&self) -> Vec<Hit> {
        let query = self.search.trim().to_lowercase();
        if query.is_empty() {
            return Vec::new();
        }
        let today = model::today();
        let mut hits: Vec<Hit> = self
            .store
            .events
            .iter()
            .filter(|e| !e.draft && self.calendar(&e.calendar).is_some())
            .filter(|e| {
                [&e.title, &e.location, &e.description].iter().any(|text| text.to_lowercase().contains(&query))
                    || e.categories.iter().any(|c| c.to_lowercase().contains(&query))
            })
            .map(|e| {
                // A series is listed once, at its next repetition (or its last one).
                let next = e.repeats().then(|| {
                    (0..400).find_map(|d| {
                        let day = today + Duration::days(d);
                        e.starting_on(day).into_iter().next().map(|(occurrence, origin)| (occurrence.start.date(), origin))
                    })
                });
                let (date, origin) = match next.flatten() {
                    Some(found) => found,
                    None => {
                        let last = e.repeat_until.unwrap_or(e.start.date());
                        let day = if e.repeats() { last } else { e.start.date() };
                        (day, day)
                    }
                };
                Hit { id: e.id.clone(), title: e.title.clone(), date, origin, color: self.event_color(e) }
            })
            .collect();
        hits.sort_by_key(|h| (h.date < today, if h.date >= today { h.date.num_days_from_ce() } else { -h.date.num_days_from_ce() }));
        hits
    }

    pub(crate) fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
                    let (export_app, export_id) = (menu_app.clone(), menu_id.clone());
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
                    .item(PopupMenuItem::new("Als .ics exportieren …").on_click(move |_, _, cx| {
                        export_app.update(cx, |app, cx| app.export_calendar(&export_id, cx))
                    }))
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
        let searching = !self.search.trim().is_empty();
        let hits: Vec<AnyElement> = self
            .search_hits()
            .into_iter()
            .take(60)
            .enumerate()
            .map(|(i, hit)| {
                let Hit { id, title, date, origin, color } = hit;
                div()
                    .id(("hit", i))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().list_hover))
                    .child(div().flex_none().size_2().rounded_full().bg(rgb(color)))
                    .child(div().flex_none().text_xs().text_color(muted).child(format!(
                        "{} {}",
                        WEEKDAYS[date.weekday().num_days_from_monday() as usize],
                        date.format("%d.%m.%y")
                    )))
                    .child(div().truncate().child(title))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.go_to(date, window, cx);
                        this.select_occurrence(id.clone(), origin, cx);
                    }))
                    .into_any_element()
            })
            .collect();

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
                // Clicks in here must not hand the focus back to the grid.
                div()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(Input::new(&self.search_input)),
            )
            .when(searching, |d| {
                d.child(
                    div().flex().flex_col().id("search-results").overflow_y_scroll().min_h_0().children(
                        if hits.is_empty() {
                            vec![div().px_2().text_sm().text_color(muted).child("Keine Treffer").into_any_element()]
                        } else {
                            hits
                        },
                    ),
                )
            })
            .when(!searching, |d| {
                d.child(
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .child(div().flex_1().px_2().font_weight(FontWeight::SEMIBOLD).child("Meine Kalender"))
                            .child(
                                Button::new("import-pst")
                                    .ghost()
                                    .xsmall()
                                    .label("Import")
                                    .on_click(cx.listener(|this, _, _, cx| this.import_pst(cx))),
                            )
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
            })
            .when(has_invites && !searching, |d| {
                d.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(div().px_2().font_weight(FontWeight::SEMIBOLD).child("Einladungen"))
                        .children(invites),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().px_2().text_xs().text_color(muted).child("Zeitzone der Anzeige"))
                    .child(Select::new(&self.zone_select)),
            )
            .when_some(self.import_note.clone(), |d, message| {
                d.child(div().px_2().text_sm().text_color(muted).child(message))
            })
            .when_some(self.notice.clone(), |d, message| {
                d.child(div().px_2().text_sm().text_color(rgb(0xc42b1c)).child(message))
            })
    }

}
