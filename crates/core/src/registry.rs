use std::sync::Arc;

use futures::future::join_all;

use crate::{Extension, Query, ResultItem};

/// Holds the registered extensions and dispatches queries to them.
#[derive(Default)]
pub struct Registry {
    extensions: Vec<Arc<dyn Extension>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, ext: Arc<dyn Extension>) {
        self.extensions.push(ext);
    }

    pub fn extensions(&self) -> &[Arc<dyn Extension>] {
        &self.extensions
    }

    /// Find the extension that produced items with the given `source` id.
    pub fn owner(&self, source: &str) -> Option<&Arc<dyn Extension>> {
        self.extensions.iter().find(|e| e.id() == source)
    }

    /// Run a query against the appropriate extensions and return ranked results.
    ///
    /// If the first word matches a registered extension's keyword, only that
    /// extension is queried with the remaining text. Otherwise every extension
    /// is queried concurrently and the results are merged and ranked.
    pub async fn query(&self, raw: &str) -> Vec<ResultItem> {
        let trimmed = raw.trim();

        if let Some((first, rest)) = trimmed.split_once(char::is_whitespace) {
            if let Some(ext) = self
                .extensions
                .iter()
                .find(|e| e.keyword().is_some_and(|k| k == first))
            {
                let q = Query::with_keyword(raw, first, rest.trim());
                let mut items = ext.query(&q).await;
                rank(&mut items);
                return items;
            }
        }

        let q = Query::new(raw);
        if q.is_empty() {
            return Vec::new();
        }

        let results = join_all(self.extensions.iter().map(|e| e.query(&q))).await;
        let mut items: Vec<ResultItem> = results.into_iter().flatten().collect();
        rank(&mut items);
        items
    }
}

fn rank(items: &mut [ResultItem]) {
    items.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.title.cmp(&b.title)));
}
