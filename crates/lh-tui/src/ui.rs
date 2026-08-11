//! Pure rendering: `draw` is a function of `&App` only, no I/O, no mutation
//! -- everything it needs to know is already in `App`'s fields. This is
//! what lets it be tested against `ratatui::backend::TestBackend` with no
//! real terminal and no daemon connection (see `tests` below).

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, ConnPhase, TranscriptItem};

pub fn draw(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3), Constraint::Length(1)])
        .split(frame.area());

    draw_transcript(frame, chunks[0], app);
    draw_input(frame, chunks[1], app);
    draw_status(frame, chunks[2], app);
}

fn draw_transcript(frame: &mut Frame, area: Rect, app: &App) {
    let lines: Vec<Line> = app.transcript.iter().flat_map(transcript_lines).collect();
    let block = Block::default().borders(Borders::ALL).title(" lite-harness ");
    let paragraph = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
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
        " prompt "
    };
    let style = if app.input_enabled() {
        Style::default()
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let paragraph = Paragraph::new(app.input.as_str()).style(style).block(block);
    frame.render_widget(paragraph, area);
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
    let line = Line::from(vec![
        Span::styled(format!(" {phase} "), Style::default().fg(Color::Green)),
        Span::raw(format!("| session {session} | {} | Ctrl+C to quit", app.status)),
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
}
