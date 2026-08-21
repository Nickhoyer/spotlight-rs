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

    /// Resolve a leading keyword to the extension it routes to, together with
    /// the [`Query`] to run against it. `None` when the query isn't routed.
    ///
    /// Exposed so a shell can treat routed queries differently from the
    /// broadcast path — they are explicitly invoked and may do network I/O, so
    /// the UI runs them off the main thread.
    pub fn route(&self, raw: &str) -> Option<(Arc<dyn Extension>, Query)> {
        let (first, rest) = raw.trim().split_once(char::is_whitespace)?;
        let ext = self
            .extensions
            .iter()
            .find(|e| e.keyword().is_some_and(|k| k == first))?;
        Some((ext.clone(), Query::with_keyword(raw, first, rest.trim())))
    }

    /// Run a query against the appropriate extensions and return ranked results.
    ///
    /// If the first word matches a registered extension's keyword, only that
    /// extension is queried with the remaining text. Otherwise every extension
    /// *without* a keyword is queried concurrently and the results are merged
    /// and ranked.
    pub async fn query(&self, raw: &str) -> Vec<ResultItem> {
        if let Some((ext, q)) = self.route(raw) {
            let mut items = ext.query(&q).await;
            rank(&mut items);
            return items;
        }

        let q = Query::new(raw);
        if q.is_empty() {
            return Vec::new();
        }

        // A keyword is opt-in, both ways: an extension that declares one answers
        // *only* under it. Otherwise its results compete with — and, scoring
        // themselves, tend to bury — ordinary matches on every query, and a
        // network-backed one would do I/O on every keystroke.
        let results = join_all(
            self.extensions
                .iter()
                .filter(|e| e.keyword().is_none())
                .map(|e| e.query(&q)),
        )
        .await;
        let mut items: Vec<ResultItem> = results.into_iter().flatten().collect();
        rank(&mut items);
        items
    }
}

fn rank(items: &mut [ResultItem]) {
    items.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.title.cmp(&b.title)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Action, Icon};
    use async_trait::async_trait;

    struct Ext {
        id: &'static str,
        keyword: Option<&'static str>,
    }

    #[async_trait]
    impl Extension for Ext {
        fn id(&self) -> &'static str {
            self.id
        }
        fn name(&self) -> &'static str {
            self.id
        }
        fn keyword(&self) -> Option<&'static str> {
            self.keyword
        }
        async fn query(&self, query: &Query) -> Vec<ResultItem> {
            vec![ResultItem {
                id: format!("{}:{}", self.id, query.text),
                title: query.text.clone(),
                subtitle: None,
                icon: None::<Icon>,
                action: Action::None,
                score: 0,
                source: self.id.to_string(),
            }]
        }
    }

    fn registry() -> Registry {
        let mut r = Registry::new();
        r.register(Arc::new(Ext { id: "plain", keyword: None }));
        r.register(Arc::new(Ext { id: "kw", keyword: Some("radio") }));
        r
    }

    fn sources(items: &[ResultItem]) -> Vec<&str> {
        items.iter().map(|i| i.source.as_str()).collect()
    }

    #[test]
    fn keyword_extension_sits_out_the_broadcast_path() {
        let items = futures::executor::block_on(registry().query("radiohead live"));
        assert_eq!(sources(&items), ["plain"]);
    }

    #[test]
    fn keyword_routes_to_its_extension_alone() {
        let items = futures::executor::block_on(registry().query("radio daylight"));
        assert_eq!(sources(&items), ["kw"]);
        assert_eq!(items[0].title, "daylight");
    }

    #[test]
    fn route_reports_the_routed_extension() {
        let r = registry();
        let (ext, q) = r.route("radio  daylight ").expect("routed");
        assert_eq!(ext.id(), "kw");
        assert_eq!(q.keyword.as_deref(), Some("radio"));
        assert_eq!(q.text, "daylight");
        assert!(r.route("radio").is_none());
        assert!(r.route("something else").is_none());
    }
}
