//! `lite-harness-tui` binary entry point -- thin startup wiring only (real
//! terminal setup, the ratatui event loop). All actual state and dispatch
//! logic lives in this crate's lib (`src/lib.rs`); see its module doc for
//! why the split exists.

use anyhow::Result;
use crossterm::event::{Event as CtEvent, EventStream, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use futures::StreamExt;
use lh_protocol::default_socket_path;
use lh_tui::app::App;
use lh_tui::client::DaemonClient;
use lh_tui::{dispatch, ui};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

/// RAII guard: undoes raw mode / alternate screen on drop, including the
/// panic path -- without this, a panic mid-render leaves the user's real
/// terminal in raw mode with no visible cursor or scrollback, unusable
/// until they blindly type `reset`.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        execute!(std::io::stdout(), EnterAlternateScreen)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let primary = dispatch::parse_primary_arg(&args)?;

    let cwd = std::env::current_dir()?;
    let sock_path = default_socket_path(&cwd);

    let (mut client, mut events) = DaemonClient::connect(&sock_path).await?;

    let _guard = TerminalGuard::enter();
    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();
    app.primary = primary;
    dispatch::start_handshake(&mut client, &mut app).await?;

    let mut ct_events = EventStream::new();

    loop {
        terminal.draw(|frame| ui::draw(frame, &app))?;

        if app.should_quit {
            break;
        }

        tokio::select! {
            maybe_client_event = events.recv() => {
                match maybe_client_event {
                    Some(client_event) => dispatch::handle_client_event(&mut app, &mut client, &cwd, client_event).await?,
                    None => {
                        app.push_error("connection to daemon lost");
                        app.should_quit = true;
                    }
                }
            }
            maybe_ct_event = ct_events.next() => {
                match maybe_ct_event {
                    Some(Ok(CtEvent::Key(key))) if key.kind == KeyEventKind::Press => {
                        dispatch::handle_key(&mut app, &mut client, key).await?;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => {
                        app.should_quit = true;
                    }
                }
            }
        }
    }

    Ok(())
}
