//! The Mac-side half of playlist cleanup. Apple's official API is add-only, so
//! the ampm server can only *queue* deletions; this worker executes them
//! against Music.app via AppleScript (the deletion then syncs to every device
//! through iCloud Music Library) and confirms the outcome back to the server.
//!
//! Politeness rule: `tell application "Music"` launches Music if it isn't
//! running, which would be jarring from a background sweep. So hourly ticks
//! only act while Music is already running, but one tick per day proceeds
//! regardless so cleanup can't be starved on a Mac that never opens Music.
//!
//! Requires the Automation (Apple Events → Music) TCC grant; see
//! NSAppleEventsUsageDescription in packaging/Info.plist.

use std::process::Command;
use std::time::Duration;

use crate::search::escape_applescript;

const TICK: Duration = Duration::from_secs(3600);
const FORCE_EVERY_TICKS: u32 = 24;

/// Spawn the background cleanup worker (mirrors the clipboard monitor:
/// a named thread started from `app`'s main before the UI runs).
pub fn spawn_cleanup_worker() {
    std::thread::Builder::new()
        .name("music-cleanup".into())
        .spawn(|| {
            let mut ticks_since_forced = FORCE_EVERY_TICKS; // first tick may force
            loop {
                match run_once(ticks_since_forced >= FORCE_EVERY_TICKS) {
                    Ok(CleanupOutcome::Ran) => ticks_since_forced = 0,
                    Ok(CleanupOutcome::Skipped) => ticks_since_forced += 1,
                    Err(e) => {
                        eprintln!("music-cleanup: {e:#}");
                        ticks_since_forced += 1;
                    }
                }
                std::thread::sleep(TICK);
            }
        })
        .expect("spawn music cleanup worker");
}

enum CleanupOutcome {
    Ran,
    Skipped,
}

fn run_once(force: bool) -> anyhow::Result<CleanupOutcome> {
    let Some(client) = crate::build_client() else {
        return Ok(CleanupOutcome::Skipped); // not configured yet
    };
    let pending = client.cleanup_pending()?;
    if pending.is_empty() {
        return Ok(CleanupOutcome::Skipped);
    }
    if !music_is_running() && !force {
        // Don't launch Music from the background for a non-urgent sweep.
        return Ok(CleanupOutcome::Skipped);
    }

    let mut results: Vec<(i64, &str)> = Vec::new();
    let mut outcomes: Vec<(i64, String)> = Vec::new();
    for (id, name) in &pending {
        let status = delete_playlist(name);
        eprintln!("music-cleanup: '{name}' -> {status}");
        outcomes.push((*id, status));
    }
    for (id, status) in &outcomes {
        results.push((*id, status.as_str()));
    }
    client.cleanup_confirm(&results)?;
    Ok(CleanupOutcome::Ran)
}

/// Whether Music.app is already running. Every AppleScript in this extension
/// gates on this: `tell application "Music"` launches it otherwise.
pub(crate) fn music_is_running() -> bool {
    Command::new("/usr/bin/pgrep")
        .args(["-x", "Music"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Delete a playlist by its exact name. Only names from our own server-side
/// registry ever reach this, so there is no risk of touching user playlists.
fn delete_playlist(name: &str) -> String {
    let escaped = escape_applescript(name);
    // `with timeout` keeps a busy Music from stalling the sweep for minutes.
    let script = format!(
        "with timeout of 30 seconds\n\
           tell application \"Music\"\n\
             if (exists playlist \"{escaped}\") then\n\
               delete playlist \"{escaped}\"\n\
               return \"deleted\"\n\
             else\n\
               return \"not_found\"\n\
             end if\n\
           end tell\n\
         end timeout"
    );
    match Command::new("/usr/bin/osascript").arg("-e").arg(&script).output() {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            match stdout.trim() {
                "deleted" => "deleted".to_string(),
                "not_found" => "not_found".to_string(),
                other => {
                    eprintln!("music-cleanup: unexpected osascript output: {other}");
                    "error".to_string()
                }
            }
        }
        Ok(out) => {
            eprintln!(
                "music-cleanup: osascript failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
            "error".to_string()
        }
        Err(e) => {
            eprintln!("music-cleanup: cannot run osascript: {e}");
            "error".to_string()
        }
    }
}
