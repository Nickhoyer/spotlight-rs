//! Apple Music extension: a thin client for the `ampm` server (see
//! ~/Repos/personal/apple-music-playlist-manager).
//!
//! Three surfaces, following the Jira extension's split:
//! - `radio <track>` in the main bar: typeahead against the server's catalog
//!   search; Enter asks the server to generate a radio playlist.
//! - A "Music" panel: recent scrobbles, generated playlists, run-daily-now.
//! - A settings tab: server URL + bearer token + connection test.
//!
//! Plus one background worker unique to this extension: the hourly cleanup
//! sweep. Apple's API cannot delete playlists, so the server only *queues*
//! deletions (`/cleanup/pending`) and this Mac-side worker executes them
//! against Music.app via AppleScript, then confirms (`/cleanup/confirm`).

mod cleanup;
mod client;
mod search;
mod settings_tab;
mod view;

pub use cleanup::spawn_cleanup_worker;
pub use search::MusicSearch;

use gpui::prelude::*;
use serde::{Deserialize, Serialize};
use spotlight_config::AppConfig;
use spotlight_ui::{PanelEntry, SettingsTabFactory};

use crate::client::MusicClient;
use crate::settings_tab::MusicSettingsTab;
use crate::view::MusicView;

/// Extension id; also the key for this extension's settings blob in config.
pub const EXT_ID: &str = "music";
/// Secret-store key for the ampm server bearer token.
pub const TOKEN_KEY: &str = "music-server-token";

/// Persisted settings (the bearer token lives in the secret store, not here).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MusicConfig {
    #[serde(default = "default_server_url")]
    pub server_url: String,
}

fn default_server_url() -> String {
    "http://127.0.0.1:8787".to_string()
}

impl Default for MusicConfig {
    fn default() -> Self {
        MusicConfig { server_url: default_server_url() }
    }
}

pub fn load_config() -> MusicConfig {
    AppConfig::load().get::<MusicConfig>(EXT_ID).unwrap_or_default()
}

/// Build a client from settings + stored token, or `None` if not configured.
pub fn build_client() -> Option<MusicClient> {
    let cfg = load_config();
    let token = spotlight_config::load_secret(TOKEN_KEY)?;
    if cfg.server_url.trim().is_empty() || token.trim().is_empty() {
        return None;
    }
    Some(MusicClient::new(&cfg.server_url, &token))
}

/// The Home shortcut + full-screen panel.
pub fn panel_entry() -> PanelEntry {
    PanelEntry {
        id: EXT_ID.to_string(),
        title: "Music".to_string(),
        glyph: "🎵".to_string(),
        icon: None,
        make_view: Box::new(|cx, _seed| cx.new(MusicView::new).into()),
    }
}

/// The "Music" tab in Settings.
pub fn settings_tab() -> SettingsTabFactory {
    SettingsTabFactory {
        title: "Music".to_string(),
        make_view: Box::new(|cx| cx.new(MusicSettingsTab::new).into()),
    }
}
