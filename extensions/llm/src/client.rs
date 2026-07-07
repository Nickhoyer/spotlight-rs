//! Blocking, streaming chat client over `ureq`.
//!
//! Speaks two wire formats — an OpenAI-compatible `/chat/completions` and the
//! Anthropic Messages API — both as Server-Sent Events. Methods block on the
//! network and are meant to run on gpui's background executor; deltas are pushed
//! back to the UI thread over a channel (see `view.rs`). A shared `cancel` flag
//! lets the UI stop generation mid-stream (Esc).

use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc::UnboundedSender;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::websearch::{self, SearchCtx};
use crate::{ApiFormat, Provider};

/// System prompt for the launcher's chat. Two jobs: keep answers short, and —
/// critically — stop the model from inventing live data it cannot possibly know
/// (it has no internet, tools, or real-time access), which otherwise produces
/// confident lies about weather, news, prices, etc.
const CONCISE_SYSTEM: &str = "You are a concise assistant embedded in a desktop launcher. \
Keep answers short and direct — usually a sentence or two, or a short list. Skip preamble, \
don't restate the question, and avoid filler; expand only when explicitly asked.\n\n\
CRITICAL — you have NO internet access and NO tools, so you cannot look anything up. The current \
date, time, timezone and approximate location may be supplied to you as ambient context below; \
when they are, rely on them. But for anything else that changes over time or depends on the live \
world — the weather, current news, live scores, stock or crypto prices, traffic, what's happening \
'now', or the contents of any URL, file, or account — you genuinely do not know it. Do NOT guess, \
estimate, or make up a plausible-sounding answer — that is worse than no answer. Instead, say \
plainly that you can't access it (e.g. \"I can't check the weather — I have no internet \
access\"), and, if useful, point the user to where they could find it. Otherwise answer from your \
own trained knowledge, and when you're unsure or something may be outdated, say so rather than \
inventing specifics.";

/// System prompt used when the `web_search` tool is available. The inverse of
/// [`CONCISE_SYSTEM`]'s "you have no internet" stance: the model *can* look
/// things up, so it should — for anything live, recent, or uncertain — and cite
/// what it finds instead of guessing.
const SEARCH_SYSTEM: &str = "You are a concise assistant embedded in a desktop launcher. \
Keep answers short and direct — usually a sentence or two, or a short list. Skip preamble, \
don't restate the question, and avoid filler; expand only when explicitly asked.\n\n\
You have a `web_search` tool backed by a privacy-preserving meta-search engine. Your own \
knowledge is frozen at training time and you have no other live data, so whenever a question \
depends on current, recent, local, or niche facts — news, weather, prices, scores, release \
dates, documentation, anything after your cutoff or that may have changed — call `web_search` \
rather than guessing. You may search more than once to refine or follow up. When you use \
results, state the key facts and cite the sources as inline Markdown links. If a search turns \
up nothing useful, say so plainly instead of inventing an answer.";

/// Cap on tool round-trips per reply, so a misbehaving model can't loop forever.
/// The final iteration runs with tools disabled to force a text answer.
const MAX_TOOL_ROUNDS: usize = 5;

/// A single chat message. Roles are `"user"` / `"assistant"`.
#[derive(Debug, Clone)]
pub struct Message {
    pub role: String,
    pub content: String,
}

/// An event emitted while streaming a reply.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A chunk of assistant text.
    Delta(String),
    /// A transient status line (e.g. "Searching the web…") shown while the agent
    /// runs a tool between turns. Cleared once assistant text resumes.
    Status(String),
    /// A completed web search: the query the model chose and the full results
    /// digest handed back to it. Rendered as an expandable entry in the log.
    Search { query: String, results: String },
    /// The reply finished normally.
    Done,
    /// The request failed; carries a human-readable message.
    Error(String),
}

/// A tool call the model requested, accumulated across streamed deltas.
#[derive(Debug, Default, Clone)]
struct ToolCall {
    id: String,
    name: String,
    /// Raw JSON arguments string, concatenated from streamed fragments.
    arguments: String,
}

impl ToolCall {
    /// The `query` argument, falling back to the raw arguments if the model sent
    /// a bare string instead of the expected `{"query": "..."}` object.
    fn query(&self) -> String {
        serde_json::from_str::<Value>(&self.arguments)
            .ok()
            .and_then(|v| v.get("query").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| self.arguments.trim().trim_matches('"').to_string())
    }

    /// The arguments as a JSON *object*, for echoing an Anthropic `tool_use`
    /// block (which requires an object input). Non-object or unparseable
    /// arguments are normalized to `{"query": ...}`.
    fn input(&self) -> Value {
        match serde_json::from_str::<Value>(&self.arguments) {
            Ok(v) if v.is_object() => v,
            _ => json!({ "query": self.query() }),
        }
    }
}

/// Stream a chat completion for `messages`, pushing [`StreamEvent`]s to `tx`.
///
/// When `search` is `Some`, the model is offered a `web_search` tool and this
/// runs an agentic loop — streaming text, executing any searches the model asks
/// for, feeding the results back, and continuing until it produces a final text
/// answer (see [`run_agent`]). When `None`, it's a single plain completion.
///
/// `context` is an optional ambient-context block (date/time, location, …)
/// appended to the system prompt; pass `""` for none.
///
/// Blocks until the reply ends, `cancel` is set, or the connection drops. `key`
/// is the API key (ignored for `provider.local`).
pub fn stream(
    provider: &Provider,
    key: &str,
    messages: &[Message],
    search: Option<&SearchCtx>,
    context: &str,
    tx: UnboundedSender<StreamEvent>,
    cancel: Arc<AtomicBool>,
) {
    let result = run_agent(provider, key, messages, search, context, &tx, &cancel);
    match result {
        Ok(()) => {
            let _ = tx.unbounded_send(StreamEvent::Done);
        }
        Err(e) => {
            // A cancel drops the reader mid-read, surfacing as an IO error we
            // don't want to report as a failure.
            if !cancel.load(Ordering::Relaxed) {
                let _ = tx.unbounded_send(StreamEvent::Error(e));
            }
        }
    }
}

/// The tool-calling loop. Maintains the wire conversation (format-specific JSON,
/// seeded from the text history) and runs turns until the model answers with
/// text instead of a tool call. Each turn streams its text to `tx`; a turn that
/// ends in `web_search` calls triggers the searches, appends their results, and
/// loops. The final iteration disables tools so the model must answer.
fn run_agent(
    provider: &Provider,
    key: &str,
    messages: &[Message],
    search: Option<&SearchCtx>,
    context: &str,
    tx: &UnboundedSender<StreamEvent>,
    cancel: &Arc<AtomicBool>,
) -> Result<(), String> {
    let base = if search.is_some() { SEARCH_SYSTEM } else { CONCISE_SYSTEM };
    // Append the ambient-context block (date/time, location, …) when present.
    let system = if context.trim().is_empty() {
        base.to_string()
    } else {
        format!("{base}\n\n{context}")
    };
    let system = system.as_str();
    let mut wire: Vec<Value> = messages
        .iter()
        .map(|m| json!({ "role": m.role, "content": m.content }))
        .collect();

    for round in 0..=MAX_TOOL_ROUNDS {
        // Advertise tools every round except the last (a forced text answer).
        let tools = search.is_some() && round < MAX_TOOL_ROUNDS;
        let calls = match provider.format {
            ApiFormat::OpenAi => openai_turn(provider, key, system, &mut wire, tools, tx, cancel)?,
            ApiFormat::Anthropic => anthropic_turn(provider, key, system, &mut wire, tools, tx, cancel)?,
        };
        if calls.is_empty() {
            return Ok(()); // the model answered with text — done.
        }
        // Tools were only advertised when `search` is `Some`, so this holds.
        let Some(ctx) = search else { return Ok(()) };

        // Run each requested search on this thread, collecting the digests.
        let mut results: Vec<(&ToolCall, String)> = Vec::with_capacity(calls.len());
        for call in &calls {
            if cancel.load(Ordering::Relaxed) {
                return Ok(());
            }
            let query = call.query();
            let _ = tx.unbounded_send(StreamEvent::Status(format!(
                "Searching the web for \u{201c}{query}\u{201d}\u{2026}"
            )));
            let digest = websearch::run(ctx, &query)
                .unwrap_or_else(|e| format!("web_search failed: {e}. Answer as best you can."));
            // Surface the completed search to the UI as an expandable log entry.
            let _ = tx.unbounded_send(StreamEvent::Search {
                query: query.clone(),
                results: digest.clone(),
            });
            results.push((call, digest));
        }

        // Append the tool results in the shape each API expects, then loop.
        match provider.format {
            ApiFormat::OpenAi => {
                for (call, digest) in &results {
                    wire.push(json!({ "role": "tool", "tool_call_id": call.id, "content": digest }));
                }
            }
            ApiFormat::Anthropic => {
                // Anthropic wants all tool_results in one following user turn.
                let content: Vec<Value> = results
                    .iter()
                    .map(|(call, digest)| {
                        json!({ "type": "tool_result", "tool_use_id": call.id, "content": digest })
                    })
                    .collect();
                wire.push(json!({ "role": "user", "content": content }));
            }
        }
    }
    Ok(())
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        // No read timeout: streamed replies can pause between tokens.
        .build()
}

fn base(provider: &Provider) -> String {
    provider.base_url.trim_end_matches('/').to_string()
}

/// One OpenAI-compatible turn. Streams text deltas to `tx` and accumulates any
/// `web_search` tool calls the model emits. On a tool call, echoes the
/// assistant's tool-call turn into `wire` (so the follow-up results correlate)
/// and returns the calls; otherwise returns an empty vec (the text answer).
fn openai_turn(
    provider: &Provider,
    key: &str,
    system: &str,
    wire: &mut Vec<Value>,
    tools: bool,
    tx: &UnboundedSender<StreamEvent>,
    cancel: &Arc<AtomicBool>,
) -> Result<Vec<ToolCall>, String> {
    // Prepend the system message before the running conversation.
    let mut msgs = vec![json!({ "role": "system", "content": system })];
    msgs.extend(wire.iter().cloned());
    let mut body = json!({
        "model": provider.model,
        "stream": true,
        "messages": msgs,
    });
    if tools {
        body["tools"] = json!([websearch::openai_tool()]);
    }
    let mut req = agent().post(&format!("{}/chat/completions", base(provider)));
    if !provider.local && !key.is_empty() {
        req = req.set("Authorization", &format!("Bearer {key}"));
    }
    let resp = req.send_json(body).map_err(describe_ureq)?;

    // Tool calls stream as fragments across deltas, keyed by `index`.
    let mut calls: Vec<ToolCall> = Vec::new();
    for_each_sse(resp.into_reader(), cancel, |data| {
        if data == "[DONE]" {
            return SseAction::Stop;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            return SseAction::Continue;
        };
        let Some(delta) =
            chunk.get("choices").and_then(|c| c.get(0)).and_then(|c| c.get("delta"))
        else {
            return SseAction::Continue;
        };
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            if !text.is_empty() {
                let _ = tx.unbounded_send(StreamEvent::Delta(text.to_string()));
            }
        }
        if let Some(tcs) = delta.get("tool_calls").and_then(Value::as_array) {
            for tc in tcs {
                let idx = tc.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                while calls.len() <= idx {
                    calls.push(ToolCall::default());
                }
                let slot = &mut calls[idx];
                if let Some(id) = tc.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                    slot.id = id.to_string();
                }
                let func = tc.get("function");
                if let Some(name) =
                    func.and_then(|f| f.get("name")).and_then(Value::as_str).filter(|s| !s.is_empty())
                {
                    slot.name = name.to_string();
                }
                if let Some(args) = func.and_then(|f| f.get("arguments")).and_then(Value::as_str) {
                    slot.arguments.push_str(args);
                }
            }
        }
        SseAction::Continue
    })?;

    // Keep only `web_search` calls (with our single tool, an unnamed call is one).
    calls.retain(|c| c.name.is_empty() || c.name == websearch::TOOL_NAME);
    for (i, c) in calls.iter_mut().enumerate() {
        if c.id.is_empty() {
            c.id = format!("call_{i}");
        }
    }

    if !calls.is_empty() {
        let tool_calls: Vec<Value> = calls
            .iter()
            .map(|c| {
                json!({
                    "id": c.id,
                    "type": "function",
                    "function": { "name": websearch::TOOL_NAME, "arguments": c.arguments },
                })
            })
            .collect();
        wire.push(json!({ "role": "assistant", "content": Value::Null, "tool_calls": tool_calls }));
    }
    Ok(calls)
}

/// One accumulated Anthropic content block: streamed text, or a tool-use call.
enum Block {
    Text(String),
    Tool(ToolCall),
}

/// One Anthropic Messages turn. Mirrors [`openai_turn`]: streams text to `tx`,
/// accumulates `tool_use` blocks, and — on a tool call — echoes the assistant
/// turn (text + tool_use blocks) into `wire` before returning the calls.
fn anthropic_turn(
    provider: &Provider,
    key: &str,
    system: &str,
    wire: &mut Vec<Value>,
    tools: bool,
    tx: &UnboundedSender<StreamEvent>,
    cancel: &Arc<AtomicBool>,
) -> Result<Vec<ToolCall>, String> {
    // Anthropic takes the system prompt as a top-level field, not a message.
    let mut body = json!({
        "model": provider.model,
        "max_tokens": 4096,
        "stream": true,
        "system": system,
        "messages": wire.clone(),
    });
    if tools {
        body["tools"] = json!([websearch::anthropic_tool()]);
    }
    let resp = agent()
        .post(&format!("{}/messages", base(provider)))
        .set("x-api-key", key)
        .set("anthropic-version", "2023-06-01")
        .send_json(body)
        .map_err(describe_ureq)?;

    // Content blocks arrive in order: each `content_block_start` opens a block at
    // an index, deltas fill it, `content_block_stop` closes it.
    let mut blocks: Vec<Block> = Vec::new();
    for_each_sse(resp.into_reader(), cancel, |data| {
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            return SseAction::Continue;
        };
        match event.get("type").and_then(Value::as_str) {
            Some("content_block_start") => {
                let cb = event.get("content_block");
                match cb.and_then(|c| c.get("type")).and_then(Value::as_str) {
                    Some("tool_use") => {
                        let id = cb
                            .and_then(|c| c.get("id"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let name = cb
                            .and_then(|c| c.get("name"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        blocks.push(Block::Tool(ToolCall { id, name, arguments: String::new() }));
                    }
                    _ => blocks.push(Block::Text(String::new())),
                }
                SseAction::Continue
            }
            Some("content_block_delta") => {
                let idx = event.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let delta = event.get("delta");
                if let Some(text) = delta.and_then(|d| d.get("text")).and_then(Value::as_str) {
                    if !text.is_empty() {
                        let _ = tx.unbounded_send(StreamEvent::Delta(text.to_string()));
                        if let Some(Block::Text(s)) = blocks.get_mut(idx) {
                            s.push_str(text);
                        }
                    }
                }
                if let Some(pj) =
                    delta.and_then(|d| d.get("partial_json")).and_then(Value::as_str)
                {
                    if let Some(Block::Tool(tc)) = blocks.get_mut(idx) {
                        tc.arguments.push_str(pj);
                    }
                }
                SseAction::Continue
            }
            Some("message_stop") => SseAction::Stop,
            Some("error") => {
                let msg = event
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("stream error");
                let _ = tx.unbounded_send(StreamEvent::Error(msg.to_string()));
                SseAction::Stop
            }
            _ => SseAction::Continue,
        }
    })?;

    // Split the blocks into tool calls plus the assistant turn to echo back.
    let mut calls: Vec<ToolCall> = Vec::new();
    let mut echo: Vec<Value> = Vec::new();
    for block in &blocks {
        match block {
            Block::Text(s) if !s.is_empty() => echo.push(json!({ "type": "text", "text": s })),
            Block::Text(_) => {}
            Block::Tool(tc) if tc.name == websearch::TOOL_NAME => {
                echo.push(json!({ "type": "tool_use", "id": tc.id, "name": tc.name, "input": tc.input() }));
                calls.push(tc.clone());
            }
            Block::Tool(_) => {} // unknown tool — ignore.
        }
    }
    if !calls.is_empty() {
        wire.push(json!({ "role": "assistant", "content": echo }));
    }
    Ok(calls)
}

enum SseAction {
    Continue,
    Stop,
}

/// Read an SSE stream line-by-line, invoking `on_data` with each `data:` payload.
/// Checks `cancel` between lines so Esc stops promptly.
fn for_each_sse(
    reader: impl std::io::Read,
    cancel: &Arc<AtomicBool>,
    mut on_data: impl FnMut(&str) -> SseAction,
) -> Result<(), String> {
    let mut lines = BufReader::new(reader).lines();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        let Some(line) = lines.next() else {
            return Ok(());
        };
        let line = line.map_err(|e| e.to_string())?;
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        if let SseAction::Stop = on_data(data.trim()) {
            return Ok(());
        }
    }
}

/// Turn a `ureq::Error` into a readable message, including the response body for
/// HTTP status errors (which usually explain the failure — bad key, bad model).
fn describe_ureq(err: ureq::Error) -> String {
    match err {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            let detail = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| {
                    v.get("error")
                        .and_then(|e| e.get("message").or(Some(e)))
                        .map(|m| m.as_str().map(str::to_string).unwrap_or_else(|| m.to_string()))
                })
                .unwrap_or(body);
            format!("HTTP {code}: {}", detail.trim())
        }
        ureq::Error::Transport(t) => format!("connection failed: {t}"),
    }
}

// --- Local server discovery -------------------------------------------------

#[derive(Deserialize)]
struct ModelsResponse {
    #[serde(default)]
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
}

/// A candidate local server: (display name, OpenAI-compatible base URL). Uses
/// `127.0.0.1` rather than `localhost` to avoid IPv6-only resolution missing an
/// IPv4-bound server. Covers the common OpenAI-compatible local servers.
const LOCAL_CANDIDATES: &[(&str, &str)] = &[
    ("Ollama", "http://127.0.0.1:11434/v1"),
    ("LM Studio", "http://127.0.0.1:1234/v1"),
    ("omlx", "http://127.0.0.1:8000/v1"),
    ("MLX", "http://127.0.0.1:8080/v1"),
    ("MLX Omni", "http://127.0.0.1:10240/v1"),
    ("Jan", "http://127.0.0.1:1337/v1"),
];

/// Probe well-known local LLM servers and return a [`Provider`] for each one
/// that responds. A `2xx` means no key is needed (`local: true`) and we read the
/// model list; a `401`/`403` means the server is up but wants an API key
/// (`local: false`, so Settings shows a key field) — either way it's a hit.
/// Blocking; run on the background executor.
pub fn probe_local() -> Vec<(Provider, Option<String>)> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_millis(1200))
        .build();
    LOCAL_CANDIDATES
        .iter()
        .filter_map(|(name, base_url)| probe_one(&agent, name, base_url))
        .collect()
}

/// Probe a single candidate. Returns the discovered [`Provider`] plus an
/// optional API key we found on disk (pre-filled into Settings). `Some` when the
/// server responds — either serving models, or demanding a key with `401`/`403`;
/// `None` when unreachable.
fn probe_one(agent: &ureq::Agent, name: &str, base_url: &str) -> Option<(Provider, Option<String>)> {
    let url = format!("{base_url}/models");
    match agent.get(&url).call() {
        Ok(resp) => {
            let models = parse_models(resp);
            Some((
                Provider {
                    name: name.to_string(),
                    base_url: base_url.to_string(),
                    model: models.first().cloned().unwrap_or_default(),
                    models,
                    format: ApiFormat::OpenAi,
                    local: true,
                },
                None,
            ))
        }
        // Server is up but requires an API key. Try to find the key on disk (e.g.
        // omlx's settings.json); if we do, re-probe with it to fill the model
        // list. Either way surface the server so the key field shows.
        Err(ureq::Error::Status(401 | 403, _)) => {
            let key = discovered_key(name);
            let models = key
                .as_deref()
                .and_then(|k| {
                    agent
                        .get(&url)
                        .set("Authorization", &format!("Bearer {k}"))
                        .call()
                        .ok()
                })
                .map(parse_models)
                .unwrap_or_default();
            Some((
                Provider {
                    name: name.to_string(),
                    base_url: base_url.to_string(),
                    model: models.first().cloned().unwrap_or_default(),
                    models,
                    format: ApiFormat::OpenAi,
                    local: false,
                },
                key,
            ))
        }
        Err(_) => None,
    }
}

fn parse_models(resp: ureq::Response) -> Vec<String> {
    resp.into_json::<ModelsResponse>()
        .map(|m| m.data.into_iter().map(|e| e.id).collect())
        .unwrap_or_default()
}

/// An API key found on disk for a known local server, so discovery can pre-fill
/// it. Currently omlx (`~/.omlx/settings.json` \u{2192} `auth.api_key`).
fn discovered_key(name: &str) -> Option<String> {
    if name != "omlx" {
        return None;
    }
    let home = std::env::var_os("HOME")?;
    let path = std::path::Path::new(&home).join(".omlx").join("settings.json");
    read_api_key(&path)
}

/// Read `auth.api_key` from an omlx-style settings file.
fn read_api_key(path: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    v.get("auth")?
        .get("api_key")?
        .as_str()
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt as _;
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::thread;

    /// Send a canned HTTP response, robustly: read the request headers first (so
    /// the client finishes sending), write the response, then half-close and
    /// drain to EOF so the client always reads the body without a reset.
    fn respond(mut sock: TcpStream, resp: String) {
        let mut tmp = [0u8; 512];
        let mut seen = Vec::new();
        while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
            match sock.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => seen.extend_from_slice(&tmp[..n]),
                Err(_) => break,
            }
        }
        let _ = sock.write_all(resp.as_bytes());
        let _ = sock.flush();
        let _ = sock.shutdown(Shutdown::Write);
        let _ = sock.read(&mut tmp); // wait for the client to close
    }

    /// Spin up a one-shot HTTP server that replies with `sse_body`, and return a
    /// `Provider` pointed at it. `Connection: close` (no content-length) makes
    /// `ureq`'s reader consume until EOF — exactly the streaming shape.
    fn serve_sse(format: ApiFormat, sse_body: &'static str) -> Provider {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            respond(
                sock,
                format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{sse_body}"),
            );
        });
        Provider {
            name: "Test".into(),
            base_url: format!("http://127.0.0.1:{port}"),
            model: "test-model".into(),
            models: vec![],
            format,
            local: true,
        }
    }

    fn collect(provider: &Provider) -> Vec<StreamEvent> {
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let cancel = Arc::new(AtomicBool::new(false));
        stream(provider, "", &[Message { role: "user".into(), content: "hi".into() }], None, "", tx, cancel);
        futures::executor::block_on(async {
            let mut out = Vec::new();
            while let Some(ev) = rx.next().await {
                out.push(ev);
            }
            out
        })
    }

    fn text(events: &[StreamEvent]) -> String {
        events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Delta(s) => Some(s.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn tool_call_query_parses_json_and_falls_back() {
        // Well-formed arguments — extract the `query` field.
        let tc = ToolCall {
            id: "c1".into(),
            name: "web_search".into(),
            arguments: r#"{"query":"rust lifetimes"}"#.into(),
        };
        assert_eq!(tc.query(), "rust lifetimes");
        assert_eq!(tc.input(), json!({ "query": "rust lifetimes" }));

        // A bare JSON string instead of an object — fall back to the raw value,
        // unquoted, and still produce a valid `{query: ...}` input.
        let bare = ToolCall { arguments: r#""weather in oslo""#.into(), ..Default::default() };
        assert_eq!(bare.query(), "weather in oslo");
        assert_eq!(bare.input(), json!({ "query": "weather in oslo" }));

        // Garbage arguments — query is the raw text; input wraps it.
        let junk = ToolCall { arguments: "not json".into(), ..Default::default() };
        assert_eq!(junk.query(), "not json");
        assert_eq!(junk.input(), json!({ "query": "not json" }));
    }

    #[test]
    fn openai_stream_accumulates_deltas_and_stops_on_done() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n\
                    data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n\
                    data: [DONE]\n\n";
        let events = collect(&serve_sse(ApiFormat::OpenAi, body));
        assert_eq!(text(&events), "Hello");
        assert!(matches!(events.last(), Some(StreamEvent::Done)));
    }

    /// A one-shot server returning a fixed status + body for `GET /v1/models`.
    fn serve_once(status_line: &'static str, body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            respond(
                sock,
                format!(
                    "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                ),
            );
        });
        format!("http://127.0.0.1:{port}/v1")
    }

    #[test]
    fn probe_one_reads_models_on_success() {
        let base = serve_once("200 OK", "{\"data\":[{\"id\":\"llama3.2\"},{\"id\":\"qwen\"}]}");
        let agent = ureq::AgentBuilder::new().build();
        let (p, key) = probe_one(&agent, "Test", &base).expect("reachable");
        assert!(p.local);
        assert!(key.is_none());
        assert_eq!(p.models, vec!["llama3.2".to_string(), "qwen".to_string()]);
        assert_eq!(p.model, "llama3.2");
    }

    #[test]
    fn probe_one_surfaces_auth_required_server() {
        // A server that responds 401 without a key must still be discovered. Use
        // a non-omlx name so no on-disk key lookup happens (keeps the test
        // hermetic).
        let base = serve_once("401 Unauthorized", "{\"error\":{\"message\":\"API key required\"}}");
        let agent = ureq::AgentBuilder::new().build();
        let (p, key) = probe_one(&agent, "Test", &base).expect("found despite 401");
        assert!(!p.local, "auth-required server needs a key field");
        assert!(key.is_none());
        assert!(p.models.is_empty());
    }

    #[test]
    fn read_api_key_extracts_omlx_key() {
        let dir = std::env::temp_dir().join(format!("omlx-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, "{\"auth\":{\"api_key\":\"nick-local-key-8000\"}}").unwrap();
        assert_eq!(read_api_key(&path).as_deref(), Some("nick-local-key-8000"));

        std::fs::write(&path, "{\"auth\":{\"api_key\":\"\"}}").unwrap();
        assert_eq!(read_api_key(&path), None, "empty key is treated as absent");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn anthropic_stream_extracts_text_deltas_and_stops() {
        let body = "event: content_block_delta\n\
                    data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n\
                    data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\" there\"}}\n\n\
                    data: {\"type\":\"message_stop\"}\n\n";
        let events = collect(&serve_sse(ApiFormat::Anthropic, body));
        assert_eq!(text(&events), "Hi there");
        assert!(matches!(events.last(), Some(StreamEvent::Done)));
    }
}
