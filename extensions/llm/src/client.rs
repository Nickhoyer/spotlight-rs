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
use serde_json::Value;

use crate::{ApiFormat, Provider};

/// System prompt for the launcher's chat. Two jobs: keep answers short, and —
/// critically — stop the model from inventing live data it cannot possibly know
/// (it has no internet, tools, or real-time access), which otherwise produces
/// confident lies about weather, news, prices, etc.
const CONCISE_SYSTEM: &str = "You are a concise assistant embedded in a desktop launcher. \
Keep answers short and direct — usually a sentence or two, or a short list. Skip preamble, \
don't restate the question, and avoid filler; expand only when explicitly asked.\n\n\
CRITICAL — you have NO internet access, NO tools, and NO real-time data. You cannot look \
anything up. So you genuinely do not know anything that changes over time or depends on the \
current moment or the user's location: the weather, today's date or time, current news, live \
scores, stock or crypto prices, traffic, what's happening 'now', or the contents of any URL, \
file, or account. If a question needs that kind of live or local information, do NOT guess, \
estimate, or make up a plausible-sounding answer — that is worse than no answer. Instead, say \
plainly that you can't access it (e.g. \"I can't check the weather — I have no internet \
access\"), and, if useful, point the user to where they could find it. Only answer from your \
own trained knowledge, and when you're unsure or something may be outdated, say so rather than \
inventing specifics.";

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
    /// The reply finished normally.
    Done,
    /// The request failed; carries a human-readable message.
    Error(String),
}

/// Stream a chat completion for `messages`, pushing [`StreamEvent`]s to `tx`.
///
/// Blocks until the stream ends, `cancel` is set, or the connection drops. `key`
/// is the API key (ignored for `provider.local`).
pub fn stream(
    provider: &Provider,
    key: &str,
    messages: &[Message],
    tx: UnboundedSender<StreamEvent>,
    cancel: Arc<AtomicBool>,
) {
    let result = match provider.format {
        ApiFormat::OpenAi => stream_openai(provider, key, messages, &tx, &cancel),
        ApiFormat::Anthropic => stream_anthropic(provider, key, messages, &tx, &cancel),
    };
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

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        // No read timeout: streamed replies can pause between tokens.
        .build()
}

fn base(provider: &Provider) -> String {
    provider.base_url.trim_end_matches('/').to_string()
}

fn stream_openai(
    provider: &Provider,
    key: &str,
    messages: &[Message],
    tx: &UnboundedSender<StreamEvent>,
    cancel: &Arc<AtomicBool>,
) -> Result<(), String> {
    // Prepend the concise system message before the conversation.
    let mut msgs = vec![serde_json::json!({ "role": "system", "content": CONCISE_SYSTEM })];
    msgs.extend(
        messages
            .iter()
            .map(|m| serde_json::json!({ "role": m.role, "content": m.content })),
    );
    let body = serde_json::json!({
        "model": provider.model,
        "stream": true,
        "messages": msgs,
    });
    let mut req = agent().post(&format!("{}/chat/completions", base(provider)));
    if !provider.local && !key.is_empty() {
        req = req.set("Authorization", &format!("Bearer {key}"));
    }
    let resp = req.send_json(body).map_err(describe_ureq)?;

    for_each_sse(resp.into_reader(), cancel, |data| {
        if data == "[DONE]" {
            return SseAction::Stop;
        }
        if let Ok(chunk) = serde_json::from_str::<Value>(data) {
            if let Some(text) = chunk
                .get("choices")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("delta"))
                .and_then(|d| d.get("content"))
                .and_then(Value::as_str)
            {
                if !text.is_empty() {
                    let _ = tx.unbounded_send(StreamEvent::Delta(text.to_string()));
                }
            }
        }
        SseAction::Continue
    })
}

fn stream_anthropic(
    provider: &Provider,
    key: &str,
    messages: &[Message],
    tx: &UnboundedSender<StreamEvent>,
    cancel: &Arc<AtomicBool>,
) -> Result<(), String> {
    // Anthropic takes the system prompt as a top-level field, not a message.
    let body = serde_json::json!({
        "model": provider.model,
        "max_tokens": 4096,
        "stream": true,
        "system": CONCISE_SYSTEM,
        "messages": messages.iter().map(|m| serde_json::json!({
            "role": m.role, "content": m.content,
        })).collect::<Vec<_>>(),
    });
    let resp = agent()
        .post(&format!("{}/messages", base(provider)))
        .set("x-api-key", key)
        .set("anthropic-version", "2023-06-01")
        .send_json(body)
        .map_err(describe_ureq)?;

    for_each_sse(resp.into_reader(), cancel, |data| {
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            return SseAction::Continue;
        };
        match event.get("type").and_then(Value::as_str) {
            Some("content_block_delta") => {
                if let Some(text) = event
                    .get("delta")
                    .and_then(|d| d.get("text"))
                    .and_then(Value::as_str)
                {
                    if !text.is_empty() {
                        let _ = tx.unbounded_send(StreamEvent::Delta(text.to_string()));
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
    })
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
        stream(provider, "", &[Message { role: "user".into(), content: "hi".into() }], tx, cancel);
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
