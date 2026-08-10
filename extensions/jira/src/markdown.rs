//! Turning Jira's rendered HTML into Markdown, for handing a ticket to an LLM.
//!
//! The input is not arbitrary web HTML — it is what Jira's renderer emits for
//! Atlassian Document Format, a small and predictable vocabulary: paragraphs,
//! headings, nested `ul`/`ol`, `tt` for inline code, `pre` blocks, `b`/`i`,
//! tables, blockquotes, `img`, and anchors. That is why a focused converter
//! beats a general HTML parser here; the tests below are built from real
//! ticket markup.
//!
//! User mentions arrive as anchors carrying an `accountId`, which is what lets
//! the caller swap in a pseudonym — see [`html_to_markdown`]'s `on_mention`.

/// Elements that never have children.
const VOID: [&str; 8] = ["br", "img", "hr", "input", "meta", "link", "col", "source"];

#[derive(Debug, Clone)]
enum Node {
    Text(String),
    Element {
        name: String,
        attrs: Vec<(String, String)>,
        children: Vec<Node>,
    },
}

impl Node {
    fn attr(&self, key: &str) -> Option<&str> {
        match self {
            Node::Element { attrs, .. } => attrs
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str()),
            Node::Text(_) => None,
        }
    }

    /// All descendant text, entities already decoded.
    fn text(&self) -> String {
        match self {
            Node::Text(t) => t.clone(),
            Node::Element { children, .. } => children.iter().map(Node::text).collect(),
        }
    }
}

/// Convert rendered Jira HTML to Markdown.
///
/// `on_mention` is called with the display name of each user mention and
/// returns what to write instead — identity to keep names, or a pseudonym.
pub fn html_to_markdown(html: &str, on_mention: &mut dyn FnMut(&str) -> String) -> String {
    let nodes = parse(html);
    let mut out = String::new();
    render_blocks(&nodes, &mut out, 0, on_mention);
    tidy(&out)
}

// ---- parsing --------------------------------------------------------------

fn parse(html: &str) -> Vec<Node> {
    let bytes = html.as_bytes();
    let mut root: Vec<Node> = Vec::new();
    // (name, attrs, children-so-far)
    let mut open: Vec<(String, Vec<(String, String)>, Vec<Node>)> = Vec::new();
    let mut i = 0;

    let push = |open: &mut Vec<(String, Vec<(String, String)>, Vec<Node>)>,
                root: &mut Vec<Node>,
                node: Node| {
        match open.last_mut() {
            Some((_, _, children)) => children.push(node),
            None => root.push(node),
        }
    };

    while i < bytes.len() {
        if bytes[i] != b'<' {
            let end = html[i..].find('<').map(|p| i + p).unwrap_or(html.len());
            let text = decode_entities(&html[i..end]);
            if !text.is_empty() {
                push(&mut open, &mut root, Node::Text(text));
            }
            i = end;
            continue;
        }
        // Comment or doctype.
        if html[i..].starts_with("<!--") {
            i = html[i..].find("-->").map(|p| i + p + 3).unwrap_or(html.len());
            continue;
        }
        if html[i..].starts_with("<!") {
            i = html[i..].find('>').map(|p| i + p + 1).unwrap_or(html.len());
            continue;
        }
        // Closing tag.
        if html[i..].starts_with("</") {
            let end = html[i..].find('>').map(|p| i + p).unwrap_or(html.len());
            let name = html[i + 2..end].trim().to_ascii_lowercase();
            i = (end + 1).min(html.len());
            // Unwind to the matching element; ignore strays.
            if open.iter().any(|(n, _, _)| *n == name) {
                while let Some((n, attrs, children)) = open.pop() {
                    let node = Node::Element {
                        name: n.clone(),
                        attrs,
                        children,
                    };
                    push(&mut open, &mut root, node);
                    if n == name {
                        break;
                    }
                }
            }
            continue;
        }
        // Opening tag.
        let Some((name, attrs, self_closing, next)) = parse_tag(html, i) else {
            // A stray '<' — treat as text.
            push(&mut open, &mut root, Node::Text("<".to_string()));
            i += 1;
            continue;
        };
        i = next;
        if self_closing || VOID.contains(&name.as_str()) {
            push(
                &mut open,
                &mut root,
                Node::Element {
                    name,
                    attrs,
                    children: Vec::new(),
                },
            );
        } else {
            open.push((name, attrs, Vec::new()));
        }
    }

    // Anything still open at EOF closes here.
    while let Some((name, attrs, children)) = open.pop() {
        let node = Node::Element {
            name,
            attrs,
            children,
        };
        match open.last_mut() {
            Some((_, _, kids)) => kids.push(node),
            None => root.push(node),
        }
    }
    root
}

/// Parse `<name attr="v" …>` at `start`, returning the tag and the index just
/// past its `>`.
fn parse_tag(html: &str, start: usize) -> Option<(String, Vec<(String, String)>, bool, usize)> {
    let bytes = html.as_bytes();
    let mut i = start + 1;
    let name_start = i;
    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-') {
        i += 1;
    }
    if i == name_start {
        return None;
    }
    let name = html[name_start..i].to_ascii_lowercase();
    let mut attrs = Vec::new();
    let mut self_closing = false;

    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        match bytes.get(i) {
            None => return Some((name, attrs, self_closing, i)),
            Some(b'>') => return Some((name, attrs, self_closing, i + 1)),
            Some(b'/') => {
                self_closing = true;
                i += 1;
                continue;
            }
            _ => {}
        }
        // Attribute name.
        let key_start = i;
        while i < bytes.len()
            && !bytes[i].is_ascii_whitespace()
            && bytes[i] != b'='
            && bytes[i] != b'>'
            && bytes[i] != b'/'
        {
            i += 1;
        }
        if i == key_start {
            i += 1; // no progress — skip the odd byte
            continue;
        }
        let key = html[key_start..i].to_ascii_lowercase();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if bytes.get(i) == Some(&b'=') {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            match bytes.get(i) {
                Some(q @ (b'"' | b'\'')) => {
                    let quote = *q;
                    i += 1;
                    let vstart = i;
                    while i < bytes.len() && bytes[i] != quote {
                        i += 1;
                    }
                    value = decode_entities(&html[vstart..i]);
                    i = (i + 1).min(html.len());
                }
                _ => {
                    let vstart = i;
                    while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' {
                        i += 1;
                    }
                    value = decode_entities(&html[vstart..i]);
                }
            }
        }
        // Jira emits `rel` twice on mention anchors; first value wins.
        if !attrs.iter().any(|(k, _): &(String, String)| *k == key) {
            attrs.push((key, value));
        }
    }
}

fn decode_entities(input: &str) -> String {
    if !input.contains('&') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let Some(semi) = after.find(';').filter(|p| *p <= 10) else {
            out.push('&');
            rest = after;
            continue;
        };
        let entity = &after[..semi];
        let decoded = match entity.to_ascii_lowercase().as_str() {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            "hellip" => Some('…'),
            "mdash" => Some('—'),
            "ndash" => Some('–'),
            _ => entity
                .strip_prefix('#')
                .and_then(|n| match n.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => n.parse::<u32>().ok(),
                })
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => out.push(c),
            None => {
                out.push('&');
                out.push_str(entity);
                out.push(';');
            }
        }
        rest = &after[semi + 1..];
    }
    out.push_str(rest);
    out
}

// ---- rendering ------------------------------------------------------------

/// Render block-level content, separating blocks with a blank line.
fn render_blocks(
    nodes: &[Node],
    out: &mut String,
    depth: usize,
    on_mention: &mut dyn FnMut(&str) -> String,
) {
    for node in nodes {
        match node {
            Node::Text(text) => {
                if !text.trim().is_empty() {
                    block_gap(out);
                    out.push_str(text.trim());
                }
            }
            Node::Element { name, children, .. } => match name.as_str() {
                "p" | "div" | "section" | "article" | "header" | "footer" => {
                    let inline = render_inline(children, on_mention);
                    // A wrapper whose content is itself blocks (Jira nests
                    // divs around panels and code) renders as blocks instead.
                    if has_block_child(children) {
                        render_blocks(children, out, depth, on_mention);
                    } else if !inline.trim().is_empty() {
                        block_gap(out);
                        out.push_str(inline.trim());
                    }
                }
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                    let level = name[1..].parse::<usize>().unwrap_or(3);
                    block_gap(out);
                    out.push_str(&"#".repeat(level.clamp(1, 6)));
                    out.push(' ');
                    out.push_str(render_inline(children, on_mention).trim());
                }
                "ul" | "ol" => render_list(node, out, "", on_mention),
                "pre" => {
                    block_gap(out);
                    out.push_str("```\n");
                    out.push_str(node.text().trim_matches('\n'));
                    out.push_str("\n```");
                }
                "blockquote" => {
                    let mut inner = String::new();
                    render_blocks(children, &mut inner, depth, on_mention);
                    block_gap(out);
                    for (i, line) in tidy(&inner).lines().enumerate() {
                        if i > 0 {
                            out.push('\n');
                        }
                        out.push_str("> ");
                        out.push_str(line);
                    }
                }
                "hr" => {
                    block_gap(out);
                    out.push_str("---");
                }
                "table" => render_table(node, out, on_mention),
                "br" => out.push('\n'),
                // Anything else: blocks recurse, inline content is emitted.
                _ => {
                    if has_block_child(children) {
                        render_blocks(children, out, depth, on_mention);
                    } else {
                        let inline = render_inline(std::slice::from_ref(node), on_mention);
                        if !inline.trim().is_empty() {
                            block_gap(out);
                            out.push_str(inline.trim());
                        }
                    }
                }
            },
        }
    }
}

fn has_block_child(nodes: &[Node]) -> bool {
    nodes.iter().any(|n| match n {
        Node::Element { name, .. } => matches!(
            name.as_str(),
            "p" | "div" | "ul" | "ol" | "pre" | "blockquote" | "table" | "hr" | "h1" | "h2"
                | "h3" | "h4" | "h5" | "h6"
        ),
        Node::Text(_) => false,
    })
}

/// Ensure the next block starts after a blank line.
fn block_gap(out: &mut String) {
    if out.is_empty() {
        return;
    }
    while out.ends_with('\n') {
        out.pop();
    }
    out.push_str("\n\n");
}

/// Render a list, indenting nested content to the parent item's content column
/// (3 for `1. `, 2 for `- `) — the width CommonMark needs to read it as nested
/// rather than as a sibling list.
fn render_list(
    list: &Node,
    out: &mut String,
    indent: &str,
    on_mention: &mut dyn FnMut(&str) -> String,
) {
    let Node::Element { name, children, .. } = list else {
        return;
    };
    let ordered = name == "ol";
    let mut index = 1;

    // A top-level list is its own block; nested ones continue the line above.
    if indent.is_empty() {
        block_gap(out);
    }

    for item in children {
        let Node::Element { name, children, .. } = item else {
            continue;
        };
        if name != "li" {
            continue;
        }
        // An item's own text, then any lists nested inside it.
        let inline: Vec<Node> = children
            .iter()
            .filter(|c| !matches!(c, Node::Element { name, .. } if name == "ul" || name == "ol"))
            .cloned()
            .collect();
        let text = render_inline(&inline, on_mention);
        let text = text.trim();

        // One line per item — items are not blocks, so no blank line between.
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        let marker = if ordered {
            format!("{index}. ")
        } else {
            "- ".to_string()
        };
        let child_indent = format!("{indent}{}", " ".repeat(marker.chars().count()));
        out.push_str(indent);
        out.push_str(&marker);
        // Continuation lines and nested lists line up under the item's text.
        out.push_str(&text.replace('\n', &format!("\n{child_indent}")));
        index += 1;

        for nested in children {
            if matches!(nested, Node::Element { name, .. } if name == "ul" || name == "ol") {
                out.push('\n');
                render_list(nested, out, &child_indent, on_mention);
            }
        }
    }
}

fn render_table(table: &Node, out: &mut String, on_mention: &mut dyn FnMut(&str) -> String) {
    let mut rows: Vec<(bool, Vec<String>)> = Vec::new();
    collect_rows(table, &mut rows, on_mention);
    if rows.is_empty() {
        return;
    }
    block_gap(out);
    let width = rows.iter().map(|(_, c)| c.len()).max().unwrap_or(0);
    for (i, (is_header, cells)) in rows.iter().enumerate() {
        out.push_str("| ");
        for c in 0..width {
            out.push_str(cells.get(c).map(String::as_str).unwrap_or(""));
            out.push_str(" | ");
        }
        out.truncate(out.trim_end().len());
        out.push('\n');
        // Header separator after the first row if it was a header row (or
        // unconditionally after row 0, so the table stays valid Markdown).
        if i == 0 {
            out.push('|');
            for _ in 0..width {
                out.push_str(" --- |");
            }
            out.push('\n');
            let _ = is_header;
        }
    }
    while out.ends_with('\n') {
        out.pop();
    }
}

fn collect_rows(
    node: &Node,
    rows: &mut Vec<(bool, Vec<String>)>,
    on_mention: &mut dyn FnMut(&str) -> String,
) {
    let Node::Element { name, children, .. } = node else {
        return;
    };
    if name == "tr" {
        let mut cells = Vec::new();
        let mut header = false;
        for cell in children {
            if let Node::Element { name, children, .. } = cell {
                if name == "th" || name == "td" {
                    header |= name == "th";
                    cells.push(render_inline(children, on_mention).trim().replace('\n', " "));
                }
            }
        }
        if !cells.is_empty() {
            rows.push((header, cells));
        }
        return;
    }
    for child in children {
        collect_rows(child, rows, on_mention);
    }
}

/// Render inline content (no block structure) to Markdown.
fn render_inline(nodes: &[Node], on_mention: &mut dyn FnMut(&str) -> String) -> String {
    let mut out = String::new();
    for node in nodes {
        match node {
            Node::Text(text) => out.push_str(&collapse_ws(text)),
            Node::Element {
                name, children, ..
            } => match name.as_str() {
                "b" | "strong" => wrap(&mut out, "**", &render_inline(children, on_mention)),
                "i" | "em" => wrap(&mut out, "*", &render_inline(children, on_mention)),
                "del" | "s" | "strike" => {
                    wrap(&mut out, "~~", &render_inline(children, on_mention))
                }
                "code" | "tt" | "kbd" | "samp" => {
                    let text = collapse_ws(&node.text());
                    let text = text.trim();
                    if !text.is_empty() {
                        out.push('`');
                        out.push_str(text);
                        out.push('`');
                    }
                }
                "a" => {
                    let text = render_inline(children, on_mention);
                    let text = text.trim();
                    let href = node.attr("href").unwrap_or("").trim();
                    if is_mention(node) {
                        // A person, not a destination: the pseudonym replaces
                        // the whole link so no profile URL leaks the name.
                        out.push('@');
                        out.push_str(&on_mention(text));
                    } else if text.is_empty() {
                        out.push_str(href);
                    } else if href.is_empty() || href == text {
                        out.push_str(text);
                    } else {
                        out.push_str(&format!("[{text}]({href})"));
                    }
                }
                "img" => {
                    let alt = node.attr("alt").unwrap_or("image");
                    let src = node.attr("src").unwrap_or("");
                    out.push_str(&format!("![{alt}]({src})"));
                }
                "br" => out.push('\n'),
                _ => out.push_str(&render_inline(children, on_mention)),
            },
        }
    }
    out
}

/// Whether an anchor is a user mention. Jira renders those as profile links
/// carrying the account id — see the `accountId=` href and `data-account-id`.
fn is_mention(node: &Node) -> bool {
    node.attr("data-account-id").is_some()
        || node.attr("accountid").is_some()
        || node
            .attr("href")
            .is_some_and(|h| h.contains("accountId=") || h.contains("ViewProfile.jspa"))
}

fn wrap(out: &mut String, marker: &str, inner: &str) {
    let trimmed = inner.trim();
    if trimmed.is_empty() {
        return;
    }
    // Keep any leading/trailing space outside the emphasis, or Markdown
    // renderers won't apply it.
    if inner.starts_with(char::is_whitespace) {
        out.push(' ');
    }
    out.push_str(marker);
    out.push_str(trimmed);
    out.push_str(marker);
    if inner.ends_with(char::is_whitespace) {
        out.push(' ');
    }
}

/// Collapse HTML's insignificant whitespace (Jira indents its list markup with
/// tabs and newlines) down to single spaces.
///
/// Edge whitespace is kept: this runs per text node, and the space in
/// `<tt>x</tt> next` lives at the start of the following node. Blocks are
/// trimmed at their edges later, so keeping it here is safe.
fn collapse_ws(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            space = true;
            continue;
        }
        if space {
            out.push(' ');
        }
        space = false;
        out.push(c);
    }
    if space {
        out.push(' ');
    }
    out
}

/// Trim trailing spaces and collapse runs of blank lines.
fn tidy(text: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() && lines.last().is_some_and(|l: &&str| l.is_empty()) {
            continue;
        }
        lines.push(line);
    }
    while lines.first().is_some_and(|l| l.is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Convert with mentions left as-is.
    fn md(html: &str) -> String {
        html_to_markdown(html, &mut |name| name.to_string())
    }

    #[test]
    fn paragraphs_and_emphasis() {
        assert_eq!(md("<p>Hello <b>world</b></p>"), "Hello **world**");
        assert_eq!(
            md("<p>One</p>\n<p>Two</p>"),
            "One\n\nTwo"
        );
        // Emphasis keeps its surrounding spaces outside the markers.
        assert_eq!(md("<p>a <i>b </i>c</p>"), "a *b* c");
    }

    #[test]
    fn inline_code_and_entities() {
        // Straight from a real comment: tt with a nested span, numeric refs.
        let html = r#"<p>ids: <tt><span class="error">&#91;900_001, 900_003&#93;</span></tt> next</p>"#;
        assert_eq!(md(html), "ids: `[900_001, 900_003]` next");
        assert_eq!(md("<p>a &amp; b &lt;c&gt;</p>"), "a & b <c>");
    }

    #[test]
    fn nested_ordered_lists_keep_their_depth() {
        // The shape comment 1 actually uses: ol > li > ol > li > ol.
        let html = r#"<ol>
	<li>I checked the database:
	<ol>
		<li>by <tt>mappedId</tt>: duplicates exist
		<ol>
			<li>define requirements per provider</li>
		</ol>
		</li>
	</ol>
	</li>
	<li>The next id is <tt>max + 1</tt></li>
</ol>"#;
        assert_eq!(
            md(html),
            "1. I checked the database:\n   \
             1. by `mappedId`: duplicates exist\n      \
             1. define requirements per provider\n\
             2. The next id is `max + 1`"
        );
    }

    #[test]
    fn bullet_lists_and_headings() {
        let html = "<h3>Acceptance criteria</h3><ul><li>One</li><li>Two</li></ul>";
        assert_eq!(md(html), "### Acceptance criteria\n\n- One\n- Two");
    }

    #[test]
    fn links_images_and_code_blocks() {
        assert_eq!(
            md(r#"<p>see <a href="https://x.test/a">the doc</a></p>"#),
            "see [the doc](https://x.test/a)"
        );
        assert_eq!(
            md(r#"<p><span class="image-wrap"><img src="https://x.test/i.png" alt="shot.png" width="671" /></span></p>"#),
            "![shot.png](https://x.test/i.png)"
        );
        assert_eq!(
            md("<pre>if x == 0 {\n    bail!();\n}</pre>"),
            "```\nif x == 0 {\n    bail!();\n}\n```"
        );
    }

    #[test]
    fn tables_and_quotes() {
        let html = "<table><tbody><tr><th>Build</th><th>Result</th></tr>\
                    <tr><td>v0.10.0</td><td>Flickers</td></tr></tbody></table>";
        assert_eq!(
            md(html),
            "| Build | Result |\n| --- | --- |\n| v0.10.0 | Flickers |"
        );
        assert_eq!(md("<blockquote><p>quoted</p></blockquote>"), "> quoted");
    }

    #[test]
    fn mentions_go_through_the_callback() {
        // The real markup: a profile anchor with the account id.
        let html = r#"<p><a href="https://x.atlassian.net/secure/ViewProfile.jspa?accountId=712020%3Ad72c" class="user-hover" data-account-id="712020:d72c" rel="noreferrer">Ihor Karaianidi</a>  </p>"#;
        let mut seen = Vec::new();
        let out = html_to_markdown(html, &mut |name| {
            seen.push(name.to_string());
            "User3".to_string()
        });
        assert_eq!(seen, vec!["Ihor Karaianidi"]);
        // The profile URL is dropped with the name — it identifies them too.
        assert_eq!(out, "@User3");
    }

    #[test]
    fn ordinary_links_are_not_mistaken_for_mentions() {
        let html = r#"<p><a href="https://x.test/browse/SO-1">SO-1</a></p>"#;
        let out = html_to_markdown(html, &mut |_| "User1".to_string());
        assert_eq!(out, "[SO-1](https://x.test/browse/SO-1)");
    }

    #[test]
    fn tolerates_malformed_markup() {
        // Unclosed tags, stray '<', a comment.
        assert_eq!(md("<p>one<p>two"), "one\n\ntwo");
        assert_eq!(md("<!-- hi --><p>body</p>"), "body");
        assert_eq!(md("<p>5 < 6</p>"), "5 < 6");
        assert_eq!(md(""), "");
    }
}
