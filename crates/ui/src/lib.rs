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

mod logo;
mod settings;

pub use logo::emit_iconset;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::executor::block_on;
use gpui::prelude::*;
use gpui::{
    div, ease_in_out, img, linear, point, px, size, Animation, AnimationExt as _, AnyElement, AnyView,
    App, Bounds, ClipboardItem, Context, FocusHandle, ImageSource, KeyDownEvent, KeyUpEvent, MouseButton,
    MouseDownEvent, ObjectFit, RenderImage, Window, WindowBackgroundAppearance, WindowBounds,
    WindowHandle, WindowKind, WindowOptions,
};

use spotlight_config::{AppConfig, Recent};
use spotlight_core::{Action, Icon, Registry, ResultItem};

use crate::settings::GeneralSettingsView;

const MAX_RESULTS: usize = 50;
/// How many recents to show on Home.
const MAX_RECENTS: usize = 5;
/// Don't ask the AI to autocomplete until the query is at least this long.
const MIN_AC_LEN: usize = 2;
/// Debounce before firing an autocomplete request, so mid-word keystrokes don't
/// each spawn a call.
const AC_DEBOUNCE_MS: u64 = 250;

/// How long Escape must be held before it collapses the whole navigation stack
/// straight back to Home, instead of tapping it once per level.
const ESCAPE_HOLD_MS: u64 = 500;

/// CoreGraphics window number of the launcher window, published once the window
/// exists so the debug capture thread (see [`run`]) can grab it. Zero until set.
static CAPTURE_WINDOW: AtomicU32 = AtomicU32::new(0);

/// Set by [`hide_launcher_window`] so the next window-deactivation (which the
/// order-out itself triggers) is swallowed rather than kicking off a second,
/// animated hide. Cleared when consumed.
static SUPPRESS_ACTIVATION_HIDE: AtomicBool = AtomicBool::new(false);

/// Hide the launcher immediately from within an extension panel (e.g. after
/// copying a clipboard entry). Orders the panel out and suppresses the redundant
/// activation-driven hide that would otherwise fire.
pub fn hide_launcher_window(window: &Window) {
    if let Some(ns_view) = appkit_view_ptr(window) {
        SUPPRESS_ACTIVATION_HIDE.store(true, Ordering::SeqCst);
        spotlight_platform_macos::window::hide_panel(ns_view);
    }
}

/// Decode PNG bytes into a cached-ready gpui [`RenderImage`], reordering
/// RGBA→BGRA to match gpui's byte order. Lets extension panels render images
/// (e.g. clipboard image previews) without duplicating the pixel plumbing.
pub fn render_image_from_png_bytes(bytes: &[u8]) -> Option<Arc<RenderImage>> {
    decode_image_bytes(bytes)
}

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
    /// itself, so settings edits take effect on the next open. Receives the
    /// search text the panel was opened from (`None` when opened from Home /
    /// recents), so panels like the LLM chat can seed themselves with it.
    pub make_view: Box<dyn Fn(&mut App, Option<&str>) -> AnyView>,
}

/// A settings tab contributed by an extension.
pub struct SettingsTabFactory {
    pub title: String,
    pub make_view: Box<dyn Fn(&mut App) -> AnyView>,
}

/// A menu-bar item contributed by an extension. The action runs on the main
/// thread with a gpui [`App`] context (bridged from the native menu click), so it
/// can touch config, spawn tasks, or quit — mirroring [`PanelEntry`].
pub struct MenuItem {
    pub title: String,
    pub action: Box<dyn Fn(&mut App)>,
}

/// Request handed to an [`AutocompleteProvider`]. Runs on a background thread, so
/// everything here is owned.
pub struct AutocompleteRequest {
    /// The current search text.
    pub query: String,
    /// A strong local match (top result title) offered to the model as a
    /// candidate completion, when the local result was high-scoring.
    pub top_hint: Option<String>,
}

/// Ghost text plus ready-made "Ask AI" suggestion rows returned by an
/// [`AutocompleteProvider`].
pub struct Suggestions {
    /// Inline continuation of the query (only the text that *follows* it); empty
    /// when there's nothing to suggest.
    pub ghost: String,
    /// Complete result rows to splice into the list (already carrying their own
    /// [`Action::OpenPanel`] seed).
    pub entries: Vec<ResultItem>,
}

/// An AI autocomplete source contributed by an extension. `suggest` is called off
/// the UI thread (blocking network is fine) and returns `None` when disabled or
/// unconfigured. Wrapped in an `Arc` so the shell can clone it into a background
/// task.
#[derive(Clone)]
pub struct AutocompleteProvider {
    pub suggest: Arc<dyn Fn(AutocompleteRequest) -> Option<Suggestions> + Send + Sync>,
}

/// A live "now playing" state source contributed by an extension, rendered as a
/// card at the top of Home.
///
/// Deliberately data-only rather than an [`AnyView`]: the shell owns the layout,
/// the Home height math, the arrow navigation and the selection pill, and none
/// of those three can reach inside a foreign view — Home's navigation is
/// hand-wired rather than focus-based, and the pill is drawn by the shell from
/// a [`gpui::ScrollHandle`] it holds itself. The extension owns polling,
/// artwork and playback control.
pub struct NowPlayingSource {
    /// Current state, or `None` when nothing is loaded (the card is hidden).
    /// Read once per frame on the UI thread, so it must be a cheap in-memory
    /// read — never I/O.
    pub snapshot: Arc<dyn Fn() -> Option<NowPlayingSnapshot> + Send + Sync>,
    /// "Home is on screen": poll at an interactive rate for the next few
    /// seconds. Lets the source stay completely idle while the launcher is
    /// closed instead of polling around the clock.
    pub poke: Arc<dyn Fn() + Send + Sync>,
    /// Run a transport command. Must return immediately (the work happens on
    /// the source's own thread).
    pub control: Arc<dyn Fn(Transport) + Send + Sync>,
}

/// One observation of what the music player is doing, as the shell sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct NowPlayingSnapshot {
    pub title: String,
    pub artist: String,
    pub album: String,
    /// True only while actually playing; paused still shows the card, because a
    /// card that vanishes the instant you hit pause reads as a bug.
    pub playing: bool,
    pub duration_secs: f64,
    pub position_secs: f64,
    /// When `position_secs` was sampled, so the shell can interpolate the
    /// progress bar between the source's polls.
    pub sampled_at: Instant,
    /// Path to a small PNG of the track artwork, or `None` (radio and streams
    /// frequently have none).
    pub artwork: Option<PathBuf>,
}

/// A transport command sent back to the [`NowPlayingSource`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    PlayPause,
    Next,
    Prev,
}

/// GPUI-aware extension registrations, passed to [`run`] alongside the
/// (GPUI-free) [`Registry`].
#[derive(Default)]
pub struct UiExtensions {
    pub panels: Vec<PanelEntry>,
    pub settings_tabs: Vec<SettingsTabFactory>,
    /// Extra entries added to the menu-bar menu (between the built-in
    /// Open/Settings and Launch-at-Login/Quit items).
    pub menu_items: Vec<MenuItem>,
    /// Optional inline AI autocomplete + "Ask AI" suggestion source.
    pub autocomplete: Option<AutocompleteProvider>,
    /// Optional now-playing card shown at the top of Home while music is loaded.
    pub now_playing: Option<NowPlayingSource>,
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
/// `NowPlaying(i)` indexes the transport row: 0 previous, 1 play/pause, 2 next.
#[derive(Clone, Copy, PartialEq)]
enum HomeSel {
    NowPlaying(usize),
    Recents(usize),
    Shortcuts(usize),
}

/// The Home selection an arrow key moves to. A free function so the arm
/// ordering — which is what makes Up prefer Recents over the now-playing card —
/// can be tested without a window.
fn next_home_sel(
    sel: HomeSel,
    key: &str,
    recents: usize,
    shortcuts: usize,
    np: bool,
) -> HomeSel {
    match (sel, key) {
        (HomeSel::NowPlaying(i), "right") => HomeSel::NowPlaying((i + 1).min(NP_BUTTONS - 1)),
        (HomeSel::NowPlaying(i), "left") => HomeSel::NowPlaying(i.saturating_sub(1)),
        (HomeSel::NowPlaying(_), "down") if recents > 0 => HomeSel::Recents(0),
        (HomeSel::NowPlaying(_), "down") => HomeSel::Shortcuts(0),

        (HomeSel::Recents(i), "right") => HomeSel::Recents((i + 1).min(recents.saturating_sub(1))),
        (HomeSel::Recents(i), "left") => HomeSel::Recents(i.saturating_sub(1)),
        (HomeSel::Recents(_), "down") => HomeSel::Shortcuts(0),
        // Land on play/pause rather than an edge button.
        (HomeSel::Recents(_), "up") if np => HomeSel::NowPlaying(NP_PLAY),

        // Recents keep priority on the way up; the card is only reached
        // directly from Shortcuts when there are no recents in between.
        (HomeSel::Shortcuts(_), "up") if recents > 0 => HomeSel::Recents(0),
        (HomeSel::Shortcuts(_), "up") if np => HomeSel::NowPlaying(NP_PLAY),
        (HomeSel::Shortcuts(i), "right") => {
            HomeSel::Shortcuts((i + 1).min(shortcuts.saturating_sub(1)))
        }
        (HomeSel::Shortcuts(i), "left") => HomeSel::Shortcuts(i.saturating_sub(1)),
        (other, _) => other,
    }
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
    /// Greyed-out inline autocomplete: the continuation shown after the caret and
    /// accepted with Tab. Seeded instantly from a strong local match, then
    /// replaced by the LLM's completion when it arrives.
    ghost: String,
    /// AI-suggested "Ask AI" rows for `ai_query`, spliced into `results`.
    ai_suggestions: Vec<ResultItem>,
    /// The query `ghost`/`ai_suggestions` were computed for; guards against
    /// splicing stale suggestions after the query has moved on.
    ai_query: String,
    /// Bumped on every query change so a late autocomplete response for an older
    /// query can be dropped.
    ac_gen: u64,
    /// The in-flight debounced autocomplete task (dropped/cancelled on the next
    /// keystroke by being replaced).
    ac_task: Option<gpui::Task<()>>,
    focus_handle: FocusHandle,
    /// Rasterized app icons keyed by path, so each icon is rasterized once and
    /// reused across frames (gpui's `RenderImage` is cached by `Arc` identity).
    icon_cache: HashMap<PathBuf, Arc<RenderImage>>,
    /// The active extension panel view (built on navigation, dropped on leave).
    panel: Option<AnyView>,
    /// Settings tabs `(title, view)`, built on entering Settings, cleared on leave.
    settings_tabs: Vec<(String, AnyView)>,
    settings_active: usize,
    /// Focus handles for the settings tab-rail chips (parallel to `settings_tabs`),
    /// so the rail is Tab-reachable. Rebuilt in `go_settings`, cleared on leave.
    settings_tab_focuses: Vec<FocusHandle>,
    /// Home keyboard selection + scroll handles for its two lists.
    home_sel: HomeSel,
    recents_scroll: gpui::ScrollHandle,
    shortcuts_scroll: gpui::ScrollHandle,
    results_scroll: gpui::ScrollHandle,
    /// Latest snapshot pulled from `ui.now_playing`, refreshed once at the top
    /// of `render`. Cached rather than re-read on demand because
    /// `screen_height` runs twice per frame and has to stay a pure, synchronous
    /// sum of constants.
    np: Option<NowPlayingSnapshot>,
    /// Decoded artwork for `np.artwork`. A single slot rather than an entry in
    /// `icon_cache`, which is never evicted — album art would accumulate there
    /// for the life of the process.
    np_art: Option<(PathBuf, Arc<RenderImage>)>,
    /// Bounds recorder for the transport row so the selection pill can track
    /// its three buttons. There is no overflow scrolling here; `track_scroll`
    /// records child bounds regardless.
    np_scroll: gpui::ScrollHandle,
    /// True while the panel is on screen. Gates the now-playing poke timer, so
    /// a launcher that is never summoned runs no AppleScript at all.
    revealed: bool,
    /// Whether the now-playing poll loop has been started (once per view).
    np_watching: bool,
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
    /// Pending "hold-Escape collapses the whole stack back to Home" timer. Armed
    /// on a fresh Escape press (on a screen with somewhere to go back to) and
    /// dropped — cancelling it — the moment Escape is released, so only a
    /// sustained hold fires it. See [`SpotlightView::on_escape_capture`].
    escape_hold: Option<gpui::Task<()>>,
    /// Current rendered panel height, eased toward the active screen's target
    /// height each frame (`None` until the first render snaps it to target).
    cur_h: Option<f32>,
    /// Current rendered panel width, eased toward the active screen's target width
    /// each frame (`None` until the first render snaps it to target). Lets Settings
    /// be wider than Home without a jump.
    cur_w: Option<f32>,
    /// Timestamp of the previous render, for the height easing's delta-time.
    last_frame: Instant,
    /// Active horizontal slide between screens of different depth.
    slide: Option<Slide>,
    /// Selection-highlight pill: current rect (content-space) + velocities, eased
    /// toward the selected item's measured bounds each frame. `hl_ctx` tracks
    /// which list owns it (snapped on switch); `hl_ready` gates rendering until
    /// item bounds exist.
    hl_x: f32,
    hl_y: f32,
    hl_w: f32,
    hl_h: f32,
    hl_vx: f32,
    hl_vy: f32,
    hl_vw: f32,
    hl_vh: f32,
    hl_ctx: Option<HlContext>,
    hl_ready: bool,
    /// Smooth scrolling that keeps the selection in view at the pill's speed
    /// (GPUI's built-in `scroll_to_item` jumps). Runs only after a selection
    /// change so it never fights mouse-wheel scrolling.
    hl_last: Option<(HlContext, usize)>,
    scroll_target: f32,
    scroll_vel: f32,
    scroll_anim: bool,
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
            ghost: String::new(),
            ai_suggestions: Vec::new(),
            ai_query: String::new(),
            ac_gen: 0,
            ac_task: None,
            focus_handle,
            icon_cache: HashMap::new(),
            panel: None,
            settings_tabs: Vec::new(),
            settings_active: 0,
            settings_tab_focuses: Vec::new(),
            home_sel: HomeSel::Shortcuts(0),
            recents_scroll: gpui::ScrollHandle::new(),
            shortcuts_scroll: gpui::ScrollHandle::new(),
            results_scroll: gpui::ScrollHandle::new(),
            np: None,
            np_art: None,
            np_scroll: gpui::ScrollHandle::new(),
            // Headless capture shows the window at launch without going
            // through `reveal`, so seed the flag the same way `run` does.
            revealed: std::env::var_os("SPOTLIGHT_CAPTURE").is_some(),
            np_watching: false,
            activation_primed: false,
            reveal_start: Instant::now(),
            exiting: false,
            exit_task: None,
            escape_hold: None,
            cur_h: None,
            cur_w: None,
            last_frame: Instant::now(),
            slide: None,
            hl_x: 0.,
            hl_y: 0.,
            hl_w: 0.,
            hl_h: 0.,
            hl_vx: 0.,
            hl_vy: 0.,
            hl_vw: 0.,
            hl_vh: 0.,
            hl_ctx: None,
            hl_ready: false,
            hl_last: None,
            scroll_target: 0.,
            scroll_vel: 0.,
            scroll_anim: false,
        };
        view.reset_home_sel();

        // Hide the launcher when it loses focus — this is the click-outside
        // dismissal path (clicking another window/app deactivates us). Result
        // activation hides explicitly (see `activate_search`), so this also acts
        // as a harmless backstop there. Skipped under capture so screenshots
        // aren't dismissed.
        if std::env::var_os("SPOTLIGHT_CAPTURE").is_none() {
            cx.observe_window_activation(window, |view, window, cx| {
                if !view.activation_primed {
                    view.activation_primed = true;
                    return;
                }
                if !window.is_window_active() {
                    // A panel dismissing itself (clipboard copy) already ordered
                    // the window out; swallow the resulting deactivation so we
                    // don't play a second, animated hide over the hidden panel.
                    if SUPPRESS_ACTIVATION_HIDE.swap(false, Ordering::SeqCst) {
                        return;
                    }
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
                // Go through the real query path so captures exercise the ghost
                // autocomplete and any AI suggestion rows.
                view.refresh_query(cx);
            }
        }
        if let Ok(screen) = std::env::var("SPOTLIGHT_CAPTURE_SCREEN") {
            match screen.as_str() {
                "settings" => view.go_settings(cx),
                id if !id.is_empty() => {
                    let id = id.to_string();
                    view.go_panel(&id, None, cx);
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
        // Place the Home selection for captures, so the highlight pill can be
        // photographed on a specific control. `np:<i>` / `recents:<i>` /
        // `shortcuts:<i>`.
        if let Ok(sel) = std::env::var("SPOTLIGHT_CAPTURE_HOME_SEL") {
            let (kind, i) = sel.split_once(':').unwrap_or((sel.as_str(), "0"));
            let i = i.parse::<usize>().unwrap_or(0);
            match kind {
                "np" => view.home_sel = HomeSel::NowPlaying(i.min(NP_BUTTONS - 1)),
                "recents" => view.home_sel = HomeSel::Recents(i),
                "shortcuts" => view.home_sel = HomeSel::Shortcuts(i),
                _ => {}
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
        // The ghost belongs to the query we just dropped. Left behind it renders
        // alongside the placeholder on Home, and Tab would accept it.
        self.ghost.clear();
        self.ac_task = None;
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
        self.settings_tab_focuses = tabs.iter().map(|_| cx.focus_handle()).collect();
        self.settings_tabs = tabs;
        self.settings_active = 0;
        self.screen = Screen::Settings;
        self.selected = 0;
        self.maybe_slide(from);
        cx.notify();
    }

    /// `seed` is the search text the panel was opened from (`None` from Home /
    /// recents), passed through to the panel's `make_view` so it can pre-fill.
    fn go_panel(&mut self, id: &str, seed: Option<String>, cx: &mut Context<Self>) {
        let Some(entry) = self.ui.panels.iter().find(|p| p.id == id) else {
            return;
        };
        // Snapshot what we need for the recents entry before the borrows below.
        let title = entry.title.clone();
        let glyph = entry.glyph.clone();
        let icon = entry.icon.clone();
        let from = self.screen.clone();
        let view = (entry.make_view)(cx, seed.as_deref());
        self.panel = Some(view);
        self.screen = Screen::Extension(id.to_string());
        self.selected = 0;
        // Record the open so the panel shows up (and ranks) in recents/search.
        self.persist_use(Recent {
            id: format!("panel:{id}"),
            title,
            subtitle: None,
            url: None,
            path: None,
            icon,
            glyph: Some(glyph),
            panel: Some(id.to_string()),
        });
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
            self.settings_tab_focuses.clear();
        }
    }

    /// Target panel height for a screen, given current state.
    fn screen_height(&self, screen: &Screen) -> f32 {
        match screen {
            Screen::Home => {
                let shortcuts = (self.ui.panels.len() + 1) as f32;
                let mut body =
                    HOME_BODY_PAD + SECTION_LABEL_H + (shortcuts * SHORTCUT_ROW_H).min(SHORTCUTS_MAX_H);
                if self.np.is_some() {
                    body += SECTION_LABEL_H + NOW_PLAYING_H;
                }
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

    /// Target panel width for a screen. Settings (and extension panels) are wider
    /// than the search-shaped Home/Search so a proper settings layout fits.
    fn panel_width(&self, screen: &Screen) -> f32 {
        match screen {
            Screen::Home | Screen::Search => OPEN_BASE_W,
            Screen::Settings | Screen::Extension(_) => SETTINGS_W,
        }
    }

    /// Top offset of the panel within the window for a screen. The large Settings
    /// panel sits higher than the search-shaped screens so it reads as centered.
    fn panel_top(&self, screen: &Screen) -> f32 {
        match screen {
            Screen::Home | Screen::Search => PANEL_TOP,
            Screen::Settings | Screen::Extension(_) => SETTINGS_TOP,
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

    /// Splice the AI "Ask AI" suggestion rows (when current for this query) into a
    /// ranked list, grouped just above the generic Ask AI entry (same source).
    fn splice_suggestions(&self, mut ranked: Vec<ResultItem>) -> Vec<ResultItem> {
        if self.ai_query == self.query {
            if let Some(first) = self.ai_suggestions.first() {
                let src = first.source.clone();
                let pos = ranked.iter().position(|r| r.source == src).unwrap_or(ranked.len());
                for (i, item) in self.ai_suggestions.iter().enumerate() {
                    ranked.insert(pos + i, item.clone());
                }
            }
        }
        ranked
    }

    /// Recompute `results` from the registry and re-splice AI suggestions. Called
    /// when an autocomplete response arrives (the query itself is unchanged).
    fn rebuild_results(&mut self) {
        let ranked = self.ranked_results(&self.query);
        self.results = self.splice_suggestions(ranked);
    }

    fn refresh_query(&mut self, cx: &mut Context<Self>) {
        let query = self.query.clone();
        // A new query supersedes any pending autocomplete and its (now stale)
        // ghost. Keep prior suggestions only if they were computed for this exact
        // query (e.g. re-entering after Escape); otherwise drop them.
        self.ac_gen = self.ac_gen.wrapping_add(1);
        self.ghost.clear();
        if self.ai_query != query {
            self.ai_suggestions.clear();
        }

        // Rank once, then reuse the ranking to seed an instant heuristic ghost and
        // a candidate hint for the model.
        let ranked = self.ranked_results(&query);
        let hint = ranked.iter().find(|r| r.score > 0).map(|r| r.title.clone());
        if !query.is_empty() {
            if let Some(h) = &hint {
                let (ql, hl) = (query.to_lowercase(), h.to_lowercase());
                if hl.starts_with(&ql) && h.chars().count() > query.chars().count() {
                    // Continuation by char count, so a case-insensitive match never
                    // slices a title mid-codepoint.
                    self.ghost = h.chars().skip(query.chars().count()).collect();
                }
            }
        }
        self.results = self.splice_suggestions(ranked);

        self.selected = 0;
        self.screen = if self.query.trim().is_empty() {
            self.reset_home_sel();
            Screen::Home
        } else {
            Screen::Search
        };

        // Debounced AI autocomplete (replaces/refines the heuristic ghost and adds
        // suggestion rows). Dropping the previous task cancels its pending timer.
        self.ac_task = if self.ui.autocomplete.is_some() && query.chars().count() >= MIN_AC_LEN {
            Some(self.schedule_autocomplete(query, hint, cx))
        } else {
            None
        };
        cx.notify();
    }

    /// Fire a debounced autocomplete request on the background executor and, when
    /// it returns for the still-current query, apply its ghost + suggestion rows.
    fn schedule_autocomplete(
        &self,
        query: String,
        hint: Option<String>,
        cx: &mut Context<Self>,
    ) -> gpui::Task<()> {
        let gen = self.ac_gen;
        let suggest = self.ui.autocomplete.as_ref().unwrap().suggest.clone();
        cx.spawn(async move |weak, cx| {
            cx.background_executor().timer(Duration::from_millis(AC_DEBOUNCE_MS)).await;
            // Superseded by a newer keystroke while we waited out the debounce.
            if weak.update(cx, |t, _| t.ac_gen != gen).unwrap_or(true) {
                return;
            }
            let req = AutocompleteRequest { query: query.clone(), top_hint: hint };
            let out = cx.background_executor().spawn(async move { (suggest)(req) }).await;
            let Some(s) = out else { return };
            let _ = weak.update(cx, |t, cx| {
                if t.ac_gen != gen || t.query != query {
                    return;
                }
                // The LLM completion replaces the instant heuristic; if it came
                // back empty, the heuristic ghost stays.
                if !s.ghost.is_empty() {
                    t.ghost = s.ghost;
                }
                t.ai_suggestions = s.entries;
                t.ai_query = query.clone();
                t.rebuild_results();
                cx.notify();
            });
        })
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

    /// Whether the now-playing card is on screen and therefore navigable.
    fn np_visible(&self) -> bool {
        self.np.is_some()
    }

    /// Drop a selection that pointed at the now-playing card after it went away
    /// (the track ended, or Music quit) so Enter can't act on a hidden control.
    fn clamp_home_sel(&mut self) {
        if matches!(self.home_sel, HomeSel::NowPlaying(_)) && !self.np_visible() {
            self.reset_home_sel();
        }
    }

    fn home_nav(&mut self, key: &str) {
        self.home_sel = next_home_sel(
            self.home_sel,
            key,
            self.recents_count(),
            self.shortcuts_count(),
            self.np_visible(),
        );
        // Scrolling the selection into view is handled by the animated scroll in
        // `tick_highlight`, at the same speed as the highlight pill.
    }

    fn home_activate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.home_sel {
            HomeSel::NowPlaying(i) => {
                let Some(src) = &self.ui.now_playing else {
                    return;
                };
                let cmd = match i {
                    0 => Transport::Prev,
                    2 => Transport::Next,
                    _ => Transport::PlayPause,
                };
                (src.control)(cmd);
                // Flip the glyph immediately rather than waiting for the next
                // poll to come back; that poll then reconciles it.
                if cmd == Transport::PlayPause {
                    if let Some(np) = &mut self.np {
                        np.playing = !np.playing;
                    }
                }
                // Deliberately does not hide the launcher: transport controls
                // are something you use repeatedly.
                cx.notify();
            }
            HomeSel::Recents(i) => {
                if let Some(recent) = self.config.recents().get(i).cloned() {
                    self.reopen_and_record(recent, window, cx);
                }
            }
            HomeSel::Shortcuts(i) => {
                if i < self.ui.panels.len() {
                    let id = self.ui.panels[i].id.clone();
                    self.go_panel(&id, None, cx);
                } else {
                    self.go_settings(cx);
                }
            }
        }
    }

    fn activate_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.results.get(self.selected).cloned() else {
            return;
        };
        match &item.action {
            Action::Open(path) => {
                let _ = spotlight_platform_macos::apps::launch(path);
                self.record_activation(&item);
                self.hide(window, cx);
                return;
            }
            Action::OpenUrl(url) => {
                open_url(url);
                self.record_activation(&item);
                self.hide(window, cx);
                return;
            }
            Action::Copy(text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                self.hide(window, cx);
                return;
            }
            Action::OpenPanel { id, seed } => {
                // Navigate into the panel (which records its own recents entry),
                // seeding it with the item's own text when set (AI suggestion
                // entries), else the current search text so chat-style panels can
                // pre-fill and auto-send.
                let id = id.clone();
                let seed = seed.clone().unwrap_or_else(|| self.query.clone());
                self.go_panel(&id, Some(seed), cx);
                return;
            }
            Action::Custom(id) => {
                if let Some(ext) = self.registry.owner(&item.source) {
                    let _ = ext.run(id);
                }
                self.hide(window, cx);
                return;
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
            panel: None,
        });
    }

    /// Re-open a recent: navigate into an extension panel, or open a URL/path.
    /// (`go_panel` records its own use, so panels don't also persist here.)
    fn reopen_and_record(&mut self, recent: Recent, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(panel) = &recent.panel {
            let id = panel.clone();
            self.go_panel(&id, None, cx);
            return;
        }
        reopen_recent(&recent);
        self.persist_use(recent);
        // Opening a URL/path hands off to another app; dismiss explicitly rather
        // than relying on window deactivation (see `activate_search`).
        self.hide(window, cx);
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
        // Snap height/width to the (Home) target so they don't ease from the last
        // screen's size while the open animation plays.
        self.cur_h = None;
        self.cur_w = None;
        self.slide = None;
        spotlight_platform_macos::window::show_panel(ns_view);
        self.revealed = true;
        // Ask for fresh playback state now rather than waiting for the first
        // timer tick, so the card is right on the frame Home appears.
        if let Some(src) = &self.ui.now_playing {
            (src.poke)();
        }
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
        self.revealed = false;
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

    /// Capture-phase Escape handler powering hold-to-close. It runs on the shell
    /// root *before* the event descends to whichever view is focused, so it sees
    /// the key even on screens (like the mail reading pane) whose own handler
    /// consumes Escape before it could bubble back up here.
    ///
    /// A fresh press arms a timer and then falls through untouched, so a plain
    /// tap still does its usual one-level back-out (in the bubble handlers). The
    /// auto-repeats macOS sends while the key stays down are swallowed here so
    /// those per-level handlers don't fire once per repeat; if the key is still
    /// held when the timer elapses, the whole stack collapses to Home in one go.
    fn on_escape_capture(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.key != "escape" {
            return;
        }
        if event.is_held {
            // Swallow the hold's auto-repeats; the armed timer owns the gesture.
            cx.stop_propagation();
            return;
        }
        // Nothing to collapse from Home — a tap there already hides the panel.
        if self.screen == Screen::Home {
            return;
        }
        self.escape_hold = Some(cx.spawn_in(window, async move |weak, cx| {
            cx.background_executor().timer(Duration::from_millis(ESCAPE_HOLD_MS)).await;
            let _ = weak.update_in(cx, |this, window, cx| {
                this.escape_hold = None;
                this.go_home(window, cx);
            });
        }));
    }

    /// Releasing Escape cancels a pending hold-to-close, so a quick tap only ever
    /// backs out one level.
    fn on_escape_release(&mut self, event: &KeyUpEvent, _window: &mut Window, _cx: &mut Context<Self>) {
        if event.keystroke.key == "escape" {
            self.escape_hold = None;
        }
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

        // Settings: keyboard navigation. Up/Down move between sidebar categories
        // (only reaches here when no text field is focused — focused fields consume
        // arrows and stop propagation), Tab/Shift-Tab move focus into/among the
        // content pane's controls, Enter steps forward.
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
                "up" => {
                    self.settings_active = self.settings_active.saturating_sub(1);
                    if let Some(f) = self.settings_tab_focuses.get(self.settings_active).cloned() {
                        window.focus(&f, cx);
                    }
                    cx.notify();
                }
                "down" => {
                    if self.settings_active + 1 < self.settings_tabs.len() {
                        self.settings_active += 1;
                        if let Some(f) = self.settings_tab_focuses.get(self.settings_active).cloned() {
                            window.focus(&f, cx);
                        }
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
                    self.home_activate(window, cx);
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
                cx.notify();
            }
            "down" => {
                let len = self.current_len();
                if self.selected + 1 < len {
                    self.selected += 1;
                    cx.notify();
                }
            }
            // Tab (and Right at the end of the line) accept the greyed-out inline
            // autocomplete, appending it to the query. Tab is swallowed either way
            // so it never inserts a tab character.
            "tab" | "right" => {
                if !self.ghost.is_empty() {
                    let ghost = std::mem::take(&mut self.ghost);
                    self.query.push_str(&ghost);
                    self.refresh_query(cx);
                }
            }
            "enter" => self.activate_search(window, cx),
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
                    // Greyed-out inline autocomplete after the caret (Tab accepts).
                    .when(!self.ghost.is_empty(), |row| {
                        row.child(
                            div().text_xl().text_color(theme::muted()).child(self.ghost.clone()),
                        )
                    })
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
            // Overlay the highlight pill behind the rows (first child paints
            // underneath), positioned in the same coordinate space as the scroll
            // viewport so it tracks scrolling.
            let pill = self.highlight_pill(HlContext::Results, &scroll, HL_RADIUS);
            let area = div()
                .relative()
                .overflow_hidden()
                .when_some(pill, |a, p| a.child(p))
                .child(list::faded_scroll(&scroll, false, inner));
            panel = panel.child(div().h(px(1.)).bg(theme::divider())).child(area);
        }

        panel.into_any_element()
    }

    /// Start the loop that keeps the card live while Home is on screen. Pokes
    /// the source (which polls Music.app at an interactive rate for a few
    /// seconds afterwards) only while the panel is actually visible and showing
    /// Home, so a closed launcher costs nothing. Mirrors the clipboard panel's
    /// store-watch loop.
    fn watch_now_playing(&mut self, cx: &mut Context<Self>) {
        if self.np_watching || self.ui.now_playing.is_none() {
            return;
        }
        self.np_watching = true;
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(NP_POKE_MS))
                .await;
            let keep = this
                .update(cx, |this, cx| {
                    if !this.revealed || this.screen != Screen::Home {
                        return;
                    }
                    if let Some(src) = &this.ui.now_playing {
                        (src.poke)();
                    }
                    // The snapshot itself is picked up by `sync_now_playing`
                    // on the frame this notify schedules.
                    cx.notify();
                })
                .is_ok();
            if !keep {
                break;
            }
        })
        .detach();
    }

    /// Pull the latest now-playing snapshot and keep the decoded artwork in
    /// step with it. Called once at the top of `render`, before `screen_height`
    /// reads `np`.
    fn sync_now_playing(&mut self, window: &mut Window) {
        let Some(src) = &self.ui.now_playing else {
            return;
        };
        self.np = (src.snapshot)();
        self.clamp_home_sel();

        let wanted = self.np.as_ref().and_then(|np| np.artwork.clone());
        if self.np_art.as_ref().map(|(path, _)| path) == wanted.as_ref() {
            return;
        }
        // Release the old texture rather than letting the atlas grow one entry
        // per track for the life of the process.
        if let Some((_, old)) = self.np_art.take() {
            let _ = window.drop_image(old);
        }
        // Already downscaled to a thumbnail by the source, so decoding it on
        // the UI thread costs well under a frame.
        self.np_art = wanted.and_then(|path| {
            let bytes = std::fs::read(&path).ok()?;
            Some((path, decode_image_bytes(&bytes)?))
        });
    }

    /// The now-playing card: artwork, title/artist, an interpolated progress bar
    /// and the transport row. Only built when a snapshot is present, so Home
    /// shrinks back to its usual height whenever music stops.
    fn render_now_playing(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let np = self.np.clone()?;
        let art = np_art_tile(self.np_art.as_ref().map(|(_, image)| image.clone()));

        let pos = position_at(
            np.position_secs,
            np.duration_secs,
            np.playing,
            np.sampled_at.elapsed().as_secs_f64(),
        );
        let bar = div()
            .h(px(3.))
            .w_full()
            .rounded_full()
            .overflow_hidden()
            .bg(theme::divider())
            .child(
                div()
                    .h_full()
                    .w(gpui::relative(progress_fraction(pos, np.duration_secs)))
                    .rounded_full()
                    .bg(theme::accent()),
            );

        let mut buttons = div()
            .id("home-np-transport")
            // No overflow scrolling here; the handle is only a bounds recorder
            // so the shared highlight pill can track the three buttons.
            .track_scroll(&self.np_scroll)
            .flex()
            .flex_row()
            .items_center()
            .gap_1();
        for i in 0..NP_BUTTONS {
            buttons = buttons.child(
                div()
                    .id(("home-np-button", i))
                    .size(px(28.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(HL_RADIUS))
                    .text_color(theme::accent())
                    .hover(|s| s.bg(theme::hover_strong()))
                    .child(np_glyph(i, np.playing).to_string())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            this.home_sel = HomeSel::NowPlaying(i);
                            this.home_activate(window, cx);
                        }),
                    ),
            );
        }
        // The pill is positioned against the tracked row's own origin, so the
        // `relative` wrapper has to sit directly around it (see `item_rect`).
        let pill = self.highlight_pill(HlContext::NowPlaying, &self.np_scroll, 4.);
        let transport = div()
            .relative()
            .when_some(pill, |a, p| a.child(p))
            .child(buttons);

        let text = div()
            .flex()
            .flex_col()
            .flex_1()
            // Without a zero min-width a flex child refuses to shrink below its
            // content, and a long title would push the transport row off-card.
            .min_w(px(0.))
            .gap_1()
            .child(
                div()
                    .text_sm()
                    .text_color(theme::text())
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(np.title.clone()),
            )
            .child(
                // Time sits inline at the end of the artist row rather than on
                // its own line, so the card stays as tall as its artwork.
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(artist_album(&np.artist, &np.album)),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .child(format!("{} / {}", mmss(pos), mmss(np.duration_secs))),
                    ),
            )
            .child(bar);

        Some(
            div()
                .mx_1()
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .px_3()
                .py(px(10.))
                .rounded_xl()
                .bg(theme::hover())
                .border_1()
                .border_color(theme::divider())
                .child(art)
                .child(text)
                .child(transport)
                .into_any_element(),
        )
    }

    fn render_home_body(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let mut col = div().flex().flex_col().px_2().pb_2().gap_1();

        // Now playing — a status banner directly under the search row, present
        // only while Music.app has a track loaded.
        if let Some(card) = self.render_now_playing(cx) {
            col = col.child(section_label("Now playing")).child(card);
        }

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
                let entry = recent.clone();
                let leading = recent_leading(&mut self.icon_cache, recent);
                strip = strip.child(
                    home_card(leading, &recent.title)
                        .id(("home-recent", i))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, window, cx| {
                                this.reopen_and_record(entry.clone(), window, cx)
                            }),
                        ),
                );
            }
            let pill = self.highlight_pill(HlContext::Recents, &self.recents_scroll, 12.);
            col = col.child(
                div()
                    .relative()
                    .overflow_hidden()
                    .when_some(pill, |a, p| a.child(p))
                    .child(strip),
            );
        }

        // Shortcuts — extension panels + Settings, as a horizontal, scrollable
        // strip of the same compact cards used by "Recently opened".
        col = col.child(section_label("Shortcuts"));
        let mut strip = div()
            .id("home-shortcuts")
            .flex()
            .flex_row()
            .gap_2()
            .px_1()
            .pb_1()
            .overflow_x_scroll()
            .track_scroll(&self.shortcuts_scroll);
        for (i, p) in self.ui.panels.iter().enumerate() {
            let id = p.id.clone();
            // Prefer a generated built-in logo (Clipboard), then the panel's own
            // image icon, then its glyph on the shared tile.
            let leading = logo_tile(&p.id)
                .or_else(|| {
                    p.icon
                        .as_ref()
                        .and_then(|icon| image_tile(&mut self.icon_cache, icon))
                })
                .unwrap_or_else(|| glyph_tile(&p.glyph));
            strip = strip.child(
                home_card(leading, &p.title).id(("home-shortcut", i)).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| this.go_panel(&id, None, cx)),
                ),
            );
        }
        strip = strip.child(
            home_card(
                logo_tile("settings").unwrap_or_else(|| glyph_tile("⚙")),
                "Settings",
            )
            .id(("home-shortcut", self.ui.panels.len()))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.go_settings(cx)),
            ),
        );
        let pill = self.highlight_pill(HlContext::Shortcuts, &self.shortcuts_scroll, 12.);
        col = col.child(
            div()
                .relative()
                .overflow_hidden()
                .when_some(pill, |a, p| a.child(p))
                .child(strip),
        );

        col.into_any_element()
    }

    /// Inner content for Settings: a left sidebar of categories + a wide, scrollable
    /// content pane holding the active tab. Modern-settings shape (System Settings /
    /// VSCode) rather than a tab rail over a form.
    fn settings_content(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let active = self.settings_active;

        // Left sidebar: one nav row per tab, keyboard-reachable.
        let mut nav = div().flex().flex_col().gap_1().w(px(210.)).flex_shrink_0().p_3();
        for (i, (title, _)) in self.settings_tabs.iter().enumerate() {
            let selected = i == active;
            let glyph = settings_glyph(title);
            let mut item = div()
                .flex()
                .items_center()
                .gap_3()
                .px_3()
                .py_2()
                .rounded_lg()
                .border_1()
                .border_color(gpui::rgba(0x00_0000_00))
                .when(selected, |t| t.bg(theme::selected()))
                .hover(|s| s.bg(theme::hover()))
                .focus(|s| s.border_color(theme::accent()))
                .text_color(if selected { theme::accent() } else { theme::muted() })
                .child(div().w(px(16.)).flex_shrink_0().text_center().child(glyph))
                .child(div().child(title.clone()))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        this.settings_active = i;
                        cx.notify();
                    }),
                )
                .on_key_down(cx.listener(
                    move |this, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>| {
                        if ev.keystroke.key == "enter" || ev.keystroke.key == "space" {
                            this.settings_active = i;
                            cx.stop_propagation();
                            cx.notify();
                        }
                    },
                ));
            if let Some(f) = self.settings_tab_focuses.get(i) {
                item = item.track_focus(f).tab_index(0);
            }
            nav = nav.child(item);
        }

        let body = self
            .settings_tabs
            .get(active)
            .map(|(_, view)| view.clone().into_any_element())
            .unwrap_or_else(|| div().into_any_element());

        // Content pane: scrolls, with the active tab in a width-capped column so
        // fields don't stretch ultra-wide.
        let pane = div()
            .id("settings-pane")
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .overflow_y_scroll()
            .px_8()
            .py_6()
            .child(div().w_full().max_w(px(680.)).child(body));

        shell_content(
            "Settings",
            cx,
            div()
                .flex()
                .flex_row()
                .size_full()
                .child(nav)
                .child(div().w(px(1.)).h_full().bg(theme::divider()))
                .child(pane),
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
    fn tick_height(&mut self, dt: f32, window: &Window) -> f32 {
        let target = self.screen_height(&self.screen);
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

    /// Advance the eased panel width toward the active screen's target and return
    /// it, requesting another frame while still moving. Mirrors [`tick_height`].
    fn tick_width(&mut self, dt: f32, window: &Window) -> f32 {
        let target = self.panel_width(&self.screen);
        let w = match self.cur_w {
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
        self.cur_w = Some(w);
        w
    }

    /// Measured rect of item `ix` in `scroll`, in scroll-content coordinates
    /// (scroll-invariant: the offset is applied later, at render).
    fn item_rect(scroll: &gpui::ScrollHandle, ix: usize) -> Option<(f32, f32, f32, f32)> {
        let b = scroll.bounds_for_item(ix)?;
        let vb = scroll.bounds();
        Some((
            f32::from(b.origin.x) - f32::from(vb.origin.x),
            f32::from(b.origin.y) - f32::from(vb.origin.y),
            f32::from(b.size.width),
            f32::from(b.size.height),
        ))
    }

    /// The list + selected index that currently owns the highlight, if any.
    fn hl_active(&self) -> Option<(HlContext, gpui::ScrollHandle, usize)> {
        match &self.screen {
            Screen::Search if !self.results.is_empty() => {
                Some((HlContext::Results, self.results_scroll.clone(), self.selected))
            }
            Screen::Home => match self.home_sel {
                HomeSel::NowPlaying(i) if self.np.is_some() => {
                    Some((HlContext::NowPlaying, self.np_scroll.clone(), i))
                }
                HomeSel::Shortcuts(i) => {
                    Some((HlContext::Shortcuts, self.shortcuts_scroll.clone(), i))
                }
                HomeSel::Recents(i) if !self.config.recents().is_empty() => {
                    Some((HlContext::Recents, self.recents_scroll.clone(), i))
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// Spring the highlight pill toward the selected item's measured bounds.
    /// Snaps (no fly) when the active list changes.
    fn tick_highlight(&mut self, dt: f32, window: &Window) {
        let Some((ctx, scroll, ix)) = self.hl_active() else {
            self.hl_ctx = None;
            self.hl_ready = false;
            return;
        };
        let Some((tx, ty, tw, th)) = Self::item_rect(&scroll, ix) else {
            // Bounds aren't painted yet; take one more frame to get them.
            window.request_animation_frame();
            return;
        };
        let ctx_changed = self.hl_ctx != Some(ctx);
        let still = |p: f32, t: f32, v: f32| (p - t).abs() > 0.4 || v.abs() > 2.0;

        // --- highlight pill: snap on list switch, otherwise spring ---
        if ctx_changed {
            self.hl_ctx = Some(ctx);
            self.hl_x = tx;
            self.hl_y = ty;
            self.hl_w = tw;
            self.hl_h = th;
            self.hl_vx = 0.;
            self.hl_vy = 0.;
            self.hl_vw = 0.;
            self.hl_vh = 0.;
        } else {
            (self.hl_x, self.hl_vx) = spring_to(self.hl_x, self.hl_vx, tx, dt);
            (self.hl_y, self.hl_vy) = spring_to(self.hl_y, self.hl_vy, ty, dt);
            (self.hl_w, self.hl_vw) = spring_to(self.hl_w, self.hl_vw, tw, dt);
            (self.hl_h, self.hl_vh) = spring_to(self.hl_h, self.hl_vh, th, dt);
            if still(self.hl_x, tx, self.hl_vx)
                || still(self.hl_y, ty, self.hl_vy)
                || still(self.hl_w, tw, self.hl_vw)
                || still(self.hl_h, th, self.hl_vh)
            {
                window.request_animation_frame();
            }
        }
        self.hl_ready = true;

        // --- scroll the selection into view at the pill's speed ---
        let horizontal =
            matches!(ctx, HlContext::Recents | HlContext::Shortcuts | HlContext::NowPlaying);
        let (pos, size, viewport, max_off, cur_off) = if horizontal {
            (
                tx,
                tw,
                f32::from(scroll.bounds().size.width),
                f32::from(scroll.max_offset().x),
                f32::from(scroll.offset().x),
            )
        } else {
            (
                ty,
                th,
                f32::from(scroll.bounds().size.height),
                f32::from(scroll.max_offset().y),
                f32::from(scroll.offset().y),
            )
        };
        let sel_changed = self.hl_last != Some((ctx, ix));
        self.hl_last = Some((ctx, ix));
        if ctx_changed {
            // New list: snap scroll to reveal, no animation across lists.
            self.scroll_anim = false;
            self.scroll_vel = 0.;
            let snap = reveal_offset(pos, size, viewport, cur_off, max_off);
            scroll.set_offset(set_axis(scroll.offset(), horizontal, snap));
        } else if sel_changed {
            self.scroll_target = reveal_offset(pos, size, viewport, cur_off, max_off);
            self.scroll_vel = 0.;
            self.scroll_anim = true;
        }
        if self.scroll_anim {
            let (next, vel) = spring_to(cur_off, self.scroll_vel, self.scroll_target, dt);
            self.scroll_vel = vel;
            scroll.set_offset(set_axis(scroll.offset(), horizontal, next));
            if (next - self.scroll_target).abs() > 0.4 || vel.abs() > 2.0 {
                window.request_animation_frame();
            } else {
                self.scroll_anim = false;
            }
        }
    }

    /// The highlight pill for `ctx`, positioned (content rect + current scroll
    /// offset) as an absolute overlay. `None` when another list owns the pill or
    /// bounds aren't ready yet.
    fn highlight_pill(
        &self,
        ctx: HlContext,
        scroll: &gpui::ScrollHandle,
        radius: f32,
    ) -> Option<gpui::Div> {
        if !self.hl_ready || self.hl_ctx != Some(ctx) {
            return None;
        }
        let off = scroll.offset();
        Some(
            div()
                .absolute()
                .left(px(self.hl_x + f32::from(off.x)))
                .top(px(self.hl_y + f32::from(off.y)))
                .w(px(self.hl_w))
                .h(px(self.hl_h))
                .rounded(px(radius))
                .bg(theme::selected()),
        )
    }

    /// Build the panel body: normally `chrome(height, content)`, or a sliding
    /// two-screen track during a depth transition.
    fn render_body(&mut self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let now = Instant::now();
        let dt = (now - self.last_frame).as_secs_f32().min(0.032);
        self.last_frame = now;
        self.tick_highlight(dt, window);
        let h = self.tick_height(dt, window);
        // Eased current width (Home 680 → Settings 760); the reveal wrapper owns it
        // and everything below fills it, so it morphs smoothly across a slide.
        let w = self.tick_width(dt, window);

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
        // Both cells fill the eased viewport width `w`; the track is 2w wide and
        // shifts by w so the incoming screen ends centered while the panel's width
        // eases toward the target screen's width.
        let cell = move |content: AnyElement| div().w(px(w)).flex_shrink_0().child(content);
        // Forward: [from, to], track slides left (0 → -w). Back: [to, from], track
        // slides right (-w → 0). Either way the incoming screen ends centered.
        let (left, right, offset) = if slide.forward {
            (cell(from_content), cell(to_content), -w * e)
        } else {
            (cell(to_content), cell(from_content), -w * (1.0 - e))
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

        // Per-screen top offset: search-shaped screens sit low (140); the large
        // Settings panel sits higher (60) so it reads as centered.
        self.sync_now_playing(window);
        self.watch_now_playing(cx);

        let top = self.panel_top(&self.screen);
        let body = self.render_body(window, cx);

        div()
            .key_context("Spotlight")
            .track_focus(&self.focus_handle)
            // Hold-to-close runs in the capture phase so it sees Escape (and its
            // held repeats) even when a focused extension panel would otherwise
            // consume it before it bubbled back to the shell.
            .capture_key_down(cx.listener(Self::on_escape_capture))
            .capture_key_up(cx.listener(Self::on_escape_release))
            .on_key_down(cx.listener(Self::on_key_down))
            // Set an explicit font + default text color on the root so all text
            // inherits a known-present family rather than relying on the default.
            .font_family("Helvetica Neue")
            .text_color(theme::text())
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .pt(px(top))
            .child(self.open_reveal(body, window, cx))
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
/// Resting panel width for the search-shaped screens (Home/Search); the
/// open-reveal container owns it and the inner panels fill it (`w_full`), so
/// animating this scales the whole panel.
const OPEN_BASE_W: f32 = 680.0;
/// Resting panel width for Settings + extension panels — wider than Home so a
/// proper settings layout fits. The width eases between screens (like height).
const SETTINGS_W: f32 = 1040.0;

/// The resting width extension panels are laid out at, for panels that need to
/// size fixed-width content (e.g. Gmail renders email HTML to an image sized
/// to fill the pane).
pub fn extension_panel_width() -> f32 {
    SETTINGS_W
}
/// Top offset for the search-shaped screens (Home/Search). Settings uses a
/// smaller offset (`SETTINGS_TOP`) so its large panel sits higher / more centered.
const PANEL_TOP: f32 = 140.0;
/// Top offset for the large Settings / extension panels.
const SETTINGS_TOP: f32 = 60.0;

/// Corner radius of the panel chrome (`chrome`). Shared so edge fades can inset
/// themselves by it and stay clear of the rounded corners (`overflow_hidden`
/// clips to the box, not the radius).
pub(crate) const PANEL_RADIUS: f32 = 24.0;

// --- Per-screen heights, so the panel can animate its height between screens. ---
/// Settings + extension panels (fixed).
const PANEL_H: f32 = 640.0;
const SEARCH_ROW_H: f32 = 64.0;
const DIVIDER_H: f32 = 1.0;
const SECTION_LABEL_H: f32 = 32.0;
const SHORTCUT_ROW_H: f32 = 48.0;
const SHORTCUTS_MAX_H: f32 = 190.0;
const RECENTS_STRIP_H: f32 = 96.0;
/// The now-playing card: 52px of artwork plus 8px of padding above and below.
/// Home's height is computed rather than measured, so this has to match what
/// the card actually renders as — too small and `chrome` clips it, too large
/// and a dead gap opens above "Recently opened". Verify with scripts/capture.sh.
const NOW_PLAYING_H: f32 = 80.0;
/// How often Home nudges the now-playing source while it is on screen.
const NP_POKE_MS: u64 = 500;
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

/// Spring stiffness/damping for the selection highlight. Damping is a touch below
/// critical (2·√stiffness ≈ 63) so the pill overshoots slightly and settles —
/// the satisfying "magic pill" glide.
const HL_STIFFNESS: f32 = 1000.0;
const HL_DAMPING: f32 = 52.0;
/// Corner radius of the highlight pill (matches the rows' `rounded_lg`).
const HL_RADIUS: f32 = 8.0;

/// Font-size bounds (px) for shrink-to-fit Home card titles.
const TITLE_MAX: f32 = 14.0;
const TITLE_MIN: f32 = 10.0;
/// Reserved two-line box uses this line height (px) at the largest font, so every
/// card is the same height regardless of how far its title shrank. Actual line
/// spacing scales with the font (see `TITLE_LINE_RATIO`).
const TITLE_LINE: f32 = 16.0;
/// Line height as a multiple of the title's font size (≈ `TITLE_LINE / TITLE_MAX`)
/// so inter-line spacing shrinks together with the text.
const TITLE_LINE_RATIO: f32 = 1.15;
/// Footprint and corner rounding (px) shared by every Home icon tile, so a row of
/// disparate icons reads as uniform. The radius mirrors the macOS app-icon curve.
const ICON_SIZE: f32 = 40.0;
const ICON_RADIUS: f32 = 9.0;

/// Which navigable list the selection highlight is currently tracking. Switching
/// lists snaps the pill rather than flying it across the panel.
#[derive(Clone, Copy, PartialEq)]
enum HlContext {
    Results,
    Shortcuts,
    Recents,
    NowPlaying,
}

/// Semi-implicit Euler step of a 1-D spring toward `target`.
fn spring_to(pos: f32, vel: f32, target: f32, dt: f32) -> (f32, f32) {
    let force = HL_STIFFNESS * (target - pos) - HL_DAMPING * vel;
    let vel = vel + force * dt;
    (pos + vel * dt, vel)
}

/// Scroll offset (along one axis) that brings item `[pos, pos+size]` fully into a
/// `viewport`, revealing a `peek` of the neighbor, clamped to the scroll range.
/// Returns the GPUI offset convention (0 at top/left, negative when scrolled).
fn reveal_offset(pos: f32, size: f32, viewport: f32, cur_off: f32, max_off: f32) -> f32 {
    let peek = size + 8.0;
    let mut start = -cur_off; // content coordinate currently at the viewport edge
    if pos - peek < start {
        start = pos - peek;
    } else if pos + size + peek > start + viewport {
        start = pos + size + peek - viewport;
    }
    // `max_off` is GPUI's positive scroll magnitude; the offset range is
    // [-max_off, 0], so the content-start range is [0, max_off].
    let max_start = max_off.max(0.0);
    -start.clamp(0.0, max_start)
}

/// Replace one axis of a scroll offset point, keeping the other axis.
fn set_axis(off: gpui::Point<gpui::Pixels>, horizontal: bool, value: f32) -> gpui::Point<gpui::Pixels> {
    if horizontal {
        point(px(value), off.y)
    } else {
        point(off.x, px(value))
    }
}

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
    /// Dismiss when a mouse-down lands inside our window but outside the panel.
    ///
    /// The window is far larger than the panel (transparent slack for the open
    /// spring and the exit drop). Its fully transparent pixels are holes in the
    /// window server's hit test — see `configure_panel` — so clicks out there
    /// reach the app underneath and the resulting deactivation hides us. The
    /// panel's soft `shadow_lg` ring is *not* fully transparent though, so
    /// clicks in that band still land on us; catch them here so the dead zone
    /// around the panel edge dismisses too.
    ///
    /// `on_mouse_down_out` is capture-phase and bounds-checked against this
    /// element, so clicks inside the panel are untouched, and it needs no
    /// element id (which the id-less wrapper below depends on).
    fn dismiss_on_click_out(&self, panel: gpui::Div, cx: &Context<Self>) -> gpui::Div {
        if std::env::var_os("SPOTLIGHT_CAPTURE").is_some() {
            return panel;
        }
        panel.on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, window, cx| {
            this.hide(window, cx);
        }))
    }

    /// Wrap the panel body in the springy open-reveal: it begins blurred,
    /// transparent, and stretched a touch wide, then unblurs, fades in, and
    /// springs down to its resting width. The exit is the same curve reversed.
    ///
    /// Driven manually from `reveal_start` (not `with_animation`) so the wrapper
    /// can stay id-less — an id-bearing wrapper would prefix every descendant's
    /// `GlobalElementId` and reset the per-screen fades on every show/hide.
    /// Skipped under capture so screenshots stay crisp.
    fn open_reveal(&self, body: AnyElement, window: &Window, cx: &Context<Self>) -> AnyElement {
        // Current eased panel width (set by `tick_width` during `render_body`).
        let base_w = self.cur_w.unwrap_or_else(|| self.panel_width(&self.screen));
        if std::env::var_os("SPOTLIGHT_CAPTURE").is_some() {
            return div().w(px(base_w)).child(body).into_any_element();
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
        self.dismiss_on_click_out(div().w(px(base_w * scale)), cx)
            .child(body)
            .blur(px(blur))
            .opacity(opacity)
            .into_any_element()
    }

    /// Exit animation: the panel drops away under gravity while fading. Physics is
    /// a body released from rest under constant acceleration, so the offset grows
    /// with the square of elapsed time (`y = ½·g·t²`).
    /// No click-out dismissal here: we're already on the way out, so a stray
    /// click in the margin has nothing left to dismiss.
    fn exit_fall(&self, body: AnyElement, secs: f32, window: &Window) -> AnyElement {
        let base_w = self.cur_w.unwrap_or_else(|| self.panel_width(&self.screen));
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
            .w(px(base_w))
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
        .rounded(px(PANEL_RADIUS))
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

// ---- Now-playing card ------------------------------------------------------

/// Buttons in the transport row (previous, play/pause, next).
const NP_BUTTONS: usize = 3;
/// Index of the play/pause button — where a vertical move into the card lands.
const NP_PLAY: usize = 1;
/// Artwork edge length (px) in the now-playing card. Larger than the Home tiles'
/// `ICON_SIZE`, since album art is the card's focal point — and tall enough to
/// govern the card's height, so the text column has slack rather than the
/// progress bar crowding the bottom border.
const NP_ART: f32 = 56.0;
/// Corner rounding of the artwork, proportional to `ICON_RADIUS` at `ICON_SIZE`.
const NP_ART_RADIUS: f32 = 12.0;

/// Playback position at render time: the last sampled position plus the wall
/// time since that sample while playing, so the progress bar advances smoothly
/// between the source's polls instead of stepping once a second.
fn position_at(sampled: f64, dur: f64, playing: bool, since: f64) -> f64 {
    let pos = if playing { sampled + since } else { sampled };
    pos.clamp(0.0, dur.max(0.0))
}

/// Progress as 0..=1. A missing or nonsensical duration (streams report 0)
/// yields an empty bar rather than a full or negative one.
fn progress_fraction(pos: f64, dur: f64) -> f32 {
    if dur <= 0.0 {
        return 0.0;
    }
    (pos / dur).clamp(0.0, 1.0) as f32
}

/// Seconds as `m:ss` (or `h:mm:ss` past an hour).
fn mmss(secs: f64) -> String {
    let total = if secs.is_finite() && secs > 0.0 { secs as u64 } else { 0 };
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// The card's secondary line: artist, or `artist · album` when both are known.
fn artist_album(artist: &str, album: &str) -> String {
    match (artist.trim(), album.trim()) {
        ("", "") => String::new(),
        (a, "") => a.to_string(),
        ("", b) => b.to_string(),
        (a, b) => format!("{a} \u{b7} {b}"),
    }
}

/// The artwork tile, at the card's larger size. Falls back to a note glyph —
/// radio and streams often carry no art, and it is also what shows for the
/// moment before the first artwork dump lands.
fn np_art_tile(image: Option<Arc<RenderImage>>) -> AnyElement {
    let inner = match image {
        Some(image) => img(ImageSource::Render(image))
            .w(px(NP_ART))
            .h(px(NP_ART))
            .rounded(px(NP_ART_RADIUS))
            .object_fit(ObjectFit::Cover)
            .into_any_element(),
        None => div()
            .text_xl()
            .text_color(theme::accent())
            .child("\u{266a}".to_string())
            .into_any_element(),
    };
    div()
        .flex_shrink_0()
        .size(px(NP_ART))
        .rounded(px(NP_ART_RADIUS))
        .overflow_hidden()
        .bg(theme::icon_bg())
        .flex()
        .items_center()
        .justify_center()
        .child(inner)
        .into_any_element()
}

/// Transport glyph for a button index, given whether playback is running.
/// Geometric shapes rather than emoji, so they inherit the accent color and the
/// panel's font instead of rendering as color glyphs.
fn np_glyph(i: usize, playing: bool) -> &'static str {
    match i {
        0 => "\u{23ee}",                                          // previous
        2 => "\u{23ed}",                                          // next
        _ if playing => "\u{23f8}",                               // pause
        _ => "\u{25b6}",                                          // play
    }
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

/// A small leading glyph for a Settings sidebar category, by title.
fn settings_glyph(title: &str) -> &'static str {
    match title {
        "General" => "\u{2699}",   // gear
        "AI" => "\u{2728}",        // sparkles
        "Jira" => "\u{25c8}",      // diamond-in-square
        "Clipboard" => "\u{2632}", // trigram (list-ish)
        _ => "\u{2022}",           // bullet
    }
}

/// A Home strip card: a centered icon over an up-to-2-line title, fixed width so
/// the horizontal strips (Recently opened, Shortcuts) share one tile shape. The
/// caller attaches `.id(..)` and the click handler; selection is drawn by the
/// animated highlight pill, so the card itself only carries hover.
fn home_card(leading: AnyElement, title: &str) -> gpui::Div {
    let font = title_font_px(title);
    div()
        .flex_shrink_0()
        .w(px(92.))
        .flex()
        .flex_col()
        .items_center()
        .gap_1()
        .px_2()
        .py_2()
        .rounded_xl()
        .hover(|s| s.bg(theme::hover()))
        .child(leading)
        .child(
            div()
                .w_full()
                // Reserve two lines at the largest font so every card is the
                // same height, then center vertically so a one-line title sits
                // half a line lower rather than hugging the icon.
                .h(px(TITLE_LINE * 2.))
                .flex()
                .flex_col()
                .justify_center()
                .text_center()
                .text_size(px(font))
                // Line spacing tracks the font, so a shrunken two-line title packs
                // its lines proportionally tighter instead of keeping a fixed gap.
                .line_height(px(font * TITLE_LINE_RATIO))
                // `line_clamp` limits the line count; `text_ellipsis` is what
                // actually truncates the last line with an ellipsis when it
                // still overflows (e.g. "Firefox Developer Edition").
                .line_clamp(2)
                .text_ellipsis()
                .text_color(theme::text())
                .child(title.to_string()),
        )
}

/// Largest font size (px, between `TITLE_MIN` and `TITLE_MAX`) at which `title`
/// word-wraps onto at most two lines inside a Home card. Short labels stay
/// comfortably large; long ones shrink to fit before the 2-line clamp has to
/// truncate them.
fn title_font_px(title: &str) -> f32 {
    // Card width (92) minus its horizontal padding; a little slack so the width
    // estimate stays on the safe side of the real glyph advances.
    const AVAIL: f32 = 74.0;
    let words: Vec<usize> = title.split_whitespace().map(|w| w.chars().count()).collect();
    let mut size = TITLE_MAX;
    while size > TITLE_MIN {
        if wrapped_lines(&words, AVAIL, size) <= 2 {
            break;
        }
        size -= 0.5;
    }
    size
}

/// Greedy word-wrap line count for `words` (their char counts) rendered at `font`
/// px within `avail` px, estimating the average glyph advance as ~0.52em.
fn wrapped_lines(words: &[usize], avail: f32, font: f32) -> usize {
    let cap = (avail / (font * 0.52)).floor().max(1.0) as usize;
    let mut lines = 1;
    let mut used = 0usize;
    for &w in words {
        if used == 0 {
            used = w;
        } else if used + 1 + w <= cap {
            used += 1 + w; // fits after a space
        } else {
            lines += 1;
            used = w; // wrap onto the next line
        }
    }
    lines
}

/// Wrap icon content in the shared rounded-square tile: fixed size, app-style
/// corner rounding, a faint neutral backing (so transparent icons and glyphs sit
/// on a subtle tile rather than floating), and a clip so every icon — a square
/// app icon, a circular one, a glyph — reads as the same rounded shape.
fn icon_tile(inner: AnyElement) -> AnyElement {
    div()
        .flex_shrink_0()
        .size(px(ICON_SIZE))
        .rounded(px(ICON_RADIUS))
        .overflow_hidden()
        .bg(theme::icon_bg())
        .flex()
        .items_center()
        .justify_center()
        .child(inner)
        .into_any_element()
}

/// A `RenderImage` scaled to cover the tile, so square art fills it edge to edge.
/// The rounding is applied to the image itself (gpui honors an image's own corner
/// radii) — a parent's `overflow_hidden` does not clip child images to the radius.
fn icon_fill(image: Arc<RenderImage>) -> AnyElement {
    img(ImageSource::Render(image))
        .w(px(ICON_SIZE))
        .h(px(ICON_SIZE))
        .rounded(px(ICON_RADIUS))
        .object_fit(ObjectFit::Cover)
        .into_any_element()
}

/// Tile for a built-in generated logo (`"clipboard"`, `"settings"`), or `None`.
fn logo_tile(kind: &str) -> Option<AnyElement> {
    logo::logo(kind).map(|image| icon_tile(icon_fill(image)))
}

/// Tile for an image file (e.g. an extension's logo), or `None` if unloadable.
fn image_tile(cache: &mut HashMap<PathBuf, Arc<RenderImage>>, path: &str) -> Option<AnyElement> {
    let image = load_image_file(cache, std::path::Path::new(path))?;
    Some(icon_tile(icon_fill(image)))
}

/// A glyph/emoji centered on the icon tile — the fallback when there's no image.
fn glyph_tile(glyph: &str) -> AnyElement {
    icon_tile(
        div()
            .text_xl()
            .text_color(theme::accent())
            .child(glyph.to_string())
            .into_any_element(),
    )
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
    let render = decode_image_bytes(&bytes)?;
    cache.insert(path.to_path_buf(), render.clone());
    Some(render)
}

/// Decode encoded image bytes (PNG/etc.) into a gpui `RenderImage`, reordering
/// RGBA→BGRA to match gpui's byte order.
fn decode_image_bytes(bytes: &[u8]) -> Option<Arc<RenderImage>> {
    let decoded = image::load_from_memory(bytes).ok()?.to_rgba8();
    let (w, h) = decoded.dimensions();
    let mut raw = decoded.into_raw();
    for px in raw.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let buffer = image::RgbaImage::from_raw(w, h, raw)?;
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])))
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

fn result_row(item: &ResultItem, icon: Option<Arc<RenderImage>>, _selected: bool) -> impl IntoElement {
    let accent = theme::accent();
    // A result that opens a built-in panel (e.g. Clipboard History) uses that
    // panel's generated logo, so search matches the Home tiles.
    let panel_logo = match &item.action {
        Action::OpenPanel { id, .. } => logo::logo(id),
        _ => None,
    };
    let leading = if let Some(image) = panel_logo {
        div()
            .size(px(28.))
            .flex()
            .items_center()
            .justify_center()
            .child(
                img(ImageSource::Render(image))
                    .w(px(28.))
                    .h(px(28.))
                    // Same corner-radius proportion as the Home tiles (9/40).
                    .rounded(px(28. * ICON_RADIUS / ICON_SIZE))
                    .object_fit(ObjectFit::Cover),
            )
    } else if let Some(render_image) = icon {
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
                    .rounded(px(28. * ICON_RADIUS / ICON_SIZE))
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

/// Leading icon tile for a recent card: a built-in panel logo (e.g. Clipboard) if
/// it points to one, else a custom image icon, else the system file icon (apps),
/// else a glyph, else a clock fallback — all in the shared rounded tile.
fn recent_leading(cache: &mut HashMap<PathBuf, Arc<RenderImage>>, recent: &Recent) -> AnyElement {
    // Built-in panel logo, so a Clipboard recent matches its shortcut tile.
    if let Some(panel) = &recent.panel {
        if let Some(tile) = logo_tile(panel) {
            return tile;
        }
    }
    // Custom extension logo (e.g. the Jira icon).
    if let Some(icon) = &recent.icon {
        if let Some(tile) = image_tile(cache, icon) {
            return tile;
        }
    }
    // App / file system icon via NSWorkspace.
    if let Some(path) = &recent.path {
        if let Some(image) = resolve_icon(cache, &Some(Icon::File(PathBuf::from(path)))) {
            return icon_tile(icon_fill(image));
        }
    }
    glyph_tile(recent.glyph.as_deref().unwrap_or("🕘"))
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
/// `SPOTLIGHT_CAPTURE_SCREEN` (`settings` or a panel id to deep-link),
/// `SPOTLIGHT_CAPTURE_HOME_SEL` (`np:1`, `recents:0`, `shortcuts:2`).
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

        // Menu-bar items are consumed here, not by the view, so take them out
        // before `ui` (non-Clone) moves into the window builder below.
        let mut ui = ui;
        let menu_items = std::mem::take(&mut ui.menu_items);

        // A background menu-bar app starts hidden and is summoned by the hotkey or
        // the "Open Spotlight" menu item — it should not slam a window open on
        // launch. Headless capture is the exception: it needs the window on screen.
        let show_on_launch = std::env::var_os("SPOTLIGHT_CAPTURE").is_some();

        // Window is larger than the widest resting panel (1040px Settings) so the
        // animations have transparent margin to bleed into rather than clipping at
        // the window edge, and so the 640px-tall Settings panel (60px top offset)
        // plus the exit drop fit inside. Panel stays centered.
        let bounds = Bounds::centered(None, size(px(1160.), px(760.)), cx);
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
                    focus: show_on_launch,
                    show: show_on_launch,
                    ..Default::default()
                },
                move |window, cx| cx.new(|cx| SpotlightView::new(registry, ui, window, cx)),
            )
            .expect("failed to open launcher window");

        register_global_hotkey(cx, window_handle);
        install_status_bar(cx, window_handle, menu_items);
    });
}

/// Ensure the launcher panel is on screen (used by the "Open Spotlight" menu
/// command). No-op if it's already visible.
impl SpotlightView {
    fn ensure_visible(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ns_view) = appkit_view_ptr(window) {
            if self.exiting || !spotlight_platform_macos::window::panel_visible(ns_view) {
                self.reveal(ns_view, window, cx);
            }
        }
    }
}

/// Install the menu-bar (`NSStatusItem`) control: a template glyph whose menu
/// exposes Open/Settings, extension-contributed items, Launch at Login, and Quit.
/// Skipped under headless capture so screenshot runs don't touch the menu bar.
fn install_status_bar(
    cx: &mut App,
    window_handle: WindowHandle<SpotlightView>,
    menu_items: Vec<MenuItem>,
) {
    use spotlight_platform_macos::login_item;
    use spotlight_platform_macos::statusbar::{MenuItem as NativeItem, StatusBar};

    if std::env::var_os("SPOTLIGHT_CAPTURE").is_some() {
        return;
    }

    // 2× the ~18pt menu-bar glyph so it stays crisp; template image tints itself.
    let Some((w, h, rgba)) = logo::menubar_icon_rgba(36) else {
        eprintln!("spotlight: failed to rasterize menu-bar icon; status item skipped");
        return;
    };

    let async_cx = cx.to_async();

    // Run `f(view, window, cx)` against the launcher window, deferred to a safe
    // point in the run loop (menu clicks arrive on the main thread mid-cycle).
    let window_action = move |f: fn(&mut SpotlightView, &mut Window, &mut Context<SpotlightView>)| {
        let cx = async_cx.clone();
        Box::new(move || {
            let handle = window_handle;
            cx.spawn(async move |cx| {
                let _ = handle.update(cx, |view, window, cx| f(view, window, cx));
            })
            .detach();
        }) as Box<dyn Fn()>
    };

    let mut items: Vec<NativeItem> = vec![
        NativeItem::Action {
            title: "Open Spotlight".into(),
            checked: None,
            on_click: window_action(|view, window, cx| view.ensure_visible(window, cx)),
        },
        NativeItem::Action {
            title: "Settings…".into(),
            checked: None,
            on_click: window_action(|view, window, cx| {
                view.ensure_visible(window, cx);
                view.go_settings(cx);
            }),
        },
    ];

    // Extension-contributed items, bridged from `Fn(&mut App)` to a main-thread
    // click that defers into gpui.
    for item in menu_items {
        let action: Arc<dyn Fn(&mut App)> = Arc::from(item.action);
        let cx = cx.to_async();
        items.push(NativeItem::Separator);
        items.push(NativeItem::Action {
            title: item.title,
            checked: None,
            on_click: Box::new(move || {
                let (cx, action) = (cx.clone(), action.clone());
                cx.spawn(async move |cx| {
                    cx.update(|cx| action(cx));
                })
                .detach();
            }),
        });
    }

    items.push(NativeItem::Separator);
    items.push(NativeItem::Action {
        title: "Launch at Login".into(),
        checked: Some(Box::new(login_item::is_enabled)),
        on_click: Box::new(|| {
            if let Err(e) = login_item::set_enabled(!login_item::is_enabled()) {
                eprintln!("spotlight: {e}");
            }
        }),
    });

    let quit_cx = cx.to_async();
    items.push(NativeItem::Action {
        title: "Quit Spotlight-rs".into(),
        checked: None,
        on_click: Box::new(move || {
            let cx = quit_cx.clone();
            cx.spawn(async move |cx| {
                cx.update(|cx| cx.quit());
            })
            .detach();
        }),
    });

    // Keep the status item alive for the whole session (like the hotkey).
    Box::leak(Box::new(StatusBar::new(&rgba, w, h, items)));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mmss_formats_common_durations() {
        assert_eq!(mmss(0.0), "0:00");
        assert_eq!(mmss(9.0), "0:09");
        assert_eq!(mmss(61.4), "1:01");
        assert_eq!(mmss(3599.0), "59:59");
        assert_eq!(mmss(3600.0), "1:00:00");
        // A stream reporting a nonsense duration shouldn't render garbage.
        assert_eq!(mmss(-5.0), "0:00");
        assert_eq!(mmss(f64::NAN), "0:00");
    }

    #[test]
    fn progress_fraction_handles_missing_durations() {
        assert_eq!(progress_fraction(30.0, 120.0), 0.25);
        // Streams report a zero duration; an empty bar beats a full one.
        assert_eq!(progress_fraction(30.0, 0.0), 0.0);
        assert_eq!(progress_fraction(30.0, -1.0), 0.0);
        // Interpolation can overshoot just past the end of a track.
        assert_eq!(progress_fraction(130.0, 120.0), 1.0);
    }

    #[test]
    fn position_advances_only_while_playing() {
        assert_eq!(position_at(10.0, 100.0, true, 2.5), 12.5);
        assert_eq!(position_at(10.0, 100.0, false, 2.5), 10.0);
        // Never runs past the end while waiting for the next poll.
        assert_eq!(position_at(99.0, 100.0, true, 30.0), 100.0);
    }

    #[test]
    fn artist_album_joins_what_is_known() {
        assert_eq!(artist_album("A", "B"), "A \u{b7} B");
        assert_eq!(artist_album("A", ""), "A");
        assert_eq!(artist_album("", "B"), "B");
        assert_eq!(artist_album("", ""), "");
    }

    fn nav(sel: HomeSel, key: &str, recents: usize, np: bool) -> HomeSel {
        next_home_sel(sel, key, recents, 6, np)
    }

    #[test]
    fn up_from_recents_reaches_the_card() {
        assert!(matches!(
            nav(HomeSel::Recents(2), "up", 5, true),
            HomeSel::NowPlaying(NP_PLAY)
        ));
        // Nothing above Recents when the card is hidden.
        assert!(matches!(nav(HomeSel::Recents(2), "up", 5, false), HomeSel::Recents(2)));
    }

    #[test]
    fn up_from_shortcuts_prefers_recents_over_the_card() {
        assert!(matches!(nav(HomeSel::Shortcuts(1), "up", 5, true), HomeSel::Recents(0)));
        // With no recents in between, Shortcuts reaches the card directly.
        assert!(matches!(
            nav(HomeSel::Shortcuts(1), "up", 0, true),
            HomeSel::NowPlaying(NP_PLAY)
        ));
        assert!(matches!(nav(HomeSel::Shortcuts(1), "up", 0, false), HomeSel::Shortcuts(1)));
    }

    #[test]
    fn transport_row_clamps_at_both_ends() {
        assert!(matches!(nav(HomeSel::NowPlaying(0), "left", 5, true), HomeSel::NowPlaying(0)));
        assert!(matches!(nav(HomeSel::NowPlaying(1), "left", 5, true), HomeSel::NowPlaying(0)));
        assert!(matches!(nav(HomeSel::NowPlaying(1), "right", 5, true), HomeSel::NowPlaying(2)));
        assert!(matches!(nav(HomeSel::NowPlaying(2), "right", 5, true), HomeSel::NowPlaying(2)));
    }

    #[test]
    fn down_from_the_card_skips_absent_recents() {
        assert!(matches!(nav(HomeSel::NowPlaying(1), "down", 5, true), HomeSel::Recents(0)));
        assert!(matches!(nav(HomeSel::NowPlaying(1), "down", 0, true), HomeSel::Shortcuts(0)));
    }

    #[test]
    fn existing_recents_and_shortcuts_navigation_is_unchanged() {
        assert!(matches!(nav(HomeSel::Recents(0), "right", 5, true), HomeSel::Recents(1)));
        assert!(matches!(nav(HomeSel::Recents(4), "right", 5, true), HomeSel::Recents(4)));
        assert!(matches!(nav(HomeSel::Recents(1), "down", 5, true), HomeSel::Shortcuts(0)));
        assert!(matches!(nav(HomeSel::Shortcuts(0), "left", 5, true), HomeSel::Shortcuts(0)));
        assert!(matches!(nav(HomeSel::Shortcuts(5), "right", 5, true), HomeSel::Shortcuts(5)));
    }
}
