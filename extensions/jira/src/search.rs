//! Makes cached Jira issues findable from the main search bar. Implements the
//! framework-agnostic [`Extension`] trait (unlike the GPUI panel), reading the
//! same on-disk caches the panel writes, so issues are searchable offline and
//! without re-fetching.

use std::collections::HashSet;
use std::path::PathBuf;

use async_trait::async_trait;

use spotlight_core::fuzzy::Fuzzy;
use spotlight_core::{Action, Extension, Icon, Query, ResultItem};

use crate::client;

pub struct JiraSearch;

#[async_trait]
impl Extension for JiraSearch {
    fn id(&self) -> &'static str {
        crate::EXT_ID
    }

    fn name(&self) -> &'static str {
        "Jira"
    }

    async fn query(&self, query: &Query) -> Vec<ResultItem> {
        let cfg = crate::load_config();
        if cfg.site.trim().is_empty() || cfg.filters.is_empty() {
            return Vec::new();
        }

        // Aggregate cached issues across all filters, de-duplicated by key.
        let mut seen = HashSet::new();
        let mut issues = Vec::new();
        for filter in &cfg.filters {
            for issue in crate::load_cache(&filter.name) {
                if seen.insert(issue.key.clone()) {
                    issues.push(issue);
                }
            }
        }

        let icon = Icon::Image(PathBuf::from(crate::icon_path()));
        let mut fuzzy = Fuzzy::new(&query.text);
        issues
            .iter()
            .filter_map(|issue| {
                // Match against the key and the summary together so either works.
                let score = fuzzy.score(&format!("{} {}", issue.key, issue.summary))?;
                Some(ResultItem {
                    id: format!("jira:{}", issue.key),
                    title: issue.summary.clone(),
                    subtitle: Some(format!("{} · {}", issue.key, issue.status)),
                    icon: Some(icon.clone()),
                    action: Action::OpenUrl(client::browse_url(&cfg.site, &issue.key)),
                    score: score as i32,
                    source: crate::EXT_ID.to_string(),
                })
            })
            .collect()
    }
}
