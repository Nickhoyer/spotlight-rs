//! Gmail extension: a panel showing unread inbox mail over IMAP, authenticated
//! with a Google app password (no OAuth dance), with in-app reading — HTML
//! bodies render via Blitz, text/plain as fallback.
//!
//! It plugs into the shell as a [`PanelEntry`] (Home shortcut + screen) and a
//! [`SettingsTabFactory`] (a "Gmail" tab). `app` registers both.

mod client;
mod htmlview;
mod models;
mod settings_tab;
mod view;

use std::sync::{Arc, Mutex, OnceLock};

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

/// The Gmail logo (Wikimedia's 2020 icon, rasterized on a square canvas),
/// embedded and written to disk so the shortcut tile can reference it by path.
const ICON_PNG: &[u8] = include_bytes!("../assets/gmail.png");

/// Path to the on-disk Gmail icon, materializing it from the embedded bytes.
/// Rewritten whenever the on-disk copy differs (not just when missing), so
/// icon updates actually reach configs that materialized an older version.
pub fn icon_path() -> String {
    let path = spotlight_config::config_dir().join("assets").join("gmail.png");
    let stale = std::fs::read(&path).map(|b| b != ICON_PNG).unwrap_or(true);
    if stale {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&path, ICON_PNG);
    }
    path.to_string_lossy().into_owned()
}

/// Persisted Gmail settings (the app password lives in the secret store).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GmailConfig {
    #[serde(default)]
    pub email: String,
    /// Fetch every opened message's remote images without asking. Off by
    /// default: loading images is what fires tracking pixels.
    #[serde(default)]
    pub auto_load_images: bool,
}

/// Load the persisted Gmail settings (defaults if unset).
pub fn load_config() -> GmailConfig {
    AppConfig::load().get::<GmailConfig>(EXT_ID).unwrap_or_default()
}

/// Build (or reuse) a client from settings + stored app password, or `None` if
/// not configured yet. The client is process-global so its IMAP session
/// survives panel re-opens; changing the address or password swaps it out.
pub fn build_client(cfg: &GmailConfig) -> Option<Arc<GmailClient>> {
    static SHARED: OnceLock<Mutex<Option<(String, Arc<GmailClient>)>>> = OnceLock::new();

    let password = spotlight_config::load_secret(PASSWORD_KEY)?;
    if cfg.email.trim().is_empty() || password.trim().is_empty() {
        return None;
    }
    let key = format!("{}\u{0}{}", cfg.email.trim(), password);
    let mut guard = SHARED.get_or_init(|| Mutex::new(None)).lock().ok()?;
    if let Some((cached_key, client)) = guard.as_ref() {
        if *cached_key == key {
            return Some(client.clone());
        }
    }
    let client = Arc::new(GmailClient::new(&cfg.email, &password));
    *guard = Some((key, client.clone()));
    Some(client)
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
        icon: Some(icon_path()),
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
