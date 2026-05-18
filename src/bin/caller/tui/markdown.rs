//! Lightweight markdown-to-ratatui renderer.
//!
//! Supports the subset of markdown that LLM outputs typically produce:
//! - Headers (`#` through `####`)
//! - Bold (`**text**`) and italic (`*text*`)
//! - Inline code (`` `code` ``)
//! - Fenced code blocks (` ``` `)
//! - Unordered list items (`- ` and `* `)
//! - Horizontal rules (`---`)

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::theme;

// Markdown-specific colors (Catppuccin Mocha palette)
const MD_HEADER_FG: Color = Color::Rgb(137, 180, 250); // blue — headers
const MD_CODE_FG: Color = Color::Rgb(166, 227, 161); // green — inline code
const MD_CODE_BLOCK_FG: Color = Color::Rgb(166, 227, 161); // green — code block content
const MD_BOLD_FG: Color = Color::Rgb(205, 214, 244); // text — bold (bright)
const MD_ITALIC_FG: Color = Color::Rgb(180, 190, 254); // lavender — italic
const MD_BULLET_FG: Color = Color::Rgb(249, 226, 175); // yellow — list bullets
const MD_RULE_FG: Color = Color::Rgb(88, 91, 112); // surface2 — horizontal rules

/// Render markdown text into styled ratatui `Line`s.
///
/// `base_style` is applied to plain text (no markdown formatting).
pub fn render_markdown(text: &str, base_style: Style) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut in_code_block = false;

    for raw_line in text.split('\n') {
        // Toggle code blocks on fence lines
        if raw_line.trim_start().starts_with("```") {
            in_code_block = !in_code_block;
            // Render the fence itself dimmed
            lines.push(Line::from(Span::styled(
                raw_line.to_string(),
                Style::default().fg(theme::LOG_DIM_FG),
            )));
            continue;
        }

        if in_code_block {
            lines.push(Line::from(Span::styled(
                raw_line.to_string(),
                Style::default().fg(MD_CODE_BLOCK_FG),
            )));
            continue;
        }

        // Horizontal rule
        let trimmed = raw_line.trim();
        if trimmed.len() >= 3 && trimmed.chars().all(|c| c == '-') {
            lines.push(Line::from(Span::styled(
                "─".repeat(trimmed.len().min(40)),
                Style::default().fg(MD_RULE_FG),
            )));
            continue;
        }

        // Headers
        if let Some(rest) = try_strip_header(trimmed) {
            lines.push(Line::from(Span::styled(
                rest.to_string(),
                Style::default()
                    .fg(MD_HEADER_FG)
                    .add_modifier(Modifier::BOLD),
            )));
            continue;
        }

        // List items: preserve leading whitespace for nesting, style the bullet
        if let Some((indent, bullet, content)) = try_strip_list_item(raw_line) {
            let mut spans = Vec::new();
            if !indent.is_empty() {
                spans.push(Span::styled(indent.to_string(), base_style));
            }
            spans.push(Span::styled(
                bullet.to_string(),
                Style::default()
                    .fg(MD_BULLET_FG)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.extend(parse_inline(content, base_style));
            lines.push(Line::from(spans));
            continue;
        }

        // Regular line with inline formatting
        let spans = parse_inline(raw_line, base_style);
        lines.push(Line::from(spans));
    }

    lines
}

/// Try to strip a markdown header prefix (`# ` through `#### `).
/// Returns the header text without the prefix, or None.
fn try_strip_header(line: &str) -> Option<&str> {
    // Match 1-4 `#` followed by a space
    let bytes = line.as_bytes();
    let mut hashes = 0;
    for &b in bytes {
        if b == b'#' {
            hashes += 1;
        } else {
            break;
        }
    }
    if hashes >= 1 && hashes <= 4 && bytes.get(hashes) == Some(&b' ') {
        Some(&line[hashes + 1..])
    } else {
        None
    }
}

/// Try to match an unordered list item. Returns (indent, bullet, content).
fn try_strip_list_item(line: &str) -> Option<(&str, &str, &str)> {
    let stripped = line.trim_start();
    let indent_len = line.len() - stripped.len();
    let indent = &line[..indent_len];

    if let Some(rest) = stripped.strip_prefix("- ") {
        Some((indent, "- ", rest))
    } else if let Some(rest) = stripped.strip_prefix("* ") {
        Some((indent, "* ", rest))
    } else {
        None
    }
}

/// Parse inline markdown formatting: **bold**, *italic*, `code`.
fn parse_inline(text: &str, base_style: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut chars = text.char_indices().peekable();
    let mut plain_start = 0;

    while let Some(&(i, ch)) = chars.peek() {
        match ch {
            '`' => {
                // Flush plain text before this marker
                if i > plain_start {
                    spans.push(Span::styled(text[plain_start..i].to_string(), base_style));
                }
                chars.next(); // consume opening `
                let content_start = i + 1;
                let mut found_end = false;
                while let Some(&(j, c)) = chars.peek() {
                    if c == '`' {
                        spans.push(Span::styled(
                            text[content_start..j].to_string(),
                            Style::default().fg(MD_CODE_FG),
                        ));
                        chars.next(); // consume closing `
                        plain_start = j + 1;
                        found_end = true;
                        break;
                    }
                    chars.next();
                }
                if !found_end {
                    // No closing ` — treat as plain text
                    spans.push(Span::styled(text[i..].to_string(), base_style));
                    return spans;
                }
            }
            '*' => {
                // Flush plain text before this marker
                if i > plain_start {
                    spans.push(Span::styled(text[plain_start..i].to_string(), base_style));
                }
                chars.next(); // consume first *

                // Check for ** (bold) or *** (bold+italic)
                let is_double = chars.peek().map(|&(_, c)| c == '*').unwrap_or(false);
                if is_double {
                    chars.next(); // consume second *
                    let is_triple = chars.peek().map(|&(_, c)| c == '*').unwrap_or(false);
                    if is_triple {
                        chars.next(); // consume third *
                                      // Bold+italic: find closing ***
                        let content_start = i + 3;
                        if let Some(end) = find_closing_marker(&text[content_start..], "***") {
                            spans.push(Span::styled(
                                text[content_start..content_start + end].to_string(),
                                Style::default()
                                    .fg(MD_BOLD_FG)
                                    .add_modifier(Modifier::BOLD | Modifier::ITALIC),
                            ));
                            // Advance past closing ***
                            let skip_to = content_start + end + 3;
                            while chars.peek().map(|&(j, _)| j < skip_to).unwrap_or(false) {
                                chars.next();
                            }
                            plain_start = skip_to;
                        } else {
                            spans.push(Span::styled("***".to_string(), base_style));
                            plain_start = i + 3;
                        }
                    } else {
                        // Bold: find closing **
                        let content_start = i + 2;
                        if let Some(end) = find_closing_marker(&text[content_start..], "**") {
                            spans.push(Span::styled(
                                text[content_start..content_start + end].to_string(),
                                Style::default().fg(MD_BOLD_FG).add_modifier(Modifier::BOLD),
                            ));
                            let skip_to = content_start + end + 2;
                            while chars.peek().map(|&(j, _)| j < skip_to).unwrap_or(false) {
                                chars.next();
                            }
                            plain_start = skip_to;
                        } else {
                            spans.push(Span::styled("**".to_string(), base_style));
                            plain_start = i + 2;
                        }
                    }
                } else {
                    // Single * — italic: find closing *
                    let content_start = i + 1;
                    if let Some(end) = find_closing_marker(&text[content_start..], "*") {
                        // Make sure we don't match ** (which is bold, not italic+empty)
                        if end > 0 {
                            spans.push(Span::styled(
                                text[content_start..content_start + end].to_string(),
                                Style::default()
                                    .fg(MD_ITALIC_FG)
                                    .add_modifier(Modifier::ITALIC),
                            ));
                            let skip_to = content_start + end + 1;
                            while chars.peek().map(|&(j, _)| j < skip_to).unwrap_or(false) {
                                chars.next();
                            }
                            plain_start = skip_to;
                        } else {
                            spans.push(Span::styled("*".to_string(), base_style));
                            plain_start = i + 1;
                        }
                    } else {
                        spans.push(Span::styled("*".to_string(), base_style));
                        plain_start = i + 1;
                    }
                }
            }
            _ => {
                chars.next();
            }
        }
    }

    // Flush remaining plain text
    if plain_start < text.len() {
        spans.push(Span::styled(text[plain_start..].to_string(), base_style));
    }

    // Ensure we return at least one span (empty line)
    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base_style));
    }

    spans
}

/// Find the byte offset of `marker` in `text`, returning None if not found.
fn find_closing_marker(text: &str, marker: &str) -> Option<usize> {
    text.find(marker)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain_style() -> Style {
        Style::default().fg(Color::White)
    }

    fn spans_text(spans: &[Span]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn lines_text(lines: &[Line]) -> String {
        lines
            .iter()
            .map(|l| spans_text(&l.spans))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn plain_text_passthrough() {
        let lines = render_markdown("hello world", plain_style());
        assert_eq!(lines.len(), 1);
        assert_eq!(spans_text(&lines[0].spans), "hello world");
    }

    #[test]
    fn bold_text() {
        let lines = render_markdown("before **bold** after", plain_style());
        assert_eq!(lines.len(), 1);
        assert_eq!(spans_text(&lines[0].spans), "before bold after");
        // The bold span should have BOLD modifier
        assert!(lines[0].spans[1]
            .style
            .add_modifier
            .contains(Modifier::BOLD));
    }

    #[test]
    fn italic_text() {
        let lines = render_markdown("before *italic* after", plain_style());
        assert_eq!(lines.len(), 1);
        assert_eq!(spans_text(&lines[0].spans), "before italic after");
        assert!(lines[0].spans[1]
            .style
            .add_modifier
            .contains(Modifier::ITALIC));
    }

    #[test]
    fn inline_code() {
        let lines = render_markdown("run `cargo test` now", plain_style());
        assert_eq!(lines.len(), 1);
        assert_eq!(spans_text(&lines[0].spans), "run cargo test now");
        assert_eq!(lines[0].spans[1].style.fg, Some(MD_CODE_FG));
    }

    #[test]
    fn header_rendering() {
        let lines = render_markdown("# Title\n## Subtitle", plain_style());
        assert_eq!(lines.len(), 2);
        assert_eq!(spans_text(&lines[0].spans), "Title");
        assert_eq!(spans_text(&lines[1].spans), "Subtitle");
        assert!(lines[0].spans[0]
            .style
            .add_modifier
            .contains(Modifier::BOLD));
        assert_eq!(lines[0].spans[0].style.fg, Some(MD_HEADER_FG));
    }

    #[test]
    fn list_items() {
        let lines = render_markdown("- first\n- second", plain_style());
        assert_eq!(lines.len(), 2);
        assert_eq!(lines_text(&lines), "- first\n- second");
        // Bullet should be styled differently from content
        assert_eq!(lines[0].spans[0].style.fg, Some(MD_BULLET_FG));
    }

    #[test]
    fn code_block() {
        let md = "before\n```\nlet x = 1;\n```\nafter";
        let lines = render_markdown(md, plain_style());
        assert_eq!(lines.len(), 5);
        // Code block content should be green
        assert_eq!(lines[2].spans[0].style.fg, Some(MD_CODE_BLOCK_FG));
        assert_eq!(spans_text(&lines[2].spans), "let x = 1;");
    }

    #[test]
    fn horizontal_rule() {
        let lines = render_markdown("above\n---\nbelow", plain_style());
        assert_eq!(lines.len(), 3);
        // Rule should be rendered as box-drawing chars
        assert!(spans_text(&lines[1].spans).contains('─'));
    }

    #[test]
    fn bold_italic() {
        let lines = render_markdown("***bold italic***", plain_style());
        assert_eq!(lines.len(), 1);
        assert_eq!(spans_text(&lines[0].spans), "bold italic");
        let style = lines[0].spans[0].style;
        assert!(style.add_modifier.contains(Modifier::BOLD));
        assert!(style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn unclosed_markers_are_plain() {
        let lines = render_markdown("open **bold but no close", plain_style());
        assert_eq!(lines.len(), 1);
        // Should not panic, text is preserved
        assert_eq!(spans_text(&lines[0].spans), "open **bold but no close");
    }

    #[test]
    fn nested_list_indentation() {
        let lines = render_markdown("- top\n  - nested", plain_style());
        assert_eq!(lines.len(), 2);
        assert_eq!(lines_text(&lines), "- top\n  - nested");
        // Nested bullet has indent span
        assert_eq!(lines[1].spans[0].content.as_ref(), "  ");
    }

    #[test]
    fn mixed_inline() {
        let lines = render_markdown("use **bold** and `code` together", plain_style());
        assert_eq!(lines.len(), 1);
        assert_eq!(spans_text(&lines[0].spans), "use bold and code together");
    }

    #[test]
    fn empty_input() {
        let lines = render_markdown("", plain_style());
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn multiline_plain() {
        let lines = render_markdown("line one\nline two\nline three", plain_style());
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn header_levels() {
        let lines = render_markdown(
            "# H1\n## H2\n### H3\n#### H4\n##### not header",
            plain_style(),
        );
        assert_eq!(lines.len(), 5);
        // H1-H4 should be headers
        for l in &lines[..4] {
            assert_eq!(l.spans[0].style.fg, Some(MD_HEADER_FG));
        }
        // H5 (5 hashes) is not a header
        assert_ne!(lines[4].spans[0].style.fg, Some(MD_HEADER_FG));
    }
}
