//! Jira REST v3 response shapes and the cleaned-up [`Issue`] the view renders.
//!
//! The raw `*Response`/`Raw*` types mirror the API JSON (and are tolerant of
//! missing fields); [`Issue`] is our own flattened, serializable form used both
//! for rendering and for the on-disk stale-while-revalidate cache.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// ---- raw API shapes -------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct SearchResponse {
    #[serde(default)]
    pub issues: Vec<RawIssue>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawIssue {
    pub key: String,
    #[serde(default)]
    pub fields: Fields,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Fields {
    #[serde(default)]
    pub summary: String,
    pub status: Option<RawStatus>,
    pub priority: Option<RawNamed>,
    pub assignee: Option<RawAssignee>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawStatus {
    #[serde(default)]
    pub name: String,
    #[serde(rename = "statusCategory", default)]
    pub status_category: RawStatusCategory,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawStatusCategory {
    #[serde(rename = "colorName", default)]
    pub color_name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawNamed {
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawAssignee {
    #[serde(rename = "displayName", default)]
    pub display_name: String,
    #[serde(rename = "accountId", default)]
    pub account_id: String,
}

// ---- issue detail (rendered HTML) -----------------------------------------

/// `GET /issue/{key}?expand=renderedFields,names` response. `renderedFields`
/// holds the same fields as `fields` but with Atlassian-Document-Format bodies
/// server-rendered to HTML strings; `names` maps field ids to their display
/// names, which is the only way to label `customfield_10042`.
#[derive(Debug, Clone, Deserialize)]
pub struct IssueDetailResponse {
    #[serde(default)]
    pub fields: DetailFields,
    #[serde(rename = "renderedFields", default)]
    pub rendered: RenderedDetailFields,
    #[serde(default)]
    pub names: HashMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DetailFields {
    #[serde(default)]
    pub summary: String,
    pub status: Option<RawStatus>,
    #[serde(default)]
    pub comment: RawComments,
    pub priority: Option<RawNamed>,
    pub issuetype: Option<RawNamed>,
    pub assignee: Option<RawAssignee>,
    pub reporter: Option<RawAssignee>,
    pub parent: Option<RawParent>,
    #[serde(default)]
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawParent {
    #[serde(default)]
    pub key: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RenderedDetailFields {
    /// Description as HTML (`null` for issues without one).
    pub description: Option<String>,
    #[serde(default)]
    pub comment: RenderedComments,
    /// Every other field Jira rendered, by field id — this is where custom
    /// fields arrive. Values are only strings for fields Jira has a renderer
    /// for; everything else (Rank, Sprint, the dev-panel blob) comes through
    /// as `null` no matter what the raw field holds.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawComments {
    #[serde(default)]
    pub comments: Vec<RawComment>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawComment {
    pub author: Option<RawAssignee>,
    #[serde(default)]
    pub created: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RenderedComments {
    #[serde(default)]
    pub comments: Vec<RenderedComment>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RenderedComment {
    /// Comment body as HTML.
    #[serde(default)]
    pub body: String,
    /// Human-formatted date, e.g. `07/Aug/26 9:15 AM`.
    #[serde(default)]
    pub created: String,
}

/// The cleaned-up detail the reading pane renders.
#[derive(Debug, Clone, Default)]
pub struct IssueDetail {
    pub key: String,
    pub summary: String,
    pub status: String,
    /// Metadata the copy-for-LLM output carries; the reading pane's header
    /// only needs the status.
    pub issue_type: String,
    pub priority: String,
    pub assignee: Option<String>,
    pub reporter: Option<String>,
    pub parent_key: Option<String>,
    pub labels: Vec<String>,
    pub description_html: Option<String>,
    /// Custom fields carrying rendered content ("Why we need this?",
    /// "Definition of Done"), labeled and ordered by display name.
    pub custom_fields: Vec<CustomField>,
    pub comments: Vec<CommentHtml>,
}

/// A custom field Jira rendered to HTML, with its human-readable label.
#[derive(Debug, Clone)]
pub struct CustomField {
    pub name: String,
    pub html: String,
}

#[derive(Debug, Clone)]
pub struct CommentHtml {
    pub author: String,
    pub created: String,
    pub body_html: String,
}

impl IssueDetail {
    pub fn from_raw(key: String, resp: IssueDetailResponse) -> Self {
        // Custom fields worth showing are the ones Jira rendered to a
        // non-empty string. That single rule is enough: the noisy internals
        // (Rank, Sprint, Development) all render as `null` however they're
        // stored, and unfilled fields render as `""`.
        let mut custom_fields: Vec<CustomField> = resp
            .rendered
            .extra
            .iter()
            .filter(|(id, _)| id.starts_with("customfield_"))
            .filter_map(|(id, value)| {
                let html = value.as_str()?.trim();
                (!html.is_empty()).then(|| CustomField {
                    name: resp.names.get(id).cloned().unwrap_or_else(|| id.clone()),
                    html: html.to_string(),
                })
            })
            .collect();
        // The API doesn't report the screen's field order, so sort by label to
        // at least keep it stable between opens.
        custom_fields.sort_by(|a, b| a.name.cmp(&b.name));

        // Rendered comments carry the HTML body and a human-formatted date;
        // authors come from the plain fields, zipped by index.
        let comments = resp
            .rendered
            .comment
            .comments
            .into_iter()
            .enumerate()
            .map(|(i, rendered)| {
                let raw = resp.fields.comment.comments.get(i);
                CommentHtml {
                    author: raw
                        .and_then(|c| c.author.as_ref())
                        .map(|a| a.display_name.clone())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "Unknown".to_string()),
                    created: if rendered.created.is_empty() {
                        raw.map(|c| c.created.clone()).unwrap_or_default()
                    } else {
                        rendered.created
                    },
                    body_html: rendered.body,
                }
            })
            .collect();
        IssueDetail {
            key,
            summary: resp.fields.summary,
            status: resp
                .fields
                .status
                .map(|s| s.name)
                .unwrap_or_default(),
            issue_type: resp.fields.issuetype.map(|t| t.name).unwrap_or_default(),
            priority: resp.fields.priority.map(|p| p.name).unwrap_or_default(),
            assignee: resp
                .fields
                .assignee
                .map(|a| a.display_name)
                .filter(|n| !n.is_empty()),
            reporter: resp
                .fields
                .reporter
                .map(|a| a.display_name)
                .filter(|n| !n.is_empty()),
            parent_key: resp.fields.parent.map(|p| p.key).filter(|k| !k.is_empty()),
            labels: resp.fields.labels,
            description_html: resp
                .rendered
                .description
                .filter(|d| !d.trim().is_empty()),
            custom_fields,
            comments,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Account {
    #[serde(rename = "accountId", default)]
    pub account_id: String,
    #[serde(rename = "displayName", default)]
    pub display_name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TransitionsResponse {
    #[serde(default)]
    pub transitions: Vec<Transition>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Transition {
    pub id: String,
    #[serde(default)]
    pub name: String,
}

// ---- cleaned-up view/cache model ------------------------------------------

/// Broad status buckets, derived from Jira's `statusCategory.colorName`, used to
/// color the status pill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StatusColor {
    Todo,
    InProgress,
    Done,
    Other,
}

impl StatusColor {
    fn from_color_name(name: &str) -> Self {
        match name {
            "blue-gray" | "blue-grey" | "medium-gray" | "medium-grey" => StatusColor::Todo,
            "yellow" | "brown" => StatusColor::InProgress,
            "green" => StatusColor::Done,
            _ => StatusColor::Other,
        }
    }

    /// `(background_rgba, text_rgb)` for the pill.
    pub fn colors(self) -> (u32, u32) {
        match self {
            StatusColor::Todo => (0x8a_93a6_22, 0xc7_cedd),
            StatusColor::InProgress => (0x6e_e7ff_22, 0x6e_e7ff),
            StatusColor::Done => (0x3f_d98a_22, 0x6d_e6a8),
            StatusColor::Other => (0xff_ffff_14, 0xc7_cedd),
        }
    }
}

/// A flattened issue, ready to render and cache.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    pub key: String,
    pub summary: String,
    pub status: String,
    pub status_color: StatusColor,
    pub priority: String,
    pub assignee: Option<String>,
    pub assignee_id: Option<String>,
}

impl Issue {
    pub fn from_raw(raw: RawIssue) -> Self {
        let status = raw.fields.status.clone();
        Issue {
            key: raw.key,
            summary: raw.fields.summary,
            status: status.as_ref().map(|s| s.name.clone()).unwrap_or_default(),
            status_color: status
                .map(|s| StatusColor::from_color_name(&s.status_category.color_name))
                .unwrap_or(StatusColor::Other),
            priority: raw
                .fields
                .priority
                .map(|p| p.name)
                .unwrap_or_default(),
            assignee: raw
                .fields
                .assignee
                .as_ref()
                .map(|a| a.display_name.clone())
                .filter(|s| !s.is_empty()),
            assignee_id: raw.fields.assignee.map(|a| a.account_id),
        }
    }
}

/// Custom status ordering for the issue list. These statuses sort first, in this
/// exact order (matched case-insensitively, since boards aren't consistent about
/// casing); every other status follows, alphabetically by name.
const STATUS_ORDER: [&str; 4] = ["in progress", "fix required", "ready to live", "todo"];

/// Rank a status by [`STATUS_ORDER`]; unlisted statuses share the trailing rank
/// and are then broken alphabetically by the caller.
fn status_rank(status: &str) -> usize {
    let lower = status.to_lowercase();
    STATUS_ORDER
        .iter()
        .position(|s| *s == lower)
        .unwrap_or(STATUS_ORDER.len())
}

/// Sort issues by the custom status order, then alphabetically by status name.
/// Stable, so issues sharing a status keep their incoming (API/cache) order.
pub fn sort_by_status(issues: &mut [Issue]) {
    issues.sort_by(|a, b| {
        status_rank(&a.status)
            .cmp(&status_rank(&b.status))
            .then_with(|| a.status.to_lowercase().cmp(&b.status.to_lowercase()))
    });
}

/// A colored emoji indicating priority. Empty/unknown priorities get a neutral dot.
pub fn priority_icon(name: &str) -> &'static str {
    match name.to_ascii_lowercase().as_str() {
        "highest" => "⏫",
        "high" => "🔼",
        "medium" => "➖",
        "low" => "🔽",
        "lowest" => "⏬",
        _ => "·",
    }
}

/// Up-to-two-letter initials from a display name, for the assignee chip.
pub fn initials(name: &str) -> String {
    let mut chars = name
        .split_whitespace()
        .filter_map(|w| w.chars().next())
        .map(|c| c.to_ascii_uppercase());
    match (chars.next(), chars.next()) {
        (Some(a), Some(b)) => format!("{a}{b}"),
        (Some(a), None) => a.to_string(),
        _ => "?".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_search_response_and_flattens() {
        let json = r#"{
            "issues": [{
                "key": "FE-42",
                "fields": {
                    "summary": "[FE] Build the thing",
                    "status": { "name": "In Progress", "statusCategory": { "colorName": "yellow" } },
                    "priority": { "name": "High" },
                    "assignee": { "displayName": "Jane Doe", "accountId": "acc-1" }
                }
            }]
        }"#;
        let resp: SearchResponse = serde_json::from_str(json).unwrap();
        let issue = Issue::from_raw(resp.issues.into_iter().next().unwrap());
        assert_eq!(issue.key, "FE-42");
        assert_eq!(issue.summary, "[FE] Build the thing");
        assert_eq!(issue.status, "In Progress");
        assert_eq!(issue.status_color, StatusColor::InProgress);
        assert_eq!(issue.priority, "High");
        assert_eq!(issue.assignee.as_deref(), Some("Jane Doe"));
        assert_eq!(issue.assignee_id.as_deref(), Some("acc-1"));
    }

    #[test]
    fn tolerates_missing_fields() {
        let json = r#"{ "issues": [{ "key": "X-1", "fields": {} }] }"#;
        let resp: SearchResponse = serde_json::from_str(json).unwrap();
        let issue = Issue::from_raw(resp.issues.into_iter().next().unwrap());
        assert_eq!(issue.key, "X-1");
        assert!(issue.assignee.is_none());
        assert_eq!(issue.status_color, StatusColor::Other);
    }

    #[test]
    fn priority_and_initials_map() {
        assert_eq!(priority_icon("Highest"), "⏫");
        assert_eq!(priority_icon("unknown"), "·");
        assert_eq!(initials("Jane Doe"), "JD");
        assert_eq!(initials("Cher"), "C");
    }

    #[test]
    fn sorts_by_custom_status_order_then_alpha() {
        let mk = |status: &str| Issue {
            key: status.into(),
            summary: String::new(),
            status: status.into(),
            status_color: StatusColor::Other,
            priority: String::new(),
            assignee: None,
            assignee_id: None,
        };
        // Includes mixed casing and statuses outside the custom order.
        let mut issues = vec![
            mk("Done"),
            mk("Todo"),
            mk("Ready to live"),
            mk("Backlog"),
            mk("FIX REQUIRED"),
            mk("in progress"),
        ];
        sort_by_status(&mut issues);
        let order: Vec<&str> = issues.iter().map(|i| i.status.as_str()).collect();
        assert_eq!(
            order,
            vec![
                "in progress",
                "FIX REQUIRED",
                "Ready to live",
                "Todo",
                "Backlog", // then alphabetical: Backlog before Done
                "Done",
            ]
        );
    }

    #[test]
    fn parses_issue_detail() {
        let json = r#"{
            "key": "FE-42",
            "fields": {
                "summary": "Build the thing",
                "status": { "name": "In Progress", "statusCategory": { "colorName": "yellow" } },
                "comment": { "comments": [
                    { "author": { "displayName": "Jane <Dev>", "accountId": "a1" }, "created": "2026-08-07T09:15:00.000+0200" }
                ]}
            },
            "renderedFields": {
                "description": "<p>Do the <b>thing</b></p>",
                "comment": { "comments": [
                    { "body": "<p>On it.</p>", "created": "07/Aug/26 9:15 AM" }
                ]}
            }
        }"#;
        let resp: IssueDetailResponse = serde_json::from_str(json).unwrap();
        let detail = IssueDetail::from_raw("FE-42".into(), resp);
        assert_eq!(detail.summary, "Build the thing");
        assert_eq!(detail.status, "In Progress");
        assert_eq!(detail.description_html.as_deref(), Some("<p>Do the <b>thing</b></p>"));
        // The rendered comment's HTML body and human date, zipped with the
        // author from the plain fields.
        assert_eq!(detail.comments.len(), 1);
        assert_eq!(detail.comments[0].author, "Jane <Dev>");
        assert_eq!(detail.comments[0].created, "07/Aug/26 9:15 AM");
        assert_eq!(detail.comments[0].body_html, "<p>On it.</p>");
    }

    #[test]
    fn keeps_rendered_custom_fields_and_drops_the_noise() {
        // The exact shape a real issue returns: two rich-text fields with
        // content, an unfilled one, and the internals — Rank and Sprint hold
        // values but Jira renders them as null, and the dev-panel blob too.
        let json = r#"{
            "fields": { "summary": "s" },
            "renderedFields": {
                "description": "<p>d</p>",
                "customfield_10101": "<p>When adding markets to PROD…</p>",
                "customfield_10102": "<ul><li>Mapped ID allows custom numbers</li></ul>",
                "customfield_10103": "",
                "customfield_10014": null,
                "customfield_10020": null,
                "customfield_10000": null
            },
            "names": {
                "customfield_10101": "Why we need this ?",
                "customfield_10102": "Definition of Done",
                "customfield_10103": "QA requirements",
                "customfield_10014": "Rank",
                "customfield_10020": "Sprint",
                "customfield_10000": "Development"
            }
        }"#;
        let resp: IssueDetailResponse = serde_json::from_str(json).unwrap();
        let detail = IssueDetail::from_raw("FE-42".into(), resp);
        let labels: Vec<&str> = detail.custom_fields.iter().map(|f| f.name.as_str()).collect();
        // Only the two with rendered content, ordered by label.
        assert_eq!(labels, vec!["Definition of Done", "Why we need this ?"]);
        assert!(detail.custom_fields[0].html.starts_with("<ul>"));
        // The description still parses alongside them.
        assert_eq!(detail.description_html.as_deref(), Some("<p>d</p>"));
    }

    #[test]
    fn custom_fields_fall_back_to_their_id_when_unnamed() {
        // No `names` map (an older expand, or a field removed mid-flight):
        // better to show the raw id than to drop the content.
        let json = r#"{
            "renderedFields": { "customfield_10101": "<p>x</p>" }
        }"#;
        let resp: IssueDetailResponse = serde_json::from_str(json).unwrap();
        let detail = IssueDetail::from_raw("FE-42".into(), resp);
        assert_eq!(detail.custom_fields.len(), 1);
        assert_eq!(detail.custom_fields[0].name, "customfield_10101");
    }

    #[test]
    fn blank_description_reads_as_absent() {
        let json = r#"{ "key": "X-1", "fields": {}, "renderedFields": { "description": "  " } }"#;
        let resp: IssueDetailResponse = serde_json::from_str(json).unwrap();
        let detail = IssueDetail::from_raw("X-1".into(), resp);
        assert!(detail.description_html.is_none());
        assert!(detail.comments.is_empty());
    }

    #[test]
    fn issue_round_trips_through_cache_json() {
        let issue = Issue {
            key: "A-1".into(),
            summary: "s".into(),
            status: "Done".into(),
            status_color: StatusColor::Done,
            priority: "Low".into(),
            assignee: Some("Me".into()),
            assignee_id: Some("id".into()),
        };
        let json = serde_json::to_string(&issue).unwrap();
        let back: Issue = serde_json::from_str(&json).unwrap();
        assert_eq!(back.key, "A-1");
        assert_eq!(back.status_color, StatusColor::Done);
    }
}
