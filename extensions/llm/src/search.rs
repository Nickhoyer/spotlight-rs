//! The dynamic "Ask AI" search entry. Unlike other providers it matches *any*
//! non-empty query, so it's always offered — but with a very low score, so it
//! sits below real results (apps, calculator, …) and only surfaces when nothing
//! better matches. Activating it opens the chat panel seeded with the query.

use async_trait::async_trait;

use spotlight_core::{Action, Extension, Query, ResultItem};

/// Base score, far below any fuzzy match or frecency boost (which caps at +500),
/// so the entry always ranks last.
const SCORE: i32 = -100_000;

pub struct LlmSearch;

#[async_trait]
impl Extension for LlmSearch {
    fn id(&self) -> &'static str {
        crate::EXT_ID
    }

    fn name(&self) -> &'static str {
        "AI Chat"
    }

    async fn query(&self, query: &Query) -> Vec<ResultItem> {
        if query.text.is_empty() {
            return Vec::new();
        }
        // The title shows the raw query; the subtitle hints at the model when
        // one is configured.
        let title = format!("\u{201c}{}\u{201d}", query.text);
        let subtitle = match crate::load_config().active_provider() {
            Some(p) if !p.model.is_empty() => format!("Ask AI \u{00b7} {}", p.model),
            _ => "Ask AI".to_string(),
        };

        vec![ResultItem {
            id: "llm-ask".to_string(),
            title,
            subtitle: Some(subtitle),
            // Icon comes from the built-in "llm" logo: `result_row` renders the
            // panel logo for `Action::OpenPanel`, matching the Home tile.
            icon: None,
            action: Action::OpenPanel(crate::EXT_ID.to_string()),
            score: SCORE,
            source: crate::EXT_ID.to_string(),
        }]
    }
}
