//! Blocking Jira Cloud REST v3 client (Basic auth: email + API token).
//!
//! All methods block on the network, so callers run them on gpui's background
//! executor rather than the UI thread (see `view.rs`).

use std::io::Read as _;
use std::time::Duration;

use anyhow::{bail, Result};
use base64::Engine as _;

/// Cap on a single fetched attachment, so a huge upload can't stall a copy.
const MAX_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;

use crate::models::{
    Account, Issue, IssueDetail, IssueDetailResponse, SearchResponse, Transition,
    TransitionsResponse,
};

#[derive(Clone)]
pub struct JiraClient {
    /// e.g. `https://acme.atlassian.net`
    base_url: String,
    /// Full `Authorization` header value (`Basic <base64>`).
    auth: String,
    agent: ureq::Agent,
}

impl JiraClient {
    pub fn new(site: &str, email: &str, token: &str) -> Self {
        let creds =
            base64::engine::general_purpose::STANDARD.encode(format!("{email}:{token}"));
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(20))
            .build();
        Self {
            base_url: normalize_base(site),
            auth: format!("Basic {creds}"),
            agent,
        }
    }

    /// The browser URL for an issue, e.g. `https://acme.atlassian.net/browse/FE-1`.
    pub fn browse_url(&self, key: &str) -> String {
        format!("{}/browse/{}", self.base_url, key)
    }

    /// Run a JQL search and return flattened issues.
    ///
    /// Uses the enhanced search endpoint `/rest/api/3/search/jql`. The classic
    /// `/rest/api/3/search` was deprecated and now returns HTTP 410 Gone.
    pub fn search(&self, jql: &str, max: u32) -> Result<Vec<Issue>> {
        let body = serde_json::json!({
            "jql": jql,
            "maxResults": max,
            "fields": ["summary", "status", "priority", "assignee"],
        });
        let resp: SearchResponse = self
            .agent
            .post(&format!("{}/rest/api/3/search/jql", self.base_url))
            .set("Authorization", &self.auth)
            .set("Accept", "application/json")
            .send_json(body)?
            .into_json()?;
        Ok(resp.issues.into_iter().map(Issue::from_raw).collect())
    }

    /// The site base URL, e.g. `https://acme.atlassian.net` (no trailing slash).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The `Authorization` header value, for the HTML renderer's same-origin
    /// image fetches (issue attachments and avatars require auth).
    pub fn auth_header(&self) -> &str {
        &self.auth
    }

    /// The request behind [`issue_detail`](Self::issue_detail).
    ///
    /// `renderedFields` only carries entries for fields named in `fields`, so
    /// anything missing from that list comes back empty however the `expand`
    /// is written — which is exactly how the description silently rendered as
    /// "No description" while comments came through. `*all` sidesteps that
    /// class of bug entirely and is what makes custom fields available;
    /// `names` is what lets us label `customfield_10042`.
    fn detail_url(&self, key: &str) -> String {
        format!(
            "{}/rest/api/3/issue/{}?fields=*all&expand=renderedFields,names",
            self.base_url, key
        )
    }

    /// One issue with its description and comments server-rendered to HTML,
    /// for the in-app reading pane.
    pub fn issue_detail(&self, key: &str) -> Result<IssueDetail> {
        let resp: IssueDetailResponse = self
            .agent
            .get(&self.detail_url(key))
            .set("Authorization", &self.auth)
            .set("Accept", "application/json")
            .call()?
            .into_json()?;
        Ok(IssueDetail::from_raw(key.to_string(), resp))
    }

    /// Fetch an inline attachment as PNG bytes, ready for the clipboard.
    ///
    /// Only URLs on the configured site are fetched, and only those carry the
    /// credential — an issue body can embed an image from anywhere. Jira
    /// redirects attachment content to its media CDN; ureq follows that but
    /// drops the auth header off-host, which is what we want (the redirect is
    /// pre-signed). Whatever format comes back is re-encoded, since the
    /// clipboard is handed PNG.
    pub fn fetch_attachment_png(&self, url: &str) -> Result<Vec<u8>> {
        if !url.starts_with(&self.base_url) {
            bail!("attachment is not on {}", self.base_url);
        }
        let resp = self
            .agent
            .get(url)
            .set("Authorization", &self.auth)
            .call()?;
        let mut bytes = Vec::new();
        resp.into_reader()
            .take(MAX_ATTACHMENT_BYTES)
            .read_to_end(&mut bytes)?;

        let image = image::load_from_memory(&bytes)?;
        let mut png = Vec::new();
        image.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)?;
        Ok(png)
    }

    /// The authenticated user (for "Assign to me").
    pub fn myself(&self) -> Result<Account> {
        let account = self
            .agent
            .get(&format!("{}/rest/api/3/myself", self.base_url))
            .set("Authorization", &self.auth)
            .set("Accept", "application/json")
            .call()?
            .into_json()?;
        Ok(account)
    }

    /// Available status transitions for an issue.
    pub fn transitions(&self, key: &str) -> Result<Vec<Transition>> {
        let resp: TransitionsResponse = self
            .agent
            .get(&format!(
                "{}/rest/api/3/issue/{}/transitions",
                self.base_url, key
            ))
            .set("Authorization", &self.auth)
            .set("Accept", "application/json")
            .call()?
            .into_json()?;
        Ok(resp.transitions)
    }

    /// Apply a status transition.
    pub fn transition(&self, key: &str, transition_id: &str) -> Result<()> {
        self.agent
            .post(&format!(
                "{}/rest/api/3/issue/{}/transitions",
                self.base_url, key
            ))
            .set("Authorization", &self.auth)
            .set("Accept", "application/json")
            .send_json(serde_json::json!({ "transition": { "id": transition_id } }))?;
        Ok(())
    }

    /// Assign an issue to the given account id.
    pub fn assign(&self, key: &str, account_id: &str) -> Result<()> {
        self.agent
            .put(&format!(
                "{}/rest/api/3/issue/{}/assignee",
                self.base_url, key
            ))
            .set("Authorization", &self.auth)
            .set("Accept", "application/json")
            .send_json(serde_json::json!({ "accountId": account_id }))?;
        Ok(())
    }
}

/// Browser URL for an issue from a configured site (no client needed).
pub fn browse_url(site: &str, key: &str) -> String {
    format!("{}/browse/{}", normalize_base(site), key)
}

/// Accept a bare site name (`acme`), a host (`acme.atlassian.net`), or a full
/// URL, and produce a canonical `https://…` base with no trailing slash.
fn normalize_base(site: &str) -> String {
    let s = site.trim().trim_end_matches('/');
    if s.starts_with("http://") || s.starts_with("https://") {
        s.to_string()
    } else if s.contains('.') {
        format!("https://{s}")
    } else {
        format!("https://{s}.atlassian.net")
    }
}

#[cfg(test)]
mod tests {
    use super::{normalize_base, JiraClient};

    #[test]
    fn detail_request_asks_for_every_field_the_pane_reads() {
        let url = JiraClient::new("acme", "me@example.com", "token").detail_url("SO-2522");
        assert!(url.starts_with("https://acme.atlassian.net/rest/api/3/issue/SO-2522?"));
        // An unrequested field comes back empty rather than erroring, which is
        // how the description silently vanished — `*all` is what prevents a
        // repeat, and it's the only way to see custom fields at all.
        assert!(url.contains("fields=*all"), "{url}");
        assert!(url.contains("expand=renderedFields"), "{url}");
        // Without `names`, custom fields have no label but their raw id.
        assert!(url.contains("names"), "{url}");
    }

    #[test]
    fn normalizes_site_forms() {
        assert_eq!(normalize_base("acme"), "https://acme.atlassian.net");
        assert_eq!(
            normalize_base("acme.atlassian.net"),
            "https://acme.atlassian.net"
        );
        assert_eq!(
            normalize_base("https://acme.atlassian.net/"),
            "https://acme.atlassian.net"
        );
    }
}
