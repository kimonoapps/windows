//! Signing in: your Kimono's address, your username and password, and a code if your account
//! has an authenticator app. All in the window; no browser.

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::*;
use kimono_core::SignInNext;

use crate::state::Kimono;
use crate::theme::{self, Tone};

pub const DEFAULT_SERVER: &str = "kimonolabs.dev";

pub struct SignIn {
    address: Entity<InputState>,
    username: Entity<InputState>,
    password: Entity<InputState>,
    code: Entity<InputState>,
    asking_code: bool,
    busy: bool,
    problem: Option<SharedString>,
    on_done: Option<Box<dyn FnOnce(&mut Window, &mut App)>>,
}

impl SignIn {
    pub fn new(window: &mut Window, cx: &mut Context<Self>, on_done: impl FnOnce(&mut Window, &mut App) + 'static) -> Self {
        let kept = cx.global::<Kimono>().client.server().map(|s| s.trim_start_matches("https://").to_string());
        let address = cx.new(|cx| InputState::new(window, cx).placeholder("Your Kimono's address").default_value(kept.unwrap_or_else(|| DEFAULT_SERVER.into())));
        let username = cx.new(|cx| InputState::new(window, cx).placeholder("Username"));
        let password = cx.new(|cx| InputState::new(window, cx).placeholder("Password").masked(true));
        let code = cx.new(|cx| InputState::new(window, cx).placeholder("6-digit code"));
        for field in [&address, &username, &password, &code] {
            cx.subscribe_in(field, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event { this.submit(window, cx); }
            }).detach();
        }
        username.update(cx, |s, cx| s.focus(window, cx));
        Self { address, username, password, code, asking_code: false, busy: false, problem: None, on_done: Some(Box::new(on_done)) }
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy { return; }
        self.busy = true;
        self.problem = None;
        cx.notify();
        let kimono = cx.global::<Kimono>();
        let (client, runtime) = (kimono.client.clone(), kimono.runtime.clone());
        let work = if self.asking_code {
            let code = self.code.read(cx).value().to_string();
            runtime.spawn(async move { client.sign_in_code(&code).await })
        } else {
            let (address, username, password) = (self.address.read(cx).value().to_string(), self.username.read(cx).value().to_string(), self.password.read(cx).value().to_string());
            runtime.spawn(async move { client.sign_in(&address, &username, &password).await })
        };
        cx.spawn_in(window, async move |this, cx| {
            let result = work.await.unwrap_or_else(|_| Err(kimono_core::Problem::unreachable()));
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(SignInNext::Code) => {
                        this.asking_code = true;
                        this.code.update(cx, |s, cx| s.focus(window, cx));
                    }
                    Ok(SignInNext::Done) => {
                        if let Some(done) = this.on_done.take() { done(window, cx); }
                    }
                    Err(p) => this.problem = Some(p.message.into()),
                }
                cx.notify();
            });
        }).detach();
    }
}

impl Render for SignIn {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = theme::palette(window);
        let field = |label: &'static str, input: &Entity<InputState>| {
            div().flex().flex_col().gap(px(6.)).child(theme::label(label, &p)).child(Input::new(input))
        };
        let form = if self.asking_code {
            div().flex().flex_col().gap(px(14.))
                .child(div().text_size(px(14.)).text_color(p.on_surface_variant).child("Enter the code from your authenticator app."))
                .child(field("Code", &self.code))
        } else {
            div().flex().flex_col().gap(px(14.))
                .child(field("Kimono", &self.address))
                .child(field("Username", &self.username))
                .child(field("Password", &self.password))
        };
        let action = if self.busy { "Signing in…" } else if self.asking_code { "Continue" } else { "Sign in" };
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(p.background)
            .text_color(p.on_surface)
            .font_family(theme::SANS)
            .p(px(32.))
            .gap(px(24.))
            .child(
                div().flex().flex_col().gap(px(6.))
                    .child(div().font_family(theme::SERIF).text_size(px(28.)).text_color(p.ink).child("Kimono"))
                    .child(div().text_size(px(15.)).text_color(p.on_surface_variant).child("Sign in to keep your Drive on this computer.")),
            )
            .child(form)
            .children(self.problem.clone().map(|m| div().text_size(px(13.)).text_color(p.error).child(m)))
            .child(div().flex_1())
            .child(
                div().flex().justify_end().gap(px(10.))
                    .children(self.asking_code.then(|| theme::seal("back", "Back", Tone::Quiet, !self.busy, &p).on_click(cx.listener(|this, _, _, cx| {
                        this.asking_code = false;
                        let client = cx.global::<Kimono>().client.clone();
                        cx.global::<Kimono>().runtime.spawn(async move { client.cancel_sign_in().await });
                        cx.notify();
                    }))))
                    .child(theme::seal("go", action, Tone::Inked, !self.busy, &p).on_click(cx.listener(|this, _, window, cx| this.submit(window, cx)))),
            )
    }
}
