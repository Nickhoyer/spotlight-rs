//! GPUI shell for the launcher. This crate isolates all GPUI usage so the
//! pre-1.0 framework's churn touches one place.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::executor::block_on;
use gpui::prelude::*;
use gpui::{
    div, ease_in_out, linear, px, rgb, rgba, size, Animation, AnimationExt as _, App, Bounds,
    Context, FocusHandle, KeyDownEvent, Rgba, Window, WindowBackgroundAppearance, WindowBounds,
    WindowKind, WindowOptions,
};

use spotlight_core::{Action, Icon, Registry, ResultItem};

const ACCENT: u32 = 0x6e_e7ff;
const TEXT: u32 = 0xe8_ecf4;
const MUTED: u32 = 0x8a_93a6;
const MAX_RESULTS: usize = 8;

/// CoreGraphics window number of the launcher window, published once the window
/// exists so the debug capture thread (see [`run`]) can grab it. Zero until set.
static CAPTURE_WINDOW: AtomicU32 = AtomicU32::new(0);

/// The root view: a search box plus a ranked results list.
pub struct SpotlightView {
    registry: Arc<Registry>,
    query: String,
    results: Vec<ResultItem>,
    selected: usize,
    focus_handle: FocusHandle,
}

impl SpotlightView {
    fn new(registry: Arc<Registry>, window: &mut Window, cx: &mut Context<Self>) -> Self {
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
            query: String::new(),
            results: Vec::new(),
            selected: 0,
            focus_handle,
        };

        // Debug aid: pre-fill a query so captures can show result states.
        if let Ok(q) = std::env::var("SPOTLIGHT_CAPTURE_QUERY") {
            if !q.is_empty() {
                view.query = q;
                view.results = block_on(view.registry.query(&view.query));
            }
        }

        view
    }

    /// Re-run the search for the current query. Extensions are in-memory today,
    /// so we block on the (immediately-ready) future on the main thread.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.results = block_on(self.registry.query(&self.query));
        self.selected = 0;
        cx.notify();
    }

    fn activate(&mut self, cx: &mut Context<Self>) {
        let Some(item) = self.results.get(self.selected).cloned() else {
            return;
        };
        match &item.action {
            Action::Open(path) => {
                let _ = spotlight_platform_macos::apps::launch(path);
            }
            Action::OpenUrl(url) => {
                let _ = std::process::Command::new("/usr/bin/open").arg(url).spawn();
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

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        match ks.key.as_str() {
            "escape" => cx.quit(),
            "backspace" => {
                self.query.pop();
                self.refresh(cx);
            }
            "up" => {
                self.selected = self.selected.saturating_sub(1);
                cx.notify();
            }
            "down" => {
                if self.selected + 1 < self.results.len().min(MAX_RESULTS) {
                    self.selected += 1;
                    cx.notify();
                }
            }
            "enter" => self.activate(cx),
            _ => {
                // Ignore shortcuts; only insert real typed characters.
                if !ks.modifiers.platform && !ks.modifiers.control {
                    if let Some(ch) = &ks.key_char {
                        self.query.push_str(ch);
                        self.refresh(cx);
                    }
                }
            }
        }
    }
}

impl Render for SpotlightView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let accent = rgb(ACCENT);
        let text = rgb(TEXT);
        let muted = rgb(MUTED);

        let search_row = div()
            .flex()
            .items_center()
            .gap_3()
            .px_5()
            .py_4()
            .child(div().text_2xl().text_color(accent).child("⌕"))
            .child(if self.query.is_empty() {
                div()
                    .text_xl()
                    .text_color(muted)
                    .child("Search apps, do math…")
            } else {
                div().text_xl().text_color(text).child(self.query.clone())
            })
            .child(
                // Blinking caret.
                div()
                    .w(px(2.))
                    .h(px(24.))
                    .rounded_full()
                    .bg(accent)
                    .with_animation(
                        "caret",
                        Animation::new(Duration::from_millis(1100))
                            .repeat()
                            .with_easing(linear),
                        |this, delta| this.opacity(if delta < 0.5 { 1.0 } else { 0.15 }),
                    ),
            );

        let results_list = div()
            .flex()
            .flex_col()
            .gap_1()
            .px_2()
            .pb_2()
            .children(
                self.results
                    .iter()
                    .take(MAX_RESULTS)
                    .enumerate()
                    .map(|(i, item)| result_row(item, i == self.selected, accent, text, muted)),
            );

        let panel = div()
            .w(px(680.))
            .bg(rgba(0x12_141c_f2))
            .rounded_3xl()
            .border_1()
            .border_color(rgba(0x6e_e7ff_40))
            .shadow_lg()
            .overflow_hidden()
            .flex()
            .flex_col()
            .child(search_row)
            .when(!self.results.is_empty(), |this| {
                this.child(div().h(px(1.)).bg(rgba(0xff_ffff_14)))
                    .child(results_list)
            })
            .with_animation(
                "enter",
                Animation::new(Duration::from_millis(170)).with_easing(ease_in_out),
                |this, delta| this.opacity(delta),
            );

        div()
            .key_context("Spotlight")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            // Set an explicit font + default text color on the root so all text
            // inherits a known-present family rather than relying on the default.
            .font_family("Helvetica Neue")
            .text_color(text)
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .pt(px(140.))
            .child(panel)
    }
}

fn result_row(
    item: &ResultItem,
    selected: bool,
    accent: Rgba,
    text: Rgba,
    muted: Rgba,
) -> impl IntoElement {
    let leading = match &item.icon {
        Some(Icon::Glyph(glyph)) => div().text_2xl().child(glyph.clone()),
        _ => div()
            .size(px(28.))
            .rounded_md()
            .bg(rgba(0x6e_e7ff_22))
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
    };

    div()
        .flex()
        .items_center()
        .gap_3()
        .px_3()
        .py_2()
        .rounded_lg()
        .when(selected, |this| this.bg(rgba(0x6e_e7ff_1f)))
        .child(leading)
        .child(
            div()
                .flex()
                .flex_col()
                .child(div().text_color(text).child(item.title.clone()))
                .when_some(item.subtitle.clone(), |this, sub| {
                    this.child(div().text_xs().text_color(muted).child(sub))
                }),
        )
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
/// `SPOTLIGHT_CAPTURE` (output path), `SPOTLIGHT_CAPTURE_DELAY_MS` (default 700),
/// `SPOTLIGHT_CAPTURE_QUERY` (pre-filled search text).
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
pub fn run(registry: Registry) {
    let registry = Arc::new(registry);
    spawn_capture_thread();
    gpui_platform::application().run(move |cx: &mut App| {
        cx.activate(true);
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let bounds = Bounds::centered(None, size(px(680.), px(520.)), cx);
        cx.open_window(
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
            move |window, cx| cx.new(|cx| SpotlightView::new(registry.clone(), window, cx)),
        )
        .expect("failed to open launcher window");
    });
}
