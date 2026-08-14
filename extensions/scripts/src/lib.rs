//! Scripts extension: a grouping for small one-shot automations that don't
//! warrant a full extension of their own. Each script is a single searchable
//! result; adding one means writing a module with a `run()` and listing it in
//! [`SCRIPTS`].

mod launch_sims;

use async_trait::async_trait;

use spotlight_core::fuzzy::Fuzzy;
use spotlight_core::{Action, Extension, Icon, Query, ResultItem};

/// One runnable script.
struct Script {
    /// Stable id, doubles as the [`Action::Custom`] id.
    id: &'static str,
    name: &'static str,
    description: &'static str,
    glyph: &'static str,
    /// Runs on a background thread, so it may block, sleep, and poll freely.
    run: fn() -> anyhow::Result<()>,
}

const SCRIPTS: &[Script] = &[Script {
    id: "launch-sims",
    name: "Launch Sims",
    description: "Open the Sims 4 Modding Tool, then the EA app once it's up",
    glyph: "🎮",
    run: launch_sims::run,
}];

pub struct ScriptsExtension;

#[async_trait]
impl Extension for ScriptsExtension {
    fn id(&self) -> &'static str {
        "scripts"
    }

    fn name(&self) -> &'static str {
        "Scripts"
    }

    async fn query(&self, query: &Query) -> Vec<ResultItem> {
        let mut fuzzy = Fuzzy::new(&query.text);
        SCRIPTS
            .iter()
            .filter_map(|script| {
                let score = fuzzy.score(script.name)?;
                Some(ResultItem {
                    id: format!("scripts:{}", script.id),
                    title: script.name.to_string(),
                    subtitle: Some(script.description.to_string()),
                    icon: Some(Icon::Glyph(script.glyph.to_string())),
                    action: Action::Custom(script.id.to_string()),
                    score: score as i32,
                    source: "scripts".to_string(),
                })
            })
            .collect()
    }

    fn run(&self, action_id: &str) -> anyhow::Result<()> {
        let Some(script) = SCRIPTS.iter().find(|s| s.id == action_id) else {
            anyhow::bail!("unknown script: {action_id}");
        };
        // Called on the UI thread; scripts wait on external state, so hand off.
        let (name, run) = (script.name, script.run);
        std::thread::spawn(move || {
            if let Err(e) = run() {
                eprintln!("scripts: {name} failed: {e:#}");
            }
        });
        Ok(())
    }
}
