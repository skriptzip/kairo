//! "Kalender verwalten": rename, recolor, delete or leave a calendar, and share it with others.
//!
//! Name and color are a draft that only "Speichern" applies. Sharing actions (inviting,
//! changing a role, removing somebody) talk to the server right away, since they are not
//! part of the calendar's own data.

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::{ActiveTheme, Disableable, IndexPath, Sizable};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use crate::{
    CalendarApp, PALETTE,
    api::Session,
    members::{self, MemberList, role_label},
    model::parse_color,
};

pub struct CalendarSnapshot {
    pub id: String,
    pub name: String,
    pub color: String,
    pub owner: String,
    pub is_owner: bool,
    /// Never saved yet, so it doesn't exist on the server and can't be shared.
    pub is_draft: bool,
}

/// What the owner can hand out, in dropdown order: (stored role, label).
const ROLES: [(&str, &str); 2] = [("editor", "Bearbeiten"), ("viewer", "Ansehen")];

pub struct CalendarWindow {
    app: Entity<CalendarApp>,
    session: Session,
    info: CalendarSnapshot,
    color: String,
    name_input: Entity<InputState>,
    invite_input: Entity<InputState>,
    role_select: Entity<SelectState<Vec<String>>>,
    role_index: usize,
    members: Option<MemberList>,
    busy: bool,
    confirm_delete: bool,
    message: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl CalendarWindow {
    pub fn new(
        app: Entity<CalendarApp>,
        session: Session,
        info: CalendarSnapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name_input = cx.new(|cx| {
            let mut state = InputState::new(window, cx).placeholder("Name des Kalenders");
            state.set_value(info.name.clone(), window, cx);
            state
        });
        let invite_input = cx.new(|cx| InputState::new(window, cx).placeholder("E-Mail-Adresse einladen"));
        let role_select = cx.new(|cx| {
            let labels: Vec<String> = ROLES.iter().map(|(_, label)| label.to_string()).collect();
            SelectState::new(labels, Some(IndexPath::new(0)), window, cx)
        });

        let (release_app, release_id) = (app.clone(), info.id.clone());
        let subscriptions = vec![
            cx.subscribe(&role_select, |this, _, ev: &SelectEvent<Vec<String>>, cx| {
                if let SelectEvent::Confirm(Some(label)) = ev {
                    if let Some(index) = ROLES.iter().position(|(_, l)| l == label) {
                        this.role_index = index;
                        cx.notify();
                    }
                }
            }),
            cx.subscribe_in(&invite_input, window, |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.invite(window, cx);
                }
            }),
            // A new calendar that was never saved disappears again, however the window closes.
            cx.on_release(move |_, cx| {
                let (app, id) = (release_app.clone(), release_id.clone());
                cx.defer(move |cx| app.update(cx, |app, cx| app.discard_calendar_draft(&id, cx)));
            }),
        ];

        let mut this = Self {
            app,
            session,
            color: info.color.clone(),
            info,
            name_input,
            invite_input,
            role_select,
            role_index: 0,
            members: None,
            busy: false,
            confirm_delete: false,
            message: None,
            _subscriptions: subscriptions,
        };
        this.reload_members(window, cx);
        this
    }

    fn reload_members(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.info.is_draft {
            return;
        }
        let (session, id) = (self.session.clone(), self.info.id.clone());
        let task = cx.background_executor().spawn(async move { members::list(&session, &id) });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(list) => this.members = Some(list),
                    Err(message) => this.message = Some(message),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Runs one sharing request in the background, then refreshes the member list.
    fn run(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        request: impl FnOnce(&Session, &str) -> Result<(), String> + Send + 'static,
    ) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.message = None;
        cx.notify();
        let (session, id) = (self.session.clone(), self.info.id.clone());
        let task = cx.background_executor().spawn(async move { request(&session, &id) });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                if let Err(message) = result {
                    this.message = Some(message);
                }
                this.reload_members(window, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn invite(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let email = self.invite_input.read(cx).value().trim().to_string();
        if email.is_empty() {
            return;
        }
        let role = ROLES[self.role_index].0;
        self.invite_input.update(cx, |input, cx| input.set_value("", window, cx));
        self.run(window, cx, move |session, id| members::invite(session, id, &email, role));
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.name_input.read(cx).value().trim().to_string();
        if name.is_empty() {
            self.message = Some("Bitte einen Namen eingeben.".into());
            cx.notify();
            return;
        }
        let (id, color) = (self.info.id.clone(), self.color.clone());
        self.app.update(cx, |app, cx| app.save_calendar(&id, name, color, cx));
        window.remove_window();
    }

    /// Deletes (owner) or leaves (member) after a second click.
    fn remove(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.confirm_delete && !self.info.is_draft {
            self.confirm_delete = true;
            cx.notify();
            return;
        }
        let id = self.info.id.clone();
        self.app.update(cx, |app, cx| app.remove_calendar(&id, cx));
        window.remove_window();
    }

    fn render_members(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        if self.info.is_draft {
            let hint = "Speichere den Kalender zuerst, dann kannst du Leute einladen.";
            return div().text_sm().text_color(muted).child(hint).into_any_element();
        }
        let Some(list) = &self.members else {
            return div().text_sm().text_color(muted).child("Lade Mitglieder …").into_any_element();
        };

        let owner_row = div()
            .flex()
            .items_center()
            .gap_2()
            .child(div().flex_1().child(list.owner.clone()))
            .child(div().text_sm().text_color(muted).child(role_label("owner")));
        let rows = list.members.iter().enumerate().map(|(i, member)| {
            let (name, role) = (member.email.clone(), member.role.clone());
            let other = if role == "editor" { "viewer" } else { "editor" };
            let (toggle_name, remove_name) = (name.clone(), name.clone());
            let status = if member.accepted {
                role_label(&role).to_string()
            } else {
                format!("{} · Einladung offen", role_label(&role))
            };
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(div().flex_1().child(name))
                .child(div().text_sm().text_color(muted).child(status))
                .when(self.info.is_owner, |row| {
                    row.child(
                        Button::new(("role", i))
                            .small()
                            .label(if role == "editor" { "→ Ansehen" } else { "→ Bearbeiten" })
                            .disabled(self.busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let name = toggle_name.clone();
                                this.run(window, cx, move |s, id| members::invite(s, id, &name, other));
                            })),
                    )
                    .child(
                        Button::new(("remove", i))
                            .small()
                            .label("Entfernen")
                            .disabled(self.busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let name = remove_name.clone();
                                this.run(window, cx, move |s, id| members::remove(s, id, &name));
                            })),
                    )
                })
        });
        div().flex().flex_col().gap_2().child(owner_row).children(rows).into_any_element()
    }
}

impl Render for CalendarWindow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let section = |title: &'static str| {
            div().text_xs().text_color(muted).child(title)
        };
        let is_owner = self.info.is_owner;

        let swatches = PALETTE.iter().enumerate().map(|(i, hex)| {
            let hex = hex.to_string();
            let selected = self.color.eq_ignore_ascii_case(&hex);
            let pick = hex.clone();
            div()
                .id(("swatch", i))
                .size_6()
                .rounded_md()
                .cursor_pointer()
                .bg(rgb(parse_color(&hex)))
                .border_2()
                .border_color(if selected { cx.theme().foreground } else { cx.theme().background })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.color = pick.clone();
                    cx.notify();
                }))
        });

        let danger_label = match (is_owner, self.confirm_delete) {
            (true, false) => "Kalender löschen",
            (true, true) => "Wirklich löschen?",
            (false, false) => "Kalender verlassen",
            (false, true) => "Wirklich verlassen?",
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .gap_3()
            .p_4()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(section("Name"))
                    .child(if is_owner {
                        Input::new(&self.name_input).into_any_element()
                    } else {
                        div()
                            .child(format!("{} (von {})", self.info.name, self.info.owner))
                            .into_any_element()
                    }),
            )
            .when(is_owner, |d| {
                d.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(section("Farbe"))
                        .child(div().flex().gap_2().children(swatches)),
                )
            })
            .child(div().flex().flex_col().gap_2().child(section("Freigabe")).child(self.render_members(cx)))
            .when(is_owner && !self.info.is_draft, |d| {
                d.child(
                    div()
                        .flex()
                        .gap_2()
                        .items_center()
                        .child(div().flex_1().child(Input::new(&self.invite_input)))
                        .child(div().w(px(130.)).child(Select::new(&self.role_select)))
                        .child(
                            Button::new("invite")
                                .primary()
                                .label("Einladen")
                                .disabled(self.busy)
                                .on_click(cx.listener(|this, _, window, cx| this.invite(window, cx))),
                        ),
                )
            })
            .when_some(self.message.clone(), |d, message| {
                d.child(div().text_sm().text_color(rgb(0xc42b1c)).child(message))
            })
            .child(div().flex_1())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("remove-calendar")
                            .danger()
                            .label(danger_label)
                            .on_click(cx.listener(|this, _, window, cx| this.remove(window, cx))),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("close")
                            .label(if is_owner { "Abbrechen" } else { "Schließen" })
                            .on_click(|_, window, _| window.remove_window()),
                    )
                    .when(is_owner, |d| {
                        d.child(
                            Button::new("save")
                                .primary()
                                .label("Speichern")
                                .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
                        )
                    }),
            )
    }
}
