//! Entry point: build the extension registry and launch the UI.

use std::sync::Arc;

use ext_apps::AppsExtension;
use ext_calculator::CalculatorExtension;
use spotlight_core::Registry;

fn main() {
    let mut registry = Registry::new();
    registry.register(Arc::new(AppsExtension::new()));
    registry.register(Arc::new(CalculatorExtension));

    spotlight_ui::run(registry);
}
