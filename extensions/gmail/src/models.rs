//! The inbox shapes the view renders, plus small date/label helpers used to
//! turn message timestamps into "12m" / "3h" / "Jul 28" labels without pulling
//! in a datetime crate.

use serde::{Deserialize, Serialize};

/// One unread message's list-row data. Serializable so the inbox list (not the
/// bodies) can be cached on disk for stale-while-revalidate rendering.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Email {
    /// IMAP UID within INBOX (uidvalidity changes are handled by refetching).
    pub uid: u32,
    pub subject: String,
    /// First line-ish of the body; filled in by the background body prefetch.
    pub snippet: String,
    pub from_name: String,
    pub from_email: String,
    /// RFC 822 Message-ID with angle brackets stripped; keys the Gmail
    /// permalink search.
    pub message_id: String,
    /// Unix seconds from the Date header; 0 when missing/unparseable.
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

    /// Browser URL for this message: Gmail's rfc822msgid search, which lands on
    /// the exact message regardless of label or mailbox.
    pub fn gmail_url(&self) -> String {
        if self.message_id.is_empty() {
            return crate::client::INBOX_URL.to_string();
        }
        format!(
            "https://mail.google.com/mail/#search/rfc822msgid%3A{}",
            percent_encode(&self.message_id)
        )
    }
}

/// A message's parsed body parts, kept in memory only (never written to disk).
#[derive(Debug, Clone, Default)]
pub struct MailBody {
    pub html: Option<String>,
    pub text: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Inbox {
    /// Total unread count in INBOX (the list itself is capped).
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

/// "Aug 4"-style label from 1-based month + day (0 / out-of-range → empty).
pub fn month_day_label(month: u8, day: u8) -> String {
    match MONTHS.get((month as usize).wrapping_sub(1)) {
        Some(name) => format!("{name} {day}"),
        None => String::new(),
    }
}

/// Collapse whitespace runs and truncate to a list-row snippet.
pub fn snippet_of(text: &str) -> String {
    let mut out = String::with_capacity(160);
    let mut last_space = true;
    for c in text.chars() {
        if c.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(c);
            last_space = false;
        }
        if out.len() >= 150 {
            break;
        }
    }
    out.trim().to_string()
}

/// Minimal RFC 3986 percent-encoding (unreserved characters pass through).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
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

    #[test]
    fn month_day_labels() {
        assert_eq!(month_day_label(8, 4), "Aug 4");
        assert_eq!(month_day_label(1, 31), "Jan 31");
        assert_eq!(month_day_label(0, 4), "");
        assert_eq!(month_day_label(13, 4), "");
    }

    #[test]
    fn snippets_collapse_whitespace() {
        assert_eq!(snippet_of("  Hi\n\n  there\tworld  "), "Hi there world");
        let long = "x".repeat(400);
        assert!(snippet_of(&long).len() <= 151);
    }

    #[test]
    fn gmail_urls_encode_message_ids() {
        let email = Email {
            message_id: "abc+def@mail.example.com".to_string(),
            ..Default::default()
        };
        assert_eq!(
            email.gmail_url(),
            "https://mail.google.com/mail/#search/rfc822msgid%3Aabc%2Bdef%40mail.example.com"
        );
        assert_eq!(Email::default().gmail_url(), crate::client::INBOX_URL);
    }
}
