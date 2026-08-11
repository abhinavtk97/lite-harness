//! Turns Markdown-formatted text into styled ratatui `Line`s -- pure
//! rendering, no I/O, called from `ui::transcript_lines` the same way
//! plain-text transcript items already are. Agent responses commonly use
//! Markdown (headings, bold/italic, fenced code blocks, lists), and
//! rendering the literal `#`/`**`/`` ` `` characters instead of styling
//! reads far worse than any modern chat UI a terminal user has used.
//!
//! `App` itself is untouched by this -- it keeps accumulating raw Markdown
//! text into `TranscriptItem::Agent`/`Thought` exactly as before, and
//! parsing happens fresh every frame in `ui::draw`, matching this crate's
//! existing "rendering is a pure function of state" discipline (no derived
//! state cached on `App` that could drift from the text it was derived
//! from).

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Parses `text` as Markdown and renders it as styled lines, `base_style`
/// applied as the starting point every span inherits from (e.g. the
/// existing per-source-session tag color) before Markdown's own emphasis/
/// heading/code styling layers on top.
pub fn render(text: &str, base_style: Style) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut style_stack: Vec<Style> = vec![base_style];
    let mut in_code_block = false;
    // `None` = bullet list, `Some(n)` = ordered list, next number `n`.
    let mut list_stack: Vec<Option<u64>> = Vec::new();

    let options = Options::ENABLE_STRIKETHROUGH;
    for event in Parser::new_ext(text, options) {
        let style = *style_stack.last().unwrap_or(&base_style);
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    flush_line(&mut lines, &mut current);
                    style_stack.push(style.fg(heading_color(level)).add_modifier(Modifier::BOLD));
                }
                Tag::Emphasis => style_stack.push(style.add_modifier(Modifier::ITALIC)),
                Tag::Strong => style_stack.push(style.add_modifier(Modifier::BOLD)),
                Tag::Strikethrough => style_stack.push(style.add_modifier(Modifier::CROSSED_OUT)),
                Tag::BlockQuote(_) => {
                    flush_line(&mut lines, &mut current);
                    style_stack.push(style.fg(Color::DarkGray).add_modifier(Modifier::ITALIC));
                    current.push(Span::styled("> ", style_stack[style_stack.len() - 1]));
                }
                Tag::CodeBlock(kind) => {
                    flush_line(&mut lines, &mut current);
                    in_code_block = true;
                    style_stack.push(Style::default().fg(Color::Green));
                    if let CodeBlockKind::Fenced(lang) = kind {
                        if !lang.is_empty() {
                            lines.push(Line::from(Span::styled(
                                format!("```{lang}"),
                                Style::default().fg(Color::DarkGray),
                            )));
                        }
                    }
                }
                Tag::List(start) => list_stack.push(start),
                Tag::Item => {
                    flush_line(&mut lines, &mut current);
                    let indent = "  ".repeat(list_stack.len().saturating_sub(1));
                    let marker = match list_stack.last_mut() {
                        Some(Some(n)) => {
                            let m = format!("{indent}{n}. ");
                            *n += 1;
                            m
                        }
                        _ => format!("{indent}- "),
                    };
                    current.push(Span::styled(marker, style));
                }
                _ => {}
            },
            Event::End(tag_end) => match tag_end {
                // Block-level: a heading always ends its own line.
                TagEnd::Heading(_) => {
                    flush_line(&mut lines, &mut current);
                    style_stack.pop();
                }
                // Inline: emphasis/strong/strikethrough just pop the style
                // -- they must NOT flush, or "**bold** and *italic*" would
                // wrongly split into three separate lines instead of
                // staying one line with differently-styled spans.
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                    style_stack.pop();
                }
                TagEnd::BlockQuote(_) => {
                    flush_line(&mut lines, &mut current);
                    style_stack.pop();
                }
                TagEnd::CodeBlock => {
                    flush_line(&mut lines, &mut current);
                    style_stack.pop();
                    in_code_block = false;
                }
                TagEnd::Item => flush_line(&mut lines, &mut current),
                TagEnd::List(_) => {
                    list_stack.pop();
                }
                TagEnd::Paragraph => {
                    flush_line(&mut lines, &mut current);
                    lines.push(Line::from(""));
                }
                _ => {}
            },
            Event::Text(text) => {
                if in_code_block {
                    let mut segments = text.split('\n');
                    if let Some(first) = segments.next() {
                        current.push(Span::styled(first.to_string(), style));
                    }
                    for segment in segments {
                        flush_line(&mut lines, &mut current);
                        current.push(Span::styled(segment.to_string(), style));
                    }
                } else {
                    current.push(Span::styled(text.to_string(), style));
                }
            }
            Event::Code(code) => {
                current.push(Span::styled(format!(" {code} "), style.fg(Color::Green)));
            }
            Event::SoftBreak => current.push(Span::styled(" ", style)),
            Event::HardBreak => flush_line(&mut lines, &mut current),
            Event::Rule => {
                flush_line(&mut lines, &mut current);
                lines.push(Line::from(Span::styled("\u{2500}".repeat(40), Style::default().fg(Color::DarkGray))));
            }
            _ => {}
        }
    }
    flush_line(&mut lines, &mut current);

    // A trailing blank paragraph-separator line (or several, if the text
    // ended mid-block) reads as dead space at the bottom of the bubble --
    // trimmed rather than left for the transcript's own line-counting
    // scroll math (`ui::draw_transcript`) to count as real content.
    while lines.last().is_some_and(is_blank) {
        lines.pop();
    }
    if lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines
}

fn flush_line(lines: &mut Vec<Line<'static>>, current: &mut Vec<Span<'static>>) {
    if !current.is_empty() {
        lines.push(Line::from(std::mem::take(current)));
    }
}

fn is_blank(line: &Line) -> bool {
    line.spans.iter().all(|s| s.content.trim().is_empty())
}

fn heading_color(level: HeadingLevel) -> Color {
    match level {
        HeadingLevel::H1 | HeadingLevel::H2 => Color::Cyan,
        _ => Color::White,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(base: Style) -> Style {
        base
    }

    fn text_of(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn render_plain(md: &str) -> Vec<Line<'static>> {
        render(md, plain(Style::default()))
    }

    #[test]
    fn plain_text_with_no_markdown_renders_as_a_single_unstyled_line() {
        let lines = render_plain("just some text");
        assert_eq!(lines.len(), 1);
        assert_eq!(text_of(&lines[0]), "just some text");
    }

    #[test]
    fn a_heading_is_bold_and_on_its_own_line() {
        let lines = render_plain("# Title\n\nbody text");
        let heading = lines.iter().find(|l| text_of(l) == "Title").expect("heading line");
        assert!(heading.spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert!(lines.iter().any(|l| text_of(l) == "body text"));
    }

    #[test]
    fn bold_and_italic_spans_carry_the_right_modifiers() {
        let lines = render_plain("**bold** and *italic*");
        let line = &lines[0];
        let bold_span = line.spans.iter().find(|s| s.content.as_ref() == "bold").expect("bold span");
        assert!(bold_span.style.add_modifier.contains(Modifier::BOLD));
        let italic_span = line.spans.iter().find(|s| s.content.as_ref() == "italic").expect("italic span");
        assert!(italic_span.style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn inline_code_is_styled_distinctly_from_surrounding_text() {
        let lines = render_plain("run `echo hi` now");
        let line = &lines[0];
        let code_span = line.spans.iter().find(|s| s.content.contains("echo hi")).expect("code span");
        assert_eq!(code_span.style.fg, Some(Color::Green));
    }

    #[test]
    fn a_fenced_code_block_preserves_every_line_and_the_language_tag() {
        let lines = render_plain("```rust\nfn main() {}\nlet x = 1;\n```");
        let joined: Vec<String> = lines.iter().map(text_of).collect();
        assert!(joined.iter().any(|l| l.contains("```rust")));
        assert!(joined.iter().any(|l| l == "fn main() {}"));
        assert!(joined.iter().any(|l| l == "let x = 1;"));
    }

    #[test]
    fn a_code_block_with_no_language_tag_has_no_language_line() {
        let lines = render_plain("```\nplain block\n```");
        let joined: Vec<String> = lines.iter().map(text_of).collect();
        assert!(!joined.iter().any(|l| l.starts_with("```") && l.len() > 3));
        assert!(joined.iter().any(|l| l == "plain block"));
    }

    #[test]
    fn a_bullet_list_prefixes_each_item_with_a_dash() {
        let lines = render_plain("- first\n- second");
        let joined: Vec<String> = lines.iter().map(text_of).collect();
        assert!(joined.iter().any(|l| l == "- first"));
        assert!(joined.iter().any(|l| l == "- second"));
    }

    #[test]
    fn an_ordered_list_numbers_its_items_in_order() {
        let lines = render_plain("1. first\n2. second\n3. third");
        let joined: Vec<String> = lines.iter().map(text_of).collect();
        assert!(joined.iter().any(|l| l == "1. first"));
        assert!(joined.iter().any(|l| l == "2. second"));
        assert!(joined.iter().any(|l| l == "3. third"));
    }

    #[test]
    fn a_blockquote_is_prefixed_and_dimmed() {
        let lines = render_plain("> quoted text");
        let line = lines.iter().find(|l| text_of(l).contains("quoted text")).expect("quote line");
        assert!(text_of(line).starts_with("> "));
        assert_eq!(line.spans[0].style.fg, Some(Color::DarkGray));
    }

    #[test]
    fn strikethrough_text_carries_the_crossed_out_modifier() {
        let lines = render_plain("~~gone~~");
        let span = lines[0].spans.iter().find(|s| s.content.as_ref() == "gone").expect("strikethrough span");
        assert!(span.style.add_modifier.contains(Modifier::CROSSED_OUT));
    }

    #[test]
    fn a_horizontal_rule_renders_as_a_visible_divider_line() {
        let lines = render_plain("above\n\n---\n\nbelow");
        assert!(lines.iter().any(|l| text_of(l).contains('\u{2500}')));
    }

    #[test]
    fn empty_text_renders_as_one_blank_line_not_zero_lines() {
        let lines = render_plain("");
        assert_eq!(lines.len(), 1);
        assert_eq!(text_of(&lines[0]), "");
    }

    #[test]
    fn no_trailing_blank_lines_are_left_after_a_paragraph() {
        let lines = render_plain("hello");
        assert!(!lines.last().unwrap().spans.is_empty() || text_of(lines.last().unwrap()) == "hello");
        assert_eq!(text_of(lines.last().unwrap()), "hello");
    }

    #[test]
    fn base_style_is_inherited_by_plain_text_spans() {
        let base = Style::default().fg(Color::Magenta);
        let lines = render("plain", base);
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Magenta));
    }

    #[test]
    fn a_hard_break_starts_a_new_line_within_the_same_paragraph() {
        let lines = render_plain("first line  \nsecond line");
        let joined: Vec<String> = lines.iter().map(text_of).collect();
        assert!(joined.iter().any(|l| l == "first line"));
        assert!(joined.iter().any(|l| l == "second line"));
    }
}
