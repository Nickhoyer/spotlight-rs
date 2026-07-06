//! Inline autocomplete backed by DuckDuckGo's search-suggestions API.
//!
//! Turns the partial query into a ghost-text continuation (accepted with Tab)
//! plus a few "Ask AI" rows seeded with each suggestion. These are real search
//! completions — fast, genuinely prefix-preserving, and needing no model — which
//! a small local LLM could never match for this task. Runs off the UI thread
//! (blocking HTTP), debounced by the shell; returns `None` when disabled or when
//! DuckDuckGo has nothing.
//!
//! Privacy: the partial query is sent to DuckDuckGo (like typing in any search
//! box). The `autocomplete` toggle in Settings → AI turns it off.

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use spotlight_core::{Action, ResultItem};
use spotlight_ui::{AutocompleteProvider, AutocompleteRequest, Suggestions};

use crate::{load_config, EXT_ID};

/// DuckDuckGo autocomplete endpoint. The default (no `type`) shape is a JSON
/// array of `{"phrase": "..."}`.
const ENDPOINT: &str = "https://duckduckgo.com/ac/";
/// How many "Ask AI" suggestion rows to surface at most.
const MAX_SUGGESTIONS: usize = 4;

/// One DuckDuckGo suggestion.
#[derive(Deserialize)]
struct Ac {
    phrase: String,
}

/// Build the [`AutocompleteProvider`] the shell calls.
pub fn provider() -> AutocompleteProvider {
    AutocompleteProvider { suggest: Arc::new(suggest) }
}

fn suggest(req: AutocompleteRequest) -> Option<Suggestions> {
    if !load_config().autocomplete {
        return None;
    }
    let suggestions = clean_suggestions(fetch(&req.query), &req.query);
    if suggestions.is_empty() {
        return None;
    }

    // The ghost previews the first genuine continuation inline; the rows are the
    // list. DuckDuckGo suggestions keep the query as a prefix, so `ghost_of`
    // almost always finds one.
    let ghost = suggestions
        .iter()
        .map(|s| ghost_of(&req.query, s))
        .find(|g| !g.is_empty())
        .unwrap_or_default();
    let entries: Vec<ResultItem> = suggestions
        .iter()
        .take(MAX_SUGGESTIONS)
        .enumerate()
        .map(|(i, s)| entry(i, s))
        .collect();

    Some(Suggestions { ghost, entries })
}

/// Fetch DuckDuckGo suggestions for `query`. Best-effort: returns an empty vec on
/// any network/parse failure (the shell then keeps its instant heuristic ghost).
fn fetch(query: &str) -> Vec<String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(3))
        .timeout(Duration::from_secs(4))
        .build();
    // `.query` percent-encodes the value; a browser-like UA avoids being turned
    // away.
    match agent.get(ENDPOINT).query("q", query).set("User-Agent", "Mozilla/5.0").call() {
        Ok(resp) => resp
            .into_json::<Vec<Ac>>()
            .map(|v| v.into_iter().map(|a| a.phrase).collect())
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Reduce a suggestion to the tail that follows the query, when it's a genuine
/// continuation (the suggestion kept the query as a prefix). Otherwise "" — the
/// shell then keeps its instant heuristic ghost rather than show text that
/// contradicts what the user typed.
fn ghost_of(query: &str, suggestion: &str) -> String {
    let s = suggestion.trim();
    if !query.is_empty()
        && s.to_lowercase().starts_with(&query.to_lowercase())
        && s.chars().count() > query.chars().count()
    {
        // Skip by char count so a case-insensitive match never slices a codepoint.
        s.chars().skip(query.chars().count()).collect()
    } else {
        String::new()
    }
}

/// Trim, drop empties, drop the raw query (the generic entry already offers it),
/// and collapse duplicates — including prefix-overlaps like "who created the
/// simpsons" vs "who created the simpsons show" — keeping the first, most
/// specific one seen. Order is otherwise preserved.
fn clean_suggestions(items: Vec<String>, query: &str) -> Vec<String> {
    let ql = query.trim().to_lowercase();
    let mut kept: Vec<String> = Vec::new();
    for item in items {
        let item = item.trim().to_string();
        if item.is_empty() {
            continue;
        }
        let lo = item.to_lowercase();
        if lo == ql {
            continue;
        }
        if kept.iter().any(|k| {
            let kl = k.to_lowercase();
            kl.starts_with(&lo) || lo.starts_with(&kl)
        }) {
            continue;
        }
        kept.push(item);
    }
    kept
}

fn entry(i: usize, phrase: &str) -> ResultItem {
    ResultItem {
        id: format!("llm-suggest-{i}"),
        title: phrase.to_string(),
        subtitle: Some("Ask AI".to_string()),
        icon: None,
        action: Action::OpenPanel { id: EXT_ID.to_string(), seed: Some(phrase.to_string()) },
        score: 0,
        source: EXT_ID.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ghost_only_keeps_genuine_continuations() {
        assert_eq!(ghost_of("how to cen", "how to center a div"), "ter a div");
        assert_eq!(ghost_of("convert pdf to", "convert pdf to word"), " word");
        assert_eq!(ghost_of("HOW", "how about now"), " about now");
        // Not a continuation (a rephrase) → dropped.
        assert_eq!(ghost_of("how do i reset my pass", "how to reset my password"), "");
        assert_eq!(ghost_of("abc", "abc"), "");
    }

    #[test]
    fn clean_suggestions_dedupes_and_drops_query_echo() {
        let items = vec![
            "how to center a div in css".to_string(),
            "how to center a div in css".to_string(), // exact dup
            "how to center".to_string(),              // prefix-overlap of the first
            "how to censor a discord server".to_string(),
            "".to_string(),
            "how to cen".to_string(), // equals the query
        ];
        let out = clean_suggestions(items, "how to cen");
        assert_eq!(
            out,
            vec![
                "how to center a div in css".to_string(),
                "how to censor a discord server".to_string(),
            ]
        );
    }
}
