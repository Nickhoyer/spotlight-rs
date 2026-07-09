//! The "Jira" settings tab: site/email/token fields plus a dynamic list of
//! named JQL filters. Changes apply live — the config blob is written and the
//! token stored in the secret store when a field loses focus (so the token is
//! only persisted on blur, not mid-typing); adding/removing a filter persists
//! immediately.
//!
//! Every control (text fields + buttons) is a focus/tab stop, so the tab is
//! fully keyboard navigable (Tab/Shift-Tab to move, Enter/Space to activate).

use gpui::prelude::*;
use gpui::{div, px, Context, Entity, FocusHandle, Subscription, Window};

use spotlight_config::AppConfig;
use spotlight_ui::text_input::TextInput;
use spotlight_ui::{controls, theme};

use crate::{JiraConfig, JqlFilter};

pub struct JiraSettingsTab {
    site: Entity<TextInput>,
    email: Entity<TextInput>,
    token: Entity<TextInput>,
    /// `(name, jql)` input pairs.
    filters: Vec<(Entity<TextInput>, Entity<TextInput>)>,
    /// Focus handles for the per-filter remove buttons (parallel to `filters`).
    remove_focuses: Vec<FocusHandle>,
    add_focus: FocusHandle,
    /// Blur subscriptions for the text fields; re-registered when the filter count
    /// changes so newly-added rows get covered.
    subs: Vec<Subscription>,
    subs_for: Option<usize>,
}

impl JiraSettingsTab {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let cfg = crate::load_config();

        let site = cx.new(|cx| {
            let mut t = TextInput::new(cx, "acme  (or acme.atlassian.net)", false);
            t.set_value(cfg.site.clone());
            t
        });
        let email = cx.new(|cx| {
            let mut t = TextInput::new(cx, "you@example.com", false);
            t.set_value(cfg.email.clone());
            t
        });
        let token = cx.new(|cx| {
            let mut t = TextInput::new(cx, "API token", true);
            if let Some(tok) = spotlight_config::load_secret(crate::TOKEN_KEY) {
                t.set_value(tok);
            }
            t
        });

        let filters: Vec<_> = cfg
            .filters
            .iter()
            .map(|f| {
                let name = f.name.clone();
                let jql = f.jql.clone();
                let n = cx.new(|cx| {
                    let mut t = TextInput::new(cx, "Filter name", false);
                    t.set_value(name);
                    t
                });
                let j = cx.new(|cx| {
                    let mut t = TextInput::new(cx, "JQL (e.g. sprint in openSprints() AND ...)", false);
                    t.set_value(jql);
                    t
                });
                (n, j)
            })
            .collect();
        let remove_focuses = filters.iter().map(|_| cx.focus_handle()).collect();

        Self {
            site,
            email,
            token,
            filters,
            remove_focuses,
            add_focus: cx.focus_handle(),
            subs: Vec::new(),
            subs_for: None,
        }
    }

    fn add_filter(&mut self, cx: &mut Context<Self>) {
        let n = cx.new(|cx| TextInput::new(cx, "Filter name", false));
        let j = cx.new(|cx| TextInput::new(cx, "JQL (e.g. sprint in openSprints() AND ...)", false));
        self.filters.push((n, j));
        self.remove_focuses.push(cx.focus_handle());
        self.persist(cx);
    }

    fn remove_filter(&mut self, i: usize, cx: &mut Context<Self>) {
        if i < self.filters.len() {
            self.filters.remove(i);
            self.remove_focuses.remove(i);
            self.persist(cx);
        }
    }

    /// Write the config blob (site/email/filters). No secrets.
    fn persist(&mut self, cx: &mut Context<Self>) {
        let site = self.site.read(cx).value().trim().to_string();
        let email = self.email.read(cx).value().trim().to_string();
        let filters: Vec<JqlFilter> = self
            .filters
            .iter()
            .filter_map(|(n, j)| {
                let name = n.read(cx).value().trim().to_string();
                let jql = j.read(cx).value().trim().to_string();
                if name.is_empty() && jql.is_empty() {
                    None
                } else {
                    Some(JqlFilter { name, jql })
                }
            })
            .collect();

        let mut app = AppConfig::load();
        let _ = app.set(crate::EXT_ID, &JiraConfig { site, email, filters });
        let _ = app.save();
        cx.notify();
    }

    /// Store the API token if set (non-empty only, so an untouched masked field
    /// never wipes the stored token). Called on blur.
    fn save_secrets(&mut self, cx: &Context<Self>) {
        let token = self.token.read(cx).value().to_string();
        if !token.trim().is_empty() {
            let _ = spotlight_config::save_secret(crate::TOKEN_KEY, &token);
        }
    }

    /// (Re)subscribe blur handlers for every text field.
    fn ensure_blur_subs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let n = 3 + self.filters.len() * 2;
        if self.subs_for == Some(n) {
            return;
        }
        self.subs_for = Some(n);
        self.subs.clear();

        let mut handles = Vec::with_capacity(n);
        handles.push(self.site.read(cx).focus_handle().clone());
        handles.push(self.email.read(cx).focus_handle().clone());
        handles.push(self.token.read(cx).focus_handle().clone());
        for (name, jql) in &self.filters {
            handles.push(name.read(cx).focus_handle().clone());
            handles.push(jql.read(cx).focus_handle().clone());
        }
        for h in handles {
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

impl Render for JiraSettingsTab {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_blur_subs(window, cx);

        let connection = div()
            .flex()
            .flex_col()
            .child(Self::field("Site", &self.site))
            .child(Self::field("Email", &self.email))
            .child(Self::field("API token", &self.token));

        let mut filters = div().flex().flex_col().gap_2().pt_1();
        for (i, (name, jql)) in self.filters.iter().enumerate() {
            filters = filters.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().w(px(150.)).child(name.clone()))
                    .child(div().flex_1().child(jql.clone()))
                    .child(controls::icon_button(
                        &self.remove_focuses[i],
                        "\u{2715}",
                        cx,
                        move |this, _, cx| this.remove_filter(i, cx),
                    )),
            );
        }
        let filters_block = div()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .pb_1()
                    .child(div().text_xs().text_color(theme::muted()).child("JQL filters"))
                    .child(controls::button(&self.add_focus, "+ Add", cx, |this, _, cx| {
                        this.add_filter(cx)
                    })),
            )
            .child(filters);

        div()
            .id("jira-settings")
            .flex()
            .flex_col()
            .size_full()
            .overflow_y_scroll()
            .child(controls::section("Connection", connection))
            .child(filters_block)
    }
}
