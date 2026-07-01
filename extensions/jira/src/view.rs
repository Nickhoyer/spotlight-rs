//! The Jira panel: filter chips, a stale-while-revalidate issue list with a
//! "syncing" indicator, and per-row hover quick actions (Assign to me / Update
//! status).

use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    div, ease_in_out, linear, px, rgb, rgba, AnyElement, Animation, AnimationExt as _, Context,
    FocusHandle, MouseButton, Window,
};

use spotlight_config::{AppConfig, Recent};
use spotlight_ui::list::ListNav;
use spotlight_ui::theme;

use crate::client::JiraClient;
use crate::models::{self, Account, Issue};
use crate::JiraConfig;

/// Delay before hovering a row reveals its quick actions.
const HOVER_REVEAL_MS: u64 = 500;

/// State for the full-panel "Update status" transition picker.
#[derive(Clone)]
struct StatusPicker {
    key: String,
    transitions: Vec<crate::models::Transition>,
    loading: bool,
}

pub struct JiraView {
    client: Option<Arc<JiraClient>>,
    config: JiraConfig,
    active: usize,
    /// Currently shown issues (may be stale while `fetching`).
    issues: Vec<Issue>,
    fetching: bool,
    error: Option<String>,
    /// The authenticated user, for "Assign to me".
    myself: Option<Account>,
    hovered: Option<usize>,
    revealed: Option<usize>,
    picker: Option<StatusPicker>,
    /// Bumped on each fetch so out-of-order responses can be discarded.
    generation: u64,
    focus_handle: FocusHandle,
    /// Keyboard selection + scroll for the issue list.
    nav: ListNav,
    /// Which control on the selected row has keyboard focus: `None` = the task
    /// itself, `Some(j)` = quick action `j`.
    action_focus: Option<usize>,
    /// Keyboard selection + scroll for the status picker.
    picker_nav: ListNav,
    /// Whether we've taken focus once (so we focus on first render only).
    focused_once: bool,
}

/// Number of quick actions per row (Assign to me, Update status).
const QUICK_ACTIONS: usize = 2;

impl JiraView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let config = crate::load_config();
        let client = crate::build_client(&config).map(Arc::new);
        // Render cached issues for the first filter immediately.
        let issues = config
            .filters
            .first()
            .map(|f| crate::load_cache(&f.name))
            .unwrap_or_default();

        let mut view = Self {
            client,
            config,
            active: 0,
            issues,
            fetching: false,
            error: None,
            myself: None,
            hovered: None,
            revealed: None,
            picker: None,
            generation: 0,
            focus_handle: cx.focus_handle(),
            nav: ListNav::new(),
            action_focus: None,
            picker_nav: ListNav::new(),
            focused_once: false,
        };

        if view.client.is_none() {
            view.error = Some("Jira isn't configured yet. Open Settings → Jira.".to_string());
        } else if view.config.filters.is_empty() {
            view.error = Some("No JQL filters yet. Add one in Settings → Jira.".to_string());
        } else {
            view.refresh(cx);
            view.fetch_myself(cx);
        }
        // Debug aid (mirrors the SPOTLIGHT_CAPTURE_* knobs): force-reveal the
        // first row's quick actions so screenshots can show the hover state.
        if std::env::var_os("SPOTLIGHT_JIRA_REVEAL").is_some() {
            view.revealed = Some(0);
            view.action_focus = Some(0);
        }
        view
    }

    fn select_filter(&mut self, i: usize, cx: &mut Context<Self>) {
        self.active = i;
        self.picker = None;
        self.revealed = None;
        self.hovered = None;
        self.nav.set(0);
        self.action_focus = None;
        self.issues = self
            .config
            .filters
            .get(i)
            .map(|f| crate::load_cache(&f.name))
            .unwrap_or_default();
        self.refresh(cx);
    }

    /// Kick off a background fetch for the active filter (stale list stays up).
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(filter) = self.config.filters.get(self.active).cloned() else {
            self.error = Some("No JQL filters yet. Add one in Settings → Jira.".to_string());
            return;
        };
        self.fetching = true;
        self.error = None;
        self.generation += 1;
        let generation = self.generation;
        let jql = filter.jql.clone();
        let name = filter.name.clone();

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { client.search(&jql, crate::MAX_ISSUES) })
                .await;
            let _ = this.update(cx, |this, cx| {
                // Discard if a newer fetch (or filter switch) superseded us.
                if this.generation != generation {
                    return;
                }
                this.fetching = false;
                match result {
                    Ok(issues) => {
                        crate::save_cache(&name, &issues);
                        let len = issues.len();
                        this.issues = issues;
                        this.nav.clamp(len);
                        this.error = None;
                    }
                    Err(e) => this.error = Some(format!("Couldn't load issues: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn fetch_myself(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let me = cx
                .background_executor()
                .spawn(async move { client.myself() })
                .await;
            if let Ok(me) = me {
                let _ = this.update(cx, |this, _| this.myself = Some(me));
            }
        })
        .detach();
    }

    fn assign_to_me(&mut self, key: String, cx: &mut Context<Self>) {
        let (Some(client), Some(me)) = (self.client.clone(), self.myself.clone()) else {
            self.error = Some("Still identifying you — try again in a moment.".to_string());
            cx.notify();
            return;
        };
        self.revealed = None;
        let account_id = me.account_id;
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move { client.assign(&key, &account_id) })
                .await;
            let _ = this.update(cx, |this, cx| match res {
                Ok(()) => this.refresh(cx),
                Err(e) => {
                    this.error = Some(format!("Assign failed: {e}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn open_status_picker(&mut self, key: String, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        self.picker = Some(StatusPicker {
            key: key.clone(),
            transitions: Vec::new(),
            loading: true,
        });
        self.picker_nav.set(0);
        cx.notify();
        let picker_key = key.clone();
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move { client.transitions(&key) })
                .await;
            let _ = this.update(cx, |this, cx| {
                // Ignore if the user cancelled or opened a different issue's picker.
                if this.picker.as_ref().map(|p| p.key.as_str()) == Some(picker_key.as_str()) {
                    match res {
                        Ok(transitions) => {
                            if let Some(p) = this.picker.as_mut() {
                                p.loading = false;
                                p.transitions = transitions;
                            }
                        }
                        Err(e) => {
                            this.error = Some(format!("Couldn't load statuses: {e}"));
                            this.picker = None;
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn apply_transition(&mut self, key: String, transition_id: String, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        self.picker = None;
        self.revealed = None;
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move { client.transition(&key, &transition_id) })
                .await;
            let _ = this.update(cx, |this, cx| match res {
                Ok(()) => this.refresh(cx),
                Err(e) => {
                    this.error = Some(format!("Update failed: {e}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn open_issue(&self, key: &str) {
        let Some(client) = &self.client else {
            return;
        };
        let url = client.browse_url(key);
        let _ = std::process::Command::new("/usr/bin/open")
            .arg(&url)
            .spawn();
        // Record the use. The id matches the search-result id (`jira:KEY`) so
        // panel-opens and search-opens share history (recents + relevance).
        let mut cfg = AppConfig::load();
        cfg.record_use(Recent {
            id: format!("jira:{}", key),
            title: key.to_string(),
            subtitle: Some("Jira issue".to_string()),
            url: Some(url),
            path: None,
            icon: Some(crate::icon_path()),
            glyph: Some("🪐".to_string()),
        });
        let _ = cfg.save();
    }

    // ---- keyboard ---------------------------------------------------------

    fn on_key_down(&mut self, event: &gpui::KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();

        // Status picker navigation takes precedence when it's open.
        if self.picker.is_some() {
            match key {
                "down" => self.move_picker(1),
                "up" => self.move_picker(-1),
                "enter" => {
                    self.activate_picker(cx);
                    cx.stop_propagation();
                    return;
                }
                "escape" | "left" => {
                    self.picker = None;
                }
                _ => return,
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }

        let len = self.issues.len();
        if len == 0 {
            return;
        }
        match key {
            "down" => {
                self.nav.next(len);
                self.action_focus = None;
            }
            "up" => {
                self.nav.prev();
                self.action_focus = None;
            }
            "right" => {
                self.action_focus = Some(match self.action_focus {
                    None => 0,
                    Some(j) => (j + 1).min(QUICK_ACTIONS - 1),
                });
            }
            "left" => {
                self.action_focus = match self.action_focus {
                    Some(0) | None => None,
                    Some(j) => Some(j - 1),
                };
            }
            "enter" => {
                self.activate_selected(cx);
                cx.stop_propagation();
                return;
            }
            // Let Escape bubble to the shell (back to Home), etc.
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn activate_selected(&mut self, cx: &mut Context<Self>) {
        let Some(issue) = self.issues.get(self.nav.selected).cloned() else {
            return;
        };
        match self.action_focus {
            None => self.open_issue(&issue.key),
            Some(0) => self.assign_to_me(issue.key, cx),
            Some(1) => self.open_status_picker(issue.key, cx),
            _ => {}
        }
    }

    fn move_picker(&mut self, delta: isize) {
        let n = self.picker.as_ref().map(|p| p.transitions.len()).unwrap_or(0);
        if delta > 0 {
            self.picker_nav.next(n);
        } else {
            self.picker_nav.prev();
        }
    }

    fn activate_picker(&mut self, cx: &mut Context<Self>) {
        let Some((key, tid)) = self.picker.as_ref().and_then(|p| {
            p.transitions
                .get(self.picker_nav.selected)
                .map(|t| (p.key.clone(), t.id.clone()))
        }) else {
            return;
        };
        self.apply_transition(key, tid, cx);
    }

    // ---- rendering --------------------------------------------------------

    fn issue_entry(&self, i: usize, issue: &Issue, cx: &mut Context<Self>) -> AnyElement {
        let key = issue.key.clone();
        let (status_bg, status_fg) = issue.status_color.colors();

        let (initials, name) = match &issue.assignee {
            Some(n) => (models::initials(n), n.clone()),
            None => ("–".to_string(), "Unassigned".to_string()),
        };

        let row = div()
            .flex()
            .items_center()
            .gap_3()
            .px_3()
            .py_2()
            .rounded_lg()
            .when(
                self.hovered == Some(i)
                    || (self.nav.selected == i && self.action_focus.is_none()),
                |t| t.bg(theme::selected()),
            )
            .hover(|s| s.bg(theme::hover()))
            .child(
                div()
                    .w(px(22.))
                    .flex()
                    .justify_center()
                    .child(models::priority_icon(&issue.priority)),
            )
            .child(
                div()
                    .w(px(70.))
                    .text_color(theme::accent())
                    .child(issue.key.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .child(div().truncate().text_color(theme::text()).child(issue.summary.clone())),
            )
            .child(
                div()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(rgba(status_bg))
                    .text_xs()
                    .text_color(rgb(status_fg))
                    .child(issue.status.clone()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .w(px(150.))
                    .child(
                        div()
                            .size(px(22.))
                            .rounded_full()
                            .bg(theme::tile())
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_xs()
                            .text_color(theme::accent())
                            .child(initials),
                    )
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .child(div().truncate().text_xs().text_color(theme::muted()).child(name)),
                    ),
            )
            .on_mouse_down(MouseButton::Left, {
                let key = key.clone();
                cx.listener(move |this, _, _, _| this.open_issue(&key))
            });

        let mut col = div()
            .id(("jira-row", i))
            .flex()
            .flex_col()
            .on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
                if *hovered {
                    this.hovered = Some(i);
                    cx.notify();
                    cx.spawn(async move |this, cx| {
                        cx.background_executor()
                            .timer(Duration::from_millis(HOVER_REVEAL_MS))
                            .await;
                        let _ = this.update(cx, |this, cx| {
                            if this.hovered == Some(i) {
                                this.revealed = Some(i);
                                cx.notify();
                            }
                        });
                    })
                    .detach();
                } else {
                    if this.hovered == Some(i) {
                        this.hovered = None;
                    }
                    if this.revealed == Some(i) {
                        this.revealed = None;
                    }
                    cx.notify();
                }
            }))
            .child(row);

        // Quick actions: revealed on hover, or when this row is keyboard-selected
        // and the user has stepped into the actions (Right). `pt_1` separates
        // them from the row above.
        let af = if self.nav.selected == i {
            self.action_focus
        } else {
            None
        };
        if self.revealed == Some(i) || af.is_some() {
            let actions = div()
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .pt_1()
                .pb_2()
                .child(quick_action("Assign to me", af == Some(0), {
                    let key = key.clone();
                    cx.listener(move |this, _, _, cx| this.assign_to_me(key.clone(), cx))
                }))
                .child(quick_action("Update status", af == Some(1), {
                    let key = key.clone();
                    cx.listener(move |this, _, _, cx| this.open_status_picker(key.clone(), cx))
                }))
                .with_animation(
                    "reveal",
                    Animation::new(Duration::from_millis(150)).with_easing(ease_in_out),
                    |this, delta| this.opacity(delta),
                );
            col = col.child(actions);
        }

        col.into_any_element()
    }

    fn chips(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.config.filters.len() <= 1 {
            return None;
        }
        let mut row = div().flex().items_center().gap_2().px_4().py_2();
        for (i, f) in self.config.filters.iter().enumerate() {
            let selected = i == self.active;
            row = row.child(
                div()
                    .px_3()
                    .py_1()
                    .rounded_lg()
                    .when(selected, |t| t.bg(theme::selected()))
                    .hover(|s| s.bg(theme::hover()))
                    .text_color(if selected {
                        theme::accent()
                    } else {
                        theme::muted()
                    })
                    .child(f.name.clone())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| this.select_filter(i, cx)),
                    ),
            );
        }
        Some(row.into_any_element())
    }

    /// Full-panel status picker (its own view). Selecting a transition applies
    /// it and returns to the list with fresh data (`apply_transition` →
    /// `refresh`); the back arrow cancels.
    fn render_status_picker(&self, picker: StatusPicker, cx: &mut Context<Self>) -> AnyElement {
        let header = div()
            .flex()
            .items_center()
            .gap_3()
            .px_4()
            .py_3()
            .child(
                div()
                    .size(px(28.))
                    .rounded_lg()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(theme::accent())
                    .text_xl()
                    .child("‹")
                    .hover(|s| s.bg(theme::hover()))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.picker = None;
                            cx.notify();
                        }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(div().text_xs().text_color(theme::muted()).child("Move issue to"))
                    .child(div().text_xl().text_color(theme::text()).child(picker.key.clone())),
            );

        let content: AnyElement = if picker.loading {
            centered("Loading statuses…")
        } else if picker.transitions.is_empty() {
            centered("No transitions available.")
        } else {
            let mut list = div()
                .id("jira-picker")
                .flex()
                .flex_col()
                .gap_1()
                .px_2()
                .py_2()
                .flex_1()
                .overflow_y_scroll()
                .track_scroll(&self.picker_nav.scroll);
            for (idx, t) in picker.transitions.iter().enumerate() {
                let key = picker.key.clone();
                let tid = t.id.clone();
                list = list.child(
                    div()
                        // Stateful id so gpui repaints hover on mouse-move inside
                        // the scroll container (stateless hover only updates while
                        // the list is actually scrolling).
                        .id(("jira-transition", idx))
                        .px_3()
                        .py_2()
                        .rounded_lg()
                        .when(idx == self.picker_nav.selected, |t| t.bg(theme::hover_strong()))
                        .when(idx != self.picker_nav.selected, |t| t.bg(theme::tile()))
                        .hover(|s| s.bg(theme::hover_strong()))
                        .text_color(theme::text())
                        .child(t.name.clone())
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                this.apply_transition(key.clone(), tid.clone(), cx)
                            }),
                        ),
                );
            }
            spotlight_ui::list::faded_scroll(&self.picker_nav.scroll, true, list.into_any_element())
        };

        div()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .size_full()
            .flex()
            .flex_col()
            .child(header)
            .child(div().h(px(1.)).bg(theme::divider()))
            .child(content)
            .into_any_element()
    }
}

impl Render for JiraView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Take keyboard focus when the panel first appears so arrow-key
        // navigation works without a click.
        if !self.focused_once {
            window.focus(&self.focus_handle, cx);
            self.focused_once = true;
        }

        // The status picker takes over the whole panel as its own view.
        if let Some(picker) = self.picker.clone() {
            return self.render_status_picker(picker, cx);
        }

        let body: AnyElement = if !self.issues.is_empty() {
            let mut list = div()
                .id("jira-list")
                .flex()
                .flex_col()
                .gap_1()
                .px_2()
                .py_2()
                .flex_1()
                .overflow_y_scroll()
                .track_scroll(&self.nav.scroll);
            for (i, issue) in self.issues.iter().enumerate() {
                list = list.child(self.issue_entry(i, issue, cx));
            }
            spotlight_ui::list::faded_scroll(&self.nav.scroll, true, list.into_any_element())
        } else if let Some(err) = &self.error {
            centered(err)
        } else if self.fetching {
            centered("Loading…")
        } else {
            centered("No issues match this filter.")
        };

        div()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .size_full()
            .flex()
            .flex_col()
            .when_some(self.chips(cx), |this, chips| this.child(chips))
            // Stale-while-revalidate indicator. The 2px height is reserved even
            // when idle so the list doesn't jump as the bar appears/disappears;
            // an indeterminate cyan bar sweeps while a refresh is in flight.
            .child(
                div().h(px(2.)).w_full().when(self.fetching, |this| {
                    this.bg(rgba(0x6e_e7ff_22)).child(
                        div().h(px(2.)).bg(theme::accent()).with_animation(
                            "jira-sync",
                            Animation::new(Duration::from_millis(1100))
                                .repeat()
                                .with_easing(linear),
                            |this, delta| this.w(px(660. * delta)).opacity(1.0 - delta * 0.3),
                        ),
                    )
                }),
            )
            .child(body)
            .into_any_element()
    }
}

/// A centered muted message filling the available space (empty/loading/error).
fn centered(msg: &str) -> AnyElement {
    div()
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .text_color(theme::muted())
        .child(msg.to_string())
        .into_any_element()
}

/// A small pill button used for the quick actions. `focused` marks the
/// keyboard-focused action with an accent outline + brighter fill.
fn quick_action(
    label: &str,
    focused: bool,
    on_click: impl Fn(&gpui::MouseDownEvent, &mut Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    div()
        // Stateful id so hover repaints on mouse-move within the scroll list.
        .id(gpui::SharedString::from(label.to_string()))
        .px_3()
        .py_1()
        .rounded_lg()
        .border_1()
        .border_color(if focused {
            theme::accent()
        } else {
            gpui::rgba(0x00_0000_00)
        })
        .bg(if focused {
            theme::hover_strong()
        } else {
            theme::tile()
        })
        .hover(|s| s.bg(theme::hover_strong()))
        .text_xs()
        .text_color(theme::accent())
        .child(label.to_string())
        .on_mouse_down(MouseButton::Left, on_click)
}
