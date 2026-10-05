use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::ActiveTheme;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use crate::api::{self, Session};

pub enum LoginEvent {
    Authenticated(Session),
}

pub struct LoginView {
    server: Entity<InputState>,
    email: Entity<InputState>,
    password: Entity<InputState>,
    invite: Entity<InputState>,
    register: bool,
    busy: bool,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<LoginEvent> for LoginView {}

impl LoginView {
    pub fn new(notice: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (last_server, last_user) = api::last_login().unwrap_or_else(|| (api::DEFAULT_SERVER.into(), String::new()));
        let field = |placeholder: &'static str, value: String, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut state = InputState::new(window, cx).placeholder(placeholder);
                state.set_value(value, window, cx);
                state
            })
        };
        let server = field("Server", last_server, window, cx);
        let email = field("E-Mail-Adresse", last_user, window, cx);
        let invite = field("Einladungscode (falls erforderlich)", String::new(), window, cx);
        let password = cx.new(|cx| InputState::new(window, cx).placeholder("Passwort").masked(true));

        let subscriptions = [&server, &email, &password, &invite]
            .into_iter()
            .map(|input| {
                cx.subscribe_in(input, window, |this, _, ev: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = ev {
                        this.submit(window, cx);
                    }
                })
            })
            .collect();

        Self { server, email, password, invite, register: false, busy: false, error: notice, _subscriptions: subscriptions }
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let server = self.server.read(cx).value().to_string();
        let email = self.email.read(cx).value().trim().to_string();
        let password = self.password.read(cx).value().to_string();
        let invite = self.invite.read(cx).value().trim().to_string();
        let register = self.register;

        let problem = if email.is_empty() {
            Some("Bitte eine E-Mail-Adresse eingeben.")
        } else if password.is_empty() {
            Some("Bitte ein Passwort eingeben.")
        } else if register && password.chars().count() < 10 {
            Some("Das Passwort muss mindestens 10 Zeichen lang sein.")
        } else {
            None
        };
        if let Some(problem) = problem {
            self.error = Some(problem.into());
            cx.notify();
            return;
        }

        self.busy = true;
        self.error = None;
        cx.notify();

        let task = cx.background_executor().spawn(async move {
            let session = api::authenticate(&server, &email, &password, register, &invite)?;
            let kept = api::save(&session);
            Ok::<_, String>((session, kept))
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok((session, _kept_in_keychain)) => {
                        cx.emit(LoginEvent::Authenticated(session));
                    }
                    Err(message) => this.error = Some(message),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn set_register(&mut self, register: bool, cx: &mut Context<Self>) {
        self.register = register;
        self.error = None;
        cx.notify();
    }
}

impl Render for LoginView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let field = |label: &'static str, input: &Entity<InputState>| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_xs().text_color(cx.theme().muted_foreground).child(label))
                .child(Input::new(input))
        };
        let register = self.register;

        div()
            .flex()
            .items_center()
            .justify_center()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .w(px(380.))
                    .p_6()
                    .rounded_lg()
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(div().text_2xl().font_weight(FontWeight::SEMIBOLD).child("Kairo"))
                    .child(
                        div()
                            .text_color(cx.theme().muted_foreground)
                            .child(if register { "Neues Konto erstellen" } else { "Bei deinem Konto anmelden" }),
                    )
                    .child(field("Server", &self.server))
                    .child(field("E-Mail-Adresse", &self.email))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().text_xs().text_color(cx.theme().muted_foreground).child("Passwort"))
                            .child(Input::new(&self.password).mask_toggle()),
                    )
                    .when(register, |d| d.child(field("Einladungscode (optional)", &self.invite)))
                    .when_some(self.error.clone(), |d, message| {
                        d.child(div().text_sm().text_color(rgb(0xc42b1c)).child(message))
                    })
                    .child(
                        Button::new("submit")
                            .primary()
                            .loading(self.busy)
                            .label(if register { "Registrieren" } else { "Anmelden" })
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    )
                    .child(
                        Button::new("toggle-mode")
                            .label(if register { "Ich habe schon ein Konto" } else { "Noch kein Konto? Registrieren" })
                            .on_click(cx.listener(move |this, _, _, cx| this.set_register(!register, cx))),
                    ),
            )
    }
}
