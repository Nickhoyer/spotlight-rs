//! The "Music" settings tab: ampm server URL + bearer token, persisted on blur
//! (mirrors the Gmail tab), plus a connection-test button hitting `/health`.

use gpui::prelude::*;
use gpui::{div, Context, Entity, FocusHandle, Subscription, Window};

use spotlight_config::AppConfig;
use spotlight_ui::text_input::TextInput;
use spotlight_ui::{controls, theme};

use crate::MusicConfig;

pub struct MusicSettingsTab {
    server_url: Entity<TextInput>,
    token: Entity<TextInput>,
    test_focus: FocusHandle,
    test_result: Option<String>,
    testing: bool,
    subs: Vec<Subscription>,
}

impl MusicSettingsTab {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let cfg = crate::load_config();
        let server_url = cx.new(|cx| {
            let mut t = TextInput::new(cx, "http://127.0.0.1:8787", false);
            t.set_value(cfg.server_url.clone());
            t
        });
        let token = cx.new(|cx| {
            let mut t = TextInput::new(cx, "serve token", true);
            if let Some(tok) = spotlight_config::load_secret(crate::TOKEN_KEY) {
                t.set_value(tok);
            }
            t
        });
        Self {
            server_url,
            token,
            test_focus: cx.focus_handle(),
            test_result: None,
            testing: false,
            subs: Vec::new(),
        }
    }

    fn persist(&mut self, cx: &mut Context<Self>) {
        let server_url = self.server_url.read(cx).value().trim().to_string();
        let mut app = AppConfig::load();
        let _ = app.set(crate::EXT_ID, &MusicConfig { server_url });
        let _ = app.save();
        // Non-empty only, so an untouched masked field never wipes the token.
        let tok = self.token.read(cx).value().trim().to_string();
        if !tok.is_empty() {
            let _ = spotlight_config::save_secret(crate::TOKEN_KEY, &tok);
        }
        cx.notify();
    }

    fn ensure_blur_subs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.subs.is_empty() {
            return;
        }
        for h in [
            self.server_url.read(cx).focus_handle().clone(),
            self.token.read(cx).focus_handle().clone(),
        ] {
            let sub = cx.on_focus_out(&h, window, |this, _ev, _win, cx| {
                this.persist(cx);
            });
            self.subs.push(sub);
        }
    }

    fn run_test(&mut self, cx: &mut Context<Self>) {
        if self.testing {
            return;
        }
        self.testing = true;
        self.test_result = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let client = crate::build_client()
                        .ok_or_else(|| anyhow::anyhow!("server URL or token not set"))?;
                    client.health()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.testing = false;
                this.test_result = Some(match result {
                    Ok(v) if v.get("ok").and_then(|o| o.as_bool()).unwrap_or(false) => {
                        format!(
                            "Connected ✓ (storefront: {})",
                            v.get("storefront").and_then(|s| s.as_str()).unwrap_or("?")
                        )
                    }
                    Ok(v) => format!("Server reachable, but unhealthy: {v}"),
                    Err(e) => format!("Failed: {e}"),
                });
                cx.notify();
            });
        })
        .detach();
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

impl Render for MusicSettingsTab {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_blur_subs(window, cx);

        let body = div()
            .flex()
            .flex_col()
            .child(Self::field("ampm server URL", &self.server_url))
            .child(Self::field("Bearer token (printed by `ampm serve` on first run)", &self.token))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(controls::button(
                        &self.test_focus,
                        if self.testing { "Testing…" } else { "Test connection" },
                        cx,
                        |this, _, cx| this.run_test(cx),
                    ))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::muted())
                            .child(self.test_result.clone().unwrap_or_default()),
                    ),
            );

        div()
            .id("music-settings")
            .flex()
            .flex_col()
            .size_full()
            .overflow_y_scroll()
            .child(controls::section("ampm server", body))
    }
}
