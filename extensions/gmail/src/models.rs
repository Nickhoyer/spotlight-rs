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
    /// Whether the message lacks `\Seen`. Defaults to true on deserialize:
    /// older caches only ever held unread mail.
    #[serde(default = "default_unread")]
    pub unread: bool,
}

fn default_unread() -> bool {
    true
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

/// Crude but dependable HTML→text conversion for the reading fallback when a
/// message has no text/plain part (and its HTML can't be or isn't rendered):
/// drops tags (and `<style>`/`<script>` contents), decodes the common
/// entities, and breaks lines at block-level closers.
pub fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut chars = html.char_indices().peekable();
    let mut skip_until: Option<&str> = None;
    while let Some((i, c)) = chars.next() {
        let rest = &html[i..];
        if let Some(closer) = skip_until {
            if rest.len() >= closer.len() && rest[..closer.len()].eq_ignore_ascii_case(closer) {
                skip_until = None;
                // Consume the rest of the closer tag too.
                for _ in 0..closer.len() - 1 {
                    chars.next();
                }
            }
            continue;
        }
        match c {
            '<' => {
                let lower = rest.get(..7).unwrap_or("").to_ascii_lowercase();
                if lower.starts_with("<style") {
                    skip_until = Some("</style>");
                } else if lower.starts_with("<script") {
                    skip_until = Some("</script>");
                }
                // Break lines at block-ish boundaries, and keep table cells
                // from running together.
                if lower.starts_with("<br") || lower.starts_with("</p") || lower.starts_with("</div")
                    || lower.starts_with("</tr") || lower.starts_with("</h") || lower.starts_with("</li")
                {
                    out.push('\n');
                } else if lower.starts_with("</td") || lower.starts_with("</th") {
                    out.push(' ');
                }
                // Skip to the end of the tag.
                for (_, tc) in chars.by_ref() {
                    if tc == '>' {
                        break;
                    }
                }
            }
            '&' => match decode_entity(rest) {
                Some((len, decoded)) => {
                    out.push(decoded);
                    for _ in 0..len - 1 {
                        chars.next();
                    }
                }
                None => out.push('&'),
            },
            _ => out.push(c),
        }
    }
    // Collapse the whitespace soup HTML leaves behind: spaces within lines,
    // runs of blank lines between them.
    let mut lines: Vec<String> = Vec::new();
    for line in out.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            if !lines.last().map(String::is_empty).unwrap_or(true) {
                lines.push(String::new());
            }
        } else {
            lines.push(line);
        }
    }
    while lines.last().map(String::is_empty).unwrap_or(false) {
        lines.pop();
    }
    lines.join("\n")
}

/// Decode one HTML entity at the start of `s`: `(byte_len, char)`. Handles the
/// named entities emails actually use plus numeric (`&#8212;` / `&#x2014;`).
fn decode_entity(s: &str) -> Option<(usize, char)> {
    const NAMED: &[(&str, char)] = &[
        ("&amp;", '&'), ("&lt;", '<'), ("&gt;", '>'), ("&quot;", '"'),
        ("&apos;", '\''), ("&nbsp;", ' '), ("&mdash;", '—'), ("&ndash;", '–'),
        ("&lsquo;", '\u{2018}'), ("&rsquo;", '\u{2019}'), ("&ldquo;", '“'),
        ("&rdquo;", '”'), ("&hellip;", '…'), ("&copy;", '©'), ("&reg;", '®'),
        ("&trade;", '™'), ("&middot;", '·'), ("&bull;", '•'),
    ];
    if let Some((name, ch)) = NAMED.iter().find(|(name, _)| s.starts_with(name)) {
        return Some((name.len(), *ch));
    }
    // Numeric: &#8212; or &#x2014; (bounded so a stray '&#' can't run away).
    let body = s.strip_prefix("&#")?;
    let end = body.char_indices().take(8).find(|(_, c)| *c == ';')?.0;
    let digits = &body[..end];
    let value = match digits.strip_prefix(['x', 'X']) {
        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
        None => digits.parse().ok()?,
    };
    Some((2 + end + 1, char::from_u32(value)?))
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
    fn strips_html_to_readable_text() {
        let html = r#"<html><head><style>body { color: red; }</style></head>
<body><div>Hi Nick,</div><p>Invoice <b>#4821</b> &amp; receipt &mdash; total <span>$29.00</span></p>
<script>tracking();</script>
<table><tr><td>Total</td><td>$29.00</td></tr></table></body></html>"#;
        let text = strip_html(html);
        assert!(text.contains("Hi Nick,"), "got: {text:?}");
        assert!(text.contains("Invoice #4821 & receipt — total"), "got: {text:?}");
        assert!(text.contains("Total $29.00"), "got: {text:?}");
        assert!(!text.contains("color"), "style leaked: {text:?}");
        assert!(!text.contains("tracking"), "script leaked: {text:?}");
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
