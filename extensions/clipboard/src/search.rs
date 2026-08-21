//! Makes "Clipboard History" itself findable from the main search bar. The
//! copied *entries* deliberately don't appear in search (that would flood
//! results and leak history); instead a single result opens the panel, where
//! items are browsed and pasted.

use async_trait::async_trait;

use spotlight_core::fuzzy::Fuzzy;
use spotlight_core::{Action, Extension, Icon, Query, ResultItem};

pub struct ClipboardSearch;

#[async_trait]
impl Extension for ClipboardSearch {
    fn id(&self) -> &'static str {
        crate::EXT_ID
    }

    fn name(&self) -> &'static str {
        "Clipboard History"
    }

    async fn query(&self, query: &Query) -> Vec<ResultItem> {
        // A single result matching the panel's name; opening it navigates in.
        let Some(score) = Fuzzy::new(&query.text).score("Clipboard History") else {
            return Vec::new();
        };
        vec![ResultItem {
            id: format!("panel:{}", crate::EXT_ID),
            title: "Clipboard History".to_string(),
            subtitle: Some("Browse and paste copied items".to_string()),
            icon: Some(Icon::Glyph(crate::GLYPH.to_string())),
            action: Action::OpenPanel { id: crate::EXT_ID.to_string() },
            score: score as i32,
            source: crate::EXT_ID.to_string(),
        }]
    }
}
