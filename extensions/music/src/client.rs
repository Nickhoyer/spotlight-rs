//! Blocking REST client for the ampm server. Same shape as the Jira client:
//! one `ureq::Agent`, bearer auth, short timeouts — except radio generation,
//! which legitimately takes up to a minute of rate-limited API fan-out.

use std::time::Duration;

use anyhow::Context;
use serde::Deserialize;
use serde_json::{json, Value};

pub struct MusicClient {
    agent: ureq::Agent,
    base: String,
    token: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SongHit {
    pub id: String,
    pub name: String,
    pub artist_name: String,
    pub album_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PlaylistRow {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub created_at: String,
    pub deleted_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScrobbleRow {
    pub artist: String,
    pub title: String,
    pub approx_ts: i64,
    pub lastfm_status: String,
}

impl MusicClient {
    pub fn new(base: &str, token: &str) -> MusicClient {
        MusicClient {
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(10))
                .build(),
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
        }
    }

    fn get(&self, path: &str) -> anyhow::Result<Value> {
        let resp = self
            .agent
            .get(&format!("{}{path}", self.base))
            .set("Authorization", &format!("Bearer {}", self.token))
            .call()
            .with_context(|| format!("GET {path}"))?;
        resp.into_json().with_context(|| format!("parsing {path}"))
    }

    fn post(&self, path: &str, body: Value, timeout: Duration) -> anyhow::Result<Value> {
        let resp = self
            .agent
            .post(&format!("{}{path}", self.base))
            .timeout(timeout)
            .set("Authorization", &format!("Bearer {}", self.token))
            .send_json(body)
            .with_context(|| format!("POST {path}"))?;
        resp.into_json().with_context(|| format!("parsing {path}"))
    }

    pub fn health(&self) -> anyhow::Result<Value> {
        self.get("/health")
    }

    pub fn search(&self, q: &str, limit: u32) -> anyhow::Result<Vec<SongHit>> {
        let q = url_escape(q);
        let v = self.get(&format!("/search?q={q}&limit={limit}"))?;
        Ok(serde_json::from_value(v.get("songs").cloned().unwrap_or_default()).unwrap_or_default())
    }

    /// Ask the server to generate a radio playlist; slow by design.
    pub fn radio(&self, seed_song_id: &str) -> anyhow::Result<Value> {
        self.post(
            "/radio",
            json!({"seed_song_id": seed_song_id}),
            Duration::from_secs(180),
        )
    }

    pub fn daily_run(&self) -> anyhow::Result<Value> {
        self.post("/daily/run", json!({"flavor": "all"}), Duration::from_secs(600))
    }

    pub fn playlists(&self) -> anyhow::Result<Vec<PlaylistRow>> {
        let v = self.get("/playlists")?;
        Ok(serde_json::from_value(v.get("playlists").cloned().unwrap_or_default()).unwrap_or_default())
    }

    pub fn scrobbles(&self, limit: u32) -> anyhow::Result<Vec<ScrobbleRow>> {
        let v = self.get(&format!("/scrobbles/recent?limit={limit}"))?;
        Ok(serde_json::from_value(v.get("scrobbles").cloned().unwrap_or_default()).unwrap_or_default())
    }

    /// Report plays captured from Music.app; the server dedups and submits.
    pub fn ingest_scrobbles(&self, plays: &[crate::scrobbler::PendingPlay]) -> anyhow::Result<Value> {
        self.post(
            "/scrobbles/ingest",
            json!({ "plays": plays }),
            Duration::from_secs(30),
        )
    }

    pub fn cleanup_pending(&self) -> anyhow::Result<Vec<(i64, String)>> {
        let v = self.get("/cleanup/pending")?;
        Ok(v.get("pending")
            .and_then(|p| p.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|e| Some((e.get("id")?.as_i64()?, e.get("name")?.as_str()?.to_string())))
                    .collect()
            })
            .unwrap_or_default())
    }

    pub fn cleanup_confirm(&self, results: &[(i64, &str)]) -> anyhow::Result<()> {
        let results: Vec<Value> = results
            .iter()
            .map(|(id, status)| json!({"id": id, "status": status}))
            .collect();
        self.post("/cleanup/confirm", json!({ "results": results }), Duration::from_secs(10))?;
        Ok(())
    }
}

/// Minimal percent-encoding for a query value.
fn url_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
