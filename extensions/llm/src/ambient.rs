//! Ambient context for the chat agent: the small, unsurprising facts about "now"
//! and the user's environment that any assistant is expected to know and that we
//! can gather *without asking permission* — the current date/time, the system
//! timezone and locale (all local, no network), plus a coarse IP-based location
//! (country/region/city), cached to disk and refreshed in the background.
//!
//! [`system_context`] renders these as a block appended to the system prompt. The
//! timezone alone usually pins the country; the IP lookup mainly helps when the
//! user is travelling with a system clock that no longer matches where they are.
//!
//! macOS-only, like the rest of the app: it shells out to `date`, reads the
//! `/etc/localtime` symlink, and asks `defaults` for the locale.

use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// How long a cached IP location stays fresh before a background refresh.
const LOCATION_TTL: u64 = 24 * 60 * 60;

/// Coarse IP-derived location, cached on disk between runs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Location {
    /// When it was fetched (unix seconds), for TTL checks.
    #[serde(default)]
    fetched_at: u64,
    #[serde(default)]
    city: String,
    #[serde(default)]
    region: String,
    #[serde(default)]
    country: String,
}

/// Build the ambient-context block for the system prompt. Returns an empty
/// string if nothing could be gathered. Cheap but not free (spawns `date` /
/// `defaults`), so call it off the UI thread.
pub fn system_context() -> String {
    format_block(local_clock(), local_tz(), locale(), read_cache())
}

/// Refresh the cached IP location if it's missing or older than [`LOCATION_TTL`].
/// Blocking (one HTTP call); run on the background executor. Best-effort — any
/// failure leaves the previous cache (or none) in place.
pub fn refresh_location_if_stale() {
    let now = unix_now();
    if let Some(c) = read_cache() {
        if !c.country.is_empty() && now.saturating_sub(c.fetched_at) < LOCATION_TTL {
            return;
        }
    }
    if let Some(mut loc) = fetch_location() {
        loc.fetched_at = now;
        write_cache(&loc);
    }
}

// --- local (no network) -----------------------------------------------------

/// The local date and time, e.g. "Tuesday, July 7, 2026, 13:12 CEST (UTC+0200)".
fn local_clock() -> Option<String> {
    let out = Command::new("date").arg("+%A, %B %e, %Y, %H:%M %Z (UTC%z)").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = collapse_ws(&String::from_utf8_lossy(&out.stdout));
    (!s.is_empty()).then_some(s)
}

/// The IANA timezone name (e.g. "Europe/Oslo") from the `/etc/localtime` symlink.
fn local_tz() -> Option<String> {
    let target = std::fs::read_link("/etc/localtime").ok()?;
    tz_from_symlink(&target.to_string_lossy())
}

/// The system locale, e.g. `("en_US", Some("DK"))` — base locale plus an optional
/// region override (macOS `AppleLocale`, falling back to `$LANG`).
fn locale() -> Option<(String, Option<String>)> {
    if let Some(raw) = Command::new("defaults")
        .args(["read", "-g", "AppleLocale"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
    {
        return Some(parse_apple_locale(&raw));
    }
    // Fallback: $LANG like "en_US.UTF-8" → "en_US".
    let lang = std::env::var("LANG").ok()?;
    let base = lang.split('.').next().unwrap_or(&lang).trim();
    (!base.is_empty()).then(|| (base.to_string(), None))
}

// --- IP location (network + cache) ------------------------------------------

/// `ipwho.is` response (HTTPS, keyless). Only the fields we ask for.
#[derive(Deserialize)]
struct IpWho {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    country: String,
    #[serde(default)]
    region: String,
    #[serde(default)]
    city: String,
}

/// Look up the coarse location of the current IP. `None` on any failure.
fn fetch_location() -> Option<Location> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout(Duration::from_secs(8))
        .build();
    let who: IpWho = agent
        .get("https://ipwho.is/?fields=success,country,country_code,region,city")
        .call()
        .ok()?
        .into_json()
        .ok()?;
    if !who.success || who.country.is_empty() {
        return None;
    }
    Some(Location { fetched_at: 0, city: who.city, region: who.region, country: who.country })
}

fn cache_path() -> std::path::PathBuf {
    spotlight_config::cache_dir().join("llm-location.json")
}

fn read_cache() -> Option<Location> {
    let text = std::fs::read_to_string(cache_path()).ok()?;
    let loc: Location = serde_json::from_str(&text).ok()?;
    (!loc.country.is_empty()).then_some(loc)
}

fn write_cache(loc: &Location) {
    let path = cache_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string(loc) {
        let _ = std::fs::write(path, text);
    }
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// --- pure helpers (unit-tested) ---------------------------------------------

/// Collapse runs of whitespace to single spaces and trim (fixes `date`'s
/// space-padded day, e.g. "July  7" → "July 7").
fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Extract the IANA name from a `/etc/localtime` target, i.e. the part after the
/// last `zoneinfo/` (handles both `/usr/share/zoneinfo/...` and macOS's
/// `/var/db/timezone/zoneinfo/...`).
fn tz_from_symlink(target: &str) -> Option<String> {
    let name = target.rsplit_once("zoneinfo/").map(|(_, tz)| tz)?;
    (!name.is_empty()).then(|| name.to_string())
}

/// Split an `AppleLocale` value like "en_US@rg=dkzzzz" into `("en_US", Some("DK"))`.
fn parse_apple_locale(raw: &str) -> (String, Option<String>) {
    let base = raw.split('@').next().unwrap_or(raw).trim().to_string();
    let region = raw
        .split("rg=")
        .nth(1)
        .map(|s| s.chars().take_while(char::is_ascii_alphabetic).take(2).collect::<String>())
        .filter(|s| s.len() == 2)
        .map(|s| s.to_ascii_uppercase());
    (base, region)
}

/// Render the gathered pieces into the prompt block. Empty when nothing is known.
fn format_block(
    clock: Option<String>,
    tz: Option<String>,
    locale: Option<(String, Option<String>)>,
    location: Option<Location>,
) -> String {
    let mut lines = Vec::new();
    if let Some(c) = clock {
        lines.push(format!("- Current date & time: {c}"));
    }
    if let Some(tz) = tz {
        lines.push(format!("- Time zone: {tz}"));
    }
    if let Some((l, region)) = locale {
        match region {
            Some(r) => lines.push(format!("- Locale: {l} (region: {r})")),
            None => lines.push(format!("- Locale: {l}")),
        }
    }
    if let Some(loc) = location {
        let place = [loc.city.as_str(), loc.region.as_str(), loc.country.as_str()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(", ");
        if !place.is_empty() {
            lines.push(format!("- Approximate location (from IP): {place}"));
        }
    }
    if lines.is_empty() {
        return String::new();
    }
    format!(
        "Ambient context (the user did not state these; treat them as reliable facts about the \
current moment and the user's environment):\n{}\n\
Use them for date/time questions and for local or \u{201c}near me\u{201d} requests. They describe \
only the present moment and a coarse location \u{2014} you still have no other live data.",
        lines.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapse_ws_normalizes_padding() {
        assert_eq!(collapse_ws("Tuesday, July  7, 2026,  13:12"), "Tuesday, July 7, 2026, 13:12");
        assert_eq!(collapse_ws("  spaced \n out \t"), "spaced out");
    }

    #[test]
    fn tz_from_symlink_extracts_iana_name() {
        assert_eq!(tz_from_symlink("/var/db/timezone/zoneinfo/Europe/Oslo").as_deref(), Some("Europe/Oslo"));
        assert_eq!(tz_from_symlink("/usr/share/zoneinfo/America/New_York").as_deref(), Some("America/New_York"));
        assert_eq!(tz_from_symlink("/etc/localtime"), None);
        assert_eq!(tz_from_symlink("/var/db/timezone/zoneinfo/"), None);
    }

    #[test]
    fn parse_apple_locale_splits_base_and_region() {
        assert_eq!(parse_apple_locale("en_US@rg=dkzzzz"), ("en_US".into(), Some("DK".into())));
        assert_eq!(parse_apple_locale("nb_NO"), ("nb_NO".into(), None));
        assert_eq!(parse_apple_locale("en_US@calendar=gregorian"), ("en_US".into(), None));
    }

    #[test]
    fn format_block_lists_available_pieces() {
        let block = format_block(
            Some("Tuesday, July 7, 2026, 13:12 CEST (UTC+0200)".into()),
            Some("Europe/Oslo".into()),
            Some(("en_US".into(), Some("DK".into()))),
            Some(Location {
                fetched_at: 0,
                city: "Espergærde".into(),
                region: "Capital Region".into(),
                country: "Denmark".into(),
            }),
        );
        assert!(block.contains("Current date & time: Tuesday, July 7, 2026"));
        assert!(block.contains("Time zone: Europe/Oslo"));
        assert!(block.contains("Locale: en_US (region: DK)"));
        assert!(block.contains("Approximate location (from IP): Espergærde, Capital Region, Denmark"));
    }

    #[test]
    fn format_block_empty_when_nothing_known() {
        assert_eq!(format_block(None, None, None, None), "");
        // A location with no country contributes nothing.
        assert_eq!(format_block(None, None, None, Some(Location::default())), "");
    }
}
