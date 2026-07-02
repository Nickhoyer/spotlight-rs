//! Clipboard-history extension: a background monitor records everything you copy
//! into a locally-encrypted store, surfaced as a searchable list from the main
//! bar and a full-screen panel with rich previews (text, links, colors, images)
//! and pinning.
//!
//! It plugs into the shell three ways, all sharing one in-memory [`ClipStore`]:
//! - an [`Extension`] so history is searchable from the main bar,
//! - a [`PanelEntry`] (Home shortcut + panel view),
//! - a [`SettingsTabFactory`] (a "Clipboard" settings tab).
//!
//! `app` builds a single [`Clipboard`] and wires all three from it.

mod crypto;
mod search;
mod settings_tab;
mod store;
mod view;

use std::sync::Arc;

use gpui::AppContext as _;
use serde::{Deserialize, Serialize};
use spotlight_config::AppConfig;
use spotlight_core::Extension;
use spotlight_ui::{MenuItem, PanelEntry, SettingsTabFactory};

use crate::search::ClipboardSearch;
use crate::settings_tab::ClipboardSettingsTab;
use crate::store::{spawn_monitor, ClipStore};
use crate::view::ClipboardView;

/// Extension id; also the settings-blob key and search-result source.
pub const EXT_ID: &str = "clipboard";
/// Home shortcut / result glyph.
pub const GLYPH: &str = "📋";

/// Persisted clipboard settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipboardConfig {
    /// Whether the background monitor records copies.
    pub enabled: bool,
    /// Whether images are captured (they can be large).
    pub capture_images: bool,
    /// Cap on un-pinned entries kept in history.
    pub max_items: usize,
}

impl Default for ClipboardConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            capture_images: true,
            max_items: 200,
        }
    }
}

/// Load the persisted clipboard settings (defaults if unset).
pub fn load_config() -> ClipboardConfig {
    AppConfig::load().get::<ClipboardConfig>(EXT_ID).unwrap_or_default()
}

/// The clipboard extension handle. Owns the shared store and starts the monitor;
/// hand out the [`Extension`], panel, and settings tab from it.
pub struct Clipboard {
    store: Arc<ClipStore>,
}

impl Clipboard {
    /// Load history from disk, apply settings, and start the background monitor.
    pub fn new() -> Self {
        let store = ClipStore::load(&load_config());
        // Debug aid for headless screenshots (mirrors the SPOTLIGHT_* capture
        // knobs): pre-populate the history with demo content.
        if std::env::var_os("SPOTLIGHT_CLIPBOARD_SEED").is_some() {
            store.seed_demo();
        }
        spawn_monitor(store.clone());
        Self { store }
    }

    /// The searchable [`Extension`] for the main bar (surfaces the panel opener,
    /// not the individual history entries).
    pub fn extension(&self) -> Arc<dyn Extension> {
        Arc::new(ClipboardSearch)
    }

    /// The Home shortcut + full-screen panel.
    pub fn panel_entry(&self) -> PanelEntry {
        let store = self.store.clone();
        PanelEntry {
            id: EXT_ID.to_string(),
            title: "Clipboard History".to_string(),
            glyph: GLYPH.to_string(),
            icon: None,
            make_view: Box::new(move |cx, _seed| {
                let store = store.clone();
                cx.new(|cx| ClipboardView::new(store, cx)).into()
            }),
        }
    }

    /// Menu-bar items contributed to the launcher's status menu. Demonstrates
    /// the extension menu API; the closure captures the shared store.
    pub fn menu_items(&self) -> Vec<MenuItem> {
        let store = self.store.clone();
        vec![MenuItem {
            title: "Clear Clipboard History".to_string(),
            action: Box::new(move |_cx| store.clear()),
        }]
    }

    /// The "Clipboard" settings tab.
    pub fn settings_tab(&self) -> SettingsTabFactory {
        let store = self.store.clone();
        SettingsTabFactory {
            title: "Clipboard".to_string(),
            make_view: Box::new(move |cx| {
                let store = store.clone();
                cx.new(|cx| ClipboardSettingsTab::new(store, cx)).into()
            }),
        }
    }
}

impl Default for Clipboard {
    fn default() -> Self {
        Self::new()
    }
}
