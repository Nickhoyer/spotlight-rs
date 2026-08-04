//! Blocking Gmail atom-feed client (Basic auth: address + app password).
//!
//! Gmail exposes unread inbox mail as an atom feed at
//! `https://mail.google.com/mail/feed/atom`. It accepts a Google app password
//! (Google account → Security → 2-Step Verification → App passwords), which
//! sidesteps OAuth entirely. Calls block on the network, so callers run them on
//! gpui's background executor (see `view.rs`).

use std::time::Duration;

use anyhow::{bail, Result};
use base64::Engine as _;
use quick_xml::events::Event;
use quick_xml::Reader;

use crate::models::{self, Email, Inbox};

const FEED_URL: &str = "https://mail.google.com/mail/feed/atom";
/// Where a plain "open my inbox" lands.
pub const INBOX_URL: &str = "https://mail.google.com/mail/";

#[derive(Clone)]
pub struct GmailClient {
    /// Full `Authorization` header value (`Basic <base64>`).
    auth: String,
    agent: ureq::Agent,
}

impl GmailClient {
    pub fn new(email: &str, app_password: &str) -> Self {
        // App passwords are displayed with spaces ("abcd efgh ..."); Google
        // accepts them without, so strip whatever form was pasted.
        let password: String = app_password.chars().filter(|c| !c.is_whitespace()).collect();
        let creds =
            base64::engine::general_purpose::STANDARD.encode(format!("{}:{password}", email.trim()));
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(20))
            .build();
        Self {
            auth: format!("Basic {creds}"),
            agent,
        }
    }

    /// Fetch and parse the unread-inbox feed.
    pub fn fetch_inbox(&self) -> Result<Inbox> {
        let resp = self
            .agent
            .get(FEED_URL)
            .set("Authorization", &self.auth)
            .call();
        match resp {
            Ok(r) => parse_feed(&r.into_string()?),
            Err(ureq::Error::Status(401, _)) => {
                bail!("Gmail rejected the sign-in — check the address and app password")
            }
            Err(e) => Err(e.into()),
        }
    }
}

/// Parse the atom feed. Gmail uses Atom 0.3: a `<fullcount>` total plus one
/// `<entry>` per unread message with `title`/`summary`/`link`/`issued`/`author`.
pub fn parse_feed(xml: &str) -> Result<Inbox> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut inbox = Inbox::default();
    let mut entry: Option<Email> = None;
    let mut in_author = false;
    // Name of the element whose text content we're inside.
    let mut current: Option<String> = None;

    loop {
        match reader.read_event()? {
            Event::Eof => break,
            Event::Start(e) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                match name.as_str() {
                    "entry" => entry = Some(Email::default()),
                    "author" => in_author = true,
                    "link" => read_link(&e, entry.as_mut())?,
                    _ => {}
                }
                current = Some(name);
            }
            Event::Empty(e) => {
                if e.name().as_ref() == b"link" {
                    read_link(&e, entry.as_mut())?;
                }
            }
            Event::Text(t) => {
                let text = t.unescape()?.into_owned();
                let Some(tag) = current.as_deref() else {
                    continue;
                };
                match (&mut entry, tag) {
                    (None, "fullcount") => inbox.fullcount = text.trim().parse().unwrap_or(0),
                    (Some(email), "title") => email.subject = text,
                    (Some(email), "summary") => email.snippet = text,
                    (Some(email), "id") => email.id = text,
                    (Some(email), "issued") => {
                        if let Some((ts, label)) = models::parse_rfc3339(&text) {
                            email.timestamp = ts;
                            email.date_label = label;
                        }
                    }
                    (Some(email), "name") if in_author => email.from_name = text,
                    (Some(email), "email") if in_author => email.from_email = text,
                    _ => {}
                }
            }
            Event::End(e) => {
                match e.name().as_ref() {
                    b"entry" => {
                        if let Some(email) = entry.take() {
                            inbox.emails.push(email);
                        }
                    }
                    b"author" => in_author = false,
                    _ => {}
                }
                current = None;
            }
            _ => {}
        }
    }
    Ok(inbox)
}

/// Pull `href` off a `<link .../>` into the current entry's link.
fn read_link(e: &quick_xml::events::BytesStart, entry: Option<&mut Email>) -> Result<()> {
    let Some(email) = entry else {
        return Ok(());
    };
    for attr in e.attributes() {
        let attr = attr?;
        if attr.key.as_ref() == b"href" {
            email.link = attr.unescape_value()?.into_owned();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_feed;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed version="0.3" xmlns="http://purl.org/atom/ns#">
  <title>Gmail - Inbox for nick@example.com</title>
  <tagline>New messages in your Gmail Inbox</tagline>
  <fullcount>2</fullcount>
  <link rel="alternate" href="https://mail.google.com/mail" type="text/html"/>
  <modified>2026-08-04T09:30:00Z</modified>
  <entry>
    <title>Your invoice &amp; receipt</title>
    <summary>Thanks for your purchase — here&#39;s the receipt.</summary>
    <link rel="alternate" href="https://mail.google.com/mail?account_id=nick%40example.com&amp;message_id=18abc&amp;view=conv&amp;extsrc=atom" type="text/html"/>
    <modified>2026-08-04T09:13:00Z</modified>
    <issued>2026-08-04T09:13:00Z</issued>
    <id>tag:gmail.google.com,2004:1234567890</id>
    <author><name>Acme Billing</name><email>billing@acme.com</email></author>
  </entry>
  <entry>
    <title></title>
    <summary>No subject here</summary>
    <link rel="alternate" href="https://mail.google.com/mail?message_id=18def" type="text/html"/>
    <issued>2026-08-03T20:00:00Z</issued>
    <id>tag:gmail.google.com,2004:987</id>
    <author><name>Someone</name><email>s@example.com</email></author>
  </entry>
</feed>"#;

    #[test]
    fn parses_gmail_feed() {
        let inbox = parse_feed(SAMPLE).unwrap();
        assert_eq!(inbox.fullcount, 2);
        assert_eq!(inbox.emails.len(), 2);

        let first = &inbox.emails[0];
        assert_eq!(first.subject, "Your invoice & receipt");
        assert_eq!(first.snippet, "Thanks for your purchase — here's the receipt.");
        assert_eq!(first.from_name, "Acme Billing");
        assert_eq!(first.from_email, "billing@acme.com");
        assert_eq!(first.id, "tag:gmail.google.com,2004:1234567890");
        // Entity-encoded ampersands in the href must come back decoded.
        assert!(first.link.contains("message_id=18abc&view=conv"));
        assert!(first.timestamp > 0);
        assert_eq!(first.date_label, "Aug 4");

        // The feed-level <title>/<link> must not bleed into entries.
        let second = &inbox.emails[1];
        assert_eq!(second.subject, "");
        assert_eq!(second.from_name, "Someone");
    }
}
