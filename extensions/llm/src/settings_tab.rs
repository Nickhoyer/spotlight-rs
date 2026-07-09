//! The "AI" settings tab. A **list** of providers plus a **Chat features** group;
//! editing or adding a provider drills into a focused sub-view (like the Jira
//! status picker goes one level deeper), with a back-out.
//!
//! * **List** — one compact row per provider (logo, name, model, key state, active
//!   badge). "+ Add provider" opens the chooser. Below the list, on/off switches
//!   for Web search, Ambient context, and inline autocomplete (+ web-search
//!   Advanced).
//! * **Edit** — a focused editor for one provider: model picker, API key, and the
//!   plumbing (base URL, wire format) under "Advanced" for presets, or fully
//!   editable for a custom endpoint. Set-active and Remove live here.
//! * **Add** — a chooser: known providers not yet added, a Custom endpoint, and
//!   "Find local". Picking one drills straight into its editor.
//!
//! Changes apply live: toggles/structural edits persist immediately; text fields
//! (including secrets) persist on blur, so API keys hit the store only when a
//! field loses focus. Every control is a focus/tab stop; Escape backs out of a
//! sub-view. Fully keyboard navigable.

use std::rc::Rc;

use gpui::prelude::*;
use gpui::{div, px, AnyElement, Context, Entity, FocusHandle, KeyDownEvent, MouseButton, SharedString, Subscription, Window};

use spotlight_config::AppConfig;
use spotlight_ui::text_input::TextInput;
use spotlight_ui::{controls, theme};

use crate::{client, secret_key, ApiFormat, LlmConfig, Provider, SearchConfig, SEARCH_SECRET_KEY};

/// Which sub-view the tab is showing.
#[derive(Clone, Copy, PartialEq)]
enum AiScreen {
    List,
    Edit(usize),
    Add,
}

/// One editable provider row (parallel controls kept together so their focus
/// handles persist across renders).
struct ProviderRow {
    name: Entity<TextInput>,
    base_url: Entity<TextInput>,
    model: Entity<TextInput>,
    key: Entity<TextInput>,
    format: ApiFormat,
    local: bool,
    /// `true` for a hand-rolled "Custom" provider — name / base URL / format are
    /// shown and editable. `false` for a known preset or a discovered local server.
    custom: bool,
    models: Vec<String>,
    model_focuses: Vec<FocusHandle>,
    row_focus: FocusHandle,
    openai_focus: FocusHandle,
    anthropic_focus: FocusHandle,
    active_focus: FocusHandle,
    remove_focus: FocusHandle,
    advanced_focus: FocusHandle,
    advanced_open: bool,
}

pub struct LlmSettingsTab {
    screen: AiScreen,
    focus_handle: FocusHandle,
    back_focus: FocusHandle,
    add_focus: FocusHandle,
    rows: Vec<ProviderRow>,
    active: usize,
    autocomplete: bool,
    search_enabled: bool,
    ambient_context: bool,
    search_endpoint: Entity<TextInput>,
    search_token: Entity<TextInput>,
    search_max_results: Entity<TextInput>,
    web_advanced_open: bool,
    preset_focuses: Vec<FocusHandle>,
    custom_focus: FocusHandle,
    find_focus: FocusHandle,
    autocomplete_focus: FocusHandle,
    search_focus: FocusHandle,
    ambient_focus: FocusHandle,
    web_advanced_focus: FocusHandle,
    fetch_focus: FocusHandle,
    /// Transient feedback from "Find local" / duplicate presets.
    status: Option<String>,
    /// Transient feedback from fetching an endpoint's model list (Edit screen).
    model_status: Option<String>,
    subs: Vec<Subscription>,
    subs_for: Option<usize>,
}

/// Build a value-seeded text input.
fn text_input(cx: &mut Context<LlmSettingsTab>, placeholder: &'static str, value: &str, masked: bool) -> Entity<TextInput> {
    let value = value.to_string();
    cx.new(move |cx| {
        let mut t = TextInput::new(cx, placeholder, masked);
        t.set_value(value);
        t
    })
}

impl LlmSettingsTab {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let cfg = crate::load_config();
        let known = crate::presets();
        let rows = cfg
            .providers
            .iter()
            .map(|p| {
                let key = if p.local {
                    None
                } else {
                    spotlight_config::load_secret(&secret_key(&p.name))
                };
                let custom = !p.local && !known.iter().any(|k| k.name == p.name);
                Self::row_from(p, key, custom, cx)
            })
            .collect();
        let preset_focuses = known.iter().map(|_| cx.focus_handle()).collect();
        let token = spotlight_config::load_secret(SEARCH_SECRET_KEY).unwrap_or_default();

        // Debug hook (headless capture only): deep-link a sub-view so Edit/Add can
        // be screenshotted without a click. Mirrors the SPOTLIGHT_CAPTURE_* hooks.
        let screen = match std::env::var("SPOTLIGHT_AI_SCREEN").as_deref() {
            Ok("add") => AiScreen::Add,
            Ok("edit") => AiScreen::Edit(0),
            _ => AiScreen::List,
        };

        Self {
            screen,
            focus_handle: cx.focus_handle(),
            back_focus: cx.focus_handle(),
            add_focus: cx.focus_handle(),
            rows,
            active: cfg.active,
            autocomplete: cfg.autocomplete,
            search_enabled: cfg.search.enabled,
            ambient_context: cfg.ambient_context,
            search_endpoint: text_input(cx, "https://degoog.org/api/search", &cfg.search.endpoint, false),
            search_token: text_input(cx, "Bearer token (optional)", &token, true),
            search_max_results: text_input(cx, "5", &cfg.search.max_results.to_string(), false),
            web_advanced_open: false,
            preset_focuses,
            custom_focus: cx.focus_handle(),
            find_focus: cx.focus_handle(),
            autocomplete_focus: cx.focus_handle(),
            search_focus: cx.focus_handle(),
            ambient_focus: cx.focus_handle(),
            web_advanced_focus: cx.focus_handle(),
            fetch_focus: cx.focus_handle(),
            status: None,
            model_status: None,
            subs: Vec::new(),
            subs_for: None,
        }
    }

    fn row_from(p: &Provider, key: Option<String>, custom: bool, cx: &mut Context<Self>) -> ProviderRow {
        ProviderRow {
            name: text_input(cx, "Name", &p.name, false),
            base_url: text_input(cx, "https://api.example.com/v1", &p.base_url, false),
            model: text_input(cx, "model id", &p.model, false),
            key: text_input(cx, "Paste your API key", key.as_deref().unwrap_or_default(), true),
            format: p.format,
            local: p.local,
            custom,
            models: p.models.clone(),
            model_focuses: p.models.iter().map(|_| cx.focus_handle()).collect(),
            row_focus: cx.focus_handle(),
            openai_focus: cx.focus_handle(),
            anthropic_focus: cx.focus_handle(),
            active_focus: cx.focus_handle(),
            remove_focus: cx.focus_handle(),
            advanced_focus: cx.focus_handle(),
            advanced_open: false,
        }
    }

    // ---- navigation between sub-views -------------------------------------

    fn go_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.screen = AiScreen::List;
        self.model_status = None;
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn go_edit(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.model_status = None;
        self.screen = AiScreen::Edit(i);
        window.focus(&self.back_focus, cx);
        cx.notify();
    }

    /// Open a provider's editor and ask its endpoint which models it serves (so
    /// the picker shows the real list, not a preset's guesses).
    fn open_provider(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.go_edit(i, window, cx);
        let has_url = self
            .rows
            .get(i)
            .map(|r| !r.base_url.read(cx).value().trim().is_empty())
            .unwrap_or(false);
        if has_url {
            self.fetch_models(i, cx);
        }
    }

    /// Query `{base_url}/models` and replace the row's offered models with what the
    /// endpoint actually serves. Leaves the selected model untouched (the user
    /// picks from the fetched chips).
    fn fetch_models(&mut self, i: usize, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(i) else { return };
        let base_url = row.base_url.read(cx).value().trim().to_string();
        if base_url.is_empty() {
            self.model_status = Some("Set a Base URL first.".into());
            cx.notify();
            return;
        }
        let key = {
            let k = row.key.read(cx).value().trim().to_string();
            if k.is_empty() { None } else { Some(k) }
        };
        self.model_status = Some("Fetching models\u{2026}".into());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { client::fetch_models(&base_url, key.as_deref()) })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(models) if !models.is_empty() => {
                        let focuses: Vec<FocusHandle> = models.iter().map(|_| cx.focus_handle()).collect();
                        if let Some(row) = this.rows.get_mut(i) {
                            row.models = models.clone();
                            row.model_focuses = focuses;
                        }
                        this.model_status = Some(format!("{} model(s) available.", models.len()));
                        this.persist(cx);
                    }
                    Ok(_) => {
                        this.model_status = Some("Endpoint returned no models.".into());
                        cx.notify();
                    }
                    Err(e) => {
                        this.model_status = Some(format!("Couldn't fetch models: {e}"));
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    fn go_add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.status = None;
        self.screen = AiScreen::Add;
        window.focus(&self.back_focus, cx);
        cx.notify();
    }

    fn add_preset_and_edit(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        let before = self.rows.len();
        self.add_preset(i, cx);
        if self.rows.len() > before {
            self.go_edit(self.rows.len() - 1, window, cx);
        }
    }

    fn add_custom_and_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.add_custom(cx);
        self.go_edit(self.rows.len() - 1, window, cx);
    }

    fn remove_in_edit(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.remove_row(i, cx);
        self.go_list(window, cx);
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // Escape backs out of a sub-view to the list (and stops so the shell
        // doesn't also go Home). On the list, let it bubble.
        if matches!(self.screen, AiScreen::Edit(_) | AiScreen::Add) && ev.keystroke.key == "escape" {
            self.go_list(window, cx);
            cx.stop_propagation();
        }
    }

    // ---- mutations (each persists) ----------------------------------------

    fn toggle_autocomplete(&mut self, cx: &mut Context<Self>) {
        self.autocomplete = !self.autocomplete;
        self.persist(cx);
    }

    fn toggle_search(&mut self, cx: &mut Context<Self>) {
        self.search_enabled = !self.search_enabled;
        self.persist(cx);
    }

    fn toggle_ambient(&mut self, cx: &mut Context<Self>) {
        self.ambient_context = !self.ambient_context;
        self.persist(cx);
    }

    fn toggle_web_advanced(&mut self, cx: &mut Context<Self>) {
        self.web_advanced_open = !self.web_advanced_open;
        cx.notify();
    }

    fn toggle_row_advanced(&mut self, i: usize, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.get_mut(i) {
            row.advanced_open = !row.advanced_open;
            cx.notify();
        }
    }

    fn add_preset(&mut self, i: usize, cx: &mut Context<Self>) {
        if let Some(p) = crate::presets().get(i) {
            if self.rows.iter().any(|r| r.name.read(cx).value() == p.name) {
                self.status = Some(format!("{} is already in the list.", p.name));
                cx.notify();
                return;
            }
            let row = Self::row_from(p, None, false, cx);
            self.rows.push(row);
            self.active = self.rows.len() - 1;
            self.status = None;
            self.persist(cx);
        }
    }

    fn add_custom(&mut self, cx: &mut Context<Self>) {
        let blank = Provider {
            name: String::new(),
            base_url: String::new(),
            model: String::new(),
            models: Vec::new(),
            format: ApiFormat::OpenAi,
            local: false,
        };
        let mut row = Self::row_from(&blank, None, true, cx);
        row.advanced_open = true;
        self.rows.push(row);
        self.active = self.rows.len() - 1;
        self.status = None;
        cx.notify();
    }

    fn remove_row(&mut self, i: usize, cx: &mut Context<Self>) {
        if i < self.rows.len() {
            self.rows.remove(i);
            if self.active >= self.rows.len() {
                self.active = self.rows.len().saturating_sub(1);
            }
            self.persist(cx);
        }
    }

    fn set_active(&mut self, i: usize, cx: &mut Context<Self>) {
        self.active = i;
        self.persist(cx);
    }

    fn set_format(&mut self, i: usize, format: ApiFormat, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.get_mut(i) {
            row.format = format;
            self.persist(cx);
        }
    }

    fn set_model(&mut self, i: usize, model: String, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.get(i) {
            let input = row.model.clone();
            input.update(cx, |t, _| t.set_value(model));
            self.persist(cx);
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
                        let row = Self::row_from(&p, key, false, cx);
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
                if added > 0 {
                    this.persist(cx);
                } else {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    // ---- persistence ------------------------------------------------------

    fn build_config(&self, cx: &Context<Self>) -> LlmConfig {
        let mut providers = Vec::new();
        for row in &self.rows {
            let name = row.name.read(cx).value().trim().to_string();
            let base_url = row.base_url.read(cx).value().trim().to_string();
            if name.is_empty() && base_url.is_empty() {
                continue;
            }
            let model = row.model.read(cx).value().trim().to_string();
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
        let endpoint = {
            let e = self.search_endpoint.read(cx).value().trim().to_string();
            if e.is_empty() { SearchConfig::default().endpoint } else { e }
        };
        let max_results = self
            .search_max_results
            .read(cx)
            .value()
            .trim()
            .parse::<usize>()
            .unwrap_or(5)
            .clamp(1, 20);
        LlmConfig {
            providers,
            active,
            autocomplete: self.autocomplete,
            search: SearchConfig { enabled: self.search_enabled, endpoint, max_results },
            ambient_context: self.ambient_context,
        }
    }

    fn persist(&mut self, cx: &mut Context<Self>) {
        let cfg = self.build_config(cx);
        let mut app = AppConfig::load();
        let _ = app.set(crate::EXT_ID, &cfg);
        let _ = app.save();
        cx.notify();
    }

    fn save_secrets(&mut self, cx: &Context<Self>) {
        for row in &self.rows {
            if row.local {
                continue;
            }
            let name = row.name.read(cx).value().trim().to_string();
            let key = row.key.read(cx).value().trim().to_string();
            if !name.is_empty() && !key.is_empty() {
                let _ = spotlight_config::save_secret(&secret_key(&name), &key);
            }
        }
        let token = self.search_token.read(cx).value().trim().to_string();
        if !token.is_empty() {
            let _ = spotlight_config::save_secret(SEARCH_SECRET_KEY, &token);
        }
    }

    fn ensure_blur_subs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let n = self.rows.len() * 4 + 3;
        if self.subs_for == Some(n) {
            return;
        }
        self.subs_for = Some(n);
        self.subs.clear();

        let mut handles = Vec::with_capacity(n);
        for row in &self.rows {
            handles.push(row.name.read(cx).focus_handle().clone());
            handles.push(row.base_url.read(cx).focus_handle().clone());
            handles.push(row.model.read(cx).focus_handle().clone());
            handles.push(row.key.read(cx).focus_handle().clone());
        }
        handles.push(self.search_endpoint.read(cx).focus_handle().clone());
        handles.push(self.search_token.read(cx).focus_handle().clone());
        handles.push(self.search_max_results.read(cx).focus_handle().clone());

        for h in handles {
            let sub = cx.on_focus_out(&h, window, |this, _ev, _win, cx| {
                this.save_secrets(cx);
                this.persist(cx);
            });
            self.subs.push(sub);
        }
    }

    // ---- shared render helpers --------------------------------------------

    /// A rounded letter tile standing in for a provider logo.
    fn logo(name: &str) -> impl IntoElement {
        let letter = name.chars().next().unwrap_or('\u{2728}').to_uppercase().to_string();
        div()
            .flex_shrink_0()
            .size(px(32.))
            .rounded_lg()
            .bg(theme::tile())
            .flex()
            .items_center()
            .justify_center()
            .text_color(theme::accent())
            .child(letter)
    }

    /// A small muted section heading.
    fn heading(text: &str) -> impl IntoElement {
        div().pb_2().text_xs().text_color(theme::muted()).child(text.to_string())
    }

    /// A stacked label + text field.
    fn field(label: &str, input: &Entity<TextInput>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .flex_1()
            .child(div().text_xs().text_color(theme::muted()).child(label.to_string()))
            .child(input.clone())
    }

    /// A focusable back link "‹ <label>" that returns to the list.
    fn back_link(&self, label: &str, cx: &mut Context<Self>) -> impl IntoElement {
        controls::button(&self.back_focus, format!("\u{2039}  {label}"), cx, |this, w, cx| {
            this.go_list(w, cx)
        })
    }

    /// A small focusable chip drawn filled + accent when `selected`.
    fn chip(
        focus: &FocusHandle,
        label: impl Into<SharedString>,
        selected: bool,
        cx: &mut Context<Self>,
        on: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        let cb = Rc::new(on);
        let on_key = cb.clone();
        let on_click = cb;
        div()
            .track_focus(focus)
            .tab_index(0)
            .px_3()
            .py_1()
            .rounded_lg()
            .border_1()
            .border_color(if selected { theme::accent() } else { theme::divider() })
            .when(selected, |s| s.bg(theme::selected()))
            .text_color(if selected { theme::accent() } else { theme::muted() })
            .hover(|s| s.bg(theme::hover()))
            .focus(|s| s.border_color(theme::accent()))
            .child(label.into())
            .on_key_down(cx.listener(
                move |this, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>| {
                    if ev.keystroke.key == "enter" || ev.keystroke.key == "space" {
                        on_key(this, window, cx);
                        cx.stop_propagation();
                    }
                },
            ))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window: &mut Window, cx: &mut Context<Self>| {
                    on_click(this, window, cx);
                }),
            )
    }

    /// A focusable full-width row used in the list and the Add chooser: a leading
    /// element, a title + subtitle, and an optional trailing element.
    fn nav_row(
        focus: &FocusHandle,
        leading: AnyElement,
        title: impl Into<SharedString>,
        subtitle: impl Into<SharedString>,
        trailing: Option<AnyElement>,
        active: bool,
        cx: &mut Context<Self>,
        on: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        let cb = Rc::new(on);
        let on_key = cb.clone();
        let on_click = cb;
        let subtitle = subtitle.into();
        div()
            .track_focus(focus)
            .tab_index(0)
            .flex()
            .items_center()
            .gap_3()
            .px_3()
            .py_3()
            .rounded_lg()
            .border_1()
            .border_color(if active { theme::accent() } else { theme::divider() })
            .bg(if active { theme::selected() } else { theme::hover() })
            .hover(|s| s.bg(theme::hover_strong()))
            .focus(|s| s.border_color(theme::accent()))
            .child(leading)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .child(div().text_color(theme::text()).child(title.into()))
                    .when(!subtitle.is_empty(), |d| {
                        d.child(div().text_xs().text_color(theme::muted()).child(subtitle))
                    }),
            )
            .when_some(trailing, |d, t| d.child(t))
            .on_key_down(cx.listener(
                move |this, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>| {
                    if ev.keystroke.key == "enter" || ev.keystroke.key == "space" {
                        on_key(this, window, cx);
                        cx.stop_propagation();
                    }
                },
            ))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window: &mut Window, cx: &mut Context<Self>| {
                    on_click(this, window, cx);
                }),
            )
    }

    fn format_selector(&self, i: usize, row: &ProviderRow, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().text_xs().text_color(theme::muted()).child("API format"))
            .child(
                div()
                    .flex()
                    .gap_1()
                    .child(Self::chip(
                        &row.openai_focus,
                        "OpenAI",
                        row.format == ApiFormat::OpenAi,
                        cx,
                        move |this, _, cx| this.set_format(i, ApiFormat::OpenAi, cx),
                    ))
                    .child(Self::chip(
                        &row.anthropic_focus,
                        "Anthropic",
                        row.format == ApiFormat::Anthropic,
                        cx,
                        move |this, _, cx| this.set_format(i, ApiFormat::Anthropic, cx),
                    )),
            )
    }

    /// Model picker: an always-editable model id, a "refresh from endpoint" action
    /// that lists what the endpoint actually serves, and those models as quick-pick
    /// chips (highlighting the current one).
    fn model_picker(&self, i: usize, row: &ProviderRow, cx: &mut Context<Self>) -> AnyElement {
        let current = row.model.read(cx).value().to_string();
        let mut chips = div().flex().flex_wrap().gap_2().pt_1();
        for (m, focus) in row.models.iter().zip(row.model_focuses.iter()) {
            let model = m.clone();
            let selected = current == *m;
            chips = chips.child(Self::chip(focus, m.clone(), selected, cx, move |this, _, cx| {
                this.set_model(i, model.clone(), cx)
            }));
        }
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .child(div().text_xs().text_color(theme::muted()).child("Model"))
            .child(controls::button(&self.fetch_focus, "\u{21bb} Refresh from endpoint", cx, move |this, _, cx| {
                this.fetch_models(i, cx)
            }));
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(header)
            .child(row.model.clone())
            .when(!row.models.is_empty(), |d| d.child(chips))
            .when_some(self.model_status.clone(), |d, s| {
                d.child(div().pt_1().text_xs().text_color(theme::muted()).child(s))
            })
            .into_any_element()
    }

    // ---- the three sub-views ----------------------------------------------

    /// A compact provider row for the list.
    fn provider_row(&self, i: usize, row: &ProviderRow, cx: &mut Context<Self>) -> AnyElement {
        let is_active = i == self.active;
        let raw_name = row.name.read(cx).value().to_string();
        let name = if raw_name.trim().is_empty() { "Untitled provider".to_string() } else { raw_name };
        let model = row.model.read(cx).value().to_string();
        let model_part = if model.trim().is_empty() { "no model".to_string() } else { model };
        let subtitle = if row.local {
            "Local server".to_string()
        } else if row.key.read(cx).value().trim().is_empty() {
            format!("{model_part} \u{00b7} no API key")
        } else {
            format!("{model_part} \u{00b7} API key set")
        };
        let trailing = div()
            .flex()
            .items_center()
            .gap_3()
            .when(is_active, |d| {
                d.child(
                    div()
                        .px_2()
                        .rounded_full()
                        .bg(theme::selected())
                        .text_xs()
                        .text_color(theme::accent())
                        .child("Active"),
                )
            })
            .child(div().text_color(theme::muted()).child("\u{203a}"))
            .into_any_element();
        Self::nav_row(
            &row.row_focus,
            Self::logo(&name).into_any_element(),
            name.clone(),
            subtitle,
            Some(trailing),
            is_active,
            cx,
            move |this, w, cx| this.open_provider(i, w, cx),
        )
        .into_any_element()
    }

    fn render_list(&self, cx: &mut Context<Self>) -> AnyElement {
        // Provider list + add row.
        let mut list = div().flex().flex_col().gap_2();
        if self.rows.is_empty() {
            list = list.child(
                div()
                    .px_3()
                    .py_4()
                    .rounded_lg()
                    .bg(theme::hover())
                    .text_sm()
                    .text_color(theme::muted())
                    .child("No providers yet \u{2014} add one and paste its API key."),
            );
        }
        for i in 0..self.rows.len() {
            list = list.child(self.provider_row(i, &self.rows[i], cx));
        }
        let add = Self::nav_row(
            &self.add_focus,
            div()
                .size(px(32.))
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme::accent())
                .child("+")
                .into_any_element(),
            "Add provider",
            "",
            None,
            false,
            cx,
            |this, w, cx| this.go_add(w, cx),
        );
        list = list.child(add);

        let providers = div()
            .flex()
            .flex_col()
            .child(Self::heading("Providers"))
            .child(list)
            .when_some(self.status.clone(), |d, s| {
                d.child(div().pt_2().text_xs().text_color(theme::muted()).child(s))
            });

        // Chat features.
        let features = div()
            .flex()
            .flex_col()
            .child(controls::settings_row(
                "Web search",
                "Let the assistant call a live web-search tool for current or niche facts.",
                controls::toggle(&self.search_focus, self.search_enabled, cx, |this, _, cx| {
                    this.toggle_search(cx)
                }),
            ))
            .child(controls::settings_row(
                "Ambient context",
                "Give the assistant the current date, time, locale and coarse location.",
                controls::toggle(&self.ambient_focus, self.ambient_context, cx, |this, _, cx| {
                    this.toggle_ambient(cx)
                }),
            ))
            .child(controls::settings_row(
                "Inline autocomplete",
                "Greyed-out search suggestions (Tab to accept) plus \u{201c}Ask AI\u{201d} rows.",
                controls::toggle(&self.autocomplete_focus, self.autocomplete, cx, |this, _, cx| {
                    this.toggle_autocomplete(cx)
                }),
            ))
            .child(div().h(px(1.)).my_2().bg(theme::divider()))
            .child(controls::disclosure(
                &self.web_advanced_focus,
                "Advanced \u{2014} web search",
                self.web_advanced_open,
                cx,
                |this, _, cx| this.toggle_web_advanced(cx),
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .pt_1()
                    .child(Self::field("Search endpoint", &self.search_endpoint))
                    .child(Self::field("Search API token", &self.search_token))
                    .child(div().w(px(120.)).child(Self::field("Results per search", &self.search_max_results))),
            ));

        div()
            .flex()
            .flex_col()
            .gap_6()
            .child(providers)
            .child(controls::section("Chat features", features))
            .into_any_element()
    }

    fn render_edit(&self, i: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.rows.get(i) else {
            return div().into_any_element();
        };
        let is_active = i == self.active;
        let raw_name = row.name.read(cx).value().to_string();
        let title_name = if raw_name.trim().is_empty() { "New provider".to_string() } else { raw_name };

        let titlebar = div()
            .flex()
            .items_center()
            .gap_3()
            .child(Self::logo(&title_name))
            .child(div().text_lg().text_color(theme::text()).child(title_name));

        let mut body = div().flex().flex_col().gap_4();
        if row.custom {
            body = body
                .child(Self::field("Name", &row.name))
                .child(Self::field("Base URL", &row.base_url))
                .child(self.format_selector(i, row, cx))
                .child(self.model_picker(i, row, cx))
                .child(Self::field("API key", &row.key));
        } else {
            body = body.child(self.model_picker(i, row, cx));
            if row.local {
                body = body.child(
                    div().text_xs().text_color(theme::muted()).child("Local server \u{2014} no API key needed."),
                );
            } else {
                body = body.child(Self::field("API key", &row.key));
            }
            let adv = div()
                .flex()
                .flex_col()
                .gap_3()
                .pt_1()
                .child(Self::field("Base URL", &row.base_url))
                .child(self.format_selector(i, row, cx));
            body = body.child(controls::disclosure(
                &row.advanced_focus,
                "Advanced",
                row.advanced_open,
                cx,
                move |this, _, cx| this.toggle_row_advanced(i, cx),
                adv,
            ));
        }

        let footer = div()
            .flex()
            .items_center()
            .gap_3()
            .pt_2()
            .child(controls::button(
                &row.active_focus,
                if is_active { "\u{2713} Active" } else { "Set active" },
                cx,
                move |this, _, cx| this.set_active(i, cx),
            ))
            .child(controls::button(&row.remove_focus, "Remove", cx, move |this, w, cx| {
                this.remove_in_edit(i, w, cx)
            }));

        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(self.back_link("Providers", cx))
            .child(titlebar)
            .child(div().h(px(1.)).bg(theme::divider()))
            .child(body)
            .child(footer)
            .into_any_element()
    }

    fn render_add(&self, cx: &mut Context<Self>) -> AnyElement {
        let present: Vec<String> = self.rows.iter().map(|r| r.name.read(cx).value().to_string()).collect();

        let mut choices = div().flex().flex_col().gap_2();
        for (i, preset) in crate::presets().into_iter().enumerate() {
            if present.iter().any(|n| n == &preset.name) {
                continue;
            }
            let base = preset.base_url.clone();
            choices = choices.child(Self::nav_row(
                &self.preset_focuses[i],
                Self::logo(&preset.name).into_any_element(),
                preset.name.clone(),
                base,
                None,
                false,
                cx,
                move |this, w, cx| this.add_preset_and_edit(i, w, cx),
            ));
        }
        choices = choices
            .child(Self::nav_row(
                &self.custom_focus,
                Self::logo("+").into_any_element(),
                "Custom provider",
                "Any OpenAI- or Anthropic-compatible endpoint",
                None,
                false,
                cx,
                |this, w, cx| this.add_custom_and_edit(w, cx),
            ))
            .child(Self::nav_row(
                &self.find_focus,
                Self::logo("\u{2728}").into_any_element(),
                "Find local models",
                "Detect Ollama, LM Studio or omlx",
                None,
                false,
                cx,
                |this, _, cx| this.find_local(cx),
            ));

        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(self.back_link("Providers", cx))
            .child(div().text_lg().text_color(theme::text()).child("Add a provider"))
            .child(div().h(px(1.)).bg(theme::divider()))
            .child(choices)
            .when_some(self.status.clone(), |d, s| {
                d.child(div().pt_1().text_xs().text_color(theme::muted()).child(s))
            })
            .into_any_element()
    }
}

impl Render for LlmSettingsTab {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_blur_subs(window, cx);
        let inner = match self.screen {
            AiScreen::List => self.render_list(cx),
            AiScreen::Edit(i) => self.render_edit(i, cx),
            AiScreen::Add => self.render_add(cx),
        };
        div()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .flex()
            .flex_col()
            .w_full()
            .child(inner)
    }
}
