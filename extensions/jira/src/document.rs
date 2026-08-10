//! Composing one styled HTML document from an issue's rendered description and
//! comments, for the shared Blitz renderer.
//!
//! Jira's `renderedFields` give us structure — headings, lists, code blocks,
//! tables, class hooks like `.panel` and `.confluenceTh` — but no palette, so
//! the stylesheet here is ours, built from the app's theme tokens. That's what
//! lets the reading pane render dark instead of in a white box.
//!
//! The wrinkle is inline colors. Ticket bodies accumulate pasted content, ADF
//! text-color and highlight marks, and Jira's own light-mode panel
//! backgrounds — all written for a white page, all as `style` attributes that
//! would otherwise win over our rules. [`adapt_inline_colors`] rewrites just
//! those declarations that would be unreadable on a dark surface, leaving
//! everything else verbatim.

use crate::models::IssueDetail;

/// Minimum contrast ratio (WCAG's threshold for large text and UI parts) an
/// inline color must reach to survive [`adapt_inline_colors`].
const MIN_CONTRAST: f32 = 3.0;

/// Text color given to elements carrying a light inline background, so those
/// islands stay legible. Jira's own ink, since that's what the content was
/// authored against.
const INK_ON_LIGHT: u32 = 0x17_2b4d;

/// The palette the document's stylesheet is built from. The view fills this in
/// from `spotlight_ui::theme` so the rendered HTML matches the panel around it.
#[derive(Debug, Clone, Copy)]
pub struct DocStyle {
    /// Canvas the document paints on (also handed to the renderer).
    pub background: u32,
    pub text: u32,
    pub muted: u32,
    /// Link color.
    pub link: u32,
    /// Hairlines: table cells, rules, quote bars.
    pub border: u32,
    /// Backing for code blocks and Jira's panel/table-header chrome —
    /// recessed relative to `background`.
    pub code_bg: u32,
}

/// Compose the full document: our stylesheet, the description, the filled-in
/// custom fields, then comments — mirroring how the issue reads on the web.
pub fn issue_document(detail: &IssueDetail, style: &DocStyle) -> String {
    let mut doc = stylesheet(style);
    match &detail.description_html {
        Some(html) => doc.push_str(&adapt_inline_colors(html, style)),
        None => doc.push_str("<p class=\"empty\">No description.</p>"),
    }
    for field in &detail.custom_fields {
        doc.push_str(&format!(
            "<div class=\"meta\"><b>{}</b></div>{}",
            escape_html(&field.name),
            adapt_inline_colors(&field.html, style)
        ));
    }
    if !detail.comments.is_empty() {
        doc.push_str("<hr>");
        let n = detail.comments.len();
        doc.push_str(&format!(
            "<div class=\"meta\"><b>{n} comment{}</b></div>",
            if n == 1 { "" } else { "s" }
        ));
        for comment in &detail.comments {
            doc.push_str(&format!(
                "<div class=\"meta\"><b>{}</b> · {}</div>{}",
                escape_html(&comment.author),
                escape_html(&comment.created),
                adapt_inline_colors(&comment.body_html, style)
            ));
        }
    }
    doc
}

/// Our stylesheet for Jira's rendered HTML.
///
/// The `!important` block covers the class hooks Jira ships light-mode inline
/// backgrounds on (code blocks, info/note panels, table headers). Author
/// `style` attributes are normal-priority declarations, so an `!important`
/// rule here outranks them — which is the only way to re-skin that chrome.
fn stylesheet(s: &DocStyle) -> String {
    format!(
        "<style>\
         body {{ font-family: -apple-system, 'Helvetica Neue', Helvetica, Arial, sans-serif;\
                 font-size: 14px; line-height: 1.55; color: #{text:06x}; }}\
         h1, h2, h3, h4, h5, h6 {{ line-height: 1.3; color: #{text:06x}; }}\
         a {{ color: #{link:06x}; }}\
         img {{ max-width: 100%; }}\
         code, pre, tt, kbd, samp {{ font-family: Menlo, Monaco, monospace;\
                 font-size: 12px; background: #{code_bg:06x}; border-radius: 3px; }}\
         pre {{ padding: 8px 12px; overflow-x: hidden; }}\
         blockquote {{ border-left: 2px solid #{border:06x}; margin-left: 0;\
                 padding-left: 12px; color: #{muted:06x}; }}\
         table {{ border-collapse: separate; }}\
         th, td {{ border: 1px solid #{border:06x}; padding: 4px 8px; }}\
         hr {{ border: none; border-top: 1px solid #{border:06x}; margin: 20px 0; }}\
         .meta {{ font-size: 12px; color: #{muted:06x}; margin: 16px 0 4px; }}\
         .empty {{ color: #{muted:06x}; font-style: italic; }}\
         .panel, .panelContent, .panelHeader, .code, .codeContent, .codeHeader,\
         .preformatted, .preformattedContent, .confluenceTh, .confluenceTd,\
         .confluence-information-macro {{\
                 background: #{code_bg:06x} !important;\
                 border-color: #{border:06x} !important;\
                 color: #{text:06x} !important; }}\
         .panel, .confluence-information-macro {{ border-radius: 3px; margin: 12px 0; }}\
         .panelContent, .panelHeader, .confluence-information-macro {{ padding: 2px 12px; }}\
         .panelHeader {{ border-bottom: 1px solid #{border:06x}; }}\
         .codeContent {{ padding: 0; }}\
         </style>",
        text = s.text & 0xff_ffff,
        link = s.link & 0xff_ffff,
        muted = s.muted & 0xff_ffff,
        border = s.border & 0xff_ffff,
        code_bg = s.code_bg & 0xff_ffff,
    )
}

/// Rewrite inline colors that don't work on a dark canvas.
///
/// Two cases, decided per element by measuring contrast rather than by
/// absolute darkness — that keeps saturated marks like Jira's red `#ff5630`
/// (~4.8:1 here) while dropping pasted body ink like `#172b4d` (~1.1:1):
///
/// - an inline **background** too close to our text color means the element is
///   a light island, so it gets dark ink unless the author already set a color;
/// - otherwise an inline **text color** too close to our canvas is dropped, so
///   it inherits the theme's.
///
/// Only `style` attribute values inside tags are touched; anything
/// unparseable is left exactly as written.
pub fn adapt_inline_colors(html: &str, style: &DocStyle) -> String {
    let bytes = html.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    let mut in_tag = false;

    while i < bytes.len() {
        let b = bytes[i];
        if !in_tag {
            if b == b'<' {
                in_tag = true;
            }
            out.push(b);
            i += 1;
            continue;
        }
        if b == b'>' {
            in_tag = false;
            out.push(b);
            i += 1;
            continue;
        }
        // A `style="…"` attribute: rewrite its declarations.
        if starts_with_ignore_case(&bytes[i..], b"style")
            && i > 0
            && bytes[i - 1].is_ascii_whitespace()
        {
            if let Some((value_start, value_end, quote)) = attr_value_span(bytes, i + 5) {
                let value = &html[value_start..value_end];
                out.extend_from_slice(b"style=");
                out.push(quote);
                out.extend_from_slice(adapt_declarations(value, style).as_bytes());
                out.push(quote);
                i = value_end + 1;
                continue;
            }
        }
        // Any other quoted attribute value: copy it wholesale, so a `>` inside
        // it doesn't read as the end of the tag.
        if b == b'"' || b == b'\'' {
            out.push(b);
            i += 1;
            while i < bytes.len() && bytes[i] != b {
                out.push(bytes[i]);
                i += 1;
            }
            if i < bytes.len() {
                out.push(bytes[i]);
                i += 1;
            }
            continue;
        }
        out.push(b);
        i += 1;
    }

    String::from_utf8(out).unwrap_or_else(|_| html.to_string())
}

/// Whether `haystack` begins with the ASCII `needle`, ignoring case (attribute
/// names are case-insensitive, and `STYLE=` shows up in pasted markup).
fn starts_with_ignore_case(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len()
        && haystack[..needle.len()].eq_ignore_ascii_case(needle)
}

/// Span of a quoted attribute value starting at/after `from` (expects
/// optional whitespace, `=`, then a quote). Returns `(start, end, quote)` with
/// `end` at the closing quote.
fn attr_value_span(bytes: &[u8], from: usize) -> Option<(usize, usize, u8)> {
    let mut i = from;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if bytes.get(i)? != &b'=' {
        return None;
    }
    i += 1;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let quote = *bytes.get(i)?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    let start = i + 1;
    let end = bytes[start..].iter().position(|b| *b == quote)? + start;
    Some((start, end, quote))
}

/// The declaration-level rewrite behind [`adapt_inline_colors`].
fn adapt_declarations(value: &str, style: &DocStyle) -> String {
    let decls: Vec<&str> = value.split(';').collect();
    let property = |decl: &str| -> Option<(String, String)> {
        let (prop, val) = decl.split_once(':')?;
        Some((prop.trim().to_ascii_lowercase(), val.trim().to_string()))
    };

    // The element's own background, if it declares a parseable one.
    let background = decls.iter().filter_map(|d| property(d)).rev().find_map(|(p, v)| {
        (p == "background-color" || p == "background").then(|| parse_color(&v)).flatten()
    });
    let has_color = decls
        .iter()
        .filter_map(|d| property(d))
        .any(|(p, v)| p == "color" && parse_color(&v).is_some());

    // A light island: keep the author's colors, but make sure there *is* one.
    if let Some(bg) = background {
        if contrast(bg, style.text) < MIN_CONTRAST {
            let mut out = value.trim_end().trim_end_matches(';').to_string();
            if !has_color {
                out.push_str(&format!("; color: #{INK_ON_LIGHT:06x}"));
            }
            return out;
        }
    }

    // Otherwise drop text colors that would disappear into the canvas.
    let kept: Vec<&str> = decls
        .iter()
        .filter(|decl| {
            let Some((prop, val)) = property(decl) else {
                return true; // unparseable — leave it alone
            };
            if prop != "color" {
                return true;
            }
            match parse_color(&val) {
                Some(c) => contrast(c, style.background) >= MIN_CONTRAST,
                None => true,
            }
        })
        .copied()
        .collect();
    kept.join(";")
}

/// Parse the first CSS color in a declaration value: `#rgb`, `#rrggbb[aa]`,
/// `rgb()`/`rgba()`, or `white`/`black`. Alpha is ignored — these are used only
/// to judge contrast. Anything else yields `None`, which means "leave as-is".
fn parse_color(value: &str) -> Option<u32> {
    let v = value.trim().to_ascii_lowercase();
    if let Some(hex) = v.strip_prefix('#') {
        let hex: String = hex.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
        return match hex.len() {
            3 => {
                let d: Vec<u32> = hex
                    .chars()
                    .map(|c| c.to_digit(16).unwrap_or(0))
                    .collect();
                Some((d[0] * 17) << 16 | (d[1] * 17) << 8 | (d[2] * 17))
            }
            6 | 8 => u32::from_str_radix(&hex[..6], 16).ok(),
            _ => None,
        };
    }
    if let Some(rest) = v.strip_prefix("rgb") {
        let inner = rest.trim_start_matches('a').trim();
        let inner = inner.strip_prefix('(')?.split(')').next()?;
        let channels: Vec<u32> = inner
            .split([',', ' ', '/'])
            .filter(|s| !s.trim().is_empty())
            .take(3)
            .filter_map(|s| s.trim().parse::<f32>().ok())
            .map(|n| n.clamp(0.0, 255.0) as u32)
            .collect();
        if channels.len() == 3 {
            return Some(channels[0] << 16 | channels[1] << 8 | channels[2]);
        }
        return None;
    }
    match v.as_str() {
        "white" => Some(0xff_ffff),
        "black" => Some(0x00_0000),
        _ => None,
    }
}

/// WCAG relative luminance of an `0xRRGGBB` color.
fn luminance(rgb: u32) -> f32 {
    let channel = |shift: u32| {
        let c = ((rgb >> shift) & 0xff) as f32 / 255.0;
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(16) + 0.7152 * channel(8) + 0.0722 * channel(0)
}

/// WCAG contrast ratio between two `0xRRGGBB` colors (1.0 … 21.0).
fn contrast(a: u32, b: u32) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// HTML-escape a text fragment interpolated into [`issue_document`].
fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{CommentHtml, CustomField, IssueDetail};

    /// The app's dark palette, near enough for contrast assertions.
    const DARK: DocStyle = DocStyle {
        background: 0x23_252c,
        text: 0xe8_ecf4,
        muted: 0x8a_93a6,
        link: 0x6e_e7ff,
        border: 0x35_363c,
        code_bg: 0x12_141c,
    };

    fn detail(description: Option<&str>, comments: Vec<CommentHtml>) -> IssueDetail {
        IssueDetail {
            key: "FE-42".into(),
            summary: "Build the thing".into(),
            status: "In Progress".into(),
            description_html: description.map(str::to_string),
            comments,
            ..Default::default()
        }
    }

    #[test]
    fn composes_description_and_comments() {
        let doc = issue_document(
            &detail(
                Some("<p>Do the <b>thing</b></p>"),
                vec![CommentHtml {
                    author: "Jane <Dev>".into(),
                    created: "07/Aug/26 9:15 AM".into(),
                    body_html: "<p>On it.</p>".into(),
                }],
            ),
            &DARK,
        );
        assert!(doc.contains("<p>Do the <b>thing</b></p>"));
        // Author names are escaped; rendered bodies pass through as-is.
        assert!(doc.contains("Jane &lt;Dev&gt;"));
        assert!(doc.contains("<p>On it.</p>"));
        assert!(doc.contains("07/Aug/26 9:15 AM"));
        assert!(doc.contains("1 comment<"), "singular comment count");
        // The stylesheet carries the theme palette, not a white page.
        assert!(doc.contains("color: #e8ecf4"));
        assert!(!doc.contains("#ffffff"));
    }

    #[test]
    fn custom_fields_are_labeled_and_adapted() {
        let mut d = detail(Some("<p>desc</p>"), Vec::new());
        d.custom_fields = vec![
            CustomField {
                name: "Why we need this ?".into(),
                html: r#"<p style="color: #172b4d">Parity with UAT</p>"#.into(),
            },
            CustomField {
                name: "Definition of Done".into(),
                html: "<ul><li>Custom ids allowed</li></ul>".into(),
            },
        ];
        let doc = issue_document(&d, &DARK);
        // Labels use the same muted treatment as the comment bylines, and the
        // field's own HTML follows.
        assert!(doc.contains(r#"<div class="meta"><b>Why we need this ?</b></div>"#), "{doc}");
        assert!(doc.contains("<li>Custom ids allowed</li>"), "{doc}");
        // Field bodies go through the same inline-color adaptation as the
        // description — pasted ink here would be just as invisible.
        assert!(!doc.contains("#172b4d"), "{doc}");
        // They sit after the description and before any comments.
        let desc = doc.find("<p>desc</p>").unwrap();
        assert!(doc.find("Why we need this ?").unwrap() > desc);
    }

    #[test]
    fn document_without_description_or_comments() {
        let doc = issue_document(&detail(None, Vec::new()), &DARK);
        assert!(doc.contains("No description."));
        assert!(!doc.contains("comment<"));
    }

    #[test]
    fn contrast_ratio_matches_wcag_anchors() {
        assert!((contrast(0xffffff, 0x000000) - 21.0).abs() < 0.01);
        assert!((contrast(0x23252c, 0x23252c) - 1.0).abs() < 0.001);
    }

    #[test]
    fn drops_pasted_ink_but_keeps_saturated_marks() {
        // Jira/Word body ink on our canvas is ~1.1:1 — invisible.
        let out = adapt_inline_colors(r#"<span style="color: #172b4d">hi</span>"#, &DARK);
        assert_eq!(out, r#"<span style="">hi</span>"#);

        // Jira's red mark is ~4.8:1 — legible, so it stays.
        let out = adapt_inline_colors(r#"<span style="color:#ff5630">blocked</span>"#, &DARK);
        assert!(out.contains("#ff5630"), "{out}");

        // rgb() notation is understood too, and other declarations survive.
        let out = adapt_inline_colors(
            r#"<p style="color: rgb(23, 43, 77); font-weight: bold">x</p>"#,
            &DARK,
        );
        assert!(!out.contains("rgb(23"), "{out}");
        assert!(out.contains("font-weight: bold"), "{out}");
    }

    #[test]
    fn light_backgrounds_get_dark_ink() {
        // An ADF highlight mark: light background, no color of its own — our
        // light theme text would vanish on it.
        let out = adapt_inline_colors(
            r#"<span style="background-color: #fff9c4">note</span>"#,
            &DARK,
        );
        assert!(out.contains("color: #172b4d"), "{out}");
        assert!(out.contains("#fff9c4"), "background preserved: {out}");

        // When the author already paired a color with the background, keep it.
        let out = adapt_inline_colors(
            r#"<div style="background: #deebff; color: #0747a6">panel</div>"#,
            &DARK,
        );
        assert!(out.contains("#0747a6"), "{out}");
        assert_eq!(out.matches("color:").count(), 1, "no second color: {out}");
    }

    #[test]
    fn leaves_everything_else_verbatim() {
        // No style attributes at all.
        let html = r#"<p>plain <a href="/browse/X-1">link</a></p>"#;
        assert_eq!(adapt_inline_colors(html, &DARK), html);

        // Unparseable colors, and properties we don't judge, pass through.
        let html = r#"<p style="color: var(--ds-text); border-color: #172b4d">x</p>"#;
        assert_eq!(adapt_inline_colors(html, &DARK), html);

        // A `style="…"` written in *text* (a code sample) is content, not an
        // attribute, so it must survive untouched.
        let html = r#"<pre>span style="color: #172b4d"</pre>"#;
        assert_eq!(adapt_inline_colors(html, &DARK), html);

        // A quoted attribute containing '>' doesn't derail tag tracking.
        let html = r#"<img alt="a > b" style="color:#172b4d"><span>after</span>"#;
        let out = adapt_inline_colors(html, &DARK);
        assert!(out.contains(r#"alt="a > b""#), "{out}");
        assert!(out.contains("<span>after</span>"), "{out}");
        assert!(!out.contains("#172b4d"), "{out}");
    }

    #[test]
    fn single_quoted_and_uppercase_attributes_are_handled() {
        let out = adapt_inline_colors(r#"<span STYLE='color: #172B4D'>x</span>"#, &DARK);
        assert!(!out.to_ascii_lowercase().contains("#172b4d"), "{out}");
        // Shorthand hex too.
        let out = adapt_inline_colors(r#"<span style="color:#111">x</span>"#, &DARK);
        assert!(!out.contains("#111"), "{out}");
    }
}
