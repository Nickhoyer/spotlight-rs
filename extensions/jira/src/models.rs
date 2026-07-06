//! Jira REST v3 response shapes and the cleaned-up [`Issue`] the view renders.
//!
//! The raw `*Response`/`Raw*` types mirror the API JSON (and are tolerant of
//! missing fields); [`Issue`] is our own flattened, serializable form used both
//! for rendering and for the on-disk stale-while-revalidate cache.

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
