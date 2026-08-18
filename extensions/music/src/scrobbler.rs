//! Captures what Music.app actually plays and reports finished plays to the
//! ampm server, which owns the Last.fm submission.
//!
//! This exists because Apple's cloud listening history (`/v1/me/recent/played/
//! tracks`) does not record station playback at all, and lags and de-duplicates
//! everything else — so the server-side poller alone silently loses most Mac
//! listening. Reading Music.app directly sees every source: stations, library,
//! playlists, radio.
//!
//! Reading Music.app itself lives in [`crate::nowplaying`], which this worker
//! shares with the Home now-playing card.
//!
//! Requires the Automation (Apple Events → Music) TCC grant, same as the
//! cleanup worker.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::nowplaying::{poll, NowPlaying, NowPlayingStore};

/// How often Music.app is polled. Short enough to time plays accurately,
/// long enough to be free in CPU terms.
const POLL: Duration = Duration::from_secs(10);
/// Last.fm's rule: half the track, or 4 minutes, whichever comes first.
const SCROBBLE_AFTER_SECS: f64 = 240.0;
/// Tracks shorter than this are never scrobbled (Last.fm ignores them anyway).
const MIN_TRACK_SECS: f64 = 30.0;

/// A play that met the threshold and is waiting to reach the server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PendingPlay {
    pub artist: String,
    pub title: String,
    pub album: Option<String>,
    pub duration_ms: Option<i64>,
    /// Unix seconds when the track started (Last.fm wants the start time).
    pub played_at: i64,
}

/// Accumulates listening time for the current track and emits a play once it
/// crosses the threshold. Kept free of I/O so it can be tested directly.
#[derive(Debug, Default)]
pub struct ScrobbleState {
    identity: Option<String>,
    listened_secs: f64,
    last_position: f64,
    started_at: i64,
    emitted: bool,
}

impl ScrobbleState {
    /// Feed one observation; returns a play when this tick completes one.
    ///
    /// `now` is unix seconds and `elapsed` the wall time since the previous
    /// observation — listening time is credited from the *position* delta so
    /// pausing and seeking can't inflate it, clamped to `elapsed` so a seek
    /// forward can't either.
    pub fn observe(&mut self, np: &NowPlaying, now: i64, elapsed: f64) -> Option<PendingPlay> {
        if np.artist.trim().is_empty() || np.title.trim().is_empty() {
            // Streaming tracks occasionally report blank metadata; ignore the
            // tick rather than recording a bogus play.
            return None;
        }
        let identity = np.identity();
        let restarted = self.identity.as_deref() == Some(identity.as_str())
            && np.position_secs + 1.0 < self.last_position
            && np.position_secs < 2.0;

        if self.identity.as_deref() != Some(identity.as_str()) || restarted {
            self.identity = Some(identity);
            self.listened_secs = 0.0;
            self.emitted = false;
            // Position tells us how far in we joined, so a track already
            // playing when the worker starts gets a sane start time.
            self.started_at = now - np.position_secs as i64;
            self.last_position = np.position_secs;
            return None;
        }

        if np.playing {
            let delta = np.position_secs - self.last_position;
            if delta > 0.0 {
                self.listened_secs += delta.min(elapsed * 1.5);
            }
        }
        self.last_position = np.position_secs;

        if self.emitted || np.duration_secs < MIN_TRACK_SECS {
            return None;
        }
        let threshold = (np.duration_secs / 2.0).min(SCROBBLE_AFTER_SECS);
        if self.listened_secs >= threshold {
            self.emitted = true;
            return Some(PendingPlay {
                artist: np.artist.clone(),
                title: np.title.clone(),
                album: Some(np.album.clone()).filter(|a| !a.trim().is_empty()),
                duration_ms: Some((np.duration_secs * 1000.0) as i64),
                played_at: self.started_at,
            });
        }
        None
    }
}

fn queue_path() -> PathBuf {
    spotlight_config::cache_dir().join("music-scrobble-queue.json")
}

fn load_queue() -> Vec<PendingPlay> {
    std::fs::read_to_string(queue_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_queue(queue: &[PendingPlay]) {
    let _ = std::fs::create_dir_all(spotlight_config::cache_dir());
    if let Ok(json) = serde_json::to_string(queue) {
        let _ = std::fs::write(queue_path(), json);
    }
}

/// Spawn the Music.app scrobble worker (started from `app`'s main alongside
/// the cleanup worker).
pub fn spawn_scrobble_worker(store: Arc<NowPlayingStore>) {
    std::thread::Builder::new()
        .name("music-scrobbler".into())
        .spawn(move || {
            let mut state = ScrobbleState::default();
            // Plays survive server downtime, sleep, and app restarts.
            let mut queue = load_queue();
            let mut last_tick = Instant::now();
            loop {
                std::thread::sleep(POLL);
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                // Wall time actually elapsed, not the nominal interval: sleep
                // and a slow Apple event both stretch a tick, and `observe`
                // clamps the credited listening time against this.
                let elapsed = last_tick.elapsed().as_secs_f64();
                last_tick = Instant::now();
                let np = poll();
                // Share every observation so the Home card has recent state the
                // moment the launcher opens, without polling on its own.
                store.publish(np.clone());
                if let Some(np) = np {
                    if let Some(play) = state.observe(&np, now, elapsed) {
                        eprintln!("music-scrobbler: {} — {}", play.artist, play.title);
                        queue.push(play);
                        save_queue(&queue);
                    }
                }
                if !queue.is_empty() {
                    if flush(&queue) {
                        queue.clear();
                        save_queue(&queue);
                    }
                }
            }
        })
        .expect("spawn music scrobbler");
}

/// Send the queue to the server; `true` if it was accepted (and may be cleared).
fn flush(queue: &[PendingPlay]) -> bool {
    let Some(client) = crate::build_client() else {
        return false; // not configured yet — keep queuing
    };
    match client.ingest_scrobbles(queue) {
        Ok(v) => {
            eprintln!(
                "music-scrobbler: sent {} plays ({} accepted, {} duplicates)",
                queue.len(),
                v.get("accepted").and_then(|a| a.as_u64()).unwrap_or(0),
                v.get("duplicates").and_then(|d| d.as_u64()).unwrap_or(0),
            );
            true
        }
        Err(e) => {
            eprintln!("music-scrobbler: send failed, will retry: {e}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::nowplaying::PlayerState;

    fn np(title: &str, pos: f64, dur: f64, playing: bool) -> NowPlaying {
        NowPlaying {
            playing,
            state: if playing { PlayerState::Playing } else { PlayerState::Paused },
            artist: "Artist".into(),
            title: title.into(),
            album: "Album".into(),
            duration_secs: dur,
            position_secs: pos,
            persistent_id: "TEST".into(),
        }
    }

    #[test]
    fn scrobbles_at_half_track() {
        let mut s = ScrobbleState::default();
        // 200s track → threshold 100s.
        assert!(s.observe(&np("A", 0.0, 200.0, true), 1000, 10.0).is_none());
        let mut emitted = None;
        for i in 1..=12 {
            let pos = i as f64 * 10.0;
            if let Some(p) = s.observe(&np("A", pos, 200.0, true), 1000 + i * 10, 10.0) {
                emitted = Some(p);
                break;
            }
        }
        let p = emitted.expect("should scrobble past half");
        assert_eq!(p.title, "A");
        assert_eq!(p.duration_ms, Some(200_000));
    }

    #[test]
    fn scrobbles_only_once_per_play() {
        let mut s = ScrobbleState::default();
        s.observe(&np("A", 0.0, 100.0, true), 1000, 10.0);
        let mut count = 0;
        for i in 1..=9 {
            if s.observe(&np("A", i as f64 * 10.0, 100.0, true), 1000 + i * 10, 10.0).is_some() {
                count += 1;
            }
        }
        assert_eq!(count, 1);
    }

    #[test]
    fn paused_time_does_not_count() {
        let mut s = ScrobbleState::default();
        s.observe(&np("A", 0.0, 200.0, true), 1000, 10.0);
        // Position frozen while paused: no credit, so no scrobble.
        for i in 1..=30 {
            assert!(s.observe(&np("A", 0.0, 200.0, false), 1000 + i * 10, 10.0).is_none());
        }
    }

    #[test]
    fn seeking_forward_does_not_inflate() {
        let mut s = ScrobbleState::default();
        s.observe(&np("A", 0.0, 300.0, true), 1000, 10.0);
        // Jump 150s ahead in one tick: credited at most 1.5x the tick.
        assert!(s.observe(&np("A", 150.0, 300.0, true), 1010, 10.0).is_none());
    }

    #[test]
    fn track_change_resets_and_restart_rescrobbles() {
        let mut s = ScrobbleState::default();
        s.observe(&np("A", 0.0, 60.0, true), 1000, 10.0);
        for i in 1..=4 {
            s.observe(&np("A", i as f64 * 10.0, 60.0, true), 1000 + i * 10, 10.0);
        }
        // Switching tracks starts fresh.
        assert!(s.observe(&np("B", 0.0, 60.0, true), 1100, 10.0).is_none());
        // Replaying the same track from the top counts again.
        s.observe(&np("B", 30.0, 60.0, true), 1130, 10.0);
        assert!(s.observe(&np("B", 0.5, 60.0, true), 1160, 10.0).is_none());
    }

    #[test]
    fn blank_metadata_is_ignored() {
        let mut s = ScrobbleState::default();
        let blank = NowPlaying {
            playing: true,
            state: PlayerState::Playing,
            artist: "".into(),
            title: "".into(),
            album: "".into(),
            duration_secs: 200.0,
            position_secs: 100.0,
            persistent_id: String::new(),
        };
        assert!(s.observe(&blank, 1000, 10.0).is_none());
    }

    #[test]
    fn short_tracks_are_never_scrobbled() {
        let mut s = ScrobbleState::default();
        s.observe(&np("Jingle", 0.0, 20.0, true), 1000, 10.0);
        for i in 1..=5 {
            assert!(s.observe(&np("Jingle", i as f64 * 5.0, 20.0, true), 1000 + i * 5, 10.0).is_none());
        }
    }

    #[test]
    fn start_time_accounts_for_join_position() {
        let mut s = ScrobbleState::default();
        // Worker starts mid-track at 40s: start time is 40s ago, not now.
        s.observe(&np("A", 40.0, 100.0, true), 5000, 10.0);
        let mut got = None;
        for i in 1..=8 {
            if let Some(p) = s.observe(&np("A", 40.0 + i as f64 * 10.0, 100.0, true), 5000 + i * 10, 10.0) {
                got = Some(p);
                break;
            }
        }
        assert_eq!(got.expect("scrobbled").played_at, 4960);
    }

}
