//! The parsed inbox shapes the view renders, plus the small date helpers used
//! to turn the feed's RFC 3339 timestamps into "12m" / "3h" / "Jul 28" labels
//! without pulling in a datetime crate.

use serde::{Deserialize, Serialize};

/// One unread message from the feed. Serializable so the whole inbox can be
/// cached on disk for stale-while-revalidate rendering.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Email {
    /// The feed's `<id>` (stable per message; used for recents).
    pub id: String,
    pub subject: String,
    pub snippet: String,
    pub from_name: String,
    pub from_email: String,
    /// Browser URL that opens this message in Gmail.
    pub link: String,
    /// Unix seconds from the feed's `<issued>`; 0 when unparseable.
    pub timestamp: i64,
    /// Pre-rendered "Jul 28" label, shown for mail older than a day.
    pub date_label: String,
}

impl Email {
    /// Sender display string: name when present, address otherwise.
    pub fn sender(&self) -> &str {
        if self.from_name.trim().is_empty() {
            &self.from_email
        } else {
            &self.from_name
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Inbox {
    /// Total unread count reported by Gmail (`<fullcount>`); the feed itself
    /// only carries the newest messages.
    pub fullcount: u32,
    pub emails: Vec<Email>,
}

/// Age label for a message: "now", "12m", "3h", or the pre-rendered date for
/// anything older than a day. Empty when the timestamp didn't parse.
pub fn age_label(email: &Email, now: i64) -> String {
    if email.timestamp <= 0 {
        return String::new();
    }
    let age = now - email.timestamp;
    if age < 60 {
        "now".to_string()
    } else if age < 3600 {
        format!("{}m", age / 60)
    } else if age < 86_400 {
        format!("{}h", age / 3600)
    } else {
        email.date_label.clone()
    }
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Parse an RFC 3339 timestamp (`2026-08-04T09:13:00Z`, optional fractional
/// seconds, `Z` or `±HH:MM` offset) into `(unix_seconds, "Aug 4")`.
pub fn parse_rfc3339(s: &str) -> Option<(i64, String)> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':'
    {
        return None;
    }
    let num = |range: std::ops::Range<usize>| -> Option<i64> { s.get(range)?.parse().ok() };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) {
        return None;
    }

    // Skip fractional seconds, then read the offset.
    let mut rest = &s[19..];
    if rest.starts_with('.') {
        let end = rest[1..]
            .find(|c: char| !c.is_ascii_digit())
            .map(|i| i + 1)
            .unwrap_or(rest.len());
        rest = &rest[end..];
    }
    let offset = match rest.as_bytes().first() {
        None | Some(b'Z') | Some(b'z') => 0,
        Some(sign @ (b'+' | b'-')) if rest.len() >= 6 && rest.as_bytes()[3] == b':' => {
            let val = rest.get(1..3)?.parse::<i64>().ok()? * 3600
                + rest.get(4..6)?.parse::<i64>().ok()? * 60;
            if *sign == b'-' {
                -val
            } else {
                val
            }
        }
        _ => return None,
    };

    let ts = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + sec - offset;
    let label = format!("{} {}", MONTHS[(mo - 1) as usize], d);
    Some((ts, label))
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil` algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Current unix time in seconds.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_epoch_and_known_dates() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z").unwrap().0, 0);
        // 2000-01-01T00:00:00Z is a well-known fixed point.
        let (ts, label) = parse_rfc3339("2000-01-01T00:00:00Z").unwrap();
        assert_eq!(ts, 946_684_800);
        assert_eq!(label, "Jan 1");
    }

    #[test]
    fn honors_offsets_and_fractions() {
        // 01:00 at +01:00 is midnight UTC.
        assert_eq!(
            parse_rfc3339("2000-01-01T01:00:00+01:00").unwrap().0,
            946_684_800
        );
        assert_eq!(
            parse_rfc3339("2000-01-01T00:00:00.123Z").unwrap().0,
            946_684_800
        );
        assert!(parse_rfc3339("not a date").is_none());
    }

    #[test]
    fn age_labels() {
        let mut email = Email {
            timestamp: 1000,
            date_label: "Jul 28".to_string(),
            ..Default::default()
        };
        assert_eq!(age_label(&email, 1030), "now");
        assert_eq!(age_label(&email, 1000 + 300), "5m");
        assert_eq!(age_label(&email, 1000 + 7200), "2h");
        assert_eq!(age_label(&email, 1000 + 200_000), "Jul 28");
        email.timestamp = 0;
        assert_eq!(age_label(&email, 5000), "");
    }
}
