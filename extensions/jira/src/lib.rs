//! Jira tasks extension: a full-screen panel listing issues from saved JQL
//! filters, with stale-while-revalidate caching and inline quick actions.
//!
//! It plugs into the shell as a [`PanelEntry`] (Home shortcut + screen) and a
//! [`SettingsTabFactory`] (a "Jira" tab). `app` registers both.

mod client;
mod document;
mod models;
mod search;
mod settings_tab;
mod view;

pub use search::JiraSearch;

use std::path::PathBuf;

use gpui::prelude::*;
use serde::{Deserialize, Serialize};
use spotlight_config::AppConfig;
use spotlight_ui::{PanelEntry, SettingsTabFactory};

use crate::client::JiraClient;
use crate::models::Issue;
use crate::settings_tab::JiraSettingsTab;
use crate::view::JiraView;

/// Extension id; also the key for this extension's settings blob in config.
pub const EXT_ID: &str = "jira";
/// Secret-store key for the API token.
pub const TOKEN_KEY: &str = "jira-token";
const MAX_ISSUES: u32 = 50;

/// The Jira logo, embedded and written to disk so it can be referenced by path
/// (the shortcut tile and issue recents render it as an image).
const ICON_PNG: &[u8] = include_bytes!("../assets/jira.png");

/// Path to the on-disk Jira icon, materializing it from the embedded bytes on
/// first call.
pub fn icon_path() -> String {
    let path = spotlight_config::config_dir().join("assets").join("jira.png");
    if !path.exists() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&path, ICON_PNG);
    }
    path.to_string_lossy().into_owned()
}

/// A named JQL query, shown as a filter chip when more than one is configured.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JqlFilter {
    pub name: String,
    pub jql: String,
}

/// Persisted Jira settings (the token lives in the secret store, not here).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JiraConfig {
    #[serde(default)]
    pub site: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub filters: Vec<JqlFilter>,
}

/// Load the persisted Jira settings (defaults if unset).
pub fn load_config() -> JiraConfig {
    AppConfig::load().get::<JiraConfig>(EXT_ID).unwrap_or_default()
}

/// Build a client from settings + stored token, or `None` if not fully configured.
pub fn build_client(cfg: &JiraConfig) -> Option<JiraClient> {
    let token = spotlight_config::load_secret(TOKEN_KEY)?;
    if cfg.site.trim().is_empty() || cfg.email.trim().is_empty() || token.trim().is_empty() {
        return None;
    }
    Some(JiraClient::new(&cfg.site, &cfg.email, &token))
}

/// Filesystem-safe slug for a filter name (used in cache filenames).
fn slug(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    if s.is_empty() {
        "default".to_string()
    } else {
        s
    }
}

fn cache_path(filter_name: &str) -> PathBuf {
    spotlight_config::cache_dir().join(format!("jira-{}.json", slug(filter_name)))
}

/// Load cached issues for a filter (empty on miss/corruption).
pub fn load_cache(filter_name: &str) -> Vec<Issue> {
    let mut issues: Vec<Issue> = std::fs::read_to_string(cache_path(filter_name))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    models::sort_by_status(&mut issues);
    issues
}

/// Persist the latest issues for a filter so the next open renders instantly.
pub fn save_cache(filter_name: &str, issues: &[Issue]) {
    let _ = std::fs::create_dir_all(spotlight_config::cache_dir());
    if let Ok(json) = serde_json::to_string(issues) {
        let _ = std::fs::write(cache_path(filter_name), json);
    }
}

/// The Home shortcut + full-screen panel for Jira.
pub fn panel_entry() -> PanelEntry {
    PanelEntry {
        id: EXT_ID.to_string(),
        title: "Jira".to_string(),
        glyph: "🪐".to_string(),
        icon: Some(icon_path()),
        make_view: Box::new(|cx, _seed| cx.new(JiraView::new).into()),
    }
}

/// The "Jira" tab in Settings.
pub fn settings_tab() -> SettingsTabFactory {
    SettingsTabFactory {
        title: "Jira".to_string(),
        make_view: Box::new(|cx| cx.new(JiraSettingsTab::new).into()),
    }
}
