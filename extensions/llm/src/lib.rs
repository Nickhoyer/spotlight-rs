//! LLM chat extension: a dynamic "Ask AI" search entry that always appears
//! (ranked very low) and, on Enter, opens a full-screen chat panel seeded with
//! the typed text, which auto-sends and streams the reply.
//!
//! It plugs into the shell as a [`Extension`](spotlight_core::Extension) (the
//! search entry), a [`PanelEntry`] (the chat screen) and a
//! [`SettingsTabFactory`] (the "AI" tab). `app` registers all three.
//!
//! One client speaks two wire formats: an OpenAI-compatible `/chat/completions`
//! shape (OpenAI, Groq, OpenRouter, Ollama, LM Studio, …) and the Anthropic
//! Messages API. Both stream token-by-token over Server-Sent Events.

mod ambient;
mod autocomplete;
mod client;
mod markdown;
mod search;
mod settings_tab;
mod view;
mod websearch;

pub use search::LlmSearch;

use gpui::prelude::*;
use serde::{Deserialize, Serialize};
use spotlight_config::AppConfig;
use spotlight_ui::{PanelEntry, SettingsTabFactory};

use crate::settings_tab::LlmSettingsTab;
use crate::view::LlmView;

/// Extension id; also the key for this extension's settings blob in config and
/// the panel id used by [`Action::OpenPanel`](spotlight_core::Action::OpenPanel).
///
/// The AI-chat icon is a built-in logo rendered by the shell's logo system
/// (`logo::logo("llm")`), so it shares the gunmetal-tile look of Settings and
/// Clipboard and rounds identically in search results and Home tiles.
pub const EXT_ID: &str = "llm";

/// Wire format a provider speaks. The OpenAI-compatible shape covers most
/// providers (OpenAI, Groq, OpenRouter, and the local servers Ollama/LM Studio);
/// Anthropic uses its own Messages API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ApiFormat {
    #[default]
    OpenAi,
    Anthropic,
}

/// A configured chat provider. The API key (if any) lives in the secret store,
/// keyed by [`secret_key`], not in this blob.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Provider {
    /// Display name; also the stable key for the provider's secret and for the
    /// active-provider selection.
    pub name: String,
    /// Base URL, e.g. `https://api.openai.com/v1` (no trailing slash needed).
    pub base_url: String,
    /// Default model id used for new conversations.
    pub model: String,
    /// Known model ids, used by the in-chat model switcher. May be empty.
    #[serde(default)]
    pub models: Vec<String>,
    pub format: ApiFormat,
    /// Local servers need no API key; skips the auth header and the key field.
    #[serde(default)]
    pub local: bool,
}

/// Web-search settings for the agent's `web_search` tool. Backed by a Degoog
/// meta-search server; defaults to the project's hosted instance so it works out
/// of the box. There's no Settings UI for this yet — edit the config file (or the
/// secret store for the key) to change it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchConfig {
    /// When on, chats advertise a `web_search` tool the model can call for live
    /// or niche facts. On by default.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Degoog search endpoint (GET `?q=`). The hosted instance by default.
    #[serde(default = "default_search_endpoint")]
    pub endpoint: String,
    /// How many results to hand back to the model per search.
    #[serde(default = "default_max_results")]
    pub max_results: usize,
}

fn default_search_endpoint() -> String {
    "https://degoog.org/api/search".to_string()
}

fn default_max_results() -> usize {
    5
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            endpoint: default_search_endpoint(),
            max_results: default_max_results(),
        }
    }
}

/// Secret-store key for the search endpoint's Bearer token (optional — the
/// hosted Degoog instance is usable without one).
pub const SEARCH_SECRET_KEY: &str = "llm-search-key";

/// Persisted AI settings (provider API keys live in the secret store).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    #[serde(default)]
    pub providers: Vec<Provider>,
    /// Index into `providers` of the provider used for new chats.
    #[serde(default)]
    pub active: usize,
    /// Whether the search bar offers inline autocomplete (DuckDuckGo-backed) +
    /// "Ask AI" suggestion rows. On by default.
    #[serde(default = "default_true")]
    pub autocomplete: bool,
    /// Web-search tool settings for the chat agent.
    #[serde(default)]
    pub search: SearchConfig,
    /// Whether to give the agent ambient context — current date/time, timezone,
    /// locale, and coarse IP location — in its system prompt. On by default; the
    /// IP lookup (cached, once a day) is the only part that touches the network.
    #[serde(default = "default_true")]
    pub ambient_context: bool,
}

fn default_true() -> bool {
    true
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            providers: Vec::new(),
            active: 0,
            autocomplete: true,
            search: SearchConfig::default(),
            ambient_context: true,
        }
    }
}

impl LlmConfig {
    /// The active provider, clamped to a valid index.
    pub fn active_provider(&self) -> Option<&Provider> {
        self.providers.get(self.active).or_else(|| self.providers.first())
    }
}

/// Load the persisted AI settings (defaults if unset).
pub fn load_config() -> LlmConfig {
    AppConfig::load().get::<LlmConfig>(EXT_ID).unwrap_or_default()
}

/// Secret-store key for a provider's API key (filesystem/keychain-safe slug).
pub fn secret_key(provider_name: &str) -> String {
    let slug: String = provider_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    format!("llm-key-{}", if slug.is_empty() { "default" } else { &slug })
}

/// The built-in provider presets offered in Settings. Cloud providers need an
/// API key; local ones (Ollama, LM Studio) don't.
pub fn presets() -> Vec<Provider> {
    vec![
        Provider {
            name: "OpenAI".into(),
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-4o-mini".into(),
            models: vec!["gpt-4o-mini".into(), "gpt-4o".into()],
            format: ApiFormat::OpenAi,
            local: false,
        },
        Provider {
            name: "Anthropic".into(),
            base_url: "https://api.anthropic.com/v1".into(),
            model: "claude-opus-4-8".into(),
            models: vec!["claude-opus-4-8".into(), "claude-sonnet-5".into(), "claude-haiku-4-5".into()],
            format: ApiFormat::Anthropic,
            local: false,
        },
        Provider {
            name: "Groq".into(),
            base_url: "https://api.groq.com/openai/v1".into(),
            model: "llama-3.3-70b-versatile".into(),
            models: vec!["llama-3.3-70b-versatile".into()],
            format: ApiFormat::OpenAi,
            local: false,
        },
        Provider {
            name: "OpenRouter".into(),
            base_url: "https://openrouter.ai/api/v1".into(),
            model: "anthropic/claude-3.5-sonnet".into(),
            models: vec!["anthropic/claude-3.5-sonnet".into()],
            format: ApiFormat::OpenAi,
            local: false,
        },
        Provider {
            name: "Ollama".into(),
            base_url: "http://localhost:11434/v1".into(),
            model: "llama3.2".into(),
            models: vec![],
            format: ApiFormat::OpenAi,
            local: true,
        },
        Provider {
            name: "LM Studio".into(),
            base_url: "http://localhost:1234/v1".into(),
            model: String::new(),
            models: vec![],
            format: ApiFormat::OpenAi,
            local: true,
        },
    ]
}

/// The Home shortcut + full-screen chat panel.
pub fn panel_entry() -> PanelEntry {
    PanelEntry {
        id: EXT_ID.to_string(),
        title: "Ask AI".to_string(),
        glyph: "\u{2728}".to_string(), // ✨ fallback if the logo can't be rasterized
        icon: None, // the shell renders the built-in "llm" logo (logo_tile)
        make_view: Box::new(|cx, seed| {
            let seed = seed.map(str::to_owned);
            cx.new(|cx| LlmView::new(seed, cx)).into()
        }),
    }
}

/// The inline AI autocomplete source (ghost text + "Ask AI" suggestion rows).
pub fn autocomplete_provider() -> spotlight_ui::AutocompleteProvider {
    autocomplete::provider()
}

/// The "AI" tab in Settings.
pub fn settings_tab() -> SettingsTabFactory {
    SettingsTabFactory {
        title: "AI".to_string(),
        make_view: Box::new(|cx| cx.new(LlmSettingsTab::new).into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_key_slugs_provider_name() {
        assert_eq!(secret_key("OpenAI"), "llm-key-openai");
        assert_eq!(secret_key("LM Studio"), "llm-key-lm-studio");
        assert_eq!(secret_key(""), "llm-key-default");
    }

    #[test]
    fn active_provider_falls_back_to_first() {
        let cfg = LlmConfig {
            providers: vec![Provider { name: "A".into(), ..Default::default() }],
            active: 5, // out of range
            ..Default::default()
        };
        assert_eq!(cfg.active_provider().map(|p| p.name.as_str()), Some("A"));
    }
}
