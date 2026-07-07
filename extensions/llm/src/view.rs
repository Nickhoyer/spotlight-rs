//! The chat panel: a scrolling transcript, a composer, and an in-chat model
//! switcher. Replies stream token-by-token with a pulsing caret; Esc cancels a
//! generation in progress (and otherwise bubbles to the shell to back out).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc;
use futures::StreamExt as _;
use gpui::prelude::*;
use gpui::{
    div, ease_in_out, px, AnyElement, Animation, AnimationExt as _, Context, FocusHandle,
    MouseButton, ScrollHandle, Window,
};

use spotlight_ui::text_input::TextInput;
use spotlight_ui::theme;

use crate::client::{self, Message, StreamEvent};
use crate::{load_config, markdown, secret_key, LlmConfig};

/// One item in the visible transcript.
enum Entry {
    /// A message the user sent.
    User(String),
    /// A finished assistant reply (Markdown).
    Assistant(String),
    /// A completed web search the agent ran, expandable to show the raw results
    /// the tool returned.
    Search(SearchLog),
}

/// A logged `web_search` tool call: the query the model chose, the full results
/// digest it received, and whether the user has expanded it.
struct SearchLog {
    query: String,
    results: String,
    expanded: bool,
}

pub struct LlmView {
    config: LlmConfig,
    /// Flattened `(provider index, model id)` pairs the switcher cycles through.
    options: Vec<(usize, String)>,
    /// Current index into `options`.
    sel: usize,
    /// The visible transcript: user/assistant messages plus expandable
    /// web-search entries, in display order. The LLM history sent on each turn
    /// is derived from this (search entries are display-only) — see [`history`].
    ///
    /// [`history`]: Self::history
    entries: Vec<Entry>,
    input: gpui::Entity<TextInput>,
    /// True while a reply is streaming.
    streaming: bool,
    /// The in-progress assistant reply, appended token-by-token.
    partial: String,
    /// Transient agent status (e.g. "Searching the web…"), shown before the
    /// answer starts streaming. Cleared once text arrives.
    status: Option<String>,
    error: Option<String>,
    /// Flips a running stream off (Esc); the client drops the connection.
    cancel: Arc<AtomicBool>,
    transcript_scroll: ScrollHandle,
    focus_handle: FocusHandle,
    focused_once: bool,
}

impl LlmView {
    pub fn new(seed: Option<String>, cx: &mut Context<Self>) -> Self {
        let config = load_config();

        // One switcher option per model; providers with no model list contribute
        // their single default model.
        let mut options = Vec::new();
        for (i, p) in config.providers.iter().enumerate() {
            if p.models.is_empty() {
                options.push((i, p.model.clone()));
            } else {
                for m in &p.models {
                    options.push((i, m.clone()));
                }
            }
        }
        let sel = options
            .iter()
            .position(|(pi, _)| *pi == config.active)
            .unwrap_or(0);

        let input = cx.new(|cx| TextInput::new(cx, "Ask anything\u{2026}", false));

        let mut view = Self {
            config,
            options,
            sel,
            entries: Vec::new(),
            input,
            streaming: false,
            partial: String::new(),
            status: None,
            error: None,
            cancel: Arc::new(AtomicBool::new(false)),
            transcript_scroll: ScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            focused_once: false,
        };

        // Debug aid (mirrors the SPOTLIGHT_CAPTURE_* knobs): seed a demo
        // conversation so Markdown rendering / wrapping / scrolling / the caret
        // can be screenshotted headlessly. Writes nothing and makes no network
        // calls.
        if std::env::var_os("SPOTLIGHT_LLM_DEMO").is_some() {
            view.seed_demo();
            return view;
        }

        // Warm the cached IP location in the background so it's ready by the time
        // a reply needs it (best-effort; refreshes at most once a day).
        if view.config.ambient_context {
            cx.background_executor()
                .spawn(async { crate::ambient::refresh_location_if_stale() })
                .detach();
        }

        // Auto-send the search text the panel was opened from.
        if let Some(seed) = seed {
            let seed = seed.trim().to_string();
            if !seed.is_empty() {
                view.submit(seed, cx);
            }
        }
        view
    }

    fn seed_demo(&mut self) {
        let assistant = "Here's a quick **Markdown** check:\n\n\
            ## Steps\n\
            1. Wrap *long* lines so they never overflow the bubble horizontally, even when the reply is quite verbose and keeps going for a while.\n\
            2. Render `inline code` and lists.\n\n\
            - bullet one\n\
            - bullet two with `code`\n\n\
            ```\nfn main() {\n    println!(\"hello from a code block\");\n}\n```\n\n\
            > And a short blockquote to finish.";
        self.entries = vec![
            Entry::User("Show me what markdown looks like.".into()),
            Entry::Assistant(assistant.into()),
            Entry::User("Great — now stream a bit.".into()),
            Entry::Search(SearchLog {
                query: "gpui markdown rendering".into(),
                results: "Search results for \"gpui markdown rendering\":\n\n\
                    - GPUI — Zed's GPU UI framework\n  https://zed.dev/gpui\n  A fast, GPU-accelerated UI framework.\n\n\
                    - Rendering text in GPUI\n  https://example.dev/gpui-text\n  Notes on text runs and wrapping."
                    .into(),
                expanded: false,
            }),
        ];
        self.partial =
            "Sure! This reply is currently streaming, and it is long enough that it should wrap onto a second line with a thin caret trailing the last word.".into();
        self.streaming = true;
        // Reveal the tail so a capture shows the code block, blockquote and caret.
        self.transcript_scroll.scroll_to_bottom();
    }

    /// The provider + model for the current switcher selection.
    fn selected(&self) -> Option<(crate::Provider, String)> {
        let (pidx, model) = self.options.get(self.sel)?;
        let provider = self.config.providers.get(*pidx)?.clone();
        Some((provider, model.clone()))
    }

    /// A short "Provider · model" label for the switcher pill.
    fn model_label(&self) -> String {
        match self.selected() {
            Some((p, m)) if !m.is_empty() => format!("{} \u{00b7} {}", p.name, m),
            Some((p, _)) => p.name,
            None => "No provider".to_string(),
        }
    }

    fn cycle_model(&mut self, cx: &mut Context<Self>) {
        if self.options.len() > 1 {
            self.sel = (self.sel + 1) % self.options.len();
            cx.notify();
        }
    }

    /// Append a user message and start streaming the reply.
    fn submit(&mut self, text: String, cx: &mut Context<Self>) {
        if self.streaming || text.trim().is_empty() {
            return;
        }
        let Some((mut provider, model)) = self.selected() else {
            self.error = Some("No AI provider configured yet. Open Settings \u{2192} AI.".into());
            cx.notify();
            return;
        };
        provider.model = model;

        self.entries.push(Entry::User(text));
        self.error = None;
        self.partial.clear();
        self.status = None;
        self.streaming = true;
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = cancel.clone();

        let key = if provider.local {
            String::new()
        } else {
            spotlight_config::load_secret(&secret_key(&provider.name)).unwrap_or_default()
        };
        // Resolve the web-search tool for this reply (its key lives in the secret
        // store); `None` when search is disabled.
        let search = self.config.search.enabled.then(|| crate::websearch::SearchCtx {
            endpoint: self.config.search.endpoint.clone(),
            key: spotlight_config::load_secret(crate::SEARCH_SECRET_KEY).filter(|s| !s.is_empty()),
            max_results: self.config.search.max_results,
        });
        let ambient_on = self.config.ambient_context;
        let history = self.history();
        let (tx, mut rx) = mpsc::unbounded();

        // Producer: blocking SSE read on the background executor. The ambient
        // context (date/time/location) is built here, off the UI thread, since it
        // shells out to `date`/`defaults`.
        cx.background_executor()
            .spawn(async move {
                let context = if ambient_on { crate::ambient::system_context() } else { String::new() };
                client::stream(&provider, &key, &history, search.as_ref(), &context, tx, cancel);
            })
            .detach();

        // Consumer: drain deltas on the UI thread, rendering as they arrive.
        cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.next().await {
                let stop = this
                    .update(cx, |this, cx| this.on_stream_event(ev, cx))
                    .unwrap_or(true);
                if stop {
                    break;
                }
            }
        })
        .detach();

        self.transcript_scroll.scroll_to_bottom();
        cx.notify();
    }

    /// Handle one streamed event; returns `true` when the stream is finished.
    fn on_stream_event(&mut self, ev: StreamEvent, cx: &mut Context<Self>) -> bool {
        // Already finished (e.g. cancelled via Esc): drop any late events so a
        // trailing token can't start a second reply bubble.
        if !self.streaming {
            return true;
        }
        match ev {
            StreamEvent::Delta(s) => {
                // First text after a search clears the status line.
                self.status = None;
                self.partial.push_str(&s);
                self.transcript_scroll.scroll_to_bottom();
                cx.notify();
                false
            }
            StreamEvent::Status(s) => {
                self.status = Some(s);
                self.transcript_scroll.scroll_to_bottom();
                cx.notify();
                false
            }
            StreamEvent::Search { query, results } => {
                // Commit any assistant text streamed before the search as its own
                // bubble, so the search entry lands between it and the follow-up
                // answer rather than splitting a single bubble.
                if !self.partial.is_empty() {
                    self.entries.push(Entry::Assistant(std::mem::take(&mut self.partial)));
                }
                self.status = None;
                self.entries.push(Entry::Search(SearchLog { query, results, expanded: false }));
                self.transcript_scroll.scroll_to_bottom();
                cx.notify();
                false
            }
            StreamEvent::Done => {
                self.finish(cx);
                true
            }
            StreamEvent::Error(e) => {
                self.error = Some(e);
                self.finish(cx);
                true
            }
        }
    }

    /// The LLM history to send: user/assistant messages only (search entries are
    /// display-only — the model already saw those results mid-stream).
    fn history(&self) -> Vec<Message> {
        self.entries
            .iter()
            .filter_map(|e| match e {
                Entry::User(t) => Some(Message { role: "user".into(), content: t.clone() }),
                Entry::Assistant(t) => Some(Message { role: "assistant".into(), content: t.clone() }),
                Entry::Search(_) => None,
            })
            .collect()
    }

    /// Toggle an expandable search entry open/closed.
    fn toggle_search(&mut self, i: usize, cx: &mut Context<Self>) {
        if let Some(Entry::Search(log)) = self.entries.get_mut(i) {
            log.expanded = !log.expanded;
            cx.notify();
        }
    }

    /// Commit the streamed reply to history and clear the streaming state.
    fn finish(&mut self, cx: &mut Context<Self>) {
        if !self.partial.is_empty() {
            self.entries.push(Entry::Assistant(std::mem::take(&mut self.partial)));
        }
        self.partial.clear();
        self.status = None;
        self.streaming = false;
        self.transcript_scroll.scroll_to_bottom();
        cx.notify();
    }

    // ---- keyboard ---------------------------------------------------------

    fn on_key_down(&mut self, event: &gpui::KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "enter" => {
                if !self.streaming {
                    let text = self.input.read(cx).value().trim().to_string();
                    if !text.is_empty() {
                        self.input.update(cx, |i, cx| {
                            i.set_value("");
                            cx.notify();
                        });
                        self.submit(text, cx);
                    }
                }
                cx.stop_propagation();
            }
            "escape" => {
                // Stop a running generation; otherwise let it bubble to the shell
                // so Escape backs out of the panel.
                if self.streaming {
                    self.cancel.store(true, Ordering::Relaxed);
                    self.finish(cx);
                    cx.stop_propagation();
                }
            }
            "tab" => {
                self.cycle_model(cx);
                cx.stop_propagation();
            }
            _ => {}
        }
    }

    // ---- rendering --------------------------------------------------------

    fn header(&self, cx: &mut Context<Self>) -> AnyElement {
        // The shell already draws the panel title; here we only offer the model
        // switcher pill (click or Tab to cycle), right-aligned.
        div()
            .flex()
            .items_center()
            .justify_end()
            .px_4()
            .py_2()
            .child(
                div()
                    .id("llm-model")
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_3()
                    .py_1()
                    .rounded_lg()
                    .bg(theme::tile())
                    .text_xs()
                    .text_color(theme::accent())
                    .hover(|s| s.bg(theme::hover_strong()))
                    .child("\u{2728}")
                    .child(self.model_label())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.cycle_model(cx)),
                    ),
            )
            .into_any_element()
    }

    fn transcript(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut list = div()
            .id("llm-transcript")
            .flex()
            .flex_col()
            .gap_3()
            .px_4()
            .py_3()
            .flex_1()
            // min-height:0 lets this flex child shrink below its content so the
            // overflow actually scrolls (and the composer stays visible).
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.transcript_scroll);

        if self.entries.is_empty() && !self.streaming && self.error.is_none() {
            list = list.child(
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(theme::muted())
                    .child(if self.options.is_empty() {
                        "No AI provider yet \u{2014} add one in Settings \u{2192} AI.".to_string()
                    } else {
                        "Ask anything to start.".to_string()
                    }),
            );
        }

        for (i, entry) in self.entries.iter().enumerate() {
            list = list.child(match entry {
                Entry::User(text) => bubble("user", text),
                Entry::Assistant(text) => bubble("assistant", text),
                Entry::Search(log) => self.search_entry(i, log, cx),
            });
        }
        if self.streaming || !self.partial.is_empty() {
            list = list.child(streaming_bubble(&self.partial, self.status.as_deref()));
        }
        if let Some(err) = &self.error {
            list = list.child(
                div()
                    .px_3()
                    .py_2()
                    .rounded_lg()
                    .bg(gpui::rgba(0xff_5c5c_1f))
                    .text_color(gpui::rgb(0xff_b3b3))
                    .child(err.clone()),
            );
        }

        list.into_any_element()
    }

    /// An expandable "Searched the web" entry. The header row (query + chevron)
    /// toggles open to reveal the raw results the tool handed the model.
    fn search_entry(&self, i: usize, log: &SearchLog, cx: &mut Context<Self>) -> AnyElement {
        let chevron = if log.expanded { "\u{25be}" } else { "\u{25b8}" }; // ▾ / ▸

        let head = div()
            .id(("llm-search", i))
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .rounded_lg()
            .bg(theme::tile())
            .text_xs()
            .text_color(theme::muted())
            .hover(|s| s.bg(theme::hover_strong()))
            .child(div().text_color(theme::accent()).child(chevron))
            .child("\u{1f50e}") // 🔎
            .child(div().text_color(theme::text()).child("Searched the web"))
            .child(div().child(format!("\u{00b7} \u{201c}{}\u{201d}", log.query)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| this.toggle_search(i, cx)),
            );

        let mut card = div().flex().flex_col().gap_2().max_w(px(BUBBLE_MAX)).w_full().child(head);

        if log.expanded {
            // Render the tool output verbatim, line by line (blank lines preserved
            // as spacing between hits).
            let mut body = div()
                .flex()
                .flex_col()
                .px_3()
                .py_2()
                .rounded_lg()
                .bg(theme::hover())
                .text_xs()
                .text_color(theme::muted());
            for line in log.results.lines() {
                body = body.child(div().child(line.to_string()));
            }
            card = card.child(body);
        }

        row(false, card)
    }

    fn composer(&self) -> AnyElement {
        let hint = if self.streaming {
            "Enter to send \u{00b7} Esc to stop \u{00b7} Tab to switch model"
        } else {
            "Enter to send \u{00b7} Esc to close \u{00b7} Tab to switch model"
        };
        div()
            .flex()
            .flex_col()
            .gap_1()
            .px_4()
            .py_3()
            .child(self.input.clone())
            .child(div().text_xs().text_color(theme::muted()).child(hint))
            .into_any_element()
    }
}

impl Render for LlmView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Focus the composer on first appearance so typing works without a click.
        if !self.focused_once {
            let fh = self.input.read(cx).focus_handle().clone();
            window.focus(&fh, cx);
            self.focused_once = true;
        }

        div()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .size_full()
            .flex()
            .flex_col()
            .child(self.header(cx))
            .child(div().h(px(1.)).bg(theme::divider()))
            .child(self.transcript(cx))
            .child(div().h(px(1.)).bg(theme::divider()))
            .child(self.composer())
    }
}

/// Max bubble width — narrower than the panel so replies always wrap and never
/// push the layout horizontally.
const BUBBLE_MAX: f32 = 540.;

/// A finished chat bubble. User messages align right on a faint cyan tile;
/// assistant messages align left and render Markdown.
fn bubble(role: &str, text: &str) -> AnyElement {
    let is_user = role == "user";
    let content: AnyElement = if is_user {
        div().child(text.to_string()).into_any_element()
    } else {
        markdown::render(text)
    };
    row(is_user, bubble_body(is_user).child(content))
}

/// The in-progress assistant reply: plain wrapped text (Markdown is applied once
/// it finalizes) followed by a thin, blinking caret. Before the first token
/// arrives it shows the current agent status ("Searching the web…") if one is
/// set, otherwise "Thinking…".
fn streaming_bubble(partial: &str, status: Option<&str>) -> AnyElement {
    let mut body = bubble_body(false).flex().flex_row().flex_wrap().items_end().gap_1();
    if partial.is_empty() {
        let note = status.unwrap_or("Thinking\u{2026}");
        body = body.child(div().text_color(theme::muted()).child(note.to_string()));
    } else {
        // Split into words so the row wraps and the caret trails the last word.
        // (Markdown styling is applied once the reply finalizes.)
        for word in partial.split_whitespace() {
            body = body.child(div().child(word.to_string()));
        }
    }
    body = body.child(caret());
    row(false, body)
}

/// A thin (2px) blinking caret aligned to the text baseline.
fn caret() -> AnyElement {
    div()
        .w(px(2.))
        .h(px(15.))
        .mb(px(2.))
        .ml(px(1.))
        .rounded_full()
        .bg(theme::accent())
        .with_animation(
            "llm-caret",
            Animation::new(Duration::from_millis(1000)).repeat().with_easing(ease_in_out),
            |el, d| el.opacity(1.0 - (d * 2.0 - 1.0).abs()),
        )
        .into_any_element()
}

/// The bubble container (rounded tile, bounded width, wrapping, clipped).
/// Assistant bubbles take a definite width (capped at `BUBBLE_MAX`) so their
/// text has a bound to wrap within; user bubbles hug their (short) content.
fn bubble_body(is_user: bool) -> gpui::Div {
    div()
        .max_w(px(BUBBLE_MAX))
        .when(!is_user, |d| d.w_full())
        .px_3()
        .py_2()
        .rounded_lg()
        .overflow_hidden()
        .bg(if is_user { theme::tile() } else { theme::hover() })
        .text_color(theme::text())
}

/// Wrap a bubble body in a full-width row, aligned right for the user.
fn row(is_user: bool, body: gpui::Div) -> AnyElement {
    div()
        .flex()
        .w_full()
        .when(is_user, |d| d.justify_end())
        .child(body)
        .into_any_element()
}
