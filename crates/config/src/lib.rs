//! Persisted configuration and secret storage for the launcher.
//!
//! This crate is framework-agnostic (no GPUI). It owns the on-disk JSON config
//! at `~/Library/Application Support/spotlight-rs/config.json`, a cache
//! directory for extensions, and a small secret store.
//!
//! Each extension keeps its own typed settings blob under [`AppConfig::get`] /
//! [`AppConfig::set`] keyed by the extension id, so adding an extension never
//! touches this crate's types.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde::{de::DeserializeOwned, Deserialize, Serialize};

/// `~/Library/Application Support/spotlight-rs`, or the path in
/// `SPOTLIGHT_CONFIG_DIR` when set. The override exists so screenshots/tests can
/// point at a throwaway directory and never read or clobber the real config.
/// Falls back to the current dir only if `$HOME` is somehow unset.
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("SPOTLIGHT_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("Library")
        .join("Application Support")
        .join("spotlight-rs")
}

/// The main config file path.
pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

/// A per-extension cache directory (created on demand by callers).
pub fn cache_dir() -> PathBuf {
    config_dir().join("cache")
}

/// A previously-opened item, shown on the Home screen's "Recently opened" list.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Recent {
    /// Stable id used for de-duplication (e.g. the URL or item key).
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub subtitle: Option<String>,
    /// If set, re-opening the recent opens this URL in the browser.
    #[serde(default)]
    pub url: Option<String>,
    /// If set, re-opening launches this filesystem path (e.g. an `.app`); also
    /// used to render the item's system icon.
    #[serde(default)]
    pub path: Option<String>,
    /// If set, a path to an image file rendered directly as the icon (takes
    /// precedence over `path`/`glyph`). Used for custom extension logos.
    #[serde(default)]
    pub icon: Option<String>,
    /// Optional leading glyph/emoji (used when there's no image/`path` icon).
    #[serde(default)]
    pub glyph: Option<String>,
}

/// Cap on the raw usage-history log (kept with duplicates for frecency).
pub const HISTORY_CAP: usize = 250;

/// The persisted application configuration.
#[derive(Default, Debug, Serialize, Deserialize)]
pub struct AppConfig {
    /// Raw log of activated items, most-recent first, **including duplicates**,
    /// capped at [`HISTORY_CAP`]. Drives both the de-duplicated Home recents and
    /// the search relevance boost. (Reads the legacy `recents` key for migration.)
    #[serde(default, alias = "recents")]
    pub history: Vec<Recent>,
    /// Per-extension settings, keyed by extension id. Each value is that
    /// extension's own serialized config struct.
    #[serde(default)]
    pub extensions: serde_json::Map<String, serde_json::Value>,
    /// Plaintext secrets — only populated in debug builds (see [`save_secret`]).
    /// In release builds secrets live in the macOS Keychain and this stays empty.
    #[serde(default)]
    secrets: BTreeMap<String, String>,
}

impl AppConfig {
    /// Load from disk, or return defaults if missing/corrupt.
    pub fn load() -> Self {
        std::fs::read_to_string(config_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Write to disk, creating the config directory if needed.
    pub fn save(&self) -> anyhow::Result<()> {
        std::fs::create_dir_all(config_dir())?;
        std::fs::write(config_path(), serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Deserialize the settings blob for extension `id`, if present and valid.
    pub fn get<T: DeserializeOwned>(&self, id: &str) -> Option<T> {
        self.extensions
            .get(id)
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
    }

    /// Replace the settings blob for extension `id`.
    pub fn set<T: Serialize>(&mut self, id: &str, value: &T) -> anyhow::Result<()> {
        self.extensions
            .insert(id.to_string(), serde_json::to_value(value)?);
        Ok(())
    }

    /// Append an activation to the front of the history log, keeping duplicates,
    /// capped at [`HISTORY_CAP`].
    pub fn record_use(&mut self, entry: Recent) {
        self.history.insert(0, entry);
        self.history.truncate(HISTORY_CAP);
    }

    /// De-duplicated recents (most-recent first), for the Home screen.
    pub fn recents(&self) -> Vec<Recent> {
        let mut seen = HashSet::new();
        self.history
            .iter()
            .filter(|r| seen.insert(r.id.clone()))
            .cloned()
            .collect()
    }

    /// A search-relevance boost for `id`: a simple frecency over the history log
    /// (more frequent and more recent uses score higher), bounded so it nudges
    /// ranking without overriding strong fuzzy matches.
    pub fn usage_boost(&self, id: &str) -> i32 {
        let mut boost = 0i32;
        for (i, entry) in self.history.iter().enumerate() {
            if entry.id == id {
                boost += match i {
                    0..=4 => 40,
                    5..=19 => 25,
                    20..=59 => 12,
                    60..=149 => 5,
                    _ => 2,
                };
            }
        }
        boost.min(500)
    }
}

/// Process-lifetime cache of secrets, so a secret is read from its backing
/// store at most once per launch (which caps release-build Keychain prompts at
/// one per launch).
fn secret_cache() -> &'static Mutex<HashMap<String, Option<String>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Read a secret by key. Cached after the first read.
///
/// Debug builds read from the plaintext `secrets` map in config.json; release
/// builds read from the macOS Keychain.
pub fn load_secret(key: &str) -> Option<String> {
    if let Some(hit) = secret_cache().lock().unwrap().get(key) {
        return hit.clone();
    }
    let value = read_secret_uncached(key);
    secret_cache()
        .lock()
        .unwrap()
        .insert(key.to_string(), value.clone());
    value
}

/// Store a secret by key and refresh the in-process cache.
pub fn save_secret(key: &str, value: &str) -> anyhow::Result<()> {
    write_secret(key, value)?;
    secret_cache()
        .lock()
        .unwrap()
        .insert(key.to_string(), Some(value.to_string()));
    Ok(())
}

#[cfg(debug_assertions)]
fn read_secret_uncached(key: &str) -> Option<String> {
    AppConfig::load().secrets.get(key).cloned()
}

#[cfg(debug_assertions)]
fn write_secret(key: &str, value: &str) -> anyhow::Result<()> {
    let mut cfg = AppConfig::load();
    cfg.secrets.insert(key.to_string(), value.to_string());
    cfg.save()
}

#[cfg(not(debug_assertions))]
fn read_secret_uncached(key: &str) -> Option<String> {
    security_framework::passwords::get_generic_password("spotlight-rs", key)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
}

#[cfg(not(debug_assertions))]
fn write_secret(key: &str, value: &str) -> anyhow::Result<()> {
    security_framework::passwords::set_generic_password("spotlight-rs", key, value.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Demo {
        site: String,
        n: u32,
    }

    fn entry(id: &str) -> Recent {
        Recent {
            id: id.into(),
            title: id.into(),
            subtitle: None,
            url: None,
            path: None,
            icon: None,
            glyph: None,
        }
    }

    #[test]
    fn config_round_trips_extension_blobs_and_history() {
        let mut cfg = AppConfig::default();
        let demo = Demo {
            site: "acme".to_string(),
            n: 7,
        };
        cfg.set("jira", &demo).unwrap();
        cfg.record_use(entry("x"));

        let json = serde_json::to_string(&cfg).unwrap();
        let back: AppConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.get::<Demo>("jira"), Some(demo));
        assert_eq!(back.history.len(), 1);
        assert_eq!(back.history[0].id, "x");
    }

    #[test]
    fn legacy_recents_key_migrates_into_history() {
        let json = r#"{ "recents": [ { "id": "old", "title": "Old" } ] }"#;
        let cfg: AppConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.history.len(), 1);
        assert_eq!(cfg.history[0].id, "old");
    }

    #[test]
    fn history_keeps_duplicates_recents_dedups() {
        let mut cfg = AppConfig::default();
        for i in 0..5 {
            cfg.record_use(entry(&format!("id{}", i % 2)));
        }
        // History keeps every use (with duplicates), most-recent first.
        assert_eq!(cfg.history.len(), 5);
        assert_eq!(cfg.history[0].id, "id0"); // i=4 → "id0"

        // Recents de-duplicate, most-recent first.
        let recents = cfg.recents();
        assert_eq!(recents.len(), 2);
        assert_eq!(recents[0].id, "id0");
        assert_eq!(recents[1].id, "id1");
    }

    #[test]
    fn usage_boost_rewards_frequency_and_recency() {
        let mut cfg = AppConfig::default();
        cfg.record_use(entry("rare"));
        for _ in 0..3 {
            cfg.record_use(entry("common"));
        }
        assert!(cfg.usage_boost("common") > cfg.usage_boost("rare"));
        assert_eq!(cfg.usage_boost("never"), 0);
    }
}
