//! Inline calculator extension: evaluates math expressions as you type.

use async_trait::async_trait;

use spotlight_core::{Action, Extension, Icon, Query, ResultItem};

pub struct CalculatorExtension;

#[async_trait]
impl Extension for CalculatorExtension {
    fn id(&self) -> &'static str {
        "calc"
    }

    fn name(&self) -> &'static str {
        "Calculator"
    }

    async fn query(&self, query: &Query) -> Vec<ResultItem> {
        let expr = query.text.trim();
        if !looks_like_math(expr) {
            return Vec::new();
        }
        let Ok(value) = meval::eval_str(expr) else {
            return Vec::new();
        };
        let formatted = format_number(value);
        vec![ResultItem {
            id: "calc:result".to_string(),
            title: formatted.clone(),
            subtitle: Some(format!("{} = {}", expr, formatted)),
            icon: Some(Icon::Glyph("🧮".to_string())),
            // Math is unambiguous, so rank it above fuzzy app matches.
            action: Action::Copy(formatted),
            score: 1_000_000,
            source: "calc".to_string(),
        }]
    }
}

/// Heuristic: only treat input as math when it has at least one digit and one
/// arithmetic operator, so plain words don't trigger the calculator.
fn looks_like_math(s: &str) -> bool {
    s.chars().any(|c| c.is_ascii_digit()) && s.chars().any(|c| "+-*/^%".contains(c))
}

fn format_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        // Trim trailing zeros from the default float formatting.
        let s = format!("{value:.6}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}
