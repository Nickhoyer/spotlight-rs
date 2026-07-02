//! The "AI" settings tab: a list of chat providers (name / base URL / model /
//! API key), preset buttons that pre-fill a new provider, a "Find local LLM"
//! button that probes the machine, and an active-provider selector. Saving
//! writes the [`LlmConfig`](crate::LlmConfig) blob and stores each key in the
//! secret store.
//!
//! Every control is a focus/tab stop, so the tab is fully keyboard navigable.

use gpui::prelude::*;
use gpui::{div, px, Context, Entity, FocusHandle, Window};

use spotlight_config::AppConfig;
use spotlight_ui::text_input::TextInput;
use spotlight_ui::{controls, theme};

use crate::{client, secret_key, ApiFormat, LlmConfig, Provider};

/// One editable provider row (parallel controls kept together so their focus
/// handles persist across renders).
struct ProviderRow {
    name: Entity<TextInput>,
    base_url: Entity<TextInput>,
    model: Entity<TextInput>,
    key: Entity<TextInput>,
    format: ApiFormat,
    local: bool,
    /// Model list carried through (from a preset / discovery) for the in-chat
    /// switcher; not user-editable here.
    models: Vec<String>,
    format_focus: FocusHandle,
    active_focus: FocusHandle,
    remove_focus: FocusHandle,
}

pub struct LlmSettingsTab {
    rows: Vec<ProviderRow>,
    active: usize,
    preset_focuses: Vec<FocusHandle>,
    find_focus: FocusHandle,
    save_focus: FocusHandle,
    status: Option<String>,
}

impl LlmSettingsTab {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let cfg = crate::load_config();
        let rows = cfg
            .providers
            .iter()
            .map(|p| {
                let key = if p.local {
                    None
                } else {
                    spotlight_config::load_secret(&secret_key(&p.name))
                };
                Self::row_from(p, key, cx)
            })
            .collect();
        let preset_focuses = crate::presets().iter().map(|_| cx.focus_handle()).collect();

        Self {
            rows,
            active: cfg.active,
            preset_focuses,
            find_focus: cx.focus_handle(),
            save_focus: cx.focus_handle(),
            status: None,
        }
    }

    fn row_from(p: &Provider, key: Option<String>, cx: &mut Context<Self>) -> ProviderRow {
        let field = |cx: &mut Context<Self>, placeholder: &'static str, value: &str, masked: bool| {
            let value = value.to_string();
            cx.new(move |cx| {
                let mut t = TextInput::new(cx, placeholder, masked);
                t.set_value(value);
                t
            })
        };
        ProviderRow {
            name: field(cx, "Name", &p.name, false),
            base_url: field(cx, "https://api.example.com/v1", &p.base_url, false),
            model: field(cx, "model id", &p.model, false),
            key: field(cx, "API key", key.as_deref().unwrap_or_default(), true),
            format: p.format,
            local: p.local,
            models: p.models.clone(),
            format_focus: cx.focus_handle(),
            active_focus: cx.focus_handle(),
            remove_focus: cx.focus_handle(),
        }
    }

    fn add_preset(&mut self, i: usize, cx: &mut Context<Self>) {
        if let Some(p) = crate::presets().get(i) {
            let row = Self::row_from(p, None, cx);
            self.rows.push(row);
            cx.notify();
        }
    }

    fn remove_row(&mut self, i: usize, cx: &mut Context<Self>) {
        if i < self.rows.len() {
            self.rows.remove(i);
            if self.active >= self.rows.len() {
                self.active = self.rows.len().saturating_sub(1);
            }
            cx.notify();
        }
    }

    fn set_active(&mut self, i: usize, cx: &mut Context<Self>) {
        self.active = i;
        cx.notify();
    }

    fn toggle_format(&mut self, i: usize, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.get_mut(i) {
            row.format = match row.format {
                ApiFormat::OpenAi => ApiFormat::Anthropic,
                ApiFormat::Anthropic => ApiFormat::OpenAi,
            };
            cx.notify();
        }
    }

    fn find_local(&mut self, cx: &mut Context<Self>) {
        self.status = Some("Searching for local servers\u{2026}".into());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(async move { client::probe_local() })
                .await;
            let _ = this.update(cx, |this, cx| {
                let mut added = 0;
                let mut with_key = 0;
                for (p, key) in found {
                    let exists = this.rows.iter().any(|r| r.name.read(cx).value() == p.name);
                    if !exists {
                        if key.is_some() {
                            with_key += 1;
                        }
                        let row = Self::row_from(&p, key, cx);
                        this.rows.push(row);
                        added += 1;
                    }
                }
                this.status = Some(if added > 0 {
                    let mut msg = format!("Found {added} local server(s).");
                    if with_key > 0 {
                        msg.push_str(&format!(" Pre-filled {with_key} API key(s) from disk."));
                    }
                    msg
                } else {
                    "No local servers found (is Ollama, LM Studio or omlx running?)".into()
                });
                cx.notify();
            });
        })
        .detach();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let mut providers = Vec::new();
        for row in &self.rows {
            let name = row.name.read(cx).value().trim().to_string();
            let base_url = row.base_url.read(cx).value().trim().to_string();
            if name.is_empty() && base_url.is_empty() {
                continue;
            }
            let model = row.model.read(cx).value().trim().to_string();
            let key = row.key.read(cx).value().trim().to_string();
            if !row.local && !key.is_empty() {
                let _ = spotlight_config::save_secret(&secret_key(&name), &key);
            }
            providers.push(Provider {
                name,
                base_url,
                model,
                models: row.models.clone(),
                format: row.format,
                local: row.local,
            });
        }
        let active = self.active.min(providers.len().saturating_sub(1));

        let mut cfg = AppConfig::load();
        let _ = cfg.set(crate::EXT_ID, &LlmConfig { providers, active });
        self.status = Some(match cfg.save() {
            Ok(()) => "Saved. Reopen AI Chat to use the new settings.".to_string(),
            Err(e) => format!("Couldn't save: {e}"),
        });
        cx.notify();
    }

    fn field(label: &str, input: &Entity<TextInput>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .flex_1()
            .child(div().text_xs().text_color(theme::muted()).child(label.to_string()))
            .child(input.clone())
    }
}

impl Render for LlmSettingsTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Preset buttons — each pre-fills a new provider row.
        let mut presets_row = div().flex().flex_wrap().gap_2().pb_2();
        for (i, preset) in crate::presets().into_iter().enumerate() {
            presets_row = presets_row.child(controls::button(
                &self.preset_focuses[i],
                format!("+ {}", preset.name),
                cx,
                move |this, _, cx| this.add_preset(i, cx),
            ));
        }

        // Provider cards.
        let mut cards = div().flex().flex_col().gap_3().pb_2();
        if self.rows.is_empty() {
            cards = cards.child(
                div()
                    .px_3()
                    .py_4()
                    .rounded_lg()
                    .bg(theme::hover())
                    .text_sm()
                    .text_color(theme::muted())
                    .child("No providers yet \u{2014} pick a preset above or \u{201c}Find local LLM\u{201d}."),
            );
        }
        for (i, row) in self.rows.iter().enumerate() {
            let is_active = i == self.active;
            let format_label = match row.format {
                ApiFormat::OpenAi => "OpenAI",
                ApiFormat::Anthropic => "Anthropic",
            };
            // Row 1: name + base URL, with the remove button on the right.
            let top = div()
                .flex()
                .items_end()
                .gap_2()
                .child(div().w(px(160.)).child(Self::field("Name", &row.name)))
                .child(Self::field("Base URL", &row.base_url))
                .child(controls::icon_button(&row.remove_focus, "\u{2715}", cx, move |this, _, cx| {
                    this.remove_row(i, cx)
                }));

            // Row 2: model + format toggle + active selector.
            let second = div()
                .flex()
                .items_end()
                .gap_2()
                .child(Self::field("Model", &row.model))
                .child(controls::button(&row.format_focus, format_label, cx, move |this, _, cx| {
                    this.toggle_format(i, cx)
                }))
                .child(controls::button(
                    &row.active_focus,
                    if is_active { "\u{2713} Active" } else { "Set active" },
                    cx,
                    move |this, _, cx| this.set_active(i, cx),
                ));

            // Row 3: the API key, or a note for keyless local servers.
            let third = if row.local {
                div()
                    .text_xs()
                    .text_color(theme::muted())
                    .child("Local server \u{2014} no API key needed.")
                    .into_any_element()
            } else {
                Self::field("API key", &row.key).into_any_element()
            };

            let card = div()
                .flex()
                .flex_col()
                .gap_2()
                .p_3()
                .rounded_lg()
                .border_1()
                .border_color(if is_active { theme::accent() } else { theme::divider() })
                .bg(if is_active { theme::selected() } else { theme::hover() })
                .child(top)
                .child(second)
                .child(third);
            cards = cards.child(card);
        }

        div()
            .id("llm-settings")
            .flex()
            .flex_col()
            .size_full()
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .pb_1()
                    .child(div().text_xs().text_color(theme::muted()).child("Add a provider"))
                    .child(controls::button(&self.find_focus, "\u{2728} Find local LLM", cx, |this, _, cx| {
                        this.find_local(cx)
                    })),
            )
            .child(presets_row)
            .child(div().h(px(1.)).bg(theme::divider()))
            .child(div().pt_2().text_xs().text_color(theme::muted()).child("Providers"))
            .child(cards)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .pt_2()
                    .child(controls::button(&self.save_focus, "Save", cx, |this, _, cx| this.save(cx)))
                    .when_some(self.status.clone(), |this, status| {
                        this.child(div().text_xs().text_color(theme::muted()).child(status))
                    }),
            )
    }
}
