//! The Gmail panel: a stale-while-revalidate list of unread inbox mail, and a
//! drill-in reading pane. HTML bodies render in-app via Blitz to an image
//! (links stay clickable through pre-extracted link boxes); text/plain is the
//! fallback. Escape/Left returns to the list; the browser is always one
//! "Open in Gmail ↗" away.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    canvas, div, img, linear, px, rgba, AnyElement, Animation, AnimationExt as _, Bounds, Context,
    FocusHandle, ImageSource, MouseButton, MouseDownEvent, Pixels, Point, RenderImage,
    ScrollHandle, Window,
};

use spotlight_ui::list::ListNav;
use spotlight_ui::theme;

use crate::client::{GmailClient, INBOX_URL};
use crate::htmlview::{self, HitTester};
use crate::models::{self, Email, Inbox, MailBody};

/// Logical width the email body renders at: the panel's content width (the
/// panel is fixed-width; a small margin keeps the card off the panel edges).
fn read_width() -> f32 {
    spotlight_ui::extension_panel_width() - 32.0
}

/// Arrow-key scroll step in the reading pane.
const SCROLL_STEP: f32 = 80.0;

/// Process-global body cache, keyed by account so panel re-opens don't refetch
/// every message. Bodies never touch the disk.
fn body_cache() -> &'static Mutex<(String, HashMap<u32, MailBody>)> {
    static CACHE: OnceLock<Mutex<(String, HashMap<u32, MailBody>)>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new((String::new(), HashMap::new())))
}

fn cached_body(account: &str, uid: u32) -> Option<MailBody> {
    let guard = body_cache().lock().ok()?;
    (guard.0 == account).then(|| guard.1.get(&uid).cloned())?
}

fn store_body(account: &str, uid: u32, body: MailBody) {
    if let Ok(mut guard) = body_cache().lock() {
        if guard.0 != account {
            guard.0 = account.to_string();
            guard.1.clear();
        }
        guard.1.insert(uid, body);
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
    /// text/plain fallback.
    Text(String),
    Failed(String),
}

struct Reading {
    email: Email,
    state: ReadState,
}

pub struct GmailView {
    client: Option<Arc<GmailClient>>,
    account: String,
    /// Currently shown inbox (may be stale while `fetching`).
    inbox: Inbox,
    fetching: bool,
    error: Option<String>,
    /// Open message, if any (list stays behind it).
    reading: Option<Reading>,
    /// Bumped on each fetch so out-of-order responses can be discarded.
    generation: u64,
    focus_handle: FocusHandle,
    /// Keyboard selection + scroll for the mail list.
    nav: ListNav,
    /// Scroll state for the reading pane.
    read_scroll: ScrollHandle,
    /// Last-painted bounds of the rendered-HTML card, for click→link mapping.
    body_bounds: Rc<Cell<Bounds<Pixels>>>,
    /// Whether we've taken focus once (so we focus on first render only).
    focused_once: bool,
    /// Debug aid: body to open in the reading pane on first render
    /// (`SPOTLIGHT_GMAIL_DEMO_HTML=<path>`; a `.txt` path exercises the
    /// text/plain fallback), for headless captures.
    demo_body: Option<MailBody>,
}

impl GmailView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let config = crate::load_config();
        let client = crate::build_client(&config);

        let mut view = Self {
            client,
            account: config.email.trim().to_string(),
            inbox: crate::load_cache(),
            fetching: false,
            error: None,
            reading: None,
            generation: 0,
            focus_handle: cx.focus_handle(),
            nav: ListNav::new(),
            read_scroll: ScrollHandle::new(),
            body_bounds: Rc::new(Cell::new(Bounds::default())),
            focused_once: false,
            demo_body: std::env::var("SPOTLIGHT_GMAIL_DEMO_HTML").ok().and_then(|path| {
                let content = std::fs::read_to_string(&path).ok()?;
                Some(if path.ends_with(".txt") {
                    MailBody {
                        html: None,
                        text: Some(content),
                    }
                } else {
                    MailBody {
                        html: Some(content),
                        text: None,
                    }
                })
            }),
        };

        if view.client.is_none() {
            view.error = Some("Gmail isn't configured yet. Open Settings → Gmail.".to_string());
        } else {
            view.refresh(cx);
        }
        view
    }

    /// Kick off a background fetch (the stale list stays up meanwhile).
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        self.fetching = true;
        self.error = None;
        self.generation += 1;
        let generation = self.generation;

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { client.fetch_inbox() })
                .await;
            let _ = this.update(cx, |this, cx| {
                // Discard if a newer fetch superseded us.
                if this.generation != generation {
                    return;
                }
                this.fetching = false;
                match result {
                    Ok(mut inbox) => {
                        // Keep snippets already known from cache or prefetch.
                        for email in &mut inbox.emails {
                            if let Some(body) = cached_body(&this.account, email.uid) {
                                fill_snippet(email, &body);
                            } else if let Some(old) = this
                                .inbox
                                .emails
                                .iter()
                                .find(|e| e.uid == email.uid && !e.snippet.is_empty())
                            {
                                email.snippet = old.snippet.clone();
                            }
                        }
                        crate::save_cache(&inbox);
                        this.nav.clamp(inbox.emails.len());
                        this.inbox = inbox;
                        this.error = None;
                        this.prefetch_bodies(cx);
                    }
                    Err(e) => this.error = Some(format!("Couldn't load mail: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Fetch bodies for listed messages in the background, filling row snippets
    /// as they arrive (and warming the cache so drill-ins are instant).
    fn prefetch_bodies(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let account = self.account.clone();
        let uids: Vec<u32> = self
            .inbox
            .emails
            .iter()
            .map(|e| e.uid)
            .filter(|&uid| uid != 0 && cached_body(&account, uid).is_none())
            .collect();
        if uids.is_empty() {
            return;
        }
        let generation = self.generation;

        cx.spawn(async move |this, cx| {
            for uid in uids {
                let client = client.clone();
                let result = cx
                    .background_executor()
                    .spawn(async move { client.fetch_body(uid) })
                    .await;
                let stale = this
                    .update(cx, |this, cx| {
                        if this.generation != generation {
                            return true;
                        }
                        if let Ok(body) = result {
                            store_body(&this.account, uid, body.clone());
                            if let Some(email) =
                                this.inbox.emails.iter_mut().find(|e| e.uid == uid)
                            {
                                fill_snippet(email, &body);
                            }
                            cx.notify();
                        }
                        false
                    })
                    .unwrap_or(true);
                if stale {
                    return;
                }
            }
            // Persist the now-filled snippets for the next cold open.
            let _ = this.update(cx, |this, _| crate::save_cache(&this.inbox));
        })
        .detach();
    }

    // ---- reading pane -----------------------------------------------------

    fn open_reading(&mut self, email: Email, window: &Window, cx: &mut Context<Self>) {
        // Rows from a pre-IMAP cache have no UID to fetch by; browser instead.
        if email.uid == 0 {
            open_url(&email.gmail_url());
            return;
        }
        let uid = email.uid;
        let scale = window.scale_factor() as f64;
        crate::debug_log(&format!(
            "open uid={uid} cached={} scale={scale}",
            cached_body(&self.account, uid).is_some()
        ));
        self.read_scroll.set_offset(Point::default());
        self.reading = Some(Reading {
            email: email.clone(),
            state: ReadState::Loading,
        });

        if let Some(body) = cached_body(&self.account, uid) {
            self.present_body(uid, body, scale, cx);
        } else if let Some(client) = self.client.clone() {
            cx.spawn(async move |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move { client.fetch_body(uid) })
                    .await;
                let _ = this.update(cx, |this, cx| match result {
                    Ok(body) => {
                        store_body(&this.account, uid, body.clone());
                        if let Some(email) = this.inbox.emails.iter_mut().find(|e| e.uid == uid) {
                            fill_snippet(email, &body);
                        }
                        this.present_body(uid, body, scale, cx);
                    }
                    Err(e) => {
                        crate::debug_log(&format!("fetch uid={uid}: FAILED: {e}"));
                        this.set_read_state(uid, ReadState::Failed(format!("Couldn't load message: {e}")));
                        cx.notify();
                    }
                });
            })
            .detach();
        } else {
            self.set_read_state(uid, ReadState::Failed("Gmail isn't configured.".to_string()));
        }
        cx.notify();
    }

    /// Move a fetched body into the pane: render HTML via Blitz in the
    /// background, or fall straight through to text.
    fn present_body(&mut self, uid: u32, body: MailBody, scale: f64, cx: &mut Context<Self>) {
        if self.reading.as_ref().map(|r| r.email.uid) != Some(uid) {
            return;
        }
        let text_fallback = body.text.clone();
        if let Some(html) = body.html {
            let width = read_width() as u32;
            cx.spawn(async move |this, cx| {
                let rendered = cx
                    .background_executor()
                    .spawn(async move { htmlview::render_email(&html, width, scale) })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    let state = match rendered {
                        Ok((r, hit)) => {
                            let buffer = image::RgbaImage::from_raw(r.width, r.height, r.bgra);
                            match buffer {
                                Some(buffer) => ReadState::Html {
                                    image: Arc::new(RenderImage::new(vec![image::Frame::new(
                                        buffer,
                                    )])),
                                    hit,
                                    logical_w: r.logical_width,
                                    logical_h: r.logical_height,
                                },
                                None => {
                                    crate::debug_log(&format!(
                                        "present uid={uid}: buffer size mismatch, falling back"
                                    ));
                                    text_or_failed(text_fallback)
                                }
                            }
                        }
                        Err(_) => text_or_failed(text_fallback),
                    };
                    crate::debug_log(&format!("state uid={uid} -> {}", state_name(&state)));
                    this.set_read_state(uid, state);
                    cx.notify();
                });
            })
            .detach();
        } else {
            let state = text_or_failed(text_fallback);
            crate::debug_log(&format!("state uid={uid} (no html) -> {}", state_name(&state)));
            self.set_read_state(uid, state);
        }
        cx.notify();
    }

    fn set_read_state(&mut self, uid: u32, state: ReadState) {
        if let Some(reading) = self.reading.as_mut() {
            if reading.email.uid == uid {
                reading.state = state;
            }
        }
    }

    fn close_reading(&mut self) {
        self.reading = None;
    }

    /// A click inside the rendered-HTML card: map window coordinates to
    /// document coordinates and open the link under the pointer, if any.
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
        if let Some(href) = hit.hit(local_x, local_y) {
            let href = href.trim();
            if href.starts_with("https://") || href.starts_with("http://") || href.starts_with("mailto:") {
                open_url(href);
            }
        }
    }

    fn scroll_reading(&mut self, delta: f32) {
        let mut offset = self.read_scroll.offset();
        offset.y = px((f32::from(offset.y) + delta).min(0.0));
        self.read_scroll.set_offset(offset);
    }

    // ---- keyboard ---------------------------------------------------------

    fn on_key_down(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();

        // Reading pane takes precedence while open.
        if let Some(reading) = &self.reading {
            match key {
                "escape" | "left" => self.close_reading(),
                "up" => self.scroll_reading(SCROLL_STEP),
                "down" => self.scroll_reading(-SCROLL_STEP),
                "pageup" => self.scroll_reading(SCROLL_STEP * 5.0),
                "pagedown" => self.scroll_reading(-SCROLL_STEP * 5.0),
                "o" => {
                    open_url(&reading.email.gmail_url());
                }
                _ => return,
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }

        let len = self.inbox.emails.len();
        match key {
            "down" if len > 0 => self.nav.next(len),
            "up" if len > 0 => self.nav.prev(),
            "enter" => match self.inbox.emails.get(self.nav.selected).cloned() {
                Some(email) => self.open_reading(email, window, cx),
                // Empty inbox: Enter still gets you to Gmail.
                None => open_url(INBOX_URL),
            },
            // Let Escape bubble to the shell (back to Home), etc.
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    // ---- rendering --------------------------------------------------------

    fn email_entry(&self, i: usize, email: &Email, now: i64, cx: &mut Context<Self>) -> AnyElement {
        let age = models::age_label(email, now);
        let subject = if email.subject.is_empty() {
            "(no subject)".to_string()
        } else {
            email.subject.clone()
        };

        div()
            // Stateful id so gpui repaints hover on mouse-move inside the
            // scroll container.
            .id(("gmail-row", i))
            .flex()
            .items_center()
            .gap_3()
            .px_3()
            .py_2()
            .rounded_lg()
            .when(self.nav.selected == i, |t| t.bg(theme::selected()))
            .hover(|s| s.bg(theme::hover()))
            .child(
                div()
                    .w(px(160.))
                    .flex_none()
                    .overflow_hidden()
                    .child(div().truncate().text_color(theme::text()).child(email.sender().to_string())),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .gap_2()
                    .overflow_hidden()
                    .child(div().truncate().text_color(theme::accent()).child(subject))
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .child(div().truncate().text_xs().text_color(theme::muted()).child(email.snippet.clone())),
                    ),
            )
            .child(div().flex_none().text_xs().text_color(theme::muted()).child(age))
            .on_mouse_down(MouseButton::Left, {
                let email = email.clone();
                cx.listener(move |this, _, window: &mut Window, cx| {
                    this.open_reading(email.clone(), window, cx)
                })
            })
            .into_any_element()
    }

    /// "N unread" on the left, an "Open Gmail ↗" pill on the right.
    fn header(&self, cx: &mut Context<Self>) -> AnyElement {
        let count = self.inbox.fullcount;
        let label = match count {
            0 => "No unread mail".to_string(),
            1 => "1 unread".to_string(),
            n => format!("{n} unread"),
        };
        div()
            .flex()
            .items_center()
            .justify_between()
            .px_4()
            .py_2()
            .child(div().text_xs().text_color(theme::muted()).child(label))
            .child(
                div()
                    .id("gmail-open-inbox")
                    .px_3()
                    .py_1()
                    .rounded_lg()
                    .bg(theme::tile())
                    .hover(|s| s.bg(theme::hover_strong()))
                    .text_xs()
                    .text_color(theme::accent())
                    .child("Open Gmail ↗")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_, _, _, _| open_url(INBOX_URL)),
                    ),
            )
            .into_any_element()
    }

    fn render_reading(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(reading) = &self.reading else {
            return centered("");
        };
        let email = reading.email.clone();
        let age = models::age_label(&email, models::now_unix());
        let byline = match (email.sender(), age.as_str()) {
            (s, "") => s.to_string(),
            (s, age) => format!("{s} · {age}"),
        };

        let header = div()
            .flex()
            .items_center()
            .gap_3()
            .px_4()
            .py_3()
            .child(
                div()
                    .id("gmail-read-back")
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
                    .child(
                        div().truncate().text_xl().text_color(theme::text()).child(
                            if email.subject.is_empty() {
                                "(no subject)".to_string()
                            } else {
                                email.subject.clone()
                            },
                        ),
                    ),
            )
            .child(
                div()
                    .id("gmail-read-open")
                    .flex_none()
                    .px_3()
                    .py_1()
                    .rounded_lg()
                    .bg(theme::tile())
                    .hover(|s| s.bg(theme::hover_strong()))
                    .text_xs()
                    .text_color(theme::accent())
                    .child("Open in Gmail ↗")
                    .on_mouse_down(MouseButton::Left, {
                        let url = email.gmail_url();
                        cx.listener(move |_, _, _, _| open_url(&url))
                    }),
            );

        let content: AnyElement = match &reading.state {
            ReadState::Loading => centered("Loading message…"),
            ReadState::Failed(msg) => centered(msg),
            ReadState::Text(text) => {
                let mut column = div()
                    .w(px(read_width()))
                    .flex()
                    .flex_col()
                    .py_4()
                    .text_color(theme::text());
                for line in text.lines().take(2000) {
                    column = if line.trim().is_empty() {
                        column.child(div().h(px(12.)))
                    } else {
                        column.child(div().child(line.to_string()))
                    };
                }
                self.scrollable_body(div().flex().justify_center().child(column))
            }
            ReadState::Html {
                image,
                logical_w,
                logical_h,
                ..
            } => {
                let bounds_cell = self.body_bounds.clone();
                let card = div()
                    .id("gmail-read-card")
                    .relative()
                    .w(px(*logical_w))
                    .h(px(*logical_h))
                    .rounded_lg()
                    .overflow_hidden()
                    // White under the image: emails are white anyway, and if
                    // the image itself fails to paint this shows a white card
                    // (image problem) instead of nothing (layout problem).
                    .bg(gpui::rgb(0xffffff))
                    .child(
                        img(ImageSource::Render(image.clone()))
                            .w(px(*logical_w))
                            .h(px(*logical_h)),
                    )
                    // Capture the card's painted bounds each frame so clicks can
                    // be mapped from window space into document space. Log when
                    // the size changes — i.e. once per open — as proof the card
                    // actually laid out.
                    .child(
                        canvas(
                            move |bounds, _, _| {
                                if bounds_cell.get().size != bounds.size {
                                    crate::debug_log(&format!("card painted at {bounds:?}"));
                                }
                                bounds_cell.set(bounds)
                            },
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
            .id("gmail-read-scroll")
            .flex_1()
            // min-height:0 lets this flex child shrink below its content so the
            // overflow actually scrolls (see the jira-list note).
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.read_scroll)
            .child(inner)
            .into_any_element()
    }
}

impl Render for GmailView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Take keyboard focus when the panel first appears so arrow-key
        // navigation works without a click.
        if !self.focused_once {
            window.focus(&self.focus_handle, cx);
            self.focused_once = true;
        }
        // Debug aid: drop straight into the reading pane with a file's body.
        if let Some(body) = self.demo_body.take() {
            let email = self.inbox.emails.first().cloned().unwrap_or_else(|| Email {
                uid: 1,
                subject: "Demo message".to_string(),
                from_name: "Demo".to_string(),
                ..Default::default()
            });
            store_body(&self.account, email.uid, body);
            self.open_reading(email, window, cx);
        }

        // The reading pane takes over the whole panel as its own view.
        if self.reading.is_some() {
            return self.render_reading(cx);
        }

        let body: AnyElement = if !self.inbox.emails.is_empty() {
            let now = models::now_unix();
            let mut list = div()
                .id("gmail-list")
                .flex()
                .flex_col()
                .gap_1()
                .px_2()
                .py_2()
                .flex_1()
                // min-height:0 lets this flex child shrink below its content so
                // the overflow actually scrolls (see the jira-list note).
                .min_h(px(0.))
                .overflow_y_scroll()
                .track_scroll(&self.nav.scroll);
            for (i, email) in self.inbox.emails.clone().iter().enumerate() {
                list = list.child(self.email_entry(i, email, now, cx));
            }
            spotlight_ui::list::faded_scroll(&self.nav.scroll, true, list.into_any_element())
        } else if let Some(err) = &self.error {
            centered(err)
        } else if self.fetching {
            centered("Checking mail…")
        } else {
            centered("Inbox zero — nothing unread. ✨")
        };

        div()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .size_full()
            .flex()
            .flex_col()
            .child(self.header(cx))
            // Stale-while-revalidate indicator (same treatment as the Jira
            // panel): 2px reserved so the list doesn't jump, sweeping while a
            // refresh is in flight.
            .child(
                div().h(px(2.)).w_full().when(self.fetching, |this| {
                    this.bg(rgba(0x6e_e7ff_22)).child(
                        div().h(px(2.)).bg(theme::accent()).with_animation(
                            "gmail-sync",
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

/// Prefer the body's text for a row snippet (mail-parser synthesizes text from
/// HTML-only messages, so this is nearly always present).
fn fill_snippet(email: &mut Email, body: &MailBody) {
    if let Some(text) = &body.text {
        email.snippet = models::snippet_of(text);
    }
}

fn state_name(state: &ReadState) -> String {
    match state {
        ReadState::Loading => "Loading".to_string(),
        ReadState::Html { logical_w, logical_h, .. } => format!("Html({logical_w}x{logical_h})"),
        ReadState::Text(t) => format!("Text({}B)", t.len()),
        ReadState::Failed(msg) => format!("Failed({msg})"),
    }
}

fn text_or_failed(text: Option<String>) -> ReadState {
    match text {
        Some(text) if !text.trim().is_empty() => ReadState::Text(text),
        _ => ReadState::Failed("This message has no readable content.".to_string()),
    }
}

fn open_url(url: &str) {
    let _ = std::process::Command::new("/usr/bin/open").arg(url).spawn();
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
