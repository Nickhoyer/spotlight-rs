//! Entry point: build the extension registry and launch the UI.

use std::sync::Arc;

use ext_apps::AppsExtension;
use ext_calculator::CalculatorExtension;
use spotlight_core::Registry;
use spotlight_ui::UiExtensions;

fn main() {
    // Packaging hook: `spotlight --emit-iconset <dir>` renders the app-logo
    // iconset PNGs into <dir> and exits (scripts/bundle.sh feeds them to
    // `iconutil`). Handled before any GUI/AppKit setup.
    let args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|a| a == "--emit-iconset") {
        let dir = args.get(pos + 1).map(String::as_str).unwrap_or(".");
        match spotlight_ui::emit_iconset(std::path::Path::new(dir)) {
            Ok(()) => return,
            Err(e) => {
                eprintln!("spotlight: --emit-iconset failed: {e}");
                std::process::exit(1);
            }
        }
    }

    // The clipboard extension owns a shared, encrypted history store and starts
    // its background monitor here; its search/panel/settings all read from it.
    let clipboard = ext_clipboard::Clipboard::new();

    let mut registry = Registry::new();
    registry.register(Arc::new(AppsExtension::new()));
    registry.register(Arc::new(CalculatorExtension));
    // Cached Jira issues are searchable from the main bar.
    registry.register(Arc::new(ext_jira::JiraSearch));
    // Clipboard history is searchable from the main bar (keyword: `clip`).
    registry.register(clipboard.extension());
    // A dynamic "Ask AI" entry that always appears (ranked very low) and opens
    // the chat panel seeded with the typed text.
    registry.register(Arc::new(ext_llm::LlmSearch));

    // GPUI-aware extensions (panels + settings tabs + menu-bar items) are wired
    // here.
    let ui = UiExtensions {
        panels: vec![
            ext_jira::panel_entry(),
            ext_gmail::panel_entry(),
            clipboard.panel_entry(),
            ext_llm::panel_entry(),
        ],
        settings_tabs: vec![
            ext_jira::settings_tab(),
            ext_gmail::settings_tab(),
            clipboard.settings_tab(),
            ext_llm::settings_tab(),
        ],
        menu_items: clipboard.menu_items(),
        // Inline AI autocomplete (ghost text) + "Ask AI" suggestion rows.
        autocomplete: Some(ext_llm::autocomplete_provider()),
    };

    spotlight_ui::run(registry, ui);
}
