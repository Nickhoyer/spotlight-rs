//! The Gmail panel: a stale-while-revalidate list of unread inbox mail.
//! Enter (or a click) opens the selected message in Gmail; with an empty inbox
//! Enter opens Gmail itself.

use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    div, linear, px, rgba, AnyElement, Animation, AnimationExt as _, Context, FocusHandle,
    MouseButton, Window,
};

use spotlight_ui::list::ListNav;
use spotlight_ui::theme;

use crate::client::{GmailClient, INBOX_URL};
use crate::models::{self, Email, Inbox};

pub struct GmailView {
    client: Option<Arc<GmailClient>>,
    /// Currently shown inbox (may be stale while `fetching`).
    inbox: Inbox,
    fetching: bool,
    error: Option<String>,
    /// Bumped on each fetch so out-of-order responses can be discarded.
    generation: u64,
    focus_handle: FocusHandle,
    /// Keyboard selection + scroll for the mail list.
    nav: ListNav,
    /// Whether we've taken focus once (so we focus on first render only).
    focused_once: bool,
}

impl GmailView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let config = crate::load_config();
        let client = crate::build_client(&config).map(Arc::new);

        let mut view = Self {
            client,
            inbox: crate::load_cache(),
            fetching: false,
            error: None,
            generation: 0,
            focus_handle: cx.focus_handle(),
            nav: ListNav::new(),
            focused_once: false,
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
                    Ok(inbox) => {
                        crate::save_cache(&inbox);
                        this.nav.clamp(inbox.emails.len());
                        this.inbox = inbox;
                        this.error = None;
                    }
                    Err(e) => this.error = Some(format!("Couldn't load mail: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn open_email(&self, email: &Email) {
        let url = if email.link.is_empty() {
            INBOX_URL.to_string()
        } else {
            email.link.clone()
        };
        open_url(&url);
    }

    // ---- keyboard ---------------------------------------------------------

    fn on_key_down(
        &mut self,
        event: &gpui::KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let len = self.inbox.emails.len();
        match event.keystroke.key.as_str() {
            "down" if len > 0 => self.nav.next(len),
            "up" if len > 0 => self.nav.prev(),
            "enter" => {
                match self.inbox.emails.get(self.nav.selected) {
                    Some(email) => self.open_email(email),
                    // Empty inbox: Enter still gets you to Gmail.
                    None => open_url(INBOX_URL),
                }
            }
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
                cx.listener(move |this, _, _, _| this.open_email(&email))
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
}

impl Render for GmailView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Take keyboard focus when the panel first appears so arrow-key
        // navigation works without a click.
        if !self.focused_once {
            window.focus(&self.focus_handle, cx);
            self.focused_once = true;
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
