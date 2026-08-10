//! Blocking Jira Cloud REST v3 client (Basic auth: email + API token).
//!
//! All methods block on the network, so callers run them on gpui's background
//! executor rather than the UI thread (see `view.rs`).

use std::time::Duration;

use anyhow::Result;
use base64::Engine as _;

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
    /// "No description" while comments came through.
    fn detail_url(&self, key: &str) -> String {
        format!(
            "{}/rest/api/3/issue/{}?fields=summary,status,description,comment\
             &expand=renderedFields",
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
    fn detail_request_asks_for_every_rendered_field_it_reads() {
        let url = JiraClient::new("acme", "me@example.com", "token").detail_url("SO-2522");
        assert!(url.starts_with("https://acme.atlassian.net/rest/api/3/issue/SO-2522?"));
        assert!(url.contains("expand=renderedFields"), "{url}");
        // Every field the reading pane renders must be requested by name —
        // an unlisted field comes back empty rather than erroring.
        for field in ["summary", "status", "description", "comment"] {
            assert!(url.contains(field), "{field} not requested: {url}");
        }
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
