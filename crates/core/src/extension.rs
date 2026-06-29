use async_trait::async_trait;

use crate::{Query, ResultItem};

/// A source of results. Extensions are compile-time crates registered with the
/// [`Registry`](crate::Registry); each one turns a [`Query`] into [`ResultItem`]s.
///
/// `query` is async so extensions that do I/O (e.g. hitting the Jira API) don't
/// block the UI. Purely in-memory extensions can simply return synchronously.
#[async_trait]
pub trait Extension: Send + Sync {
    /// Stable, unique id. Also used as [`ResultItem::source`].
    fn id(&self) -> &'static str;

    /// Human-readable name.
    fn name(&self) -> &'static str;

    /// Optional activation keyword. When the query begins with `"<keyword> "`,
    /// only this extension is queried, with the remaining text.
    fn keyword(&self) -> Option<&'static str> {
        None
    }

    /// Produce results for `query`.
    async fn query(&self, query: &Query) -> Vec<ResultItem>;

    /// Handle an [`Action::Custom`](crate::Action::Custom) emitted by this
    /// extension's items.
    fn run(&self, _action_id: &str) -> anyhow::Result<()> {
        Ok(())
    }
}
