//! Pure rendering: `draw` is a function of `&App` only, no I/O, no mutation
//! -- everything it needs to know is already in `App`'s fields. This is
//! what lets it be tested against `ratatui::backend::TestBackend` with no
//! real terminal and no daemon connection (see `tests` below).

use lh_event::{ChildKind, ChildOutcome, PermissionAction, PlanStepStatus};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{describe_permission_action, App, ConnPhase, PendingPermission, PermissionChoice, TranscriptItem};

/// Highest input box height (in text lines, before the +2 for borders) --
/// a multi-line prompt can grow the input box, but not so far that it
/// crowds out the transcript entirely.
const MAX_INPUT_LINES: u16 = 5;

/// Fixed sidebar width -- wide enough for a plan step or a session id
/// fragment without wrapping every other line, narrow enough to leave the
/// transcript as the dominant pane.
const SIDEBAR_WIDTH: u16 = 30;

pub fn draw(frame: &mut Frame, app: &App) {
    let input_lines = (app.input.line_count() as u16).min(MAX_INPUT_LINES);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(input_lines + 2), Constraint::Length(1)])
        .split(frame.area());

    draw_main(frame, chunks[0], app);
    draw_input(frame, chunks[1], app);
    draw_status(frame, chunks[2], app);

    // The permission modal takes priority when both could apply (it can't
    // happen in practice -- typing is disabled while a permission is
    // pending, see `App::input_enabled` -- but the dropdown reads straight
    // from `app.input`, not from whether input is currently enabled, so this
    // guard is what actually prevents the overlap rather than relying on
    // that indirectly).
    if app.pending_permission.is_none() {
        draw_autocomplete_dropdown(frame, chunks[1], app);
    }

    // Drawn last, over everything else -- a modal, not another pane.
    if let Some(pending) = &app.pending_permission {
        draw_permission_modal(frame, frame.area(), pending);
    }
}

/// A small popup listing the slash commands matching what's typed so far,
/// anchored directly above the input box -- `input_area` is that box's own
/// `Rect` (`chunks[1]` from `draw`), used both to align the popup's left edge
/// and to place it just above rather than duplicating the input's own
/// position math.
fn draw_autocomplete_dropdown(frame: &mut Frame, input_area: Rect, app: &App) {
    if app.autocomplete_dismissed {
        return;
    }
    let candidates = crate::app::command_candidates(&app.input.text());
    if candidates.is_empty() {
        return;
    }
    let height = (candidates.len() as u16 + 2).min(input_area.y);
    if height == 0 {
        return;
    }
    let popup = Rect {
        x: input_area.x,
        y: input_area.y - height,
        width: input_area.width.min(60),
        height,
    };
    frame.render_widget(Clear, popup);

    let selected = app.autocomplete_selected.min(candidates.len() - 1);
    let lines: Vec<Line> = candidates
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let style = if i == selected {
                Style::default().fg(Color::Black).bg(Color::White).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Line::from(Span::styled(format!("/{}  {}", c.name, c.usage), style))
        })
        .collect();
    let block = Block::default().borders(Borders::ALL).title(" commands ");
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

/// A `Rect` centered within `area`, `percent_x`/`percent_y` of its size --
/// the standard ratatui popup-centering recipe.
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

/// The modal's outer popup `Rect` -- shared with `mouse.rs` so a click's
/// hit-test uses the exact same bounds `draw` rendered into, rather than a
/// second, possibly-drifting computation of "where the modal is".
pub(crate) fn permission_popup_rect(area: Rect) -> Rect {
    centered_rect(70, 60, area)
}

/// Splits the modal's inner (post-border) area into (content, options row,
/// hint row) -- the options row and hint row are always exactly 1 row
/// each, pinned to the *bottom* via `Constraint::Min` on the content area
/// above them, so a long action description or a big diff can never push
/// the options off-screen (they'd just get clipped themselves, which is
/// the point: the options stay visible either way). Fixed-height rows
/// also mean their bounds are deterministic regardless of how much text
/// content contains -- exactly what `mouse.rs` needs for a reliable click
/// hit-test without duplicating `content`'s word-wrap math.
pub(crate) fn permission_modal_rows(popup: Rect) -> (Rect, Rect, Rect) {
    let inner = Block::default().borders(Borders::ALL).inner(popup);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1), Constraint::Length(1)])
        .split(inner);
    (rows[0], rows[1], rows[2])
}

/// The options row split into `PermissionChoice::ALL.len()` equal columns,
/// in the same order the choices are drawn in -- index `i` here is choice
/// `PermissionChoice::ALL[i]`.
pub(crate) fn permission_option_rects(options_row: Rect) -> Vec<Rect> {
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints(vec![Constraint::Ratio(1, PermissionChoice::ALL.len() as u32); PermissionChoice::ALL.len()])
        .split(options_row)
        .to_vec()
}

fn draw_permission_modal(frame: &mut Frame, area: Rect, pending: &PendingPermission) {
    let popup = permission_popup_rect(area);
    // Without this, the modal would be alpha-blended onto whatever the
    // transcript/sidebar already drew there -- `Clear` erases the cells
    // first so the popup reads as solid, not a ghostly overlay.
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(" permission requested ");
    frame.render_widget(block, popup);

    let (content_area, options_row, hint_row) = permission_modal_rows(popup);

    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(
            format!("{:?} risk -- {}", pending.request.risk_tier, crate::app::source_label(&pending.request.tool_source)),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(describe_permission_action(&pending.request.action)),
    ];
    if let PermissionAction::FileWrite { diff_summary: Some(diff), .. } = &pending.request.action {
        lines.push(Line::from(""));
        for diff_line in diff.lines() {
            lines.push(Line::from(Span::styled(diff_line.to_string(), Style::default().fg(Color::DarkGray))));
        }
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), content_area);

    let option_rects = permission_option_rects(options_row);
    for (i, choice) in PermissionChoice::ALL.iter().enumerate() {
        let style = if i == pending.selected {
            Style::default().fg(Color::Black).bg(Color::White).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let label = Paragraph::new(Line::from(Span::styled(choice.label(), style))).alignment(Alignment::Center);
        frame.render_widget(label, option_rects[i]);
    }

    let hint = Paragraph::new(Line::from(Span::styled(
        "click or left/right to choose, enter to confirm (y/n/a/d shortcuts)",
        Style::default().fg(Color::DarkGray),
    )))
    .alignment(Alignment::Center);
    frame.render_widget(hint, hint_row);
}

/// The sidebar is additive: with nothing to show yet (no plan, no child
/// sessions) the transcript alone fills the whole row, exactly like before
/// this phase -- it only appears once there's something worth showing.
fn draw_main(frame: &mut Frame, area: Rect, app: &App) {
    if app.plan_steps.is_empty() && app.child_sessions.is_empty() && app.background_bash.is_empty() {
        draw_transcript(frame, area, app);
        return;
    }

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(20), Constraint::Length(SIDEBAR_WIDTH)])
        .split(area);
    draw_transcript(frame, cols[0], app);
    draw_sidebar(frame, cols[1], app);
}

fn draw_sidebar(frame: &mut Frame, area: Rect, app: &App) {
    let mut lines: Vec<Line> = Vec::new();

    if !app.plan_steps.is_empty() {
        lines.push(Line::from(Span::styled("plan", Style::default().add_modifier(Modifier::BOLD))));
        for step in &app.plan_steps {
            let (mark, style) = match step.status {
                PlanStepStatus::Completed => ("[x]", Style::default().fg(Color::Green)),
                PlanStepStatus::InProgress => ("[.]", Style::default().fg(Color::Yellow)),
                PlanStepStatus::Pending => ("[ ]", Style::default().fg(Color::DarkGray)),
            };
            lines.push(Line::from(Span::styled(format!("{mark} {}", step.description), style)));
        }
    }

    if !app.child_sessions.is_empty() {
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled("sessions", Style::default().add_modifier(Modifier::BOLD))));
        for child in &app.child_sessions {
            let label = match &child.kind {
                ChildKind::NativeSubagent { role } => format!("subagent:{role}"),
                ChildKind::Delegated { agent } => format!("acp:{agent:?}"),
            };
            let (mark, style) = match &child.outcome {
                None => ("...", Style::default().fg(Color::Yellow)),
                Some(ChildOutcome::Success { .. }) => ("ok", Style::default().fg(Color::Green)),
                Some(ChildOutcome::Failed { .. }) => ("fail", Style::default().fg(Color::Red)),
                Some(ChildOutcome::Cancelled) => ("cancelled", Style::default().fg(Color::DarkGray)),
            };
            lines.push(Line::from(Span::styled(format!("{mark} {label}"), style)));
        }
    }

    if !app.background_bash.is_empty() {
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled("background", Style::default().add_modifier(Modifier::BOLD))));
        for proc in &app.background_bash {
            let (mark, style) = if proc.running {
                ("running", Style::default().fg(Color::Yellow))
            } else {
                ("done", Style::default().fg(Color::Green))
            };
            // Bash ids are full UUIDs (see `handle_one_tool_call`'s
            // `bash_background` arm) -- an 8-char prefix is plenty to tell
            // entries apart in a 30-column sidebar without wrapping.
            let short_id = proc.id.get(..8).unwrap_or(&proc.id);
            lines.push(Line::from(Span::styled(format!("{mark} {short_id}"), style)));
        }
    }

    let block = Block::default().borders(Borders::ALL).title(" activity ");
    let paragraph = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn draw_transcript(frame: &mut Frame, area: Rect, app: &App) {
    let lines: Vec<Line> = app.transcript.iter().flat_map(transcript_lines).collect();
    // Counts *logical* (pre-wrap) lines -- accurate scroll math for lines
    // that fit within the terminal width; a long line that soft-wraps
    // (`Wrap` below) renders as more visual rows than this counts, so
    // scrolling near a wrapped line is approximate, not exact. Precise
    // wrap-aware scrolling would need reimplementing ratatui's own
    // wrapping calculation just to count rows -- not worth it for what's
    // still a readable, correct-direction scroll experience.
    let total = lines.len() as u16;
    // Borders eat 2 rows; `area.height` can theoretically be 0 or 1 in a
    // very small terminal, so this is saturating throughout rather than
    // assuming there's always room for content.
    let viewport = area.height.saturating_sub(2);
    let max_scroll = total.saturating_sub(viewport);
    // `app.transcript_scroll` counts lines scrolled *up* from the bottom;
    // ratatui's `Paragraph::scroll` counts lines skipped from the *top* --
    // converting here is what lets `App` stay ignorant of screen height.
    let scroll_from_top = max_scroll.saturating_sub(app.transcript_scroll.min(max_scroll));

    let block = Block::default().borders(Borders::ALL).title(" lite-harness ");
    let paragraph = Paragraph::new(lines).block(block).wrap(Wrap { trim: false }).scroll((scroll_from_top, 0));
    frame.render_widget(paragraph, area);
}

fn transcript_lines(item: &TranscriptItem) -> Vec<Line<'static>> {
    match item {
        // The user's own typed text is rendered literally, not parsed as
        // Markdown -- it's plain intent, not authored/formatted content,
        // and re-flowing exactly what someone just typed would surprise
        // them more than it'd help.
        TranscriptItem::User(text) => wrapped_lines(text, "you", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        TranscriptItem::Agent(text) => markdown_lines(text, "", Style::default()),
        TranscriptItem::Thought(text) => markdown_lines(text, "thinking", Style::default().fg(Color::DarkGray)),
        TranscriptItem::ToolCall { tool_name, source } => {
            vec![Line::from(Span::styled(
                format!("  -> {tool_name} [{source}]"),
                Style::default().fg(Color::Yellow),
            ))]
        }
        TranscriptItem::ToolCallUpdate { status } => {
            vec![Line::from(Span::styled(
                format!("  <- {status:?}"),
                Style::default().fg(Color::Yellow),
            ))]
        }
        TranscriptItem::System(text) => vec![Line::from(Span::styled(
            format!("  [{text}]"),
            Style::default().fg(Color::DarkGray),
        ))],
        TranscriptItem::Error(text) => vec![Line::from(Span::styled(
            format!("  [error] {text}"),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ))],
    }
}

fn wrapped_lines(text: &str, label: &str, style: Style) -> Vec<Line<'static>> {
    let prefix = if label.is_empty() { String::new() } else { format!("{label}: ") };
    vec![Line::from(Span::styled(format!("{prefix}{text}"), style))]
}

/// Renders `text` as Markdown (see `markdown::render`) and prepends `label`
/// (if any) as its own bold span on the first line only -- everything
/// after that first line is the message's own content, unprefixed, same
/// as how a chat UI's "who's speaking" label only ever appears once per
/// message, not once per line.
fn markdown_lines(text: &str, label: &str, base_style: Style) -> Vec<Line<'static>> {
    let mut lines = crate::markdown::render(text, base_style);
    if !label.is_empty() {
        if let Some(first) = lines.first_mut() {
            first.spans.insert(0, Span::styled(format!("{label}: "), base_style.add_modifier(Modifier::BOLD)));
        }
    }
    lines
}

fn draw_input(frame: &mut Frame, area: Rect, app: &App) {
    // The permission modal (drawn on top, see `draw`) is the actual UI for
    // that decision now -- this just needs to read as "not your turn" the
    // same way any other non-editable state does, not duplicate the modal's
    // own prompt.
    let title = if !app.input_enabled() {
        " waiting... "
    } else {
        " prompt (Enter to send, Alt+Enter for a new line) "
    };
    let style = if app.input_enabled() {
        Style::default()
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let paragraph = Paragraph::new(app.input.text()).style(style).block(block);
    frame.render_widget(paragraph, area);

    if app.input_enabled() {
        let (col, row) = app.input.cursor_row_col();
        frame.set_cursor_position((area.x + 1 + col, area.y + 1 + row));
    }
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let session = app
        .session_id
        .map(|id| id.to_string())
        .unwrap_or_else(|| "-".to_string());
    let phase = match app.phase {
        ConnPhase::Connecting => "connecting",
        ConnPhase::Ready => "ready",
    };
    let cost = app
        .last_ledger
        .as_ref()
        .map(|rollup| match rollup.cost_usd {
            Some(usd) => format!("${usd:.4}"),
            None => "$?".to_string(),
        })
        .unwrap_or_else(|| "-".to_string());
    let line = Line::from(vec![
        Span::styled(format!(" {phase} "), Style::default().fg(Color::Green)),
        Span::raw(format!("| session {session} | cost {cost} | {} | Ctrl+C to quit", app.status)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use super::*;

    fn render(app: &App) -> TestBackend {
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        terminal.backend().clone()
    }

    fn buffer_contains(backend: &TestBackend, needle: &str) -> bool {
        let buffer = backend.buffer();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text.contains(needle)
    }

    #[test]
    fn connecting_state_shows_a_connecting_status() {
        let app = App::new();
        let backend = render(&app);
        assert!(buffer_contains(&backend, "connecting"));
    }

    #[test]
    fn a_user_message_renders_with_a_you_label() {
        let mut app = App::new();
        app.phase = ConnPhase::Ready;
        app.push_user_message("hello there".to_string());
        let backend = render(&app);
        assert!(buffer_contains(&backend, "you: hello there"));
    }

    #[test]
    fn an_agent_message_renders_its_markdown_formatting_not_literal_syntax_characters() {
        let mut app = App::new();
        app.transcript.push(TranscriptItem::Agent("# Heading\n\nsome **bold** text".to_string()));
        let backend = render(&app);
        assert!(buffer_contains(&backend, "Heading"));
        assert!(buffer_contains(&backend, "bold"));
        assert!(!buffer_contains(&backend, "**bold**"), "the ** markers should be styling, not literal characters");
        assert!(!buffer_contains(&backend, "# Heading"), "the # marker should be styling, not a literal character");
    }

    #[test]
    fn a_thought_message_still_carries_its_thinking_label_alongside_markdown_rendering() {
        let mut app = App::new();
        app.transcript.push(TranscriptItem::Thought("plain thought".to_string()));
        let backend = render(&app);
        assert!(buffer_contains(&backend, "thinking: plain thought"));
    }

    #[test]
    fn an_error_item_renders_with_an_error_label() {
        let mut app = App::new();
        app.push_error("boom");
        let backend = render(&app);
        assert!(buffer_contains(&backend, "[error] boom"));
    }

    #[test]
    fn every_remaining_transcript_item_variant_renders_something_recognizable() {
        let mut app = App::new();
        app.transcript.push(TranscriptItem::Agent("hi there".to_string()));
        app.transcript.push(TranscriptItem::Thought("hmm".to_string()));
        app.transcript.push(TranscriptItem::ToolCall { tool_name: "bash".to_string(), source: "native:bash".to_string() });
        app.transcript.push(TranscriptItem::ToolCallUpdate { status: lh_event::ToolCallStatus::Completed });
        app.push_system("a system line");

        let backend = render(&app);
        assert!(buffer_contains(&backend, "hi there"));
        assert!(buffer_contains(&backend, "thinking: hmm"));
        assert!(buffer_contains(&backend, "bash [native:bash]"));
        assert!(buffer_contains(&backend, "Completed"));
        assert!(buffer_contains(&backend, "a system line"));
    }

    #[test]
    fn a_pending_permission_switches_the_input_box_to_a_waiting_title() {
        let mut app = App::new();
        app.phase = ConnPhase::Ready;
        app.pending_permission =
            Some(crate::app::PendingPermission { request_id: -1, request: crate::app::fake_permission_request(), selected: 0 });
        let backend = render(&app);
        assert!(buffer_contains(&backend, "waiting..."), "the modal is the real UI now, input just reads not-your-turn");
    }

    #[test]
    fn a_pending_permission_renders_a_modal_with_the_request_and_deny_highlighted_by_default() {
        let mut app = App::new();
        app.phase = ConnPhase::Ready;
        app.pending_permission =
            Some(crate::app::PendingPermission { request_id: -1, request: crate::app::fake_permission_request(), selected: 0 });
        let backend = render(&app);
        assert!(buffer_contains(&backend, "permission requested"));
        assert!(buffer_contains(&backend, "execute: echo hi"));
        assert!(buffer_contains(&backend, "Allow"));
        assert!(buffer_contains(&backend, "Always Allow"));
        assert!(buffer_contains(&backend, "Always Deny"));
    }

    #[test]
    fn a_permission_modal_shows_the_diff_summary_for_a_file_write() {
        let mut app = App::new();
        app.phase = ConnPhase::Ready;
        let mut request = crate::app::fake_permission_request();
        request.action = lh_event::PermissionAction::FileWrite {
            path: "src/main.rs".into(),
            diff_summary: Some("-old line\n+new line".to_string()),
        };
        app.pending_permission = Some(crate::app::PendingPermission { request_id: -1, request, selected: 0 });
        let backend = render(&app);
        assert!(buffer_contains(&backend, "write src/main.rs"));
        assert!(buffer_contains(&backend, "-old line"));
        assert!(buffer_contains(&backend, "+new line"));
    }

    #[test]
    fn a_permission_modal_with_no_diff_summary_shows_no_diff_lines() {
        let mut app = App::new();
        app.phase = ConnPhase::Ready;
        let mut request = crate::app::fake_permission_request();
        request.action = lh_event::PermissionAction::FileWrite { path: "src/main.rs".into(), diff_summary: None };
        app.pending_permission = Some(crate::app::PendingPermission { request_id: -1, request, selected: 0 });
        let backend = render(&app);
        assert!(buffer_contains(&backend, "write src/main.rs"));
        assert!(!buffer_contains(&backend, "-old line"));
    }

    #[test]
    fn a_multiline_input_grows_the_input_box_and_both_lines_are_visible() {
        let mut app = App::new();
        app.phase = ConnPhase::Ready;
        for c in "first line".chars() {
            app.input.insert_char(c);
        }
        app.input.insert_newline();
        for c in "second line".chars() {
            app.input.insert_char(c);
        }
        let backend = render(&app);
        assert!(buffer_contains(&backend, "first line"));
        assert!(buffer_contains(&backend, "second line"));
    }

    #[test]
    fn typing_a_slash_shows_the_autocomplete_dropdown_with_every_command() {
        let mut app = App::new();
        app.phase = ConnPhase::Ready;
        app.input.insert_char('/');
        let backend = render(&app);
        assert!(buffer_contains(&backend, "commands"));
        assert!(buffer_contains(&backend, "/help"));
        assert!(buffer_contains(&backend, "/quit"));
        assert!(buffer_contains(&backend, "/agents"));
        assert!(buffer_contains(&backend, "/delegate"));
    }

    #[test]
    fn the_autocomplete_dropdown_narrows_to_matching_commands_as_you_type() {
        let mut app = App::new();
        app.phase = ConnPhase::Ready;
        for c in "/de".chars() {
            app.input.insert_char(c);
        }
        let backend = render(&app);
        assert!(buffer_contains(&backend, "/delegate"));
        assert!(!buffer_contains(&backend, "/help"), "only the matching command should be listed");
    }

    #[test]
    fn the_autocomplete_dropdown_stays_hidden_for_plain_text_input() {
        let mut app = App::new();
        app.phase = ConnPhase::Ready;
        for c in "hello there".chars() {
            app.input.insert_char(c);
        }
        let backend = render(&app);
        assert!(!buffer_contains(&backend, "commands"), "no leading slash -- nothing to complete");
    }

    #[test]
    fn the_autocomplete_dropdown_disappears_once_dismissed() {
        let mut app = App::new();
        app.phase = ConnPhase::Ready;
        app.input.insert_char('/');
        app.autocomplete_dismissed = true;
        let backend = render(&app);
        assert!(!buffer_contains(&backend, "commands"));
    }

    #[test]
    fn the_autocomplete_dropdown_stays_hidden_while_a_permission_is_pending() {
        let mut app = App::new();
        app.phase = ConnPhase::Ready;
        app.input.insert_char('/');
        app.pending_permission =
            Some(crate::app::PendingPermission { request_id: -1, request: crate::app::fake_permission_request(), selected: 0 });
        let backend = render(&app);
        assert!(!buffer_contains(&backend, " commands "), "the permission modal owns the screen, not the dropdown");
    }

    #[test]
    fn scrolling_up_reveals_older_content_and_hides_the_newest() {
        let mut app = App::new();
        app.phase = ConnPhase::Ready;
        // The 20-row TestBackend has well under 30 rows of transcript
        // space once the input box and status bar are subtracted, so 30
        // distinct system lines guarantees the transcript overflows and
        // scrolling actually has something to reveal.
        for i in 0..30 {
            app.push_system(format!("line-{i}"));
        }

        let bottom = render(&app);
        assert!(buffer_contains(&bottom, "line-29"), "pinned to the newest content by default");
        assert!(!buffer_contains(&bottom, "line-0"), "oldest content shouldn't be visible yet");

        app.scroll_up(100); // clamped for rendering, not stored -- see draw_transcript
        let scrolled = render(&app);
        assert!(buffer_contains(&scrolled, "line-0"), "scrolled all the way up to the oldest content");
        assert!(!buffer_contains(&scrolled, "line-29"), "newest content scrolled out of view");
    }

    #[test]
    fn with_nothing_to_show_the_sidebar_stays_hidden() {
        let app = App::new();
        let backend = render(&app);
        assert!(!buffer_contains(&backend, "activity"), "no plan, no child sessions -- no sidebar yet");
    }

    #[test]
    fn a_plan_renders_its_steps_with_status_markers_in_the_sidebar() {
        let mut app = App::new();
        app.plan_steps = vec![
            lh_event::PlanStep { description: "write the tests".to_string(), status: lh_event::PlanStepStatus::Completed },
            lh_event::PlanStep { description: "write the code".to_string(), status: lh_event::PlanStepStatus::InProgress },
        ];
        let backend = render(&app);
        assert!(buffer_contains(&backend, "activity"));
        assert!(buffer_contains(&backend, "[x] write the tests"));
        assert!(buffer_contains(&backend, "[.] write the code"));
    }

    #[test]
    fn a_child_session_shows_its_running_then_finished_state_in_the_sidebar() {
        let mut app = App::new();
        app.child_sessions.push(crate::app::ChildSessionInfo {
            id: lh_event::SessionId::now_v7(),
            kind: lh_event::ChildKind::NativeSubagent { role: "researcher".to_string() },
            outcome: None,
        });
        let running = render(&app);
        assert!(buffer_contains(&running, "subagent:researcher"));
        assert!(buffer_contains(&running, "..."));

        app.child_sessions[0].outcome = Some(lh_event::ChildOutcome::Success { summary: "done".to_string() });
        let finished = render(&app);
        assert!(buffer_contains(&finished, "ok subagent:researcher"));
    }

    #[test]
    fn a_background_bash_process_shows_its_running_then_done_state_in_the_sidebar() {
        let mut app = App::new();
        app.background_bash.push(crate::app::BackgroundBash { id: "abcdef01-2345-6789".to_string(), running: true });

        let running = render(&app);
        assert!(buffer_contains(&running, "background"));
        assert!(buffer_contains(&running, "running abcdef01"));

        app.background_bash[0].running = false;
        let done = render(&app);
        assert!(buffer_contains(&done, "done abcdef01"));
    }

    #[test]
    fn a_lone_background_bash_process_is_enough_to_show_the_sidebar_on_its_own() {
        let mut app = App::new();
        app.background_bash.push(crate::app::BackgroundBash { id: "xyz".to_string(), running: true });

        let backend = render(&app);
        assert!(buffer_contains(&backend, "activity"), "no plan, no child sessions, but background alone must still show it");
    }

    #[test]
    fn the_status_bar_shows_a_dash_until_a_ledger_rollup_arrives_then_the_cost() {
        let mut app = App::new();
        let backend = render(&app);
        assert!(buffer_contains(&backend, "cost -"));

        app.last_ledger = Some(lh_ledger::LedgerRollup {
            session_id: lh_event::SessionId::now_v7(),
            input_tokens: Some(10),
            output_tokens: Some(5),
            cost_usd: Some(0.0025),
            turns: 1,
            confidence: lh_event::UsageConfidence::Exact,
            children: Vec::new(),
        });
        let backend = render(&app);
        assert!(buffer_contains(&backend, "cost $0.0025"));
    }
}
