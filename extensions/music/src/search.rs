//! `radio <track>` in the main bar: live typeahead against the ampm server's
//! catalog search. Unlike Jira (offline cache), this queries the network —
//! acceptable because the keyword gates it to explicit use, the shell debounces
//! input, and the client timeout is short.

use async_trait::async_trait;

use spotlight_core::{Action, Extension, Icon, Query, ResultItem};

pub struct MusicSearch;

/// Action ids are `radio:<catalogId>:<display label>`; `run` parses them back.
fn action_id(id: &str, label: &str) -> String {
    format!("radio:{id}:{label}")
}

#[async_trait]
impl Extension for MusicSearch {
    fn id(&self) -> &'static str {
        crate::EXT_ID
    }

    fn name(&self) -> &'static str {
        "Music"
    }

    fn keyword(&self) -> Option<&'static str> {
        Some("radio")
    }

    async fn query(&self, query: &Query) -> Vec<ResultItem> {
        if query.text.len() < 2 {
            return Vec::new();
        }
        let Some(client) = crate::build_client() else {
            return vec![ResultItem {
                id: "music:unconfigured".into(),
                title: "Music server not configured".into(),
                subtitle: Some("Settings → Music: server URL and token".into()),
                icon: Some(Icon::Glyph("🎵".into())),
                action: Action::None,
                score: 0,
                source: crate::EXT_ID.to_string(),
            }];
        };
        match client.search(&query.text, 8) {
            Ok(songs) => songs
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    let label = format!("{} — {}", s.artist_name, s.name);
                    ResultItem {
                        id: format!("music:{}", s.id),
                        title: format!("Radio from: {label}"),
                        subtitle: s.album_name.clone(),
                        icon: Some(Icon::Glyph("📻".into())),
                        action: Action::Custom(action_id(&s.id, &label)),
                        // Preserve the server's (catalog relevance) order.
                        score: 1000 - i as i32,
                        source: crate::EXT_ID.to_string(),
                    }
                })
                .collect(),
            Err(e) => vec![ResultItem {
                id: "music:error".into(),
                title: "Music server unreachable".into(),
                subtitle: Some(e.to_string()),
                icon: Some(Icon::Glyph("🎵".into())),
                action: Action::None,
                score: 0,
                source: crate::EXT_ID.to_string(),
            }],
        }
    }

    fn run(&self, action_id: &str) -> anyhow::Result<()> {
        let rest = action_id
            .strip_prefix("radio:")
            .ok_or_else(|| anyhow::anyhow!("unknown action: {action_id}"))?;
        let (song_id, label) = rest.split_once(':').unwrap_or((rest, ""));
        let (song_id, label) = (song_id.to_string(), label.to_string());
        // Generation takes up to a minute; run it off the UI thread and notify
        // when the playlist lands (mirrors ext-scripts' fire-and-forget).
        std::thread::spawn(move || {
            let Some(client) = crate::build_client() else {
                return;
            };
            match client.radio(&song_id) {
                Ok(v) => {
                    let name = v.get("name").and_then(|n| n.as_str()).unwrap_or("radio playlist");
                    let n = v.get("tracks").and_then(|t| t.as_array()).map(|t| t.len()).unwrap_or(0);
                    notify("Radio playlist ready", &format!("{name} — {n} tracks"));
                }
                Err(e) => {
                    eprintln!("music: radio from {label} failed: {e:#}");
                    notify("Radio failed", &format!("{label}: {e}"));
                }
            }
        });
        Ok(())
    }
}

/// Best-effort macOS notification; failures are irrelevant.
fn notify(title: &str, body: &str) {
    let script = format!(
        "display notification \"{}\" with title \"{}\"",
        escape_applescript(body),
        escape_applescript(title)
    );
    let _ = std::process::Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .output();
}

pub(crate) fn escape_applescript(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}
