//! GPUI shell for the launcher. This crate isolates all GPUI usage so the
//! pre-1.0 framework's churn touches one place.
//!
//! The root [`SpotlightView`] is a small router over a [`Screen`]: the floating
//! search box (Home/Search), a tabbed Settings screen, and full-screen
//! *extension panels* (e.g. Jira). Extensions contribute panels and settings
//! tabs as type-erased [`AnyView`]s via [`UiExtensions`], so this crate never
//! depends on any extension crate — `app` wires them together at startup.

pub mod controls;
pub mod list;
pub mod text_input;
pub mod theme;

mod settings;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::executor::block_on;
use gpui::prelude::*;
use gpui::{
    div, ease_in_out, img, linear, px, size, Animation, AnimationExt as _, AnyElement, AnyView,
    App, Bounds, Context, FocusHandle, ImageSource, KeyDownEvent, MouseButton, ObjectFit,
    RenderImage, Window, WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind,
    WindowOptions,
};

use spotlight_config::{AppConfig, Recent};
use spotlight_core::{Action, Icon, Registry, ResultItem};

use crate::settings::GeneralSettingsView;

const MAX_RESULTS: usize = 50;
/// How many recents to show on Home.
const MAX_RECENTS: usize = 5;

/// CoreGraphics window number of the launcher window, published once the window
/// exists so the debug capture thread (see [`run`]) can grab it. Zero until set.
static CAPTURE_WINDOW: AtomicU32 = AtomicU32::new(0);

/// A full-screen panel contributed by an extension (e.g. the Jira task list).
/// Surfaced as a shortcut on Home and rendered when navigated to.
pub struct PanelEntry {
    pub id: String,
    pub title: String,
    /// Leading glyph/emoji for the Home shortcut tile (fallback when `icon` unset).
    pub glyph: String,
    /// Optional path to an image file used as the shortcut icon instead of `glyph`.
    pub icon: Option<String>,
    /// Builds a fresh view each time the panel is opened. Reads current config
    /// itself, so settings edits take effect on the next open.
    pub make_view: Box<dyn Fn(&mut App) -> AnyView>,
}

/// A settings tab contributed by an extension.
pub struct SettingsTabFactory {
    pub title: String,
    pub make_view: Box<dyn Fn(&mut App) -> AnyView>,
}

/// GPUI-aware extension registrations, passed to [`run`] alongside the
/// (GPUI-free) [`Registry`].
#[derive(Default)]
pub struct UiExtensions {
    pub panels: Vec<PanelEntry>,
    pub settings_tabs: Vec<SettingsTabFactory>,
}

/// Which screen the launcher is currently showing.
#[derive(Clone, PartialEq)]
enum Screen {
    /// Empty search box + recents/shortcuts.
    Home,
    /// Search box with live results.
    Search,
    /// Tabbed settings.
    Settings,
    /// A full-screen extension panel, by panel id.
    Extension(String),
}

/// Keyboard selection on the Home screen. Recents are a horizontal strip
/// (Left/Right) and shortcuts a vertical list (Up/Down); Down/Up cross between.
/// `Shortcuts(i)` indexes panels first, then the trailing Settings entry.
#[derive(Clone, Copy, PartialEq)]
enum HomeSel {
    Recents(usize),
    Shortcuts(usize),
}

/// The root router view.
pub struct SpotlightView {
    registry: Arc<Registry>,
    ui: UiExtensions,
    config: AppConfig,
    screen: Screen,
    query: String,
    results: Vec<ResultItem>,
    selected: usize,
    focus_handle: FocusHandle,
    /// Rasterized app icons keyed by path, so each icon is rasterized once and
    /// reused across frames (gpui's `RenderImage` is cached by `Arc` identity).
    icon_cache: HashMap<PathBuf, Arc<RenderImage>>,
    /// The active extension panel view (built on navigation, dropped on leave).
    panel: Option<AnyView>,
    /// Settings tabs `(title, view)`, built on entering Settings, cleared on leave.
    settings_tabs: Vec<(String, AnyView)>,
    settings_active: usize,
    /// Home keyboard selection + scroll handles for its two lists.
    home_sel: HomeSel,
    recents_scroll: gpui::ScrollHandle,
    shortcuts_scroll: gpui::ScrollHandle,
    results_scroll: gpui::ScrollHandle,
    /// `observe_window_activation` fires once on registration; we skip that first
    /// call so we don't hide the panel during construction.
    activation_primed: bool,
    /// When the current open/exit reveal animation started. Driven manually
    /// (rather than via `with_animation`) so the reveal wrapper can stay
    /// id-less: an id-bearing wrapper would prefix every descendant's
    /// `GlobalElementId`, resetting the per-screen fade animations on each
    /// show/hide and making them fight the reveal.
    reveal_start: Instant,
    /// True while the reverse (exit) animation is playing. The native panel
    /// stays visible until it finishes, then is actually hidden.
    exiting: bool,
    /// Pending "hide the native panel once the exit animation finishes" timer.
    /// Held so a re-summon mid-exit can cancel it (dropping the task) and reveal
    /// again instead of vanishing.
    exit_task: Option<gpui::Task<()>>,
    /// Current rendered panel height, eased toward the active screen's target
    /// height each frame (`None` until the first render snaps it to target).
    cur_h: Option<f32>,
    /// Timestamp of the previous render, for the height easing's delta-time.
    last_frame: Instant,
    /// Active horizontal slide between screens of different depth.
    slide: Option<Slide>,
}

impl SpotlightView {
    fn new(
        registry: Arc<Registry>,
        ui: UiExtensions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);

        if let Some(ns_view) = appkit_view_ptr(window) {
            // Disable the native window shadow; we draw our own rounded one.
            spotlight_platform_macos::window::configure_panel(ns_view);
            // Publish the window number for the debug capture thread.
            if std::env::var_os("SPOTLIGHT_CAPTURE").is_some() {
                if let Some(num) = spotlight_platform_macos::capture::window_number(ns_view) {
                    CAPTURE_WINDOW.store(num, Ordering::SeqCst);
                }
            }
        }

        let mut view = Self {
            registry,
            ui,
            config: AppConfig::load(),
            screen: Screen::Home,
            query: String::new(),
            results: Vec::new(),
            selected: 0,
            focus_handle,
            icon_cache: HashMap::new(),
            panel: None,
            settings_tabs: Vec::new(),
            settings_active: 0,
            home_sel: HomeSel::Shortcuts(0),
            recents_scroll: gpui::ScrollHandle::new(),
            shortcuts_scroll: gpui::ScrollHandle::new(),
            results_scroll: gpui::ScrollHandle::new(),
            activation_primed: false,
            reveal_start: Instant::now(),
            exiting: false,
            exit_task: None,
            cur_h: None,
            last_frame: Instant::now(),
            slide: None,
        };
        view.reset_home_sel();

        // Hide the launcher when it loses focus — clicking outside, opening a
        // Jira link, or launching an app all deactivate our window. Skipped under
        // capture so screenshots aren't dismissed.
        if std::env::var_os("SPOTLIGHT_CAPTURE").is_none() {
            cx.observe_window_activation(window, |view, window, cx| {
                if !view.activation_primed {
                    view.activation_primed = true;
                    return;
                }
                if !window.is_window_active() {
                    view.hide(window, cx);
                }
            })
            .detach();
        }

        // Debug aid: pre-fill a query, or deep-link a screen, so captures can
        // show non-search states. `SPOTLIGHT_CAPTURE_SCREEN=settings|<panel-id>`.
        if let Ok(q) = std::env::var("SPOTLIGHT_CAPTURE_QUERY") {
            if !q.is_empty() {
                view.query = q;
                let query = view.query.clone();
                view.results = view.ranked_results(&query);
                view.screen = Screen::Search;
            }
        }
        if let Ok(screen) = std::env::var("SPOTLIGHT_CAPTURE_SCREEN") {
            match screen.as_str() {
                "settings" => view.go_settings(cx),
                id if !id.is_empty() => {
                    let id = id.to_string();
                    view.go_panel(&id, cx);
                }
                _ => {}
            }
            // Optionally deep-link a specific settings tab index for captures.
            if let Ok(tab) = std::env::var("SPOTLIGHT_CAPTURE_TAB") {
                if let Ok(i) = tab.parse::<usize>() {
                    if i < view.settings_tabs.len() {
                        view.settings_active = i;
                    }
                }
            }
        }

        view
    }

    // ---- navigation -------------------------------------------------------

    fn go_home(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let from = self.screen.clone();
        self.screen = Screen::Home;
        self.query.clear();
        self.results.clear();
        self.selected = 0;
        self.reset_home_sel();
        window.focus(&self.focus_handle, cx);
        // Keep panel/tabs alive if a slide will render the outgoing screen; the
        // slide-end cleanup (or `maybe_slide` when there's no slide) drops them.
        self.maybe_slide(from);
        cx.notify();
    }

    fn go_settings(&mut self, cx: &mut Context<Self>) {
        let from = self.screen.clone();
        let mut tabs: Vec<(String, AnyView)> =
            vec![("General".to_string(), cx.new(GeneralSettingsView::new).into())];
        for f in &self.ui.settings_tabs {
            tabs.push((f.title.clone(), (f.make_view)(cx)));
        }
        self.settings_tabs = tabs;
        self.settings_active = 0;
        self.screen = Screen::Settings;
        self.selected = 0;
        self.maybe_slide(from);
        cx.notify();
    }

    fn go_panel(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(entry) = self.ui.panels.iter().find(|p| p.id == id) else {
            return;
        };
        let from = self.screen.clone();
        let view = (entry.make_view)(cx);
        self.panel = Some(view);
        self.screen = Screen::Extension(id.to_string());
        self.selected = 0;
        self.maybe_slide(from);
        cx.notify();
    }

    fn screen_depth(s: &Screen) -> u8 {
        match s {
            Screen::Home | Screen::Search => 0,
            Screen::Settings | Screen::Extension(_) => 1,
        }
    }

    /// Start a horizontal slide if the navigation crossed a depth boundary (main
    /// ↔ settings/extension). Same-depth changes (e.g. Home↔Search) just let the
    /// height ease and swap content, so no slide is needed.
    fn maybe_slide(&mut self, from: Screen) {
        let to = self.screen.clone();
        if Self::screen_depth(&from) != Self::screen_depth(&to) {
            let forward = Self::screen_depth(&to) > Self::screen_depth(&from);
            self.slide = Some(Slide { from, forward, start: Instant::now() });
        } else {
            self.slide = None;
            self.cleanup_inactive_screens();
        }
    }

    /// Drop the backing state (panel view / settings tabs) of screens that aren't
    /// active. Safe once no slide still needs to render the outgoing screen.
    fn cleanup_inactive_screens(&mut self) {
        if !matches!(self.screen, Screen::Extension(_)) {
            self.panel = None;
        }
        if !matches!(self.screen, Screen::Settings) {
            self.settings_tabs.clear();
        }
    }

    /// Target panel height for a screen, given current state.
    fn screen_height(&self, screen: &Screen) -> f32 {
        match screen {
            Screen::Home => {
                let shortcuts = (self.ui.panels.len() + 1) as f32;
                let mut body =
                    HOME_BODY_PAD + SECTION_LABEL_H + (shortcuts * SHORTCUT_ROW_H).min(SHORTCUTS_MAX_H);
                if !self.config.recents().is_empty() {
                    body += SECTION_LABEL_H + RECENTS_STRIP_H;
                }
                SEARCH_ROW_H + DIVIDER_H + body
            }
            Screen::Search => {
                let n = self.results.len().min(MAX_RESULTS) as f32;
                if n == 0.0 {
                    SEARCH_ROW_H
                } else {
                    SEARCH_ROW_H + DIVIDER_H + (RESULTS_PAD + n * RESULT_ROW_H).min(RESULTS_MAX_H)
                }
            }
            Screen::Settings | Screen::Extension(_) => PANEL_H,
        }
    }

    // ---- query/search -----------------------------------------------------

    /// Query all extensions, then apply the frecency usage boost and re-rank, so
    /// frequently/recently used items float up among comparable matches.
    fn ranked_results(&self, query: &str) -> Vec<ResultItem> {
        let mut results = block_on(self.registry.query(query));
        for item in &mut results {
            item.score = item.score.saturating_add(self.config.usage_boost(&item.id));
        }
        results.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.title.cmp(&b.title)));
        results
    }

    fn refresh_query(&mut self, cx: &mut Context<Self>) {
        let query = self.query.clone();
        self.results = self.ranked_results(&query);
        self.selected = 0;
        self.screen = if self.query.trim().is_empty() {
            self.reset_home_sel();
            Screen::Home
        } else {
            Screen::Search
        };
        cx.notify();
    }

    /// Length of the currently navigable Search results list.
    fn current_len(&self) -> usize {
        match self.screen {
            Screen::Search => self.results.len().min(MAX_RESULTS),
            _ => 0,
        }
    }

    // ---- Home keyboard navigation -----------------------------------------

    fn reset_home_sel(&mut self) {
        self.home_sel = if self.recents_count() == 0 {
            HomeSel::Shortcuts(0)
        } else {
            HomeSel::Recents(0)
        };
    }

    fn recents_count(&self) -> usize {
        self.config.recents().len().min(MAX_RECENTS)
    }

    /// Panels plus the trailing Settings entry.
    fn shortcuts_count(&self) -> usize {
        self.ui.panels.len() + 1
    }

    fn home_nav(&mut self, key: &str) {
        let recents = self.recents_count();
        let shortcuts = self.shortcuts_count();
        self.home_sel = match (self.home_sel, key) {
            (HomeSel::Recents(i), "right") => {
                HomeSel::Recents((i + 1).min(recents.saturating_sub(1)))
            }
            (HomeSel::Recents(i), "left") => HomeSel::Recents(i.saturating_sub(1)),
            (HomeSel::Recents(_), "down") => HomeSel::Shortcuts(0),
            (HomeSel::Shortcuts(0), "up") if recents > 0 => HomeSel::Recents(0),
            (HomeSel::Shortcuts(i), "up") => HomeSel::Shortcuts(i.saturating_sub(1)),
            (HomeSel::Shortcuts(i), "down") => {
                HomeSel::Shortcuts((i + 1).min(shortcuts.saturating_sub(1)))
            }
            (other, _) => other,
        };
        let forward = matches!(key, "down" | "right");
        match self.home_sel {
            HomeSel::Recents(i) => list::peek(&self.recents_scroll, i, recents, forward),
            HomeSel::Shortcuts(i) => list::peek(&self.shortcuts_scroll, i, shortcuts, forward),
        }
    }

    fn home_activate(&mut self, cx: &mut Context<Self>) {
        match self.home_sel {
            HomeSel::Recents(i) => {
                if let Some(recent) = self.config.recents().get(i).cloned() {
                    self.reopen_and_record(recent, cx);
                }
            }
            HomeSel::Shortcuts(i) => {
                if i < self.ui.panels.len() {
                    let id = self.ui.panels[i].id.clone();
                    self.go_panel(&id, cx);
                } else {
                    self.go_settings(cx);
                }
            }
        }
    }

    fn activate_search(&mut self, cx: &mut Context<Self>) {
        let Some(item) = self.results.get(self.selected).cloned() else {
            return;
        };
        match &item.action {
            Action::Open(path) => {
                let _ = spotlight_platform_macos::apps::launch(path);
                self.record_activation(&item);
            }
            Action::OpenUrl(url) => {
                open_url(url);
                self.record_activation(&item);
            }
            Action::Copy(_text) => { /* TODO: clipboard integration */ }
            Action::Custom(id) => {
                if let Some(ext) = self.registry.owner(&item.source) {
                    let _ = ext.run(id);
                }
            }
            Action::None => {}
        }
        cx.notify();
    }

    /// Record an activated search result in the usage history (derives the
    /// reopen target + icon from the item so Home can show and re-open it).
    fn record_activation(&mut self, item: &ResultItem) {
        let (url, mut path) = match &item.action {
            Action::OpenUrl(u) => (Some(u.clone()), None),
            Action::Open(p) => (None, Some(p.display().to_string())),
            _ => (None, None),
        };
        let mut icon = None;
        let mut glyph = None;
        match &item.icon {
            Some(Icon::Image(p)) => icon = Some(p.display().to_string()),
            Some(Icon::File(p)) => {
                path.get_or_insert_with(|| p.display().to_string());
            }
            Some(Icon::Glyph(g)) => glyph = Some(g.clone()),
            _ => {}
        }
        self.persist_use(Recent {
            id: item.id.clone(),
            title: item.title.clone(),
            subtitle: item.subtitle.clone(),
            url,
            path,
            icon,
            glyph,
        });
    }

    /// Re-open a recent (URL or path) and record the use so it ranks higher.
    fn reopen_and_record(&mut self, recent: Recent, cx: &mut Context<Self>) {
        reopen_recent(&recent);
        self.persist_use(recent);
        cx.notify();
    }

    /// Append to the usage history. Loads/saves config fresh so it doesn't
    /// clobber history written by extensions (e.g. Jira).
    fn persist_use(&mut self, entry: Recent) {
        let mut cfg = AppConfig::load();
        cfg.record_use(entry);
        let _ = cfg.save();
        self.config = cfg;
    }

    // ---- window visibility ------------------------------------------------

    fn toggle_visibility(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ns_view) = appkit_view_ptr(window) else {
            return;
        };
        // Mid-exit the panel is still on screen; re-summoning cancels the pending
        // hide and reveals again rather than toggling it back off.
        if self.exiting || !spotlight_platform_macos::window::panel_visible(ns_view) {
            self.reveal(ns_view, window, cx);
        } else {
            self.hide(window, cx);
        }
    }

    /// Show the panel and (re)play the springy open-reveal from the next frame.
    fn reveal(
        &mut self,
        ns_view: *mut std::ffi::c_void,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Cancel any in-flight exit (drops the timer task) and drop the reverse.
        self.exit_task = None;
        self.exiting = false;
        // Snap height to the (Home) target so it doesn't ease down from the last
        // screen's height while the open animation plays.
        self.cur_h = None;
        self.slide = None;
        spotlight_platform_macos::window::show_panel(ns_view);
        // Restart the springy open-reveal from the next frame.
        self.reveal_start = Instant::now();
        // Reload config so settings changed last session take effect, and start
        // fresh on Home.
        self.config = AppConfig::load();
        self.go_home(window, cx);
    }

    /// Play the exit animation (the reveal reversed), then hide the native panel
    /// once it finishes. Under capture we hide immediately so screenshots don't
    /// race the animation.
    fn hide(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.exiting {
            return;
        }
        let Some(ns_view) = appkit_view_ptr(window) else {
            return;
        };
        if std::env::var_os("SPOTLIGHT_CAPTURE").is_some() {
            spotlight_platform_macos::window::hide_panel(ns_view);
            return;
        }
        // Keep the panel on screen and play the reverse animation.
        self.exiting = true;
        self.reveal_start = Instant::now();
        cx.notify();
        self.exit_task = Some(cx.spawn_in(window, async move |weak, cx| {
            cx.background_executor().timer(Duration::from_millis(EXIT_MS)).await;
            let _ = weak.update_in(cx, |this, window, _cx| {
                // A re-summon may have cleared `exiting` and cancelled us first;
                // guard so a late tick can't hide a freshly revealed panel.
                if !this.exiting {
                    return;
                }
                if let Some(ns_view) = appkit_view_ptr(window) {
                    spotlight_platform_macos::window::hide_panel(ns_view);
                }
                // Stay in the exited state (no re-render): the last painted frame
                // is the fully-faded-out one, so the next `reveal` fades in
                // cleanly from nothing. `reveal` clears `exiting`. Flipping it
                // false + notifying here would instead paint a fresh, fully-sharp
                // reveal onto the hidden surface, which then flashes on reopen.
            });
        }));
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.clone();
        let modifiers = event.keystroke.modifiers;

        // Escape: back out one level (works on every screen).
        if key == "escape" {
            match self.screen {
                Screen::Search => self.go_home(window, cx),
                Screen::Home => self.hide(window, cx),
                _ => self.go_home(window, cx),
            }
            return;
        }

        // Extension panels handle their own keys; the shell only owns Escape there.
        if matches!(self.screen, Screen::Extension(_)) {
            return;
        }

        // Settings: keyboard navigation. Left/Right switch tabs (only reaches
        // here when no text field is focused — focused fields consume arrows and
        // stop propagation), Tab/Shift-Tab move focus between fields, Enter steps
        // into the form.
        if self.screen == Screen::Settings {
            match key.as_str() {
                "tab" => {
                    if modifiers.shift {
                        window.focus_prev(cx);
                    } else {
                        window.focus_next(cx);
                    }
                }
                "enter" => window.focus_next(cx),
                "left" => {
                    self.settings_active = self.settings_active.saturating_sub(1);
                    cx.notify();
                }
                "right" => {
                    if self.settings_active + 1 < self.settings_tabs.len() {
                        self.settings_active += 1;
                        cx.notify();
                    }
                }
                _ => {}
            }
            return;
        }

        // Home: 2D keyboard navigation over recents (horizontal) + shortcuts
        // (vertical). Typed characters fall through to start a search.
        if self.screen == Screen::Home {
            match key.as_str() {
                "up" | "down" | "left" | "right" => {
                    self.home_nav(key.as_str());
                    cx.notify();
                    return;
                }
                "enter" => {
                    self.home_activate(cx);
                    return;
                }
                "backspace" => return,
                _ => {}
            }
        }

        // Home/Search: paste appends to the query.
        if modifiers.platform && key == "v" {
            if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
                self.query.push_str(&text.replace(['\n', '\r'], " "));
                self.refresh_query(cx);
            }
            return;
        }

        match key.as_str() {
            "backspace" => {
                self.query.pop();
                self.refresh_query(cx);
            }
            "up" => {
                self.selected = self.selected.saturating_sub(1);
                list::peek(&self.results_scroll, self.selected, 0, false);
                cx.notify();
            }
            "down" => {
                let len = self.current_len();
                if self.selected + 1 < len {
                    self.selected += 1;
                    list::peek(&self.results_scroll, self.selected, len, true);
                    cx.notify();
                }
            }
            "enter" => self.activate_search(cx),
            _ => {
                if !modifiers.platform && !modifiers.control {
                    if let Some(ch) = &event.keystroke.key_char {
                        self.query.push_str(ch);
                        self.refresh_query(cx);
                    }
                }
            }
        }
    }

    // ---- rendering --------------------------------------------------------

    /// Inner content for Home/Search (no panel chrome). `on_home` selects the
    /// recents/shortcuts body vs. live results.
    fn main_content(&mut self, on_home: bool, cx: &mut Context<Self>) -> AnyElement {
        let caret = div()
            .w(px(2.))
            .h(px(24.))
            .rounded_full()
            .bg(theme::accent())
            .with_animation(
                "caret",
                Animation::new(Duration::from_millis(1100))
                    .repeat()
                    .with_easing(linear),
                |this, delta| this.opacity(if delta < 0.5 { 1.0 } else { 0.15 }),
            );

        let search_row = div()
            .flex()
            .items_center()
            .gap_3()
            .px_5()
            .py_4()
            .child(div().text_2xl().text_color(theme::accent()).child("⌕"))
            // Query text, then the caret, then the placeholder (only when empty).
            // Keeping the caret right after the (possibly empty) query text means
            // it doesn't jump when the first character is typed.
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(div().text_xl().text_color(theme::text()).child(self.query.clone()))
                    .child(caret)
                    .when(self.query.is_empty(), |row| {
                        row.child(
                            div()
                                .ml_3()
                                .text_xl()
                                .text_color(theme::muted())
                                .child("Search apps, do math…"),
                        )
                    }),
            );

        let mut panel = div().w_full().flex().flex_col().child(search_row);

        if on_home {
            let home = self.render_home_body(cx);
            panel = panel
                .child(div().h(px(1.)).bg(theme::divider()))
                .child(home);
        } else if !self.results.is_empty() {
            // `ScrollHandle` is Rc-backed, so cloning shares the same scroll
            // state with the field; this lets the closure below keep its disjoint
            // borrow of `icon_cache`.
            let scroll = self.results_scroll.clone();
            let inner = {
                let results = &self.results;
                let selected = self.selected;
                let icon_cache = &mut self.icon_cache;
                div()
                    .id("search-results")
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_2()
                    .pt_1()
                    .pb_6()
                    .max_h(px(320.))
                    .overflow_y_scroll()
                    .track_scroll(&scroll)
                    .children(results.iter().take(MAX_RESULTS).enumerate().map(
                        |(i, item)| {
                            let icon = resolve_icon(icon_cache, &item.icon);
                            result_row(item, icon, i == selected)
                        },
                    ))
                    .into_any_element()
            };
            panel = panel
                .child(div().h(px(1.)).bg(theme::divider()))
                .child(list::faded_scroll(&scroll, false, inner));
        }

        panel.into_any_element()
    }

    fn render_home_body(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let mut col = div().flex().flex_col().px_2().pb_2().gap_1();

        // Recently opened — a horizontal, scrollable strip of compact cards, so
        // it uses width rather than stacking vertically. Derived (de-duplicated)
        // from the usage history.
        let recents = self.config.recents();
        if !recents.is_empty() {
            col = col.child(section_label("Recently opened"));
            let mut strip = div()
                .id("home-recents")
                .flex()
                .flex_row()
                .gap_2()
                .px_1()
                .pb_1()
                .overflow_x_scroll()
                .track_scroll(&self.recents_scroll);
            for (i, recent) in recents.iter().take(MAX_RECENTS).enumerate() {
                let selected = self.home_sel == HomeSel::Recents(i);
                let entry = recent.clone();
                strip = strip.child(
                    div()
                        .id(("home-recent", i))
                        .flex_shrink_0()
                        .w(px(92.))
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .py_3()
                        .rounded_xl()
                        .when(selected, |t| t.bg(theme::selected()))
                        .hover(|s| s.bg(theme::hover()))
                        .child(recent_leading(&mut self.icon_cache, recent, 40.))
                        .child(
                            div()
                                .w_full()
                                .text_center()
                                .text_sm()
                                .truncate()
                                .text_color(theme::text())
                                .child(recent.title.clone()),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                this.reopen_and_record(entry.clone(), cx)
                            }),
                        ),
                );
            }
            col = col.child(strip);
        }

        // Shortcuts — extension panels + Settings, in a vertical scroll list.
        col = col.child(section_label("Shortcuts"));
        let mut list = div()
            .id("home-shortcuts")
            .flex()
            .flex_col()
            .gap_1()
            .max_h(px(190.))
            .overflow_y_scroll()
            .track_scroll(&self.shortcuts_scroll);
        for (i, p) in self.ui.panels.iter().enumerate() {
            let selected = self.home_sel == HomeSel::Shortcuts(i);
            let id = p.id.clone();
            let leading = match &p.icon {
                Some(icon) => image_leading(&mut self.icon_cache, icon, 28.)
                    .unwrap_or_else(|| glyph_tile(&p.glyph)),
                None => glyph_tile(&p.glyph),
            };
            list = list.child(
                simple_row(leading, &p.title, selected).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| this.go_panel(&id, cx)),
                ),
            );
        }
        let settings_selected = self.home_sel == HomeSel::Shortcuts(self.ui.panels.len());
        list = list.child(
            simple_row(glyph_tile("⚙"), "Settings", settings_selected).on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.go_settings(cx)),
            ),
        );
        col = col.child(list::faded_scroll(
            &self.shortcuts_scroll,
            false,
            list.into_any_element(),
        ));

        col.into_any_element()
    }

    /// Inner content for Settings (header + tabs + active tab body, no chrome).
    fn settings_content(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let active = self.settings_active;
        let mut rail = div().flex().items_center().gap_1().px_2().pt_2().pb_2();
        for (i, (title, _)) in self.settings_tabs.iter().enumerate() {
            let selected = i == active;
            rail = rail.child(
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
                    .child(title.clone())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.settings_active = i;
                            cx.notify();
                        }),
                    ),
            );
        }

        let body = self
            .settings_tabs
            .get(active)
            .map(|(_, view)| view.clone().into_any_element())
            .unwrap_or_else(|| div().into_any_element());

        shell_content(
            "Settings",
            cx,
            div()
                .flex()
                .flex_col()
                .size_full()
                .child(rail)
                .child(div().h(px(1.)).bg(theme::divider()))
                .child(div().flex_1().overflow_hidden().px_5().py_4().child(body)),
        )
    }

    /// Inner content for an extension panel (header + panel view, no chrome).
    fn panel_content(&mut self, id: &str, cx: &mut Context<Self>) -> AnyElement {
        let title = self
            .ui
            .panels
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.title.clone())
            .unwrap_or_default();
        let body = self
            .panel
            .clone()
            .map(|v| v.into_any_element())
            .unwrap_or_else(|| div().into_any_element());
        shell_content(&title, cx, div().size_full().child(body))
    }

    /// A screen's content sized to its natural height (no chrome). The chrome
    /// clips to the eased height, so the content stays laid out at its own size
    /// and is revealed/hidden as the panel resizes, and slide cells get a
    /// definite height. Used by both the normal render and the slide.
    fn content_for(&mut self, screen: &Screen, cx: &mut Context<Self>) -> AnyElement {
        let h = self.screen_height(screen);
        let inner: AnyElement = match screen {
            Screen::Home => self.main_content(true, cx),
            Screen::Search => self.main_content(false, cx),
            Screen::Settings => self.settings_content(cx),
            Screen::Extension(id) => {
                let id = id.clone();
                self.panel_content(&id, cx)
            }
        };
        div().w_full().h(px(h)).flex().flex_col().child(inner).into_any_element()
    }

    /// Advance the eased panel height toward the active screen's target and return
    /// it, requesting another frame while still moving.
    fn tick_height(&mut self, window: &Window) -> f32 {
        let target = self.screen_height(&self.screen);
        let now = Instant::now();
        let dt = (now - self.last_frame).as_secs_f32().min(0.05);
        self.last_frame = now;
        let h = match self.cur_h {
            None => target,
            Some(cur) => {
                let k = 1.0 - (-dt / HEIGHT_TAU).exp();
                let next = cur + (target - cur) * k;
                if (next - target).abs() < 0.5 {
                    target
                } else {
                    window.request_animation_frame();
                    next
                }
            }
        };
        self.cur_h = Some(h);
        h
    }

    /// Build the panel body: normally `chrome(height, content)`, or a sliding
    /// two-screen track during a depth transition.
    fn render_body(&mut self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let h = self.tick_height(window);

        let Some(slide) = self.slide.clone() else {
            let screen = self.screen.clone();
            let content = self.content_for(&screen, cx);
            return chrome(h, content).into_any_element();
        };

        let raw = (slide.start.elapsed().as_secs_f32() / (SLIDE_MS as f32 / 1000.0)).clamp(0.0, 1.0);
        if raw < 1.0 {
            window.request_animation_frame();
        }
        let e = ease_in_out(raw);

        let to_screen = self.screen.clone();
        let from_content = self.content_for(&slide.from, cx);
        let to_content = self.content_for(&to_screen, cx);
        let cell = |content: AnyElement| {
            div().w(px(OPEN_BASE_W)).flex_shrink_0().child(content)
        };
        // Forward: [from, to], track slides left (0 → -W). Back: [to, from], track
        // slides right (-W → 0). Either way the incoming screen ends centered.
        let (left, right, offset) = if slide.forward {
            (cell(from_content), cell(to_content), -OPEN_BASE_W * e)
        } else {
            (cell(to_content), cell(from_content), -OPEN_BASE_W * (1.0 - e))
        };
        let track = div()
            .flex()
            .flex_row()
            .items_start()
            .ml(px(offset))
            .child(left)
            .child(right);
        chrome(h, track.into_any_element()).into_any_element()
    }
}

impl Render for SpotlightView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Retire a finished slide and drop the outgoing screen's backing state.
        if let Some(slide) = &self.slide {
            if slide.start.elapsed() >= Duration::from_millis(SLIDE_MS) {
                self.slide = None;
                self.cleanup_inactive_screens();
            }
        }

        // Same top offset on every screen so the panel's top edge stays put when
        // navigating (otherwise Settings/extension panels jump upward).
        let body = self.render_body(window, cx);

        div()
            .key_context("Spotlight")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            // Set an explicit font + default text color on the root so all text
            // inherits a known-present family rather than relying on the default.
            .font_family("Helvetica Neue")
            .text_color(theme::text())
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .pt(px(PANEL_TOP))
            .child(self.open_reveal(body, window))
    }
}

/// Duration of the blur portion of the reveal.
const OPEN_MS: u64 = 420;
/// Duration of the springy width settle.
const OPEN_SPRING_MS: u64 = 480;
/// Blur radius (logical px) the panel starts at before sharpening.
const OPEN_BLUR: f32 = 18.0;
/// Initial width multiplier: the panel begins clearly larger than its resting
/// size, then springs down to 1.0. GPUI `Div`s have no CSS scale transform, so
/// we approximate it by animating the container width.
const OPEN_START_SCALE: f32 = 1.20;
/// How many times the width overshoots its resting size before settling. 1.0 =
/// a single gentle overshoot; higher is bouncier.
const OPEN_SPRING_BOUNCES: f32 = 1.0;
/// Spring damping. Lower = the overshoot is deeper and decays more slowly (looser
/// and springier); higher settles sooner with a shallower dip.
const OPEN_SPRING_DAMPING: f32 = 5.5;
/// How fast the panel fades in relative to the blur clock. Kept high so the
/// panel is already visible while it's at its widest — otherwise the big start
/// happens under near-zero opacity and the reveal reads as growing, not shrinking.
const OPEN_FADE_GAIN: f32 = 4.5;

/// Exit: the panel drops away under gravity while fading out (a different, one-way
/// animation — not the open spring reversed). Duration of the drop.
const EXIT_MS: u64 = 340;
/// Total fall distance (logical px) under constant downward acceleration. The
/// panel is fully faded before it gets near this, so it never reaches the window
/// edge — it just needs to be large enough to read as accelerating.
const EXIT_FALL_PX: f32 = 260.0;
/// Fade-out speed relative to the fall. >1 so the panel is invisible well before
/// it falls far enough to clip at the window's bottom edge.
const EXIT_FADE_GAIN: f32 = 1.35;
/// Resting panel width; the open-reveal container owns it and the inner panels
/// fill it (`w_full`), so animating this scales the whole panel.
const OPEN_BASE_W: f32 = 680.0;
/// Top offset of the panel within the window, shared by every screen so the top
/// edge stays fixed when navigating. The 440px-tall panels fit below it.
const PANEL_TOP: f32 = 140.0;

// --- Per-screen heights, so the panel can animate its height between screens. ---
/// Settings + extension panels (fixed).
const PANEL_H: f32 = 440.0;
const SEARCH_ROW_H: f32 = 64.0;
const DIVIDER_H: f32 = 1.0;
const SECTION_LABEL_H: f32 = 32.0;
const SHORTCUT_ROW_H: f32 = 48.0;
const SHORTCUTS_MAX_H: f32 = 190.0;
const RECENTS_STRIP_H: f32 = 96.0;
const HOME_BODY_PAD: f32 = 20.0;
const RESULT_ROW_H: f32 = 65.0;
const RESULTS_PAD: f32 = 28.0;
const RESULTS_MAX_H: f32 = 320.0;
/// Exponential time-constant (seconds) for easing the panel height toward its
/// target. Smaller = snappier. Height eases continuously, so any change (screen
/// switch or result count) resizes smoothly.
const HEIGHT_TAU: f32 = 0.055;
/// Horizontal slide duration for depth changes (main ↔ settings/extension).
const SLIDE_MS: u64 = 260;

/// An in-progress horizontal slide between screens of different depth. The height
/// is handled separately by the continuous height easing.
#[derive(Clone)]
struct Slide {
    /// The screen being slid away.
    from: Screen,
    /// True when the new screen enters from the right (going deeper); false when
    /// it enters from the left (going back).
    forward: bool,
    start: Instant,
}

/// An under-damped spring settling to 1.0: rises past 1.0, then oscillates around
/// it with shrinking overshoots. Used as the width's "progress" so the panel
/// bounces around its resting size a couple of times before coming to rest.
fn spring_out(x: f32) -> f32 {
    use std::f32::consts::PI;
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    1.0 - (-OPEN_SPRING_DAMPING * x).exp() * (OPEN_SPRING_BOUNCES * 2.0 * PI * x).cos()
}

impl SpotlightView {
    /// Wrap the panel body in the springy open-reveal: it begins blurred,
    /// transparent, and stretched a touch wide, then unblurs, fades in, and
    /// springs down to its resting width. The exit is the same curve reversed.
    ///
    /// Driven manually from `reveal_start` (not `with_animation`) so the wrapper
    /// can stay id-less — an id-bearing wrapper would prefix every descendant's
    /// `GlobalElementId` and reset the per-screen fades on every show/hide.
    /// Skipped under capture so screenshots stay crisp.
    fn open_reveal(&self, body: AnyElement, window: &Window) -> AnyElement {
        if std::env::var_os("SPOTLIGHT_CAPTURE").is_some() {
            return div().w(px(OPEN_BASE_W)).child(body).into_any_element();
        }
        let secs = self.reveal_start.elapsed().as_secs_f32();
        if self.exiting {
            return self.exit_fall(body, secs, window);
        }

        // Open: blur/fade and the width spring run on separate clocks off one start.
        let raw_blur = (secs / (OPEN_MS as f32 / 1000.0)).clamp(0.0, 1.0);
        let raw_spring = (secs / (OPEN_SPRING_MS as f32 / 1000.0)).clamp(0.0, 1.0);
        // Width springs from OPEN_START_SCALE, settling on 1.0 with one overshoot.
        let scale = OPEN_START_SCALE + (1.0 - OPEN_START_SCALE) * spring_out(raw_spring);
        // Blur clears well before the width finishes settling.
        let blur = OPEN_BLUR * (1.0 - (raw_blur * 1.5).min(1.0));
        // Fast fade so the panel is visible while it's still large — otherwise the
        // big start happens under near-zero opacity and reads as growing.
        let opacity = (raw_blur * OPEN_FADE_GAIN).min(1.0);
        if raw_blur < 1.0 || raw_spring < 1.0 {
            window.request_animation_frame();
        }
        div()
            .w(px(OPEN_BASE_W * scale))
            .child(body)
            .blur(px(blur))
            .opacity(opacity)
            .into_any_element()
    }

    /// Exit animation: the panel drops away under gravity while fading. Physics is
    /// a body released from rest under constant acceleration, so the offset grows
    /// with the square of elapsed time (`y = ½·g·t²`).
    fn exit_fall(&self, body: AnyElement, secs: f32, window: &Window) -> AnyElement {
        let raw = (secs / (EXIT_MS as f32 / 1000.0)).clamp(0.0, 1.0);
        let fall = EXIT_FALL_PX * raw * raw;
        let opacity = (1.0 - raw * EXIT_FADE_GAIN).clamp(0.0, 1.0);
        // Blur in as it drops, reaching full right as it fades out (same gain as
        // the fade), so it softens away rather than dropping in sharp focus.
        let blur = OPEN_BLUR * (raw * EXIT_FADE_GAIN).min(1.0);
        if raw < 1.0 {
            window.request_animation_frame();
        }
        div()
            .w(px(OPEN_BASE_W))
            .mt(px(fall))
            .child(body)
            .blur(px(blur))
            .opacity(opacity)
            .into_any_element()
    }
}

/// The panel box (chrome): rounded, bordered, shadowed, clipped, at an explicit
/// height. Content is placed inside; a single one of these wraps every screen so
/// the box appears to resize/slide as its height and content change.
fn chrome(height: f32, content: AnyElement) -> gpui::Div {
    div()
        .w_full()
        .h(px(height))
        .bg(theme::panel_bg())
        .rounded_3xl()
        .border_1()
        .border_color(theme::border())
        .shadow_lg()
        .overflow_hidden()
        .flex()
        .flex_col()
        .child(content)
}

/// Inner content for a full-screen screen: a back chevron + title header, a
/// divider, then `content`. No chrome — the caller wraps it in [`chrome`].
fn shell_content(
    title: &str,
    cx: &mut Context<SpotlightView>,
    content: impl IntoElement,
) -> AnyElement {
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
                    cx.listener(|this, _, window, cx| this.go_home(window, cx)),
                ),
        )
        .child(div().text_xl().text_color(theme::text()).child(title.to_string()));

    // `flex_1` so this fills the natural-height wrapper `content_for` places it
    // in, letting the body's own `flex_1` expand to the remaining space.
    div()
        .w_full()
        .flex_1()
        .min_h(px(0.))
        .flex()
        .flex_col()
        .child(header)
        .child(div().h(px(1.)).bg(theme::divider()))
        .child(div().flex_1().overflow_hidden().child(content))
        .into_any_element()
}

fn section_label(text: &str) -> impl IntoElement {
    div()
        .px_3()
        .pt_2()
        .pb_1()
        .text_xs()
        .text_color(theme::muted())
        .child(text.to_string())
}

/// A Home shortcut row: a leading icon element + title.
fn simple_row(leading: AnyElement, title: &str, selected: bool) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap_3()
        .px_3()
        .py_2()
        .rounded_lg()
        .when(selected, |this| this.bg(theme::selected()))
        .hover(|s| s.bg(theme::hover()))
        .child(leading)
        .child(div().text_color(theme::text()).child(title.to_string()))
}

/// A 28px cyan tile holding a glyph/emoji — the default row icon.
fn glyph_tile(glyph: &str) -> AnyElement {
    div()
        .size(px(28.))
        .rounded_md()
        .bg(theme::tile())
        .flex()
        .items_center()
        .justify_center()
        .text_color(theme::accent())
        .child(glyph.to_string())
        .into_any_element()
}

/// Decode an image file (e.g. PNG) into a cached `RenderImage`, swapping
/// RGBA→BGRA to match gpui's `RenderImage` byte order.
fn load_image_file(
    cache: &mut HashMap<PathBuf, Arc<RenderImage>>,
    path: &std::path::Path,
) -> Option<Arc<RenderImage>> {
    if let Some(cached) = cache.get(path) {
        return Some(cached.clone());
    }
    let bytes = std::fs::read(path).ok()?;
    let decoded = image::load_from_memory(&bytes).ok()?.to_rgba8();
    let (w, h) = decoded.dimensions();
    let mut raw = decoded.into_raw();
    for px in raw.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let buffer = image::RgbaImage::from_raw(w, h, raw)?;
    let render = Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]));
    cache.insert(path.to_path_buf(), render.clone());
    Some(render)
}

/// A leading element rendering the image at `path` at `size` px, or `None` if it
/// can't be loaded.
fn image_leading(
    cache: &mut HashMap<PathBuf, Arc<RenderImage>>,
    path: &str,
    size: f32,
) -> Option<AnyElement> {
    let image = load_image_file(cache, std::path::Path::new(path))?;
    Some(
        div()
            .size(px(size))
            .flex()
            .items_center()
            .justify_center()
            .child(
                img(ImageSource::Render(image))
                    .w(px(size))
                    .h(px(size))
                    .object_fit(ObjectFit::Contain),
            )
            .into_any_element(),
    )
}

/// Resolve an item's icon to a cached gpui `RenderImage`. Only `Icon::File`
/// (app bundles) is rasterized here; glyphs render as text and everything else
/// falls back to the letter tile. Rasterization happens once per path and the
/// `Arc<RenderImage>` is reused, so re-renders are cheap and the texture isn't
/// re-uploaded each frame.
fn resolve_icon(
    cache: &mut HashMap<PathBuf, Arc<RenderImage>>,
    icon: &Option<Icon>,
) -> Option<Arc<RenderImage>> {
    let path = match icon {
        Some(Icon::File(path)) => path,
        // Logo images are decoded directly rather than icon-ized by the OS.
        Some(Icon::Image(path)) => return load_image_file(cache, path),
        _ => return None,
    };
    if let Some(cached) = cache.get(path) {
        return Some(cached.clone());
    }
    let pixels = spotlight_platform_macos::icons::icon_for_file(path)?;
    // The rasterizer emits premultiplied-alpha RGBA; gpui's `RenderImage` wants
    // straight-alpha BGRA. Un-premultiply, then swap R<->B (bytes 0&2) — the same
    // RGBA→BGRA reorder gpui's own image loader does.
    let mut bytes = (*pixels.data).clone();
    for px in bytes.chunks_exact_mut(4) {
        let a = px[3];
        if a != 0 && a != 255 {
            let unmul = |c: u8| ((c as u16 * 255 + a as u16 / 2) / a as u16).min(255) as u8;
            px[0] = unmul(px[0]);
            px[1] = unmul(px[1]);
            px[2] = unmul(px[2]);
        }
        px.swap(0, 2);
    }
    let buffer = image::RgbaImage::from_raw(pixels.width, pixels.height, bytes)?;
    let frame = image::Frame::new(buffer);
    let render_image = Arc::new(RenderImage::new(vec![frame]));
    cache.insert(path.clone(), render_image.clone());
    Some(render_image)
}

fn result_row(item: &ResultItem, icon: Option<Arc<RenderImage>>, selected: bool) -> impl IntoElement {
    let accent = theme::accent();
    let leading = if let Some(render_image) = icon {
        // Real app icon (rasterized from NSWorkspace). Contain-fit so square
        // app icons don't stretch within the 28px slot; the wrapper div fixes
        // the slot size so the leading column aligns with glyph/letter tiles.
        div()
            .size(px(28.))
            .flex()
            .items_center()
            .justify_center()
            .child(
                img(ImageSource::Render(render_image))
                    .w(px(28.))
                    .h(px(28.))
                    .object_fit(ObjectFit::Contain),
            )
    } else {
        match &item.icon {
            Some(Icon::Glyph(glyph)) => div().text_2xl().child(glyph.clone()),
            _ => div()
                .size(px(28.))
                .rounded_md()
                .bg(theme::tile())
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(accent)
                .child(
                    item.title
                        .chars()
                        .next()
                        .map(|c| c.to_uppercase().to_string())
                        .unwrap_or_default(),
                ),
        }
    };

    div()
        .flex()
        .items_center()
        .gap_3()
        .px_3()
        .py_2()
        .rounded_lg()
        .when(selected, |this| this.bg(theme::selected()))
        .child(leading)
        .child(
            div()
                .flex()
                .flex_col()
                .child(div().text_color(theme::text()).child(item.title.clone()))
                .when_some(item.subtitle.clone(), |this, sub| {
                    this.child(div().text_xs().text_color(theme::muted()).child(sub))
                }),
        )
}

/// Open a URL with the OS default browser.
fn open_url(url: &str) {
    let _ = std::process::Command::new("/usr/bin/open").arg(url).spawn();
}

/// Re-open a recent: a URL in the browser, or a filesystem path (e.g. an app).
fn reopen_recent(recent: &Recent) {
    if let Some(url) = &recent.url {
        open_url(url);
    } else if let Some(path) = &recent.path {
        let _ = spotlight_platform_macos::apps::launch(std::path::Path::new(path));
    }
}

/// Leading icon for a recent card at `size` px: a custom image icon if present,
/// else the system file icon (apps), else a glyph, else a clock fallback.
fn recent_leading(
    cache: &mut HashMap<PathBuf, Arc<RenderImage>>,
    recent: &Recent,
    size: f32,
) -> AnyElement {
    // Custom extension logo (e.g. the Jira icon).
    if let Some(icon) = &recent.icon {
        if let Some(el) = image_leading(cache, icon, size) {
            return el;
        }
    }
    // App / file system icon via NSWorkspace.
    if let Some(path) = &recent.path {
        let icon = Some(Icon::File(PathBuf::from(path)));
        if let Some(image) = resolve_icon(cache, &icon) {
            return div()
                .size(px(size))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    img(ImageSource::Render(image))
                        .w(px(size))
                        .h(px(size))
                        .object_fit(ObjectFit::Contain),
                )
                .into_any_element();
        }
    }
    div()
        .size(px(size))
        .flex()
        .items_center()
        .justify_center()
        .text_2xl()
        .child(recent.glyph.clone().unwrap_or_else(|| "🕘".to_string()))
        .into_any_element()
}

/// Extract the AppKit `NSView` pointer from a gpui window, if any.
fn appkit_view_ptr(window: &Window) -> Option<*mut std::ffi::c_void> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    // UFCS: gpui's inherent `Window::window_handle` would shadow the trait one.
    match HasWindowHandle::window_handle(window).ok()?.as_raw() {
        RawWindowHandle::AppKit(h) => Some(h.ns_view.as_ptr()),
        _ => None,
    }
}

/// If `SPOTLIGHT_CAPTURE` is set, spawn a thread that waits for the window to
/// appear, lets it render, captures it to that PNG path, and exits. Lets the
/// agent verify renders without an external screenshot tool. Env knobs:
/// `SPOTLIGHT_CAPTURE` (output path), `SPOTLIGHT_CAPTURE_DELAY_MS` (default 1000),
/// `SPOTLIGHT_CAPTURE_QUERY` (pre-filled search text),
/// `SPOTLIGHT_CAPTURE_SCREEN` (`settings` or a panel id to deep-link).
fn spawn_capture_thread() {
    let Ok(path) = std::env::var("SPOTLIGHT_CAPTURE") else {
        return;
    };
    let delay_ms: u64 = std::env::var("SPOTLIGHT_CAPTURE_DELAY_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000);

    std::thread::spawn(move || {
        // Wait (up to ~10s) for the window number to be published.
        let mut window_id = 0;
        for _ in 0..200 {
            window_id = CAPTURE_WINDOW.load(Ordering::SeqCst);
            if window_id != 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if window_id == 0 {
            eprintln!("capture: window never appeared");
            std::process::exit(2);
        }
        // Let it paint at least one frame.
        std::thread::sleep(Duration::from_millis(delay_ms));
        match spotlight_platform_macos::capture::capture_window_png(
            window_id,
            std::path::Path::new(&path),
        ) {
            Ok(()) => {
                eprintln!("capture: wrote {path}");
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("capture failed: {e}");
                std::process::exit(3);
            }
        }
    });
}

/// Open the launcher window and run the GPUI application loop.
pub fn run(registry: Registry, ui: UiExtensions) {
    let registry = Arc::new(registry);
    spawn_capture_thread();
    // `Application::run` and `open_window`'s builder are both `FnOnce`, so the
    // (non-Clone) `UiExtensions` can be moved straight through to the view.
    gpui_platform::application().run(move |cx: &mut App| {
        // No Dock icon / app menu — run as a background accessory like Spotlight.
        // The PopUp panel still floats over fullscreen Spaces and can take focus.
        spotlight_platform_macos::window::set_accessory_activation_policy();
        cx.activate(true);
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        // Window is larger than the 680px resting panel so the animations have
        // transparent margin to bleed into rather than clipping at the window
        // edge: width for the open spring's ~1.2× stretch plus blur, and height
        // for the exit drop. The panel stays centered.
        let bounds = Bounds::centered(None, size(px(880.), px(720.)), cx);
        let window_handle = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: None,
                    window_background: WindowBackgroundAppearance::Transparent,
                    kind: WindowKind::PopUp,
                    is_movable: false,
                    is_resizable: false,
                    is_minimizable: false,
                    focus: true,
                    show: true,
                    ..Default::default()
                },
                move |window, cx| cx.new(|cx| SpotlightView::new(registry, ui, window, cx)),
            )
            .expect("failed to open launcher window");

        register_global_hotkey(cx, window_handle);
    });
}

/// Register the system-wide hotkey that summons the launcher. The hotkey fires
/// on the main thread; we hand off to gpui's foreground executor via
/// `AsyncApp::spawn` so the view update runs at a safe point in the run loop
/// rather than re-entering a borrow mid-update.
///
/// The returned `GlobalHotkey` is leaked: it must outlive the `run` closure's
/// stack frame (which returns immediately after setup), and the process is a
/// single long-lived launcher, so unregistering at exit is unnecessary.
fn register_global_hotkey(cx: &mut App, window_handle: WindowHandle<SpotlightView>) {
    let spec = std::env::var("SPOTLIGHT_HOTKEY").unwrap_or_else(|_| "cmd+space".to_string());
    let (key_code, modifiers) = match spotlight_platform_macos::hotkey::parse(&spec) {
        Ok(parsed) => parsed,
        Err(e) => {
            eprintln!("spotlight: bad SPOTLIGHT_HOTKEY=`{spec}` ({e}); defaulting to cmd+space");
            (49, spotlight_platform_macos::hotkey::CMD_KEY)
        }
    };

    let async_cx = cx.to_async();
    let hotkey = match spotlight_platform_macos::hotkey::GlobalHotkey::register(
        key_code,
        modifiers,
        Box::new(move || {
            let handle = window_handle;
            async_cx
                .spawn(async move |cx| {
                    let _ = handle.update(cx, |view, window, cx| {
                        view.toggle_visibility(window, cx);
                    });
                })
                .detach();
        }),
    ) {
        Ok(hk) => hk,
        Err(e) => {
            eprintln!("spotlight: failed to register global hotkey `{spec}`: {e}");
            return;
        }
    };
    // Keep the registration alive for the lifetime of the process.
    Box::leak(Box::new(hotkey));
}
