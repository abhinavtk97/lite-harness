//! Pure rendering: `draw` is a function of `&App` only, no I/O, no mutation
//! -- everything it needs to know is already in `App`'s fields. This is
//! what lets it be tested against `ratatui::backend::TestBackend` with no
//! real terminal and no daemon connection (see `tests` below).

use lh_event::{ChildKind, ChildOutcome, PlanStepStatus};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, ConnPhase, TranscriptItem};

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
}

/// The sidebar is additive: with nothing to show yet (no plan, no child
/// sessions) the transcript alone fills the whole row, exactly like before
/// this phase -- it only appears once there's something worth showing.
fn draw_main(frame: &mut Frame, area: Rect, app: &App) {
    if app.plan_steps.is_empty() && app.child_sessions.is_empty() {
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
        TranscriptItem::User(text) => wrapped_lines(text, "you", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        TranscriptItem::Agent(text) => wrapped_lines(text, "", Style::default()),
        TranscriptItem::Thought(text) => wrapped_lines(text, "thinking", Style::default().fg(Color::DarkGray)),
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

fn draw_input(frame: &mut Frame, area: Rect, app: &App) {
    let title = if app.pending_permission.is_some() {
        " allow? [y/n] "
    } else if !app.input_enabled() {
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
    fn a_pending_permission_switches_the_input_box_title() {
        let mut app = App::new();
        app.phase = ConnPhase::Ready;
        app.pending_permission = Some(crate::app::PendingPermission { request_id: -1 });
        let backend = render(&app);
        assert!(buffer_contains(&backend, "allow? [y/n]"));
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
