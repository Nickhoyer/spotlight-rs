//! Rendering a whole ticket as Markdown for handing to an LLM, with people
//! pseudonymized.
//!
//! Names are replaced with `User1`, `User2`, … — consistently, so the thread
//! still reads as a conversation: the person who wrote comment 1 and the
//! person @-mentioned in comment 2 come out as the same `User3`.
//!
//! Substitution happens in two passes, because names reach the text two ways.
//! Mentions are structured (an anchor carrying an account id) and are swapped
//! during conversion; but a name can also be typed as plain prose ("as Anton
//! said"), so a final sweep replaces any remaining occurrences of the names we
//! know. Numbering follows reading order — reporter, assignee, then comment
//! authors — so it's stable between opens rather than depending on which
//! mention happened to be converted first.

use crate::markdown::html_to_markdown;
use crate::models::IssueDetail;

/// Assigns and remembers a pseudonym per person.
#[derive(Default)]
pub struct People {
    /// Real name → pseudonym, in assignment order.
    known: Vec<(String, String)>,
}

impl People {
    /// The pseudonym for `name`, assigning the next one if new.
    pub fn pseudonym(&mut self, name: &str) -> String {
        let name = name.trim();
        if name.is_empty() {
            return String::new();
        }
        if let Some((_, alias)) = self.known.iter().find(|(n, _)| n == name) {
            return alias.clone();
        }
        let alias = format!("User{}", self.known.len() + 1);
        self.known.push((name.to_string(), alias.clone()));
        alias
    }

    fn is_empty(&self) -> bool {
        self.known.is_empty()
    }

    /// Replace any remaining plain-text occurrences of known names. Longest
    /// first, so "Anton Kahwaji" is consumed before a bare "Anton" could be.
    fn scrub(&self, text: &str) -> String {
        let mut names: Vec<&(String, String)> = self.known.iter().collect();
        names.sort_by_key(|(name, _)| std::cmp::Reverse(name.len()));
        let mut out = text.to_string();
        for (name, alias) in names {
            if out.contains(name.as_str()) {
                out = out.replace(name.as_str(), alias);
            }
            // Also catch a bare first name, which is how people usually write
            // to each other in comments.
            if let Some(first) = name.split_whitespace().next().filter(|f| f.len() > 2) {
                if out.contains(first) {
                    out = out.replace(first, alias);
                }
            }
        }
        out
    }
}

/// The whole ticket as Markdown, ready to paste into a chat.
///
/// Deliberately carries no link back to the Jira site: pseudonymizing people
/// while leaving the instance URL in place would give the names straight back
/// to anyone who can reach it.
pub fn issue_markdown(detail: &IssueDetail) -> String {
    let mut people = People::default();
    // Seed in reading order so the numbering is meaningful and stable.
    let reporter = detail.reporter.as_ref().map(|n| people.pseudonym(n));
    let assignee = detail.assignee.as_ref().map(|n| people.pseudonym(n));
    for comment in &detail.comments {
        people.pseudonym(&comment.author);
    }

    let mut out = String::new();
    let title = if detail.summary.is_empty() {
        detail.key.clone()
    } else {
        format!("{} — {}", detail.key, detail.summary)
    };
    out.push_str(&format!("# {title}\n\n"));

    let mut meta: Vec<String> = Vec::new();
    let mut add = |label: &str, value: &str| {
        if !value.is_empty() {
            meta.push(format!("- **{label}:** {value}"));
        }
    };
    add("Type", &detail.issue_type);
    add("Status", &detail.status);
    add("Priority", &detail.priority);
    add("Assignee", assignee.as_deref().unwrap_or(""));
    add("Reporter", reporter.as_deref().unwrap_or(""));
    add("Parent", detail.parent_key.as_deref().unwrap_or(""));
    add("Labels", &detail.labels.join(", "));
    out.push_str(&meta.join("\n"));
    out.push_str("\n\n## Description\n\n");
    match &detail.description_html {
        Some(html) => out.push_str(&html_to_markdown(html, &mut |n| people.pseudonym(n))),
        None => out.push_str("_No description._"),
    }

    for field in &detail.custom_fields {
        out.push_str(&format!("\n\n## {}\n\n", field.name.trim()));
        out.push_str(&html_to_markdown(&field.html, &mut |n| people.pseudonym(n)));
    }

    if !detail.comments.is_empty() {
        out.push_str(&format!("\n\n## Comments ({})\n", detail.comments.len()));
        for (i, comment) in detail.comments.iter().enumerate() {
            let author = people.pseudonym(&comment.author);
            let when = comment.created.trim();
            out.push_str(&format!("\n### Comment {} — {author}", i + 1));
            if !when.is_empty() {
                out.push_str(&format!(" ({when})"));
            }
            out.push_str("\n\n");
            out.push_str(&html_to_markdown(&comment.body_html, &mut |n| {
                people.pseudonym(n)
            }));
            out.push('\n');
        }
    }

    if !people.is_empty() {
        out.push_str(
            "\n\n---\n_Names in this ticket are replaced with User1, User2, … \
             consistently; the same pseudonym always means the same person._",
        );
    }

    // Sweep prose mentions the structured pass couldn't see.
    let out = people.scrub(&out);
    format!("{}\n", out.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{CommentHtml, CustomField};

    fn ticket() -> IssueDetail {
        IssueDetail {
            key: "SO-2522".into(),
            summary: "Allow custom Mapped ID".into(),
            status: "To Do".into(),
            issue_type: "Story".into(),
            priority: "Medium".into(),
            assignee: Some("Nickolas Hoyer".into()),
            reporter: Some("Anton Kahwaji".into()),
            parent_key: Some("SO-2439".into()),
            labels: vec!["frontend".into()],
            description_html: "<p>Mapped ID needs a Custom option.</p>".to_string().into(),
            custom_fields: vec![CustomField {
                name: "Definition of Done".into(),
                html: "<ul><li>Custom ids allowed</li></ul>".into(),
            }],
            comments: vec![
                CommentHtml {
                    author: "Ihor Karaianidi".into(),
                    created: "03/Jul/26 10:48 PM".into(),
                    body_html: "<p>Do we validate duplicates?</p>".into(),
                },
                CommentHtml {
                    author: "Anton Kahwaji".into(),
                    created: "03/Jul/26 11:06 PM".into(),
                    // A real mention anchor, plus the same person in prose.
                    body_html: r#"<p><a href="https://x.atlassian.net/secure/ViewProfile.jspa?accountId=712020" data-account-id="712020">Ihor Karaianidi</a> good point — Ihor, I checked.</p>"#.into(),
                },
            ],
        }
    }

    #[test]
    fn renders_the_whole_ticket_as_markdown() {
        let md = issue_markdown(&ticket());
        assert!(md.starts_with("# SO-2522 — Allow custom Mapped ID\n"));
        assert!(md.contains("- **Type:** Story"));
        assert!(md.contains("- **Status:** To Do"));
        assert!(md.contains("- **Parent:** SO-2439"));
        assert!(md.contains("- **Labels:** frontend"));
        // No link back to the instance — it would undo the pseudonyms.
        assert!(!md.contains("atlassian.net"), "{md}");
        assert!(!md.to_lowercase().contains("link"), "{md}");
        assert!(md.contains("## Description\n\nMapped ID needs a Custom option."));
        assert!(md.contains("## Definition of Done\n\n- Custom ids allowed"));
        assert!(md.contains("## Comments (2)"));
        assert!(md.contains("### Comment 1 — User3 (03/Jul/26 10:48 PM)"));
        assert!(md.ends_with('\n'));
    }

    #[test]
    fn people_are_pseudonymized_consistently() {
        let md = issue_markdown(&ticket());
        // Reading order: reporter, assignee, then comment authors.
        assert!(md.contains("- **Reporter:** User1"), "{md}");
        assert!(md.contains("- **Assignee:** User2"), "{md}");
        // The comment author, their @mention, and the bare first name in prose
        // all resolve to the same person.
        assert!(md.contains("### Comment 1 — User3"), "{md}");
        assert!(md.contains("@User3 good point — User3, I checked."), "{md}");
        // No real name survives anywhere, including the mention's profile URL.
        for name in ["Nickolas", "Hoyer", "Anton", "Kahwaji", "Ihor", "Karaianidi"] {
            assert!(!md.contains(name), "leaked {name}:\n{md}");
        }
        assert!(md.contains("replaced with User1, User2"));
    }

    #[test]
    fn numbering_is_stable_and_reused() {
        let mut people = People::default();
        assert_eq!(people.pseudonym("Ada Lovelace"), "User1");
        assert_eq!(people.pseudonym("Alan Turing"), "User2");
        assert_eq!(people.pseudonym("Ada Lovelace"), "User1");
        // Whitespace differences are the same person.
        assert_eq!(people.pseudonym("  Alan Turing "), "User2");
        assert_eq!(people.pseudonym(""), "");
    }

    #[test]
    fn scrub_prefers_the_longest_name() {
        // "Anna" is a prefix of "Annabel": the longer name must win, or the
        // shorter alias would corrupt it.
        let mut people = People::default();
        people.pseudonym("Annabel Smith");
        people.pseudonym("Anna Jones");
        let out = people.scrub("Annabel Smith and Anna Jones met");
        assert_eq!(out, "User1 and User2 met");
    }

    #[test]
    fn a_ticket_without_people_has_no_footnote() {
        let bare = IssueDetail {
            key: "X-1".into(),
            summary: "Bare".into(),
            description_html: Some("<p>body</p>".into()),
            ..Default::default()
        };
        let md = issue_markdown(&bare);
        assert!(!md.contains("User1"), "{md}");
        assert!(!md.contains("replaced with"), "{md}");
        assert!(md.contains("## Description\n\nbody"));
    }
}
