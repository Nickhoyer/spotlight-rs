//! The Jira panel: filter chips, a stale-while-revalidate issue list with a
//! "syncing" indicator, per-row hover quick actions (Assign to me / Update
//! status), and a drill-in reading pane rendering the issue's description and
//! comments — Jira's server-rendered HTML drawn in-app via Blitz (links stay
//! clickable through the renderer's hit-testing). Escape/Left returns to the
//! list; the browser is always one "Open in Jira ↗" away.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    canvas, div, ease_in_out, img, linear, px, rgb, rgba, AnyElement, Animation,
    AnimationExt as _, Bounds, Context, FocusHandle, ImageSource, MouseButton, MouseDownEvent,
    Pixels, Point, RenderImage, ScrollHandle, Window,
};

use spotlight_config::{AppConfig, Recent};
use spotlight_htmlview::{HitTester, RenderOptions, Scheme};
use spotlight_ui::list::ListNav;
use spotlight_ui::theme;

use crate::client::JiraClient;
use crate::copy;
use crate::document::{self, DocStyle};
use crate::models::{self, Account, Issue};
use crate::JiraConfig;

/// Delay before hovering a row reveals its quick actions.
const HOVER_REVEAL_MS: u64 = 500;

/// Logical width the issue document renders at: the panel's content width (the
/// panel is fixed-width; a small margin keeps the card off the panel edges).
fn read_width() -> f32 {
    spotlight_ui::extension_panel_width() - 32.0
}

/// Arrow-key scroll step in the reading pane.
const SCROLL_STEP: f32 = 80.0;

/// The palette the issue document renders with, derived from the app's theme
/// tokens so the reading pane matches the panel around it.
///
/// The tokens are translucent, but a rendered image can't blend with the panel
/// the way an element would, so each is flattened to the solid color it
/// resolves to. The surface itself is the panel lifted by the same wash the
/// app puts under icon tiles — a subtle card rather than a white box — and code
/// blocks sit *back* at the panel color, so they read as recessed.
fn doc_style() -> DocStyle {
    let panel = theme::opaque_rgb(theme::PANEL_BG);
    let background = theme::wash(panel, theme::ICON_BG);
    DocStyle {
        background,
        text: theme::TEXT,
        muted: theme::MUTED,
        link: theme::ACCENT,
        border: theme::wash(background, theme::DIVIDER),
        code_bg: panel,
    }
}

/// The drill-in reading pane's content state.
enum ReadState {
    Loading,
    /// Blitz-rendered HTML: an image plus a live hit-tester for link clicks.
    Html {
        image: Arc<RenderImage>,
        hit: HitTester,
        logical_w: f32,
        logical_h: f32,
    },
    Failed(String),
}

struct Reading {
    key: String,
    summary: String,
    status: String,
    state: ReadState,
    /// The fetched issue, kept so "Copy for Claude" can render Markdown
    /// without refetching.
    detail: Option<models::IssueDetail>,
    /// Set briefly after a copy so the button can confirm it, along with how
    /// many images are waiting on the user's paste (0 = nothing follows).
    copied: bool,
    pending_images: usize,
    /// Live ⌘V watch from the last copy, if the ticket had attachments.
    /// Dropping it (closing the pane, copying again) ends the window.
    paste_watch: Option<spotlight_platform_macos::paste_watch::PasteWatch>,
}

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
    /// Open issue, if any (list stays behind it).
    reading: Option<Reading>,
    /// Scroll state for the reading pane.
    read_scroll: ScrollHandle,
    /// Last-painted bounds of the rendered-HTML card, for click→link mapping.
    body_bounds: Rc<Cell<Bounds<Pixels>>>,
    /// Debug aid: description HTML to open in the reading pane on first render
    /// (`SPOTLIGHT_JIRA_DEMO_HTML=<path>`), for headless captures — rendered
    /// without a configured client or network.
    demo_html: Option<String>,
    /// Debug aid: an issue key to drill into on first render
    /// (`SPOTLIGHT_JIRA_DEMO_ISSUE=SO-2522`). Unlike `demo_html` this runs the
    /// real fetch, so a capture exercises the actual field request — the path
    /// where a missing field looks like empty content rather than an error.
    demo_issue: Option<String>,
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
            reading: None,
            read_scroll: ScrollHandle::new(),
            body_bounds: Rc::new(Cell::new(Bounds::default())),
            demo_html: std::env::var("SPOTLIGHT_JIRA_DEMO_HTML")
                .ok()
                .and_then(|path| std::fs::read_to_string(path).ok()),
            demo_issue: std::env::var("SPOTLIGHT_JIRA_DEMO_ISSUE").ok(),
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
        self.reading = None;
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
                    Ok(mut issues) => {
                        models::sort_by_status(&mut issues);
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
        self.record_use(key, url);
    }

    /// Record an issue use (drill-in or browser open). The id matches the
    /// search-result id (`jira:KEY`) so panel-opens and search-opens share
    /// history (recents + relevance).
    fn record_use(&self, key: &str, url: String) {
        let mut cfg = AppConfig::load();
        cfg.record_use(Recent {
            id: format!("jira:{}", key),
            title: key.to_string(),
            subtitle: Some("Jira issue".to_string()),
            url: Some(url),
            path: None,
            icon: Some(crate::icon_path()),
            glyph: Some("🪐".to_string()),
            panel: None,
        });
        let _ = cfg.save();
    }

    // ---- reading pane -----------------------------------------------------

    /// Drill into an issue: fetch its rendered description + comments and
    /// draw them via the shared Blitz renderer. The header shows list-row data
    /// immediately while the fetch runs.
    fn open_reading(&mut self, issue: &Issue, window: &Window, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            // Unconfigured (shouldn't happen with rows visible, but stay safe).
            return;
        };
        let key = issue.key.clone();
        self.record_use(&key, client.browse_url(&key));
        self.read_scroll.set_offset(Point::default());
        self.reading = Some(Reading {
            key: key.clone(),
            summary: issue.summary.clone(),
            status: issue.status.clone(),
            state: ReadState::Loading,
            detail: None,
            copied: false,
            pending_images: 0,
            paste_watch: None,
        });

        let scale = window.scale_factor() as f64;
        let width = read_width() as u32;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let detail = client.issue_detail(&key)?;
                    let style = doc_style();
                    let doc = document::issue_document(&detail, &style);
                    // Load images by default: this is the user's own Jira
                    // site, not tracker-laden third-party mail. Auth rides
                    // along (same-origin only) so attachments resolve.
                    let rendered = spotlight_htmlview::render_html(
                        &doc,
                        RenderOptions {
                            logical_width: width,
                            scale,
                            load_images: true,
                            base_url: format!("{}/", client.base_url()),
                            auth: Some(client.auth_header().to_string()),
                            // We own this document's stylesheet, so it renders
                            // in the app's palette rather than on white.
                            scheme: Scheme::Dark {
                                background: style.background,
                                text: style.text,
                            },
                        },
                    )?;
                    anyhow::Ok((detail, rendered))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok((detail, (r, hit))) => {
                        // Upgrade header fields with fetched data (the list
                        // row's status can be stale).
                        if let Some(reading) = this.reading.as_mut() {
                            if reading.key == detail.key {
                                if !detail.summary.is_empty() {
                                    reading.summary = detail.summary.clone();
                                }
                                if !detail.status.is_empty() {
                                    reading.status = detail.status.clone();
                                }
                                reading.detail = Some(detail.clone());
                            }
                        }
                        let state = match image::RgbaImage::from_raw(r.width, r.height, r.bgra)
                        {
                            Some(buffer) => ReadState::Html {
                                image: Arc::new(RenderImage::new(vec![image::Frame::new(
                                    buffer,
                                )])),
                                hit,
                                logical_w: r.logical_width,
                                logical_h: r.logical_height,
                            },
                            None => ReadState::Failed(
                                "Couldn't display this issue.".to_string(),
                            ),
                        };
                        this.set_read_state(&detail.key, state);
                    }
                    Err(e) => {
                        this.set_read_state_any(ReadState::Failed(format!(
                            "Couldn't load issue: {e}"
                        )));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn set_read_state(&mut self, key: &str, state: ReadState) {
        if let Some(reading) = self.reading.as_mut() {
            if reading.key == key {
                reading.state = state;
            }
        }
    }

    fn set_read_state_any(&mut self, state: ReadState) {
        if let Some(reading) = self.reading.as_mut() {
            reading.state = state;
        }
    }

    fn close_reading(&mut self) {
        self.reading = None;
    }

    /// A click inside the rendered-HTML card: map window coordinates to
    /// document coordinates and open the link under the pointer, if any.
    /// Rendered Jira HTML is full of site-relative hrefs (`/browse/KEY`,
    /// attachment paths), resolved against the site base.
    fn click_body(&mut self, position: Point<Pixels>, _cx: &mut Context<Self>) {
        let Some(Reading {
            state: ReadState::Html { hit, .. },
            ..
        }) = &self.reading
        else {
            return;
        };
        let bounds = self.body_bounds.get();
        let local_x = f32::from(position.x - bounds.origin.x);
        let local_y = f32::from(position.y - bounds.origin.y);
        let Some(href) = hit.hit(local_x, local_y) else {
            return;
        };
        let href = href.trim();
        let url = if href.starts_with("https://") || href.starts_with("http://") || href.starts_with("mailto:") {
            Some(href.to_string())
        } else if href.starts_with('/') {
            self.client
                .as_ref()
                .map(|c| format!("{}{}", c.base_url(), href))
        } else {
            None
        };
        if let Some(url) = url {
            let _ = std::process::Command::new("/usr/bin/open").arg(&url).spawn();
        }
    }

    /// Put the open issue on the clipboard as Markdown, people pseudonymized,
    /// ready to paste into a chat. No-op until the fetch has landed.
    ///
    /// Inline images can't ride along on a text clipboard, so if the ticket
    /// has any we arm [`paste_watch`](spotlight_platform_macos::paste_watch)
    /// and hand them over when the user pastes — see [`Self::deliver_attachments`].
    fn copy_for_claude(&mut self, cx: &mut Context<Self>) {
        let Some(reading) = self.reading.as_mut() else {
            return;
        };
        let Some(detail) = reading.detail.clone() else {
            return;
        };
        let copied = copy::issue_markdown(&detail);
        spotlight_platform_macos::clipboard::write_text(&copied.markdown);
        reading.copied = true;
        // Attachments only arrive if we can actually see the user's paste.
        reading.pending_images = if spotlight_platform_macos::paste_watch::can_watch() {
            copied.attachments.len()
        } else {
            0
        };
        // Any previous window is over the moment a new copy happens.
        reading.paste_watch = None;
        if !copied.attachments.is_empty() {
            self.arm_attachment_delivery(copied, cx);
        }
        cx.notify();

        // Let the confirmation fade back to the resting label.
        let key = detail.key.clone();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(CONFIRM_FOR).await;
            let _ = this.update(cx, |this, cx| {
                if let Some(reading) = this.reading.as_mut() {
                    if reading.key == key {
                        reading.copied = false;
                        reading.pending_images = 0;
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    /// Fetch the ticket's images, then wait for the user's ⌘V and paste them in
    /// behind the text.
    ///
    /// The ⌘V is the point: it's the only signal macOS gives that a text field
    /// is focused and the target app is ready. We don't intercept it — the
    /// user's own paste delivers the text as normal, and only then do we take
    /// over the clipboard.
    fn arm_attachment_delivery(&mut self, copied: copy::Copied, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let markdown = copied.markdown.clone();
        let attachments = copied.attachments.clone();
        let (tx, rx) = std::sync::mpsc::channel();

        // Download while the user is still switching apps. The watch is armed
        // *now* rather than when this finishes, so a quick paste can't slip
        // through the gap; the callback waits on this channel instead.
        cx.background_executor()
            .spawn(async move {
                let images: Vec<Vec<u8>> = attachments
                    .iter()
                    .filter_map(|a| client.fetch_attachment_png(&a.url).ok())
                    .collect();
                let _ = tx.send(images);
            })
            .detach();

        let watch = spotlight_platform_macos::paste_watch::watch_for_paste(PASTE_WINDOW, move || {
            // Runs on the watch thread, so blocking here is fine.
            let Ok(images) = rx.recv_timeout(FETCH_WAIT) else {
                return;
            };
            if !images.is_empty() {
                deliver_attachments(images, markdown);
            }
        });
        if let Some(reading) = self.reading.as_mut() {
            reading.paste_watch = Some(watch);
        }
    }

    fn scroll_reading(&mut self, delta: f32) {
        let mut offset = self.read_scroll.offset();
        offset.y = px((f32::from(offset.y) + delta).min(0.0));
        self.read_scroll.set_offset(offset);
    }

    // ---- keyboard ---------------------------------------------------------

    fn on_key_down(&mut self, event: &gpui::KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();

        // Reading pane takes precedence while open.
        if let Some(reading) = &self.reading {
            match key {
                "escape" | "left" => self.close_reading(),
                "up" => self.scroll_reading(SCROLL_STEP),
                "down" => self.scroll_reading(-SCROLL_STEP),
                "pageup" => self.scroll_reading(SCROLL_STEP * 5.0),
                "pagedown" => self.scroll_reading(-SCROLL_STEP * 5.0),
                // Enter again (or o) continues to the browser.
                "enter" | "o" => {
                    let key = reading.key.clone();
                    self.open_issue(&key);
                }
                "c" => self.copy_for_claude(cx),
                _ => return,
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }

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
                self.activate_selected(window, cx);
                cx.stop_propagation();
                return;
            }
            // Let Escape bubble to the shell (back to Home), etc.
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn activate_selected(&mut self, window: &Window, cx: &mut Context<Self>) {
        let Some(issue) = self.issues.get(self.nav.selected).cloned() else {
            return;
        };
        match self.action_focus {
            None => self.open_reading(&issue, window, cx),
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
                let issue = issue.clone();
                cx.listener(move |this, _, window: &mut Window, cx| {
                    this.open_reading(&issue, window, cx)
                })
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
                // See the jira-list note: min-height:0 makes the overflow scroll.
                .min_h(px(0.))
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

    /// Full-panel reading pane: back arrow + key/status byline + summary on
    /// the left, "Open in Jira ↗" on the right, the rendered issue below.
    fn render_reading(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(reading) = &self.reading else {
            return centered("");
        };
        let key = reading.key.clone();
        // Copying needs the fetched issue, so the button only appears once it
        // has landed.
        let can_copy = reading.detail.is_some();
        let copied = reading.copied;
        let pending_images = reading.pending_images;
        let byline = if reading.status.is_empty() {
            key.clone()
        } else {
            format!("{} · {}", key, reading.status)
        };
        let summary = if reading.summary.is_empty() {
            key.clone()
        } else {
            reading.summary.clone()
        };

        let header = div()
            .flex()
            .items_center()
            .gap_3()
            .px_4()
            .py_3()
            .child(
                div()
                    .id("jira-read-back")
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
                            this.close_reading();
                            cx.notify();
                        }),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .child(div().text_xs().text_color(theme::muted()).child(byline))
                    .child(div().truncate().text_xl().text_color(theme::text()).child(summary)),
            )
            .when(can_copy, |this| {
                this.child(
                    div()
                        .id("jira-read-copy")
                        .flex_none()
                        .px_3()
                        .py_1()
                        .rounded_lg()
                        .bg(if copied {
                            theme::hover_strong()
                        } else {
                            theme::tile()
                        })
                        .hover(|s| s.bg(theme::hover_strong()))
                        .text_xs()
                        .text_color(theme::accent())
                        .child(match (copied, pending_images) {
                            (false, _) => "Copy for Claude".to_string(),
                            (true, 0) => "Copied ✓".to_string(),
                            // Tell the user their paste does something extra,
                            // or the images arriving would just be magic.
                            (true, 1) => "Copied ✓ · paste to add the image".to_string(),
                            (true, n) => format!("Copied ✓ · paste to add {n} images"),
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| this.copy_for_claude(cx)),
                        ),
                )
            })
            .child(
                div()
                    .id("jira-read-open")
                    .flex_none()
                    .px_3()
                    .py_1()
                    .rounded_lg()
                    .bg(theme::tile())
                    .hover(|s| s.bg(theme::hover_strong()))
                    .text_xs()
                    .text_color(theme::accent())
                    .child("Open in Jira ↗")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, _| this.open_issue(&key)),
                    ),
            );

        let content: AnyElement = match &reading.state {
            ReadState::Loading => centered("Loading issue…"),
            ReadState::Failed(msg) => centered(msg),
            ReadState::Html {
                image,
                logical_w,
                logical_h,
                ..
            } => {
                let bounds_cell = self.body_bounds.clone();
                let card = div()
                    .id("jira-read-card")
                    .relative()
                    .w(px(*logical_w))
                    .h(px(*logical_h))
                    .rounded_lg()
                    .overflow_hidden()
                    // The document's own canvas color under the image, so a
                    // failed paint shows an empty card (image problem) rather
                    // than nothing (layout problem) — and so the rounded
                    // corners the image can't draw still read as the surface.
                    .bg(gpui::rgb(doc_style().background))
                    .child(
                        img(ImageSource::Render(image.clone()))
                            .w(px(*logical_w))
                            .h(px(*logical_h)),
                    )
                    // Capture the card's painted bounds each frame so clicks
                    // can be mapped from window space into document space.
                    .child(
                        canvas(
                            move |bounds, _, _| bounds_cell.set(bounds),
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full(),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                            this.click_body(ev.position, cx)
                        }),
                    );
                self.scrollable_body(div().flex().justify_center().py_4().child(card))
            }
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

    fn scrollable_body(&self, inner: impl IntoElement) -> AnyElement {
        div()
            .id("jira-read-scroll")
            .flex_1()
            // min-height:0 lets this flex child shrink below its content so the
            // overflow actually scrolls (see the jira-list note).
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.read_scroll)
            .child(inner)
            .into_any_element()
    }

    /// Capture aid: render a file's HTML as a synthetic issue's description
    /// without a configured client or network.
    fn open_demo_reading(&mut self, html: String, window: &Window, cx: &mut Context<Self>) {
        self.read_scroll.set_offset(Point::default());
        self.reading = Some(Reading {
            key: "DEMO-1".to_string(),
            summary: "Demo issue".to_string(),
            status: "In Progress".to_string(),
            state: ReadState::Loading,
            detail: None,
            copied: false,
            pending_images: 0,
            paste_watch: None,
        });
        let detail = models::IssueDetail {
            key: "DEMO-1".to_string(),
            summary: "Demo issue".to_string(),
            status: "In Progress".to_string(),
            description_html: Some(html),
            ..Default::default()
        };
        let style = doc_style();
        let doc = document::issue_document(&detail, &style);
        let scale = window.scale_factor() as f64;
        let width = read_width() as u32;
        cx.spawn(async move |this, cx| {
            let rendered = cx
                .background_executor()
                .spawn(async move {
                    spotlight_htmlview::render_html(
                        &doc,
                        RenderOptions {
                            logical_width: width,
                            scale,
                            load_images: false,
                            base_url: "https://example.atlassian.net/".to_string(),
                            auth: None,
                            scheme: Scheme::Dark {
                                background: style.background,
                                text: style.text,
                            },
                        },
                    )
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                let state = match rendered {
                    Ok((r, hit)) => match image::RgbaImage::from_raw(r.width, r.height, r.bgra) {
                        Some(buffer) => ReadState::Html {
                            image: Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])),
                            hit,
                            logical_w: r.logical_width,
                            logical_h: r.logical_height,
                        },
                        None => ReadState::Failed("Couldn't display this issue.".to_string()),
                    },
                    Err(e) => ReadState::Failed(format!("Couldn't render: {e}")),
                };
                this.set_read_state("DEMO-1", state);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
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

        // Debug aids: drop straight into the reading pane, either with a
        // file's HTML or by really fetching an issue.
        if let Some(html) = self.demo_html.take() {
            self.open_demo_reading(html, window, cx);
        }
        if let Some(key) = self.demo_issue.take() {
            let issue = Issue {
                key,
                summary: String::new(),
                status: String::new(),
                status_color: models::StatusColor::Other,
                priority: String::new(),
                assignee: None,
                assignee_id: None,
            };
            self.open_reading(&issue, window, cx);
        }

        // The reading pane takes over the whole panel as its own view.
        if self.reading.is_some() {
            return self.render_reading(cx);
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
                // min-height:0 lets this flex child shrink below its content so the
                // overflow actually scrolls (without it the list grows to full
                // content height and is clipped, unscrollably, by the chrome).
                .min_h(px(0.))
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

/// How long the button keeps confirming the copy. Matches the paste window so
/// the hint is on screen for exactly as long as it is true.
const CONFIRM_FOR: Duration = Duration::from_secs(10);

/// How long after a copy we watch for the user's paste before giving up.
const PASTE_WINDOW: Duration = Duration::from_secs(10);

/// How long the paste callback will wait for a still-running download.
const FETCH_WAIT: Duration = Duration::from_secs(8);

/// Bundle id of the Claude desktop app — attachments are only injected when
/// it's the app in front, so a paste into anything else is left alone.
const CLAUDE_BUNDLE_ID: &str = "com.anthropic.claudefordesktop";

/// Grace period after the user's ⌘V before we touch the clipboard.
///
/// Their paste is already on its way to the app, which reads the pasteboard
/// while handling the keystroke — a few milliseconds later. Overwriting the
/// clipboard too early would turn their text paste into an image paste and
/// lose the ticket, and macOS offers no "paste completed" signal to wait on,
/// so this is a deliberate margin over an operation that takes microseconds.
const SETTLE: Duration = Duration::from_millis(150);

/// Gap between the images we paste. Each one is decoded and attached by the
/// receiving app, which is slower than a text paste.
const BETWEEN_IMAGES: Duration = Duration::from_millis(250);

/// Hand the ticket's images to the app the user just pasted into, one ⌘V each,
/// then put the text back on the clipboard.
///
/// Runs on the paste-watch thread once the tap is gone, so our own synthetic
/// keystrokes can't be mistaken for the user's.
fn deliver_attachments(images: Vec<Vec<u8>>, markdown: String) {
    // Only ever type into Claude. If the user pasted somewhere else, their
    // text landed there and we quietly stay out of it.
    if spotlight_platform_macos::apps::frontmost_bundle_id().as_deref() != Some(CLAUDE_BUNDLE_ID) {
        return;
    }
    std::thread::sleep(SETTLE);
    for png in &images {
        spotlight_platform_macos::clipboard::write_image_png(png);
        spotlight_platform_macos::input::paste();
        std::thread::sleep(BETWEEN_IMAGES);
    }
    // Leave the ticket text on the clipboard rather than the last image, so a
    // second ⌘V repeats the paste the user actually asked for.
    spotlight_platform_macos::clipboard::write_text(&markdown);
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
