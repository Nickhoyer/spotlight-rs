//! The agent's `web_search` tool, backed by a Degoog meta-search server.
//!
//! Two halves: the tool *schema* (what we advertise to the model, in each wire
//! format) and the tool *execution* ([`run`] — a blocking GET that returns a
//! compact, model-readable digest of the top results). The client's tool-calling
//! loop in `client.rs` wires them together; this module knows nothing about SSE
//! or providers.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};

/// The tool name the model calls, and the key we match on in the stream.
pub const TOOL_NAME: &str = "web_search";

const DESCRIPTION: &str = "Search the web for live, current, or niche factual \
information you are unsure about — news, weather, prices, recent events, release \
dates, documentation, or anything after your training cutoff. Returns a ranked \
list of results (title, URL, snippet). Prefer searching over guessing; cite the \
sources you use as inline Markdown links.";

/// Resolved search settings for one chat session (the persisted [`SearchConfig`]
/// with its secret-store key looked up). `None` disables the tool entirely.
///
/// [`SearchConfig`]: crate::SearchConfig
#[derive(Debug, Clone)]
pub struct SearchCtx {
    pub endpoint: String,
    pub key: Option<String>,
    pub max_results: usize,
}

/// The `web_search` tool in OpenAI `tools` format.
pub fn openai_tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": TOOL_NAME,
            "description": DESCRIPTION,
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The search query, phrased as you would type it into a search engine."
                    }
                },
                "required": ["query"]
            }
        }
    })
}

/// The `web_search` tool in Anthropic `tools` format.
pub fn anthropic_tool() -> Value {
    json!({
        "name": TOOL_NAME,
        "description": DESCRIPTION,
        "input_schema": {
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The search query, phrased as you would type it into a search engine."
                }
            },
            "required": ["query"]
        }
    })
}

/// One result row from the Degoog API (`results[]`). Extra fields (`score`,
/// `source`, …) are ignored.
#[derive(Deserialize)]
struct Hit {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    snippet: String,
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    results: Vec<Hit>,
}

/// Run a web search and return a compact, model-readable digest of the top
/// `ctx.max_results` hits. Blocking; call on the background executor.
///
/// Errors carry a human-readable message; the caller feeds either the digest or
/// the error text back to the model as the tool result (a failed search should
/// not abort the whole reply).
pub fn run(ctx: &SearchCtx, query: &str) -> Result<String, String> {
    let query = query.trim();
    if query.is_empty() {
        return Err("empty search query".to_string());
    }

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .build();

    // `.query` percent-encodes the value.
    let mut req = agent.get(&ctx.endpoint).query("q", query);
    if let Some(key) = ctx.key.as_deref().filter(|k| !k.is_empty()) {
        req = req.set("Authorization", &format!("Bearer {key}"));
    }

    let resp = req.call().map_err(describe_ureq)?;
    let parsed: SearchResponse = resp
        .into_json()
        .map_err(|e| format!("couldn't parse search response: {e}"))?;

    Ok(format_results(query, &parsed.results, ctx.max_results))
}

/// Render results as a numbered list the model can read and cite.
fn format_results(query: &str, results: &[Hit], max: usize) -> String {
    if results.is_empty() {
        return format!("No results found for \"{query}\".");
    }
    let mut out = format!("Search results for \"{query}\":\n");
    for hit in results.iter().take(max.max(1)) {
        let title = if hit.title.trim().is_empty() { "(untitled)" } else { hit.title.trim() };
        out.push_str(&format!("\n- {title}\n  {}", hit.url.trim()));
        let snippet = hit.snippet.trim();
        if !snippet.is_empty() {
            out.push_str(&format!("\n  {snippet}"));
        }
    }
    out
}

/// Turn a `ureq::Error` into a readable message (mirrors the client's helper;
/// kept local so this module stays self-contained).
fn describe_ureq(err: ureq::Error) -> String {
    match err {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            format!("search server returned HTTP {code}: {}", body.trim())
        }
        ureq::Error::Transport(t) => format!("couldn't reach search server: {t}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(title: &str, url: &str, snippet: &str) -> Hit {
        Hit { title: title.into(), url: url.into(), snippet: snippet.into() }
    }

    #[test]
    fn format_results_caps_and_lists() {
        let hits = vec![
            hit("Rust lifetimes", "https://a.example/1", "A guide."),
            hit("Elision rules", "https://b.example/2", "The three rules."),
            hit("Third", "https://c.example/3", "Overflow."),
        ];
        let out = format_results("rust lifetimes", &hits, 2);
        assert!(out.contains("Search results for \"rust lifetimes\":"));
        assert!(out.contains("Rust lifetimes"));
        assert!(out.contains("https://a.example/1"));
        assert!(out.contains("Elision rules"));
        // Capped at 2 — the third hit is dropped.
        assert!(!out.contains("Third"));
    }

    #[test]
    fn format_results_handles_empty() {
        assert_eq!(format_results("nothing", &[], 5), "No results found for \"nothing\".");
    }

    #[test]
    fn format_results_tolerates_missing_fields() {
        let out = format_results("q", &[hit("", "https://x.example", "")], 5);
        assert!(out.contains("(untitled)"));
        assert!(out.contains("https://x.example"));
    }
}
