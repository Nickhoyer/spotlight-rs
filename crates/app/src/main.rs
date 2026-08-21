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

    // The music extension owns the shared now-playing state read from Music.app
    // and starts its two background workers (hourly playlist cleanup, and
    // scrobbling what actually plays); the Home now-playing card reads the same
    // state.
    let music = ext_music::Music::new();

    let mut registry = Registry::new();
    registry.register(Arc::new(AppsExtension::new()));
    registry.register(Arc::new(CalculatorExtension));
    // Cached Jira issues are searchable from the main bar.
    registry.register(Arc::new(ext_jira::JiraSearch));
    // `radio <track>` typeahead against the ampm server.
    registry.register(Arc::new(ext_music::MusicSearch));
    // Small one-shot automations, each a single searchable entry.
    registry.register(Arc::new(ext_scripts::ScriptsExtension));
    // Clipboard history is searchable from the main bar (keyword: `clip`).
    registry.register(clipboard.extension());

    // GPUI-aware extensions (panels + settings tabs + menu-bar items) are wired
    // here.
    let ui = UiExtensions {
        panels: vec![
            ext_jira::panel_entry(),
            ext_gmail::panel_entry(),
            ext_music::panel_entry(),
            clipboard.panel_entry(),
        ],
        settings_tabs: vec![
            ext_jira::settings_tab(),
            ext_gmail::settings_tab(),
            ext_music::settings_tab(),
            clipboard.settings_tab(),
        ],
        menu_items: clipboard.menu_items(),
        // Now-playing card on Home, shown only while Music.app has a track.
        now_playing: Some(music.now_playing_source()),
    };

    spotlight_ui::run(registry, ui);
}
