//! The "Music" panel: server health, recent scrobbles, generated playlists,
//! and a run-daily-now action. Read-mostly; refreshes on open and on demand.

use gpui::prelude::*;
use gpui::{div, Context, FocusHandle};

use spotlight_ui::{controls, theme};

use crate::client::{PlaylistRow, ScrobbleRow};

pub struct MusicView {
    playlists: Vec<PlaylistRow>,
    scrobbles: Vec<ScrobbleRow>,
    health: Option<String>,
    error: Option<String>,
    fetching: bool,
    daily_running: bool,
    refresh_focus: FocusHandle,
    daily_focus: FocusHandle,
    generation: u64,
}

impl MusicView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            playlists: Vec::new(),
            scrobbles: Vec::new(),
            health: None,
            error: None,
            fetching: false,
            daily_running: false,
            refresh_focus: cx.focus_handle(),
            daily_focus: cx.focus_handle(),
            generation: 0,
        };
        view.refresh(cx);
        view
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if crate::build_client().is_none() {
            self.error = Some("Not configured — set the server URL and token in Settings → Music.".into());
            return;
        }
        self.fetching = true;
        self.error = None;
        self.generation += 1;
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let client = crate::build_client().ok_or_else(|| anyhow::anyhow!("not configured"))?;
                    let health = client.health()?;
                    let playlists = client.playlists()?;
                    let scrobbles = client.scrobbles(30)?;
                    anyhow::Ok((health, playlists, scrobbles))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.fetching = false;
                match result {
                    Ok((health, playlists, scrobbles)) => {
                        let ok = health.get("ok").and_then(|o| o.as_bool()).unwrap_or(false);
                        let apple = health.get("apple_auth").and_then(|o| o.as_bool()).unwrap_or(false);
                        this.health = Some(if ok {
                            "healthy".to_string()
                        } else if !apple {
                            "Apple token expired — run `ampm auth apple` on the server".to_string()
                        } else {
                            "unhealthy".to_string()
                        });
                        this.playlists = playlists;
                        this.scrobbles = scrobbles;
                        this.error = None;
                    }
                    Err(e) => this.error = Some(format!("Couldn't reach the ampm server: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn run_daily(&mut self, cx: &mut Context<Self>) {
        if self.daily_running {
            return;
        }
        self.daily_running = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let client = crate::build_client().ok_or_else(|| anyhow::anyhow!("not configured"))?;
                    client.daily_run()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.daily_running = false;
                match result {
                    Ok(_) => this.refresh(cx),
                    Err(e) => this.error = Some(format!("Daily run failed: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn playlist_row(p: &PlaylistRow) -> impl IntoElement {
        let status = if p.deleted_at.is_some() { " · cleaned up" } else { "" };
        div()
            .flex()
            .justify_between()
            .gap_3()
            .py_1()
            .child(div().text_color(theme::text()).child(p.name.clone()))
            .child(
                div()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(format!("{}{status}", p.kind)),
            )
    }

    fn scrobble_row(s: &ScrobbleRow) -> impl IntoElement {
        let status = match s.lastfm_status.as_str() {
            "submitted" => "",
            "pending" => " · pending",
            other => other,
        };
        div()
            .flex()
            .justify_between()
            .gap_3()
            .py_1()
            .child(
                div()
                    .text_color(theme::text())
                    .child(format!("{} — {}", s.artist, s.title)),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(format!("{}{status}", relative_time(s.approx_ts))),
            )
    }
}

impl Render for MusicView {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .pb_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().text_lg().child("🎵 Music"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::muted())
                            .child(self.health.clone().unwrap_or_else(|| {
                                if self.fetching { "loading…".into() } else { String::new() }
                            })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(controls::button(
                        &self.daily_focus,
                        if self.daily_running { "Generating…" } else { "Run daily mixes" },
                        cx,
                        |this, _, cx| this.run_daily(cx),
                    ))
                    .child(controls::button(&self.refresh_focus, "Refresh", cx, |this, _, cx| {
                        this.refresh(cx)
                    })),
            );

        let error = self.error.clone().map(|e| {
            div()
                .py_2()
                .text_color(gpui::rgba(0xff6b6bff))
                .text_xs()
                .child(e)
        });

        let playlists = div().flex().flex_col().children(
            self.playlists
                .iter()
                .filter(|p| p.deleted_at.is_none())
                .take(15)
                .map(Self::playlist_row),
        );
        let scrobbles = div()
            .flex()
            .flex_col()
            .children(self.scrobbles.iter().take(30).map(Self::scrobble_row));

        div()
            .id("music-panel")
            .flex()
            .flex_col()
            .size_full()
            .p_4()
            .overflow_y_scroll()
            .child(header)
            .children(error)
            .child(controls::section("Generated playlists", playlists))
            .child(controls::section("Recent listening", scrobbles))
    }
}

/// "3m ago"-style formatting without pulling a date dependency into the crate.
fn relative_time(ts: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let delta = (now - ts).max(0);
    match delta {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", delta / 60),
        3600..=86399 => format!("{}h ago", delta / 3600),
        _ => format!("{}d ago", delta / 86400),
    }
}
