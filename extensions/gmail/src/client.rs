//! Blocking Gmail IMAP client (app-password auth, strictly read-only).
//!
//! The mailbox is opened with EXAMINE rather than SELECT, so nothing we fetch
//! can ever set `\Seen` (and body fetches use BODY.PEEK[] besides). A Google
//! app password (Google account → Security → 2-Step Verification → App
//! passwords) authenticates without OAuth. Calls block on the network, so
//! callers run them on gpui's background executor (see `view.rs`).
//!
//! The session is kept open behind a mutex and lazily reconnected: on any IMAP
//! error the op is retried once on a fresh connection (Gmail drops idle
//! sessions after a few minutes).

use std::net::TcpStream;
use std::sync::Mutex;

use anyhow::{anyhow, bail, Result};
use mail_parser::MessageParser;

use crate::models::{self, Email, Inbox, MailBody};

const HOST: &str = "imap.gmail.com";
const PORT: u16 = 993;
/// Newest unread messages shown in the list.
const MAX_EMAILS: usize = 30;
/// Where a plain "open my inbox" lands.
pub const INBOX_URL: &str = "https://mail.google.com/mail/";

type Session = imap::Session<native_tls::TlsStream<TcpStream>>;

pub struct GmailClient {
    email: String,
    password: String,
    session: Mutex<Option<Session>>,
}

impl GmailClient {
    pub fn new(email: &str, app_password: &str) -> Self {
        // App passwords are displayed with spaces ("abcd efgh ..."); Google
        // accepts them without, so strip whatever form was pasted.
        let password: String = app_password.chars().filter(|c| !c.is_whitespace()).collect();
        Self {
            email: email.trim().to_string(),
            password,
            session: Mutex::new(None),
        }
    }

    fn connect(&self) -> Result<Session> {
        let tls = native_tls::TlsConnector::new()?;
        let client = imap::connect((HOST, PORT), HOST, &tls)?;
        let mut session = client.login(&self.email, &self.password).map_err(|(e, _)| {
            if matches!(&e, imap::error::Error::Parse(_)) {
                anyhow!("{e}")
            } else {
                anyhow!("Gmail rejected the sign-in — check the address and app password")
            }
        })?;
        // EXAMINE = read-only INBOX: fetching mail here can never mark it read.
        session.examine("INBOX")?;
        Ok(session)
    }

    /// Run `op` on the shared session, reconnecting and retrying once if the
    /// connection has gone stale.
    fn with_session<T>(&self, op: impl Fn(&mut Session) -> imap::error::Result<T>) -> Result<T> {
        let mut guard = self
            .session
            .lock()
            .map_err(|_| anyhow!("Gmail session poisoned"))?;
        if let Some(session) = guard.as_mut() {
            match op(session) {
                Ok(v) => return Ok(v),
                Err(_) => *guard = None, // stale — fall through to reconnect
            }
        }
        let mut session = self.connect()?;
        let result = op(&mut session)?;
        *guard = Some(session);
        Ok(result)
    }

    /// Fetch the unread INBOX list: total count plus headers for the newest
    /// [`MAX_EMAILS`] (no bodies — those come from [`Self::fetch_body`]).
    pub fn fetch_inbox(&self) -> Result<Inbox> {
        let uids = self.with_session(|s| s.uid_search("UNSEEN"))?;
        let fullcount = uids.len() as u32;

        let mut uids: Vec<u32> = uids.into_iter().collect();
        uids.sort_unstable_by(|a, b| b.cmp(a));
        uids.truncate(MAX_EMAILS);
        if uids.is_empty() {
            return Ok(Inbox::default());
        }

        let set = uids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let fetches = self.with_session(|s| s.uid_fetch(&set, "(UID RFC822.HEADER)"))?;

        let mut emails: Vec<Email> = fetches
            .iter()
            .filter_map(|fetch| {
                let uid = fetch.uid?;
                let header = fetch.header()?;
                Some(email_from_header(uid, header))
            })
            .collect();
        emails.sort_unstable_by(|a, b| b.timestamp.cmp(&a.timestamp));
        Ok(Inbox { fullcount, emails })
    }

    /// Fetch and parse one message's body parts. BODY.PEEK on an EXAMINEd
    /// mailbox — doubly read-only.
    pub fn fetch_body(&self, uid: u32) -> Result<MailBody> {
        let fetches = self.with_session(|s| s.uid_fetch(uid.to_string(), "(UID BODY.PEEK[])"))?;
        let Some(raw) = fetches.iter().find_map(|f| f.body()) else {
            bail!("Gmail returned no body for message {uid}");
        };
        body_from_raw(raw, uid)
    }
}

/// Parse a raw RFC 822 message into body parts. The reading pane's text
/// fallback must always have something to show, so when a message carries no
/// usable text/plain part one is synthesized by stripping the HTML.
fn body_from_raw(raw: &[u8], uid: u32) -> Result<MailBody> {
    let Some(message) = MessageParser::default().parse(raw) else {
        bail!("couldn't parse message {uid}");
    };
    let html = message.body_html(0).map(|s| s.into_owned());
    let text = message
        .body_text(0)
        .map(|s| s.into_owned())
        .filter(|t| !t.trim().is_empty())
        .or_else(|| html.as_deref().map(models::strip_html));
    Ok(MailBody { html, text })
}

/// Build a list-row [`Email`] from a raw RFC 822 header block.
fn email_from_header(uid: u32, header: &[u8]) -> Email {
    let mut email = Email {
        uid,
        ..Default::default()
    };
    let Some(message) = MessageParser::default().parse(header) else {
        email.subject = "(unreadable message)".to_string();
        return email;
    };
    email.subject = message.subject().unwrap_or_default().to_string();
    email.message_id = message.message_id().unwrap_or_default().to_string();
    if let Some(addr) = message.from().and_then(|a| a.first()) {
        email.from_name = addr.name().unwrap_or_default().to_string();
        email.from_email = addr.address().unwrap_or_default().to_string();
    }
    if let Some(date) = message.date() {
        email.timestamp = date.to_timestamp();
        email.date_label = models::month_day_label(date.month, date.day);
    }
    email
}

#[cfg(test)]
mod tests {
    use super::{body_from_raw, email_from_header};

    #[test]
    fn html_only_messages_get_synthesized_text() {
        let raw = b"From: a@b.com\r\n\
Subject: hi\r\n\
MIME-Version: 1.0\r\n\
Content-Type: text/html; charset=utf-8\r\n\
\r\n\
<html><body><p>Hello <b>world</b> &amp; friends</p></body></html>\r\n";
        let body = body_from_raw(raw, 1).unwrap();
        assert!(body.html.is_some());
        let text = body.text.expect("text synthesized from html");
        assert!(text.contains("Hello world & friends"), "got: {text:?}");
    }

    #[test]
    fn parses_header_block() {
        let header = b"From: \"Acme Billing\" <billing@acme.com>\r\n\
Subject: =?utf-8?Q?Your_invoice_=E2=80=94_ready?=\r\n\
Date: Tue, 4 Aug 2026 09:13:00 +0000\r\n\
Message-ID: <abc123@mail.acme.com>\r\n\
\r\n";
        let email = email_from_header(7, header);
        assert_eq!(email.uid, 7);
        assert_eq!(email.subject, "Your invoice — ready");
        assert_eq!(email.from_name, "Acme Billing");
        assert_eq!(email.from_email, "billing@acme.com");
        assert_eq!(email.message_id, "abc123@mail.acme.com");
        assert!(email.timestamp > 0);
        assert_eq!(email.date_label, "Aug 4");
    }
}
