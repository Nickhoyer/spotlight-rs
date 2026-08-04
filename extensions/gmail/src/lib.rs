//! Gmail extension: a panel showing unread inbox mail via Gmail's atom feed,
//! authenticated with a Google app password (no OAuth dance).
//!
//! It plugs into the shell as a [`PanelEntry`] (Home shortcut + screen) and a
//! [`SettingsTabFactory`] (a "Gmail" tab). `app` registers both.

mod client;
mod models;
mod settings_tab;
mod view;

use gpui::prelude::*;
use serde::{Deserialize, Serialize};
use spotlight_config::AppConfig;
use spotlight_ui::{PanelEntry, SettingsTabFactory};

use crate::client::GmailClient;
use crate::models::Inbox;
use crate::settings_tab::GmailSettingsTab;
use crate::view::GmailView;

/// Extension id; also the key for this extension's settings blob in config.
pub const EXT_ID: &str = "gmail";
/// Secret-store key for the app password.
pub const PASSWORD_KEY: &str = "gmail-app-password";

/// Persisted Gmail settings (the app password lives in the secret store).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GmailConfig {
    #[serde(default)]
    pub email: String,
}

/// Load the persisted Gmail settings (defaults if unset).
pub fn load_config() -> GmailConfig {
    AppConfig::load().get::<GmailConfig>(EXT_ID).unwrap_or_default()
}

/// Build a client from settings + stored app password, or `None` if not
/// configured yet.
pub fn build_client(cfg: &GmailConfig) -> Option<GmailClient> {
    let password = spotlight_config::load_secret(PASSWORD_KEY)?;
    if cfg.email.trim().is_empty() || password.trim().is_empty() {
        return None;
    }
    Some(GmailClient::new(&cfg.email, &password))
}

fn cache_path() -> std::path::PathBuf {
    spotlight_config::cache_dir().join("gmail-inbox.json")
}

/// Load the cached inbox (empty on miss/corruption).
pub fn load_cache() -> Inbox {
    std::fs::read_to_string(cache_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Persist the latest inbox so the next open renders instantly.
pub fn save_cache(inbox: &Inbox) {
    let _ = std::fs::create_dir_all(spotlight_config::cache_dir());
    if let Ok(json) = serde_json::to_string(inbox) {
        let _ = std::fs::write(cache_path(), json);
    }
}

/// The Home shortcut + full-screen panel for Gmail.
pub fn panel_entry() -> PanelEntry {
    PanelEntry {
        id: EXT_ID.to_string(),
        title: "Gmail".to_string(),
        glyph: "✉️".to_string(),
        icon: None,
        make_view: Box::new(|cx, _seed| cx.new(GmailView::new).into()),
    }
}

/// The "Gmail" tab in Settings.
pub fn settings_tab() -> SettingsTabFactory {
    SettingsTabFactory {
        title: "Gmail".to_string(),
        make_view: Box::new(|cx| cx.new(GmailSettingsTab::new).into()),
    }
}
