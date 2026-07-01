//! Entry point: build the extension registry and launch the UI.

use std::sync::Arc;

use ext_apps::AppsExtension;
use ext_calculator::CalculatorExtension;
use spotlight_core::Registry;
use spotlight_ui::UiExtensions;

fn main() {
    let mut registry = Registry::new();
    registry.register(Arc::new(AppsExtension::new()));
    registry.register(Arc::new(CalculatorExtension));
    // Cached Jira issues are searchable from the main bar.
    registry.register(Arc::new(ext_jira::JiraSearch));

    // GPUI-aware extensions (panels + settings tabs) are wired here.
    let ui = UiExtensions {
        panels: vec![ext_jira::panel_entry()],
        settings_tabs: vec![ext_jira::settings_tab()],
    };

    spotlight_ui::run(registry, ui);
}
