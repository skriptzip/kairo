//! Small pieces shared by the calendar views: the event context menu and dragging.

use super::*;

/// Context menu of an event or one repetition of a series.
pub(crate) fn event_menu(
    menu: PopupMenu,
    app: &Entity<CalendarApp>,
    id: &str,
    origin: NaiveDate,
    repeats: bool,
    editable: bool,
) -> PopupMenu {
    let (open_app, series_app, copy_app, delete_app, series_delete_app) =
        (app.clone(), app.clone(), app.clone(), app.clone(), app.clone());
    let (open_id, series_id, copy_id, delete_id, series_delete_id) =
        (id.to_string(), id.to_string(), id.to_string(), id.to_string(), id.to_string());
    let menu = menu.item(
        PopupMenuItem::new(if repeats { "Diesen Termin öffnen  (Enter)" } else { "Öffnen  (Enter)" }).on_click(
            move |_, _, cx| {
                open_app.update(cx, |app, cx| {
                    app.select_occurrence(open_id.clone(), origin, cx);
                    app.open_details(cx);
                })
            },
        ),
    );
    let menu = if repeats {
        menu.item(PopupMenuItem::new("Ganze Serie öffnen").on_click(move |_, _, cx| {
            series_app.update(cx, |app, cx| {
                app.select_occurrence(series_id.clone(), origin, cx);
                app.open_series(cx);
            })
        }))
    } else {
        menu
    };
    let menu = menu
        .item(PopupMenuItem::new("Duplizieren  (Strg+D)").disabled(!editable).on_click(move |_, _, cx| {
            copy_app.update(cx, |app, cx| app.duplicate_event(&copy_id, cx))
        }))
        .separator();
    if repeats {
        menu.item(PopupMenuItem::new("Diesen Termin löschen  (Entf)").disabled(!editable).on_click(
            move |_, _, cx| delete_app.update(cx, |app, cx| app.delete_occurrence(&delete_id, origin, cx)),
        ))
        .item(PopupMenuItem::new("Ganze Serie löschen").disabled(!editable).on_click(move |_, _, cx| {
            series_delete_app.update(cx, |app, cx| app.delete_event(&series_delete_id, cx))
        }))
    } else {
        menu.item(PopupMenuItem::new("Löschen  (Entf)").disabled(!editable).on_click(move |_, _, cx| {
            delete_app.update(cx, |app, cx| app.delete_event(&delete_id, cx))
        }))
    }
}

/// What is being dragged around in the grid.
#[derive(Clone)]
pub(crate) enum Drag {
    /// Moves an event; `grab` is how many quarter hours below the event's top it was picked up.
    /// `days`: how many days after the event's first day the grabbed piece lies.
    Move { id: String, grab: Rc<Cell<i64>>, from: NaiveDate, days: i64 },
    /// Drags the bottom edge of an event.
    Resize { id: String, from: NaiveDate },
}

/// The small label that follows the mouse while dragging.
pub(crate) struct DragPreview {
    pub(crate) title: String,
    pub(crate) color: u32,
}

impl Render for DragPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let color = rgb(self.color);
        div()
            .px_2()
            .py_1()
            .rounded_sm()
            .border_l_4()
            .border_color(color)
            .bg(color.opacity(0.45))
            .text_xs()
            .text_color(cx.theme().foreground)
            .child(self.title.clone())
    }
}
