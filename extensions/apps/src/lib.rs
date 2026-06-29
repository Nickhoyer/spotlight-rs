//! Application launcher extension: fuzzy-matches installed apps.

use async_trait::async_trait;

use spotlight_core::fuzzy::Fuzzy;
use spotlight_core::{Action, Extension, Icon, Query, ResultItem};
use spotlight_platform_macos::apps::{scan_apps, AppEntry};

pub struct AppsExtension {
    apps: Vec<AppEntry>,
}

impl AppsExtension {
    /// Scans installed applications once, up front.
    pub fn new() -> Self {
        Self { apps: scan_apps() }
    }
}

impl Default for AppsExtension {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Extension for AppsExtension {
    fn id(&self) -> &'static str {
        "apps"
    }

    fn name(&self) -> &'static str {
        "Applications"
    }

    async fn query(&self, query: &Query) -> Vec<ResultItem> {
        let mut fuzzy = Fuzzy::new(&query.text);
        self.apps
            .iter()
            .filter_map(|app| {
                let score = fuzzy.score(&app.name)?;
                Some(ResultItem {
                    id: format!("apps:{}", app.path.display()),
                    title: app.name.clone(),
                    subtitle: Some("Application".to_string()),
                    icon: Some(Icon::File(app.path.clone())),
                    action: Action::Open(app.path.clone()),
                    score: score as i32,
                    source: "apps".to_string(),
                })
            })
            .collect()
    }
}
