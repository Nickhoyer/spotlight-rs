//! A small, dependency-free Markdown renderer for chat replies. Handles the
//! subset LLMs actually emit: fenced code blocks, headings, bullet/numbered
//! lists, blockquotes, paragraphs, and inline **bold** / *italic* / `code`.
//! Everything wraps at the bubble width (no horizontal overflow); code blocks
//! keep their line breaks in a monospace box.

use std::ops::Range;

use gpui::prelude::*;
use gpui::{div, AnyElement, FontStyle, FontWeight, HighlightStyle, StyledText};

use spotlight_ui::theme;

/// Render `text` as a vertical stack of Markdown blocks.
pub fn render(text: &str) -> AnyElement {
    let mut col = div().flex().flex_col().gap_2();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();

        // Fenced code block.
        if let Some(lang_fence) = trimmed.strip_prefix("```") {
            let _ = lang_fence;
            let mut code = Vec::new();
            i += 1;
            while i < lines.len() && !lines[i].trim_start().starts_with("```") {
                code.push(lines[i]);
                i += 1;
            }
            i += 1; // consume closing fence
            col = col.child(code_block(&code.join("\n")));
            continue;
        }

        // Blank line → skip (blocks already gap-separated).
        if trimmed.is_empty() {
            i += 1;
            continue;
        }

        // Heading.
        if let Some(level) = heading_level(trimmed) {
            let content = trimmed[level..].trim_start();
            col = col.child(heading(level, content));
            i += 1;
            continue;
        }

        // Blockquote (consecutive `>` lines).
        if trimmed.starts_with('>') {
            let mut quote = Vec::new();
            while i < lines.len() && lines[i].trim_start().starts_with('>') {
                quote.push(lines[i].trim_start().trim_start_matches('>').trim_start());
                i += 1;
            }
            col = col.child(blockquote(&quote.join(" ")));
            continue;
        }

        // List (consecutive bullet/ordered items).
        if list_marker(trimmed).is_some() {
            let mut items = Vec::new();
            while i < lines.len() {
                let t = lines[i].trim_start();
                let Some((marker, rest)) = list_marker(t) else {
                    break;
                };
                items.push((marker, rest.to_string()));
                i += 1;
            }
            col = col.child(list(&items));
            continue;
        }

        // Paragraph (consecutive plain lines joined into one wrapped block).
        let mut para = Vec::new();
        while i < lines.len() {
            let t = lines[i].trim_start();
            if t.is_empty()
                || t.starts_with("```")
                || t.starts_with('>')
                || heading_level(t).is_some()
                || list_marker(t).is_some()
            {
                break;
            }
            para.push(lines[i].trim());
            i += 1;
        }
        col = col.child(paragraph(&para.join(" ")));
    }
    col.into_any_element()
}

fn heading_level(line: &str) -> Option<usize> {
    let hashes = line.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&hashes) && line[hashes..].starts_with(' ') {
        Some(hashes)
    } else {
        None
    }
}

/// `(marker, remaining text)` if the line starts a list item.
fn list_marker(line: &str) -> Option<(String, &str)> {
    for p in ["- ", "* ", "+ "] {
        if let Some(rest) = line.strip_prefix(p) {
            return Some(("\u{2022}".to_string(), rest));
        }
    }
    // Ordered: `<digits>. `
    let digits = line.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 {
        if let Some(rest) = line[digits..].strip_prefix(". ") {
            return Some((format!("{}.", &line[..digits]), rest));
        }
    }
    None
}

fn paragraph(text: &str) -> AnyElement {
    div().text_color(theme::text()).child(styled_block(None, text)).into_any_element()
}

fn heading(level: usize, text: &str) -> AnyElement {
    let mut el = div().font_weight(FontWeight::BOLD).text_color(theme::text());
    el = if level <= 2 { el.text_lg() } else { el.text_base() };
    el.child(styled_block(None, text)).into_any_element()
}

fn blockquote(text: &str) -> AnyElement {
    div()
        .border_l_2()
        .border_color(theme::border())
        .pl_3()
        .text_color(theme::muted())
        .child(styled_block(None, text))
        .into_any_element()
}

fn list(items: &[(String, String)]) -> AnyElement {
    let mut col = div().flex().flex_col().gap_1();
    for (marker, text) in items {
        // Each item is a full-width block so it wraps like a paragraph; the
        // accent marker is prepended into the wrapping text (a flex row with a
        // `flex_1` text cell would not wrap under taffy's max-content measure).
        col = col.child(
            div()
                .pl_1()
                .text_color(theme::text())
                .child(styled_block(Some(marker), text)),
        );
    }
    col.into_any_element()
}

fn code_block(code: &str) -> AnyElement {
    let mut col = div()
        .flex()
        .flex_col()
        .px_3()
        .py_2()
        .rounded_md()
        .bg(gpui::rgba(0x00_0000_38))
        .border_1()
        .border_color(theme::divider())
        .font_family("Menlo")
        .text_sm()
        .text_color(theme::text());
    // Keep line breaks; each line wraps rather than overflowing horizontally.
    for line in code.split('\n') {
        col = col.child(div().child(line.to_string()));
    }
    col.into_any_element()
}

/// Render inline text (with **bold**, *italic*/_italic_, and `code` spans) as a
/// single wrapping `StyledText`, optionally led by an accent `marker` (for list
/// bullets/numbers). Markers are stripped and their ranges highlighted over the
/// ambient style, so the result wraps like normal text — crucial for avoiding
/// horizontal overflow.
fn styled_block(marker: Option<&str>, raw: &str) -> AnyElement {
    let mut clean = String::new();
    let mut spans = Vec::new();
    if let Some(marker) = marker {
        clean.push_str(marker);
        clean.push(' ');
        spans.push((0..marker.len(), marker_style()));
    }
    let offset = clean.len();
    let (body, body_spans) = parse_inline(raw);
    clean.push_str(&body);
    spans.extend(
        body_spans
            .into_iter()
            .map(|(r, s)| (offset + r.start..offset + r.end, s)),
    );
    div()
        .child(StyledText::new(clean).with_highlights(spans))
        .into_any_element()
}

fn parse_inline(raw: &str) -> (String, Vec<(Range<usize>, HighlightStyle)>) {
    let mut clean = String::new();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        let rest = &raw[i..];
        // **bold**
        if let Some(r) = rest.strip_prefix("**") {
            if let Some(end) = r.find("**") {
                let start = clean.len();
                clean.push_str(&r[..end]);
                spans.push((start..clean.len(), bold()));
                i += 4 + end;
                continue;
            }
        }
        // `code`
        if let Some(r) = rest.strip_prefix('`') {
            if let Some(end) = r.find('`') {
                let start = clean.len();
                clean.push_str(&r[..end]);
                spans.push((start..clean.len(), code_style()));
                i += 2 + end;
                continue;
            }
        }
        // *italic* / _italic_ (single marker, not the start of **)
        let first = rest.as_bytes()[0];
        if (first == b'*' || first == b'_') && !rest.starts_with("**") {
            let marker = first as char;
            if let Some(end) = rest[1..].find(marker) {
                let start = clean.len();
                clean.push_str(&rest[1..1 + end]);
                spans.push((start..clean.len(), italic()));
                i += 2 + end;
                continue;
            }
        }
        // Literal character.
        let ch = rest.chars().next().unwrap();
        clean.push(ch);
        i += ch.len_utf8();
    }
    (clean, spans)
}

fn bold() -> HighlightStyle {
    HighlightStyle { font_weight: Some(FontWeight::BOLD), ..Default::default() }
}

fn italic() -> HighlightStyle {
    HighlightStyle { font_style: Some(FontStyle::Italic), ..Default::default() }
}

fn code_style() -> HighlightStyle {
    HighlightStyle {
        color: Some(theme::accent().into()),
        background_color: Some(gpui::rgba(0x6e_e7ff_22).into()),
        ..Default::default()
    }
}

fn marker_style() -> HighlightStyle {
    HighlightStyle {
        color: Some(theme::accent().into()),
        font_weight: Some(FontWeight::BOLD),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::parse_inline;

    #[test]
    fn strips_markers_and_records_spans() {
        let (clean, spans) = parse_inline("a **b** c `d` e *f*");
        assert_eq!(clean, "a b c d e f");
        // bold "b", code "d", italic "f"
        assert_eq!(spans.len(), 3);
        assert_eq!(&clean[spans[0].0.clone()], "b");
        assert_eq!(&clean[spans[1].0.clone()], "d");
        assert_eq!(&clean[spans[2].0.clone()], "f");
    }

    #[test]
    fn leaves_plain_text_untouched() {
        let (clean, spans) = parse_inline("just text, no markers");
        assert_eq!(clean, "just text, no markers");
        assert!(spans.is_empty());
    }
}
