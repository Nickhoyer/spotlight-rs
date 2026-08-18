//! Shared "what is Music.app doing right now" state, plus the artwork and
//! transport plumbing built on it.
//!
//! The scrobbler already reads Music.app every 10s, but it consumed each
//! observation inside its own loop and dropped it, so nothing else in the app
//! could ask what was playing. This module owns that read instead: the
//! scrobbler and the Home now-playing card are both readers of one store.
//!
//! Two poll rates, because they want different things. The scrobbler wants
//! accurate *timing* and is happy at 10s. The card wants to look live, but only
//! while the launcher is actually open — so the poller here sits blocked on a
//! condvar and does nothing at all until the UI pokes it, then polls once a
//! second for a few seconds. A launcher that is never summoned runs no
//! AppleScript beyond the scrobbler's existing tick.
//!
//! Requires the Automation (Apple Events → Music) TCC grant, same as the
//! cleanup worker.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use spotlight_ui::{NowPlayingSnapshot, NowPlayingSource, Transport};

use crate::search::escape_applescript;

/// How often to re-poll while the launcher is open.
const FAST: Duration = Duration::from_secs(1);
/// How long a single poke keeps the fast polling alive. Comfortably longer than
/// the UI's poke interval, so a visible Home never falls back to idle.
const ACTIVE: Duration = Duration::from_secs(6);
/// Music takes a moment to land on the next track, so a transport command polls
/// once immediately and again after this.
const SETTLE: Duration = Duration::from_millis(400);
/// Longest edge of a cached artwork thumbnail. Album art arrives at up to
/// 3000px; the card draws it at 52pt.
const ART_PX: u32 = 128;
/// How many artwork thumbnails to keep before pruning the oldest.
const ART_KEEP: usize = 32;

/// Music.app's player state. `NowPlaying::playing` collapses everything but
/// `Playing` into `false`, which is right for scrobbling but loses the
/// distinction the card needs — a paused track still has something to show,
/// a stopped one does not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PlayerState {
    Playing,
    Paused,
    Stopped,
    #[default]
    Unknown,
}

/// One observation of Music.app's state.
#[derive(Debug, Clone, PartialEq)]
pub struct NowPlaying {
    pub playing: bool,
    pub state: PlayerState,
    pub artist: String,
    pub title: String,
    pub album: String,
    pub duration_secs: f64,
    pub position_secs: f64,
    /// Music's own stable track id. Empty for radio and some streams, which is
    /// why the artwork cache falls back to hashing the metadata.
    pub persistent_id: String,
}

impl NowPlaying {
    pub(crate) fn identity(&self) -> String {
        format!("{}|{}", self.artist, self.title)
    }

    /// Whether this is worth showing on Home: something is loaded and Music is
    /// either playing it or holding it paused.
    fn showable(&self) -> bool {
        matches!(self.state, PlayerState::Playing | PlayerState::Paused)
            && !self.title.trim().is_empty()
    }

    /// Cache key for this track's artwork. Filesystem-safe by construction.
    fn artwork_key(&self) -> String {
        let id = self.persistent_id.trim();
        if !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric()) {
            return id.to_string();
        }
        let mut h = DefaultHasher::new();
        format!("{}|{}|{}", self.artist, self.album, self.title).hash(&mut h);
        format!("h{:016x}", h.finish())
    }
}

// ---- The shared store -------------------------------------------------------

#[derive(Default)]
struct Wake {
    /// Poll at the interactive rate until this instant.
    active_until: Option<Instant>,
    /// Transport commands to run before the next poll.
    queued: Vec<Transport>,
    /// Set once so the poller can wake immediately on the first poke.
    poked: bool,
}

/// The single source of truth for current playback, written by the poller
/// thread (and the scrobbler's own tick) and read by the UI.
pub struct NowPlayingStore {
    /// Current track, when it started being observed, and its artwork path.
    slot: Mutex<Option<(NowPlaying, Instant, Option<PathBuf>)>>,
    wake: Mutex<Wake>,
    cv: Condvar,
}

impl NowPlayingStore {
    /// Build the store and start its poller thread.
    pub fn new() -> Arc<Self> {
        let store = Arc::new(NowPlayingStore {
            slot: Mutex::new(None),
            wake: Mutex::new(Wake::default()),
            cv: Condvar::new(),
        });
        if let Some(demo) = demo_snapshot() {
            *store.slot.lock().unwrap() = Some(demo);
            return store; // no poller: there is no Music.app to talk to
        }
        let worker = store.clone();
        std::thread::Builder::new()
            .name("music-nowplaying".into())
            .spawn(move || worker.run())
            .expect("spawn music now-playing poller");
        store
    }

    /// Latest state, filtered to what Home should show.
    pub fn snapshot(&self) -> Option<NowPlayingSnapshot> {
        let slot = self.slot.lock().unwrap();
        let (np, sampled_at, artwork) = slot.as_ref()?;
        if !np.showable() {
            return None;
        }
        Some(NowPlayingSnapshot {
            title: np.title.clone(),
            artist: np.artist.clone(),
            album: np.album.clone(),
            playing: np.playing,
            duration_secs: np.duration_secs,
            position_secs: np.position_secs,
            sampled_at: *sampled_at,
            artwork: artwork.clone(),
        })
    }

    /// Record an observation (also called by the scrobbler's 10s tick, so the
    /// card has recent state the instant Home opens).
    pub fn publish(&self, np: Option<NowPlaying>) {
        if demo_snapshot().is_some() {
            return;
        }
        let mut slot = self.slot.lock().unwrap();
        match np {
            None => *slot = None,
            Some(np) => {
                // Keep the artwork path across ticks of the same track so the
                // card doesn't flicker back to the placeholder every poll.
                let art = slot
                    .as_ref()
                    .filter(|(prev, _, _)| prev.artwork_key() == np.artwork_key())
                    .and_then(|(_, _, art)| art.clone());
                *slot = Some((np, Instant::now(), art));
            }
        }
    }

    /// "Home is on screen" — poll at the interactive rate for a few seconds.
    pub fn poke(&self) {
        let mut wake = self.wake.lock().unwrap();
        wake.active_until = Some(Instant::now() + ACTIVE);
        wake.poked = true;
        self.cv.notify_all();
    }

    /// Queue a transport command. Returns immediately; the poller runs it.
    pub fn control(&self, cmd: Transport) {
        let mut wake = self.wake.lock().unwrap();
        wake.queued.push(cmd);
        wake.active_until = Some(Instant::now() + ACTIVE);
        wake.poked = true;
        self.cv.notify_all();
    }

    /// The type-erased seam handed to the UI shell.
    pub fn source(self: &Arc<Self>) -> NowPlayingSource {
        let (snap, poke, control) = (self.clone(), self.clone(), self.clone());
        NowPlayingSource {
            snapshot: Arc::new(move || snap.snapshot()),
            poke: Arc::new(move || poke.poke()),
            control: Arc::new(move |cmd| control.control(cmd)),
        }
    }

    fn run(self: Arc<Self>) {
        loop {
            let cmds = self.wait_for_work();
            for cmd in &cmds {
                transport(*cmd);
            }
            self.poll_and_publish();
            if !cmds.is_empty() {
                // Catch the track Music switched to.
                std::thread::sleep(SETTLE);
                self.poll_and_publish();
            }
        }
    }

    /// Block until there is something to do, returning any queued commands.
    /// While active, returns after `FAST`; while idle, sleeps indefinitely.
    fn wait_for_work(&self) -> Vec<Transport> {
        let mut wake = self.wake.lock().unwrap();
        loop {
            if !wake.queued.is_empty() {
                wake.poked = false;
                return std::mem::take(&mut wake.queued);
            }
            let active = wake.active_until.is_some_and(|t| Instant::now() < t);
            if active {
                if wake.poked {
                    wake.poked = false;
                    return Vec::new();
                }
                let (w, _) = self.cv.wait_timeout(wake, FAST).unwrap();
                wake = w;
                if wake.queued.is_empty() {
                    wake.poked = false;
                    return Vec::new(); // the fast-poll tick
                }
            } else {
                wake.active_until = None;
                wake = self.cv.wait(wake).unwrap();
            }
        }
    }

    fn poll_and_publish(&self) {
        let np = poll();
        let key = np.as_ref().filter(|np| np.showable()).map(|np| np.artwork_key());
        self.publish(np);
        // Artwork is a second Apple event, so only fetch it when the track
        // actually changed and we don't already have the file.
        if let Some(key) = key {
            let have = {
                let slot = self.slot.lock().unwrap();
                slot.as_ref().is_some_and(|(_, _, art)| art.is_some())
            };
            if have {
                return;
            }
            if let Some(path) = fetch_artwork(&key) {
                let mut slot = self.slot.lock().unwrap();
                if let Some((cur, _, art)) = slot.as_mut() {
                    if cur.artwork_key() == key {
                        *art = Some(path);
                    }
                }
            }
        }
    }
}

// ---- Reading Music.app ------------------------------------------------------

/// Read Music.app's current state, or `None` when it isn't running or has
/// nothing loaded. Never launches Music.
pub(crate) fn poll() -> Option<NowPlaying> {
    if !crate::cleanup::music_is_running() {
        return None;
    }
    // Tab-separated so titles containing commas/quotes stay intact. The
    // `with timeout` matters: without it a busy or permission-blocked Music
    // stalls the Apple event for ~2 minutes per tick; the outer try turns that
    // (and any transient error) into a skipped tick.
    const SCRIPT: &str = r#"
try
  with timeout of 5 seconds
    tell application "Music"
      set s to (player state as text)
      set t to current track
      set pid to ""
      try
        set pid to (get persistent ID of t)
      end try
      return s & tab & (get artist of t) & tab & (get name of t) & tab & (get album of t) & tab & (get duration of t) & tab & (get player position) & tab & pid
    end tell
  end timeout
on error
  return "none"
end try"#;
    let out = Command::new("/usr/bin/osascript").arg("-e").arg(SCRIPT).output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_now_playing(&String::from_utf8_lossy(&out.stdout))
}

/// Parse the tab-separated osascript output. Defensive: any malformed or
/// partial line yields `None` rather than a half-populated play. Tolerates the
/// pre-`persistent ID` six-field shape.
pub fn parse_now_playing(raw: &str) -> Option<NowPlaying> {
    let line = raw.trim();
    if line.is_empty() || line == "none" {
        return None;
    }
    let f: Vec<&str> = line.split('\t').collect();
    if f.len() < 6 {
        return None;
    }
    let state = match f[0].trim() {
        "playing" => PlayerState::Playing,
        "paused" => PlayerState::Paused,
        "stopped" => PlayerState::Stopped,
        _ => PlayerState::Unknown,
    };
    Some(NowPlaying {
        playing: state == PlayerState::Playing,
        state,
        artist: f[1].trim().to_string(),
        title: f[2].trim().to_string(),
        album: f[3].trim().to_string(),
        duration_secs: parse_secs(f[4])?,
        position_secs: parse_secs(f[5]).unwrap_or(0.0),
        persistent_id: f.get(6).map(|s| s.trim().to_string()).unwrap_or_default(),
    })
}

/// Parse a number AppleScript formatted for the *system locale*.
///
/// `get duration of t` coerces a real to text using the user's decimal
/// separator, so a Danish or German Mac reports `238,173` and a plain
/// `str::parse::<f64>` fails on it — silently, since the caller treats a parse
/// failure as "nothing playing". Whichever separator appears last is the
/// decimal one; anything else is digit grouping.
fn parse_secs(raw: &str) -> Option<f64> {
    let t = raw.trim();
    let normalized = match (t.rfind('.'), t.rfind(',')) {
        (Some(dot), Some(comma)) if comma > dot => t.replace('.', "").replace(',', "."),
        (Some(_), Some(_)) => t.replace(',', ""),
        (None, Some(_)) => t.replace(',', "."),
        _ => t.to_string(),
    };
    normalized.parse().ok()
}

// ---- Transport --------------------------------------------------------------

fn transport(cmd: Transport) {
    // Same politeness rule as the cleanup sweep: never launch Music.
    if !crate::cleanup::music_is_running() {
        return;
    }
    let verb = match cmd {
        Transport::PlayPause => "playpause",
        Transport::Next => "next track",
        Transport::Prev => "previous track",
    };
    let script = format!(
        "try\n  with timeout of 5 seconds\n    tell application \"Music\" to {verb}\n  end timeout\non error\nend try"
    );
    let _ = Command::new("/usr/bin/osascript").arg("-e").arg(&script).output();
}

// ---- Artwork ----------------------------------------------------------------

fn art_dir() -> PathBuf {
    spotlight_config::cache_dir().join("music-artwork")
}

/// Dump the current track's artwork, downscale it, and return the cached PNG.
/// `None` when the track has no artwork, which is common for radio and streams.
fn fetch_artwork(key: &str) -> Option<PathBuf> {
    let dir = art_dir();
    let out = dir.join(format!("{key}.png"));
    if out.exists() {
        return Some(out);
    }
    std::fs::create_dir_all(&dir).ok()?;
    let raw = dir.join(format!("{key}.raw"));

    if !dump_artwork(&raw) {
        let _ = std::fs::remove_file(&raw);
        return None;
    }
    // Downscale here rather than in the shell: the UI thread then decodes a
    // 128px PNG instead of a multi-megapixel JPEG, and the texture atlas holds
    // a thumbnail per track instead of a full-size cover.
    let bytes = std::fs::read(&raw).ok();
    let _ = std::fs::remove_file(&raw);
    let decoded = image::load_from_memory(&bytes?).ok()?;
    let thumb = decoded.thumbnail(ART_PX, ART_PX);
    thumb.save_with_format(&out, image::ImageFormat::Png).ok()?;
    prune_artwork(&dir);
    Some(out)
}

/// Ask Music.app to write the current track's artwork bytes to `path`.
/// `open for access` deliberately sits outside the `tell` block so the file is
/// opened by the script host, not by Music.
fn dump_artwork(path: &Path) -> bool {
    if !crate::cleanup::music_is_running() {
        return false;
    }
    let p = escape_applescript(&path.to_string_lossy());
    let script = format!(
        "try\n\
         \x20 with timeout of 10 seconds\n\
         \x20   tell application \"Music\"\n\
         \x20     set t to current track\n\
         \x20     if (count of artwork of t) is 0 then return \"none\"\n\
         \x20     set d to raw data of artwork 1 of t\n\
         \x20   end tell\n\
         \x20   set fh to open for access POSIX file \"{p}\" with write permission\n\
         \x20   set eof fh to 0\n\
         \x20   write d to fh\n\
         \x20   close access fh\n\
         \x20   return \"ok\"\n\
         \x20 end timeout\n\
         on error\n\
         \x20 try\n\
         \x20   close access POSIX file \"{p}\"\n\
         \x20 end try\n\
         \x20 return \"none\"\n\
         end try"
    );
    match Command::new("/usr/bin/osascript").arg("-e").arg(&script).output() {
        Ok(out) if out.status.success() => {
            String::from_utf8_lossy(&out.stdout).trim() == "ok"
                && std::fs::metadata(path).map(|m| m.len() > 0).unwrap_or(false)
        }
        _ => false,
    }
}

/// Keep the cache bounded — one thumbnail per track played, forever, would grow
/// without limit on a Mac that never restarts.
fn prune_artwork(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "png"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    if files.len() <= ART_KEEP {
        return;
    }
    files.sort_by_key(|(t, _)| *t);
    for (_, path) in files.iter().take(files.len() - ART_KEEP) {
        let _ = std::fs::remove_file(path);
    }
}

// ---- Demo seed --------------------------------------------------------------

/// `SPOTLIGHT_MUSIC_DEMO=1` publishes a fixed track instead of talking to
/// Music.app, so the card can be screenshotted headlessly. A debug binary run
/// from a shell is a different code identity than the signed bundle, so the
/// Automation grant would prompt — and can't be granted over SSH.
fn demo_snapshot() -> Option<(NowPlaying, Instant, Option<PathBuf>)> {
    std::env::var_os("SPOTLIGHT_MUSIC_DEMO")?;
    let np = NowPlaying {
        playing: true,
        state: PlayerState::Playing,
        artist: "Jon Hopkins".into(),
        title: "Emerald Rush".into(),
        album: "Singularity".into(),
        duration_secs: 353.0,
        position_secs: 96.0,
        persistent_id: "DEM0DEM0DEM0DEM0".into(),
    };
    Some((np, Instant::now(), demo_artwork()))
}

/// A generated gradient stands in for cover art under the demo seed.
fn demo_artwork() -> Option<PathBuf> {
    let dir = art_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("demo.png");
    let img = image::ImageBuffer::from_fn(ART_PX, ART_PX, |x, y| {
        let t = (x + y) as f32 / (2.0 * ART_PX as f32);
        image::Rgb([(20.0 + 90.0 * t) as u8, (40.0 + 150.0 * t) as u8, (90.0 + 130.0 * t) as u8])
    });
    image::DynamicImage::ImageRgb8(img).save_with_format(&path, image::ImageFormat::Png).ok()?;
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_osascript_output() {
        let np = parse_now_playing("playing\tArtist\tTitle\tAlbum\t210.5\t42.25\tABC123\n").unwrap();
        assert!(np.playing);
        assert_eq!(np.state, PlayerState::Playing);
        assert_eq!(np.artist, "Artist");
        assert_eq!(np.title, "Title");
        assert_eq!(np.album, "Album");
        assert_eq!(np.duration_secs, 210.5);
        assert_eq!(np.position_secs, 42.25);
        assert_eq!(np.persistent_id, "ABC123");
    }

    #[test]
    fn title_with_comma_survives() {
        let np = parse_now_playing("playing\tA\tHello, World\tAlb\t100\t0\t").unwrap();
        assert_eq!(np.title, "Hello, World");
    }

    #[test]
    fn none_and_garbage_yield_nothing() {
        assert!(parse_now_playing("none").is_none());
        assert!(parse_now_playing("").is_none());
        assert!(parse_now_playing("playing\tA\tB").is_none());
    }

    #[test]
    fn paused_and_stopped_are_distinguished() {
        let paused = parse_now_playing("paused\tA\tB\tC\t100\t10\tID").unwrap();
        assert!(!paused.playing);
        assert_eq!(paused.state, PlayerState::Paused);
        assert!(paused.showable(), "a paused track still has something to show");

        let stopped = parse_now_playing("stopped\tA\tB\tC\t100\t0\tID").unwrap();
        assert_eq!(stopped.state, PlayerState::Stopped);
        assert!(!stopped.showable());
    }

    /// Guards against a Music version dropping `persistent ID`.
    #[test]
    fn six_field_output_still_parses() {
        let np = parse_now_playing("playing\tA\tB\tC\t100\t10").unwrap();
        assert_eq!(np.persistent_id, "");
        assert!(np.playing);
    }

    #[test]
    fn artwork_key_is_stable_and_filesystem_safe() {
        let mut np = parse_now_playing("playing\tA\tB\tC\t100\t10\tABC123").unwrap();
        assert_eq!(np.artwork_key(), "ABC123");

        // Radio reports no id; the metadata hash takes over and stays stable.
        np.persistent_id = String::new();
        let key = np.artwork_key();
        assert_eq!(key, np.artwork_key());
        assert!(key.chars().all(|c| c.is_ascii_alphanumeric()), "{key}");

        // A different track hashes differently.
        let other = parse_now_playing("playing\tA\tOther\tC\t100\t10").unwrap();
        assert_ne!(key, other.artwork_key());
    }

    /// AppleScript formats reals with the system locale's decimal separator, so
    /// a Danish Mac reports `238,173`. Getting this wrong doesn't error — it
    /// just makes every tick look like "nothing playing".
    #[test]
    fn locale_formatted_numbers_parse() {
        assert_eq!(parse_secs("238.173"), Some(238.173));
        assert_eq!(parse_secs("238,173"), Some(238.173));
        // Digit grouping, either convention.
        assert_eq!(parse_secs("1.234,5"), Some(1234.5));
        assert_eq!(parse_secs("1,234.5"), Some(1234.5));
        assert_eq!(parse_secs("42"), Some(42.0));
        assert_eq!(parse_secs(" 7,5 "), Some(7.5));
        assert_eq!(parse_secs("abc"), None);
    }

    #[test]
    fn comma_decimals_from_a_european_mac_still_yield_a_track() {
        let np = parse_now_playing(
            "paused\tCrowded House\tDon't Dream It's Over\tDeluxe\t238,173004150391\t159,936004638672\t416625B339F817BA",
        )
        .expect("a comma decimal separator must not read as 'nothing playing'");
        assert_eq!(np.state, PlayerState::Paused);
        assert!((np.duration_secs - 238.173).abs() < 0.01);
        assert!((np.position_secs - 159.936).abs() < 0.01);
        assert!(np.showable());
    }

    #[test]
    fn blank_title_is_not_showable() {
        let np = parse_now_playing("playing\tA\t\tC\t100\t10\tID").unwrap();
        assert!(!np.showable());
    }
}
