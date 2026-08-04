//! The "Gmail" settings tab: address + app-password fields. Changes apply live
//! — the config blob is written and the password stored in the secret store
//! when a field loses focus (so the password is only persisted on blur, not
//! mid-typing). A helper button opens Google's app-passwords page, since that's
//! the one piece of setup that has to happen in a browser.

use gpui::prelude::*;
use gpui::{div, Context, Entity, FocusHandle, Subscription, Window};

use spotlight_config::AppConfig;
use spotlight_ui::text_input::TextInput;
use spotlight_ui::{controls, theme};

use crate::GmailConfig;

const APP_PASSWORDS_URL: &str = "https://myaccount.google.com/apppasswords";

pub struct GmailSettingsTab {
    email: Entity<TextInput>,
    password: Entity<TextInput>,
    help_focus: FocusHandle,
    /// Blur subscriptions for the text fields.
    subs: Vec<Subscription>,
}

impl GmailSettingsTab {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let cfg = crate::load_config();

        let email = cx.new(|cx| {
            let mut t = TextInput::new(cx, "you@gmail.com", false);
            t.set_value(cfg.email.clone());
            t
        });
        let password = cx.new(|cx| {
            let mut t = TextInput::new(cx, "App password", true);
            if let Some(pw) = spotlight_config::load_secret(crate::PASSWORD_KEY) {
                t.set_value(pw);
            }
            t
        });

        Self {
            email,
            password,
            help_focus: cx.focus_handle(),
            subs: Vec::new(),
        }
    }

    /// Write the config blob (address only — no secrets).
    fn persist(&mut self, cx: &mut Context<Self>) {
        let email = self.email.read(cx).value().trim().to_string();
        let mut app = AppConfig::load();
        let _ = app.set(crate::EXT_ID, &GmailConfig { email });
        let _ = app.save();
        cx.notify();
    }

    /// Store the app password if set (non-empty only, so an untouched masked
    /// field never wipes the stored password). Called on blur.
    fn save_secrets(&mut self, cx: &Context<Self>) {
        let pw = self.password.read(cx).value().to_string();
        if !pw.trim().is_empty() {
            let _ = spotlight_config::save_secret(crate::PASSWORD_KEY, &pw);
        }
    }

    /// Subscribe blur handlers for the two text fields (once).
    fn ensure_blur_subs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.subs.is_empty() {
            return;
        }
        for h in [
            self.email.read(cx).focus_handle().clone(),
            self.password.read(cx).focus_handle().clone(),
        ] {
            let sub = cx.on_focus_out(&h, window, |this, _ev, _win, cx| {
                this.save_secrets(cx);
                this.persist(cx);
            });
            self.subs.push(sub);
        }
    }

    fn field(label: &str, input: &Entity<TextInput>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .pb_3()
            .child(div().text_xs().text_color(theme::muted()).child(label.to_string()))
            .child(input.clone())
    }
}

impl Render for GmailSettingsTab {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_blur_subs(window, cx);

        let account = div()
            .flex()
            .flex_col()
            .child(Self::field("Gmail address", &self.email))
            .child(Self::field("App password", &self.password))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            // min-width:0 lets the hint shrink below its
                            // one-line width and wrap instead of pushing the
                            // button out of the card.
                            .min_w(gpui::px(0.))
                            .text_xs()
                            .text_color(theme::muted())
                            .child("Your normal password won't work — Gmail needs an app password (requires 2-Step Verification)."),
                    )
                    .child(div().flex_none().child(controls::button(
                        &self.help_focus,
                        "Create app password ↗",
                        cx,
                        |_, _, _| {
                            let _ = std::process::Command::new("/usr/bin/open")
                                .arg(APP_PASSWORDS_URL)
                                .spawn();
                        },
                    ))),
            );

        div()
            .id("gmail-settings")
            .flex()
            .flex_col()
            .size_full()
            .overflow_y_scroll()
            .child(controls::section("Account", account))
    }
}
