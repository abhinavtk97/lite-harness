//! Wires `ClientEvent`s (from the daemon) and `KeyEvent`s (from the
//! terminal) into `App` state transitions. Pure glue -- no rendering, no
//! terminal setup -- which is what lets `tests/e2e.rs` drive a real daemon
//! through these same functions with synthetic key events and no real tty.

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use lh_protocol::{
    methods, AgentsListParams, AgentsListResult, InitializeParams, LedgerQueryParams, PermissionAskResult,
    SessionCreateParams, SessionCreateResult, SessionDelegateParams, SessionDelegateResult, SessionPromptParams,
    SessionPromptResult, PROTOCOL_VERSION,
};
use ratatui::layout::Rect;

use crate::app::{describe_permission_action, App, ConnPhase, PendingKind, PendingPermission, PermissionChoice};
use crate::client::{ClientEvent, DaemonClient};
use crate::mouse;

/// Sends `initialize` and marks it pending -- the rest of the handshake
/// (`session/create`, then unlocking input) continues through
/// `handle_client_event` once its `Response` arrives, same as every other
/// request/response pair on this connection.
pub async fn start_handshake(client: &mut DaemonClient, app: &mut App) -> Result<()> {
    let init_id = client
        .send_request(methods::INITIALIZE, serde_json::to_value(InitializeParams { protocol_version: PROTOCOL_VERSION })?)
        .await?;
    app.pending = Some((init_id, PendingKind::Initialize));
    Ok(())
}

pub async fn handle_client_event(
    app: &mut App,
    client: &mut DaemonClient,
    cwd: &std::path::Path,
    event: ClientEvent,
) -> Result<()> {
    match event {
        ClientEvent::SessionEvent(event) => app.apply_session_event(&event),
        ClientEvent::PermissionAsk { request_id, request } => {
            // The modal (ui::draw_permission_modal) shows the full request
            // while it's pending; this transcript line is just the lasting
            // audit-trail record once the modal closes.
            app.push_system(format!("permission requested: {}", describe_permission_action(&request.action)));
            app.pending_permission = Some(PendingPermission { request_id, request, selected: 0 });
        }
        ClientEvent::Response(resp) => {
            // A ledger refresh is fire-and-forget from the input-gating
            // perspective (it must never block typing), so it's tracked in
            // its own field rather than `app.pending` and handled first.
            if app.pending_ledger_query == Some(resp.id) {
                app.pending_ledger_query = None;
                if let Some(result) = resp.result {
                    if let Ok(parsed) = serde_json::from_value::<lh_protocol::LedgerQueryResult>(result) {
                        app.last_ledger = Some(parsed.rollup);
                    }
                }
                return Ok(());
            }

            // Same fire-and-forget shape as the ledger refresh above --
            // the picker already closed the moment Enter was pressed, so
            // this response only needs to confirm the switch, never gate
            // typing the way `app.pending` does.
            if app.pending_model_select == Some(resp.id) {
                app.pending_model_select = None;
                if let Some(err) = resp.error {
                    app.push_error(format!("model/select failed: {}", err.message));
                    return Ok(());
                }
                if let Some(result) = resp.result {
                    if let Ok(parsed) = serde_json::from_value::<lh_protocol::ModelSelectResult>(result) {
                        app.push_system(format!("switched model to {}", parsed.model));
                        app.current_model = Some(parsed.model);
                    }
                }
                return Ok(());
            }

            let Some((pending_id, kind)) = app.pending else { return Ok(()) };
            if resp.id != pending_id {
                // Not what we're waiting on (e.g. a stray response to a
                // fire-and-forget request) -- leave `pending` alone.
                return Ok(());
            }
            app.pending = None;

            if let Some(err) = resp.error {
                app.push_error(format!("{} failed: {}", kind_label(kind), err.message));
                if matches!(kind, PendingKind::Initialize | PendingKind::AgentsList | PendingKind::SessionCreate) {
                    app.should_quit = true;
                }
                return Ok(());
            }
            let result = resp.result.unwrap_or_default();

            match kind {
                PendingKind::Initialize => {
                    app.status = "connected".to_string();
                    // Must happen here, before session/create -- the
                    // daemon's handshake loop stops answering agents/list
                    // the moment session/create arrives (see PendingKind::
                    // AgentsList's doc comment).
                    let agents_id =
                        client.send_request(methods::AGENTS_LIST, serde_json::to_value(AgentsListParams::default())?).await?;
                    app.pending = Some((agents_id, PendingKind::AgentsList));
                }
                PendingKind::AgentsList => {
                    let result: AgentsListResult = serde_json::from_value(result)?;
                    app.available_agents = result.agents;
                    let create_id = client
                        .send_request(
                            methods::SESSION_CREATE,
                            serde_json::to_value(SessionCreateParams {
                                cwd: cwd.to_string_lossy().to_string(),
                                primary: app.primary.clone(),
                            })?,
                        )
                        .await?;
                    app.pending = Some((create_id, PendingKind::SessionCreate));
                }
                PendingKind::SessionCreate => {
                    let result: SessionCreateResult = serde_json::from_value(result)?;
                    app.session_id = Some(result.session_id);
                    app.context_window = result.context_window;
                    app.current_model = result.current_model;
                    app.phase = ConnPhase::Ready;
                    app.status = "ready".to_string();
                }
                PendingKind::Prompt => {
                    let result: SessionPromptResult = serde_json::from_value(result)?;
                    app.push_system(format!("turn complete: {}", result.stop_reason));
                    // Tracked in `pending_ledger_query`, not `pending` --
                    // refreshing the cost rollup must never gate input the
                    // way an in-flight prompt does.
                    if let Some(session_id) = app.session_id {
                        let query_id = client
                            .send_request(methods::LEDGER_QUERY, serde_json::to_value(LedgerQueryParams { session_id })?)
                            .await?;
                        app.pending_ledger_query = Some(query_id);
                    }
                }
                PendingKind::Delegate => {
                    let result: SessionDelegateResult = serde_json::from_value(result)?;
                    app.push_system(format!("delegation finished ({}): {}", result.child_session_id, describe_child_outcome(&result.outcome)));
                }
                PendingKind::ModelsList => {
                    let result: lh_protocol::ModelsListResult = serde_json::from_value(result)?;
                    app.available_models = result.models.into_iter().map(|m| m.id).collect();
                    app.model_picker_selected = app.available_models.iter().position(|m| m == &result.current).unwrap_or(0);
                    app.current_model = Some(result.current);
                    app.model_picker_visible = true;
                }
            }
        }
        ClientEvent::Closed => {
            app.push_error("daemon closed the connection");
            app.should_quit = true;
        }
    }
    Ok(())
}

pub async fn handle_key(app: &mut App, client: &mut DaemonClient, key: KeyEvent) -> Result<()> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.should_quit = true;
        return Ok(());
    }

    // Toggles regardless of app state (typing, waiting, or mid-permission),
    // same as Ctrl+C above -- collapsing the sidebar to reclaim width for
    // the transcript isn't specific to any one mode.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('b') {
        app.sidebar_visible = !app.sidebar_visible;
        return Ok(());
    }

    if app.pending_permission.is_some() {
        // Left/Right/Up/Down move the modal's highlight and Enter confirms
        // whatever's currently highlighted (defaults to Deny -- see
        // `PendingPermission`'s doc comment); y/n/a/d remain as direct
        // shortcuts for anyone who'd rather not cycle through the options.
        match key.code {
            KeyCode::Left | KeyCode::Up => app.cycle_permission_selection(false),
            KeyCode::Right | KeyCode::Down => app.cycle_permission_selection(true),
            KeyCode::Enter => {
                if let Some(choice) = app.selected_permission_choice() {
                    respond_to_permission(app, client, choice.decision()).await?;
                }
            }
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                respond_to_permission(app, client, PermissionChoice::Allow.decision()).await?;
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                respond_to_permission(app, client, PermissionChoice::Deny.decision()).await?;
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                respond_to_permission(app, client, PermissionChoice::AllowAlways.decision()).await?;
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                respond_to_permission(app, client, PermissionChoice::DenyAlways.decision()).await?;
            }
            _ => {}
        }
        return Ok(());
    }

    // The model picker overlay -- same reverse-video-highlighted-row visual
    // language and arrow-key-navigate-then-Enter-confirm interaction as the
    // autocomplete dropdown below, but checked first since it's a true
    // overlay (opened by `/model`, `app.pending` already cleared by the time
    // it's showing) rather than something that reads live off the input box.
    if app.model_picker_visible {
        match key.code {
            KeyCode::Up => app.cycle_model_picker_selection(false),
            KeyCode::Down => app.cycle_model_picker_selection(true),
            KeyCode::Enter => {
                if let Some(model) = app.available_models.get(app.model_picker_selected).cloned() {
                    let id = client
                        .send_request(methods::MODEL_SELECT, serde_json::to_value(lh_protocol::ModelSelectParams { model })?)
                        .await?;
                    app.pending_model_select = Some(id);
                }
                app.model_picker_visible = false;
            }
            KeyCode::Esc => app.model_picker_visible = false,
            _ => {}
        }
        return Ok(());
    }

    // Slash-command autocomplete: while the input is still just a `/`-prefixed
    // command name being typed (no args yet) and hasn't been dismissed this
    // keystroke, Up/Down/Tab/Esc are claimed by the dropdown instead of their
    // normal behavior (scroll / nothing / nothing), and Enter snaps the input
    // to the highlighted candidate before falling through to the normal
    // submit handling below. Checked after the permission-modal branch above
    // but before the unconditional scroll block that follows -- the same
    // priority carve-out that branch already uses.
    if app.input_enabled() && !app.autocomplete_dismissed {
        let candidates = crate::app::command_candidates(&app.input.text());
        if !candidates.is_empty() {
            let selected = app.autocomplete_selected.min(candidates.len() - 1);
            match key.code {
                KeyCode::Up => {
                    app.cycle_autocomplete_selection(false, candidates.len());
                    return Ok(());
                }
                KeyCode::Down => {
                    app.cycle_autocomplete_selection(true, candidates.len());
                    return Ok(());
                }
                KeyCode::Tab => {
                    app.input.set_text(&format!("/{} ", candidates[selected].name));
                    return Ok(());
                }
                KeyCode::Esc => {
                    app.autocomplete_dismissed = true;
                    return Ok(());
                }
                KeyCode::Enter => {
                    let already_exact = app.input.text().trim() == format!("/{}", candidates[selected].name);
                    if !already_exact {
                        app.input.set_text(&format!("/{} ", candidates[selected].name));
                    }
                    // Falls through to the normal Enter-submit handling below
                    // either way -- an exact match submits as typed, a
                    // snapped completion submits what it was just snapped to.
                }
                _ => {}
            }
        }
    }

    // Scrolling the transcript works regardless of whether the input box
    // is currently editable (e.g. re-reading context while a turn is still
    // in flight), so it's checked before the `input_enabled()` gate below.
    match key.code {
        KeyCode::Up => {
            app.scroll_up(1);
            return Ok(());
        }
        KeyCode::Down => {
            app.scroll_down(1);
            return Ok(());
        }
        KeyCode::PageUp => {
            app.scroll_up(SCROLL_PAGE_LINES);
            return Ok(());
        }
        KeyCode::PageDown => {
            app.scroll_down(SCROLL_PAGE_LINES);
            return Ok(());
        }
        _ => {}
    }

    if !app.input_enabled() {
        return Ok(());
    }

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        // Alt+Enter inserts a literal newline (multi-line prompts); plain
        // Enter submits. Alt is reliably detected by crossterm without any
        // special terminal protocol, unlike Shift+Enter.
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => {
            app.input.insert_newline();
        }
        KeyCode::Enter => {
            if app.input.text().trim().is_empty() {
                return Ok(());
            }
            let text = app.input.take();
            app.push_user_message(text.clone());
            if let Some(command) = text.trim().strip_prefix('/') {
                handle_slash_command(app, client, command).await?;
                return Ok(());
            }
            let prompt_id = client
                .send_request(methods::SESSION_PROMPT, serde_json::to_value(SessionPromptParams { text })?)
                .await?;
            app.pending = Some((prompt_id, PendingKind::Prompt));
        }
        // Ctrl+Backspace and Ctrl+W both do readline's word-delete --
        // terminals vary in whether they send a distinguishable
        // Ctrl+Backspace at all, so Ctrl+W is the reliable one of the two.
        KeyCode::Backspace if ctrl => {
            app.input.delete_word_before_cursor();
            app.autocomplete_dismissed = false;
        }
        KeyCode::Char('w') if ctrl => {
            app.input.delete_word_before_cursor();
            app.autocomplete_dismissed = false;
        }
        KeyCode::Backspace => {
            app.input.backspace();
            app.autocomplete_dismissed = false;
        }
        KeyCode::Left => app.input.move_left(),
        KeyCode::Right => app.input.move_right(),
        KeyCode::Home => app.input.move_line_start(),
        KeyCode::End => app.input.move_line_end(),
        KeyCode::Char(c) => {
            app.input.insert_char(c);
            app.autocomplete_dismissed = false;
        }
        _ => {}
    }
    Ok(())
}

/// The mouse counterpart to `handle_key` -- `mouse::handle_mouse` does the
/// actual (pure, I/O-free) hit-testing and `App` mutation; this just acts
/// on its result the same way the keyboard path's `y`/`Enter` handling
/// does, via the same `respond_to_permission` helper, so a clicked and a
/// typed decision are answered identically from here on.
pub async fn handle_mouse_event(
    app: &mut App,
    client: &mut DaemonClient,
    event: MouseEvent,
    terminal_area: Rect,
) -> Result<()> {
    if let Some(choice) = mouse::handle_mouse(app, event, terminal_area) {
        respond_to_permission(app, client, choice.decision()).await?;
    }
    Ok(())
}

/// Answers whatever permission is currently pending (if anything still is
/// -- a stray key event after it was already answered is a no-op) and
/// clears it, matching the one-decision-per-request contract `permission/ask`
/// expects.
async fn respond_to_permission(
    app: &mut App,
    client: &mut DaemonClient,
    decision: lh_event::PermissionDecision,
) -> Result<()> {
    if let Some((id, decision)) = app.decide_pending_permission(decision) {
        client.respond(id, serde_json::to_value(PermissionAskResult { decision })?).await?;
    }
    Ok(())
}

const SCROLL_PAGE_LINES: u16 = 10;

fn kind_label(kind: PendingKind) -> &'static str {
    match kind {
        PendingKind::Initialize => "initialize",
        PendingKind::AgentsList => "agents/list",
        PendingKind::SessionCreate => "session/create",
        PendingKind::Prompt => "session/prompt",
        PendingKind::Delegate => "session/delegate",
        PendingKind::ModelsList => "models/list",
    }
}

/// Built from `crate::app::SLASH_COMMANDS` (the same registry the
/// autocomplete dropdown reads) rather than a separately hand-written
/// string, so the two can't drift out of sync with each other.
fn help_text() -> String {
    let mut text = String::from("commands:");
    for c in crate::app::SLASH_COMMANDS {
        text.push_str(&format!("  /{} - {}", c.name, c.usage));
    }
    // Keybindings that used to be spelled out permanently in the input
    // box's own title now live here instead, alongside Ctrl+B, which has no
    // other discoverable home now that the sidebar has no title of its own.
    text.push_str("  |  keys: Enter send, Alt+Enter newline, Tab complete, Ctrl+B toggle sidebar");
    text
}

/// A typed `/`-prefixed line from the input box, dispatched instead of a
/// `session/prompt` -- see the module-level note on why there's no
/// `/primary` here (switching the *root* driver needs a whole new
/// connection, not a command, since the daemon only accepts one
/// `session/create` per connection).
async fn handle_slash_command(app: &mut App, client: &mut DaemonClient, command: &str) -> Result<()> {
    let mut words = command.split_whitespace();
    match words.next().unwrap_or("") {
        "help" => app.push_system(help_text()),
        "quit" | "q" => app.should_quit = true,
        "agents" => app.push_system(describe_available_agents(&app.available_agents)),
        "delegate" => {
            let Some(agent_name) = words.next() else {
                app.push_error("usage: /delegate <agent> <task summary...>");
                return Ok(());
            };
            let task_summary = words.collect::<Vec<_>>().join(" ");
            if task_summary.is_empty() {
                app.push_error("usage: /delegate <agent> <task summary...>");
                return Ok(());
            }
            let agent = parse_agent_kind(agent_name);
            app.push_system(format!("delegating to {agent:?}..."));
            let id = client
                .send_request(methods::SESSION_DELEGATE, serde_json::to_value(SessionDelegateParams { agent, task_summary })?)
                .await?;
            app.pending = Some((id, PendingKind::Delegate));
        }
        "model" => {
            let id = client.send_request(methods::MODELS_LIST, serde_json::json!({})).await?;
            app.pending = Some((id, PendingKind::ModelsList));
        }
        other => app.push_error(format!("unknown command '/{other}' -- try /help")),
    }
    Ok(())
}

/// `lite-harness-tui [--primary <agent>]` -- mirrors `lite-harness`'s own
/// `--primary` flag (architecture §12, Phase 6). There's no interactive
/// equivalent (no `/primary` slash command in `handle_slash_command`): the
/// daemon only accepts one `session/create` per connection, so switching
/// the *root* driver mid-REPL would mean opening a whole new connection,
/// not answering a typed command -- see `App::primary`'s doc comment for
/// the full reason. Lives here (not in `main.rs`) despite being CLI-arg
/// parsing because it's pure and terminal-free, so it can actually be unit
/// tested -- `main.rs` is deliberately untestable glue, see `lib.rs`'s
/// module doc.
pub fn parse_primary_arg(args: &[String]) -> Result<lh_protocol::PrimarySelector> {
    match args.first().map(String::as_str) {
        None => Ok(lh_protocol::PrimarySelector::Native),
        Some("--primary") => {
            let name = args.get(1).ok_or_else(|| anyhow::anyhow!("--primary requires a value, e.g. --primary claude-code"))?;
            Ok(lh_protocol::PrimarySelector::Delegated { agent: parse_agent_kind(name) })
        }
        Some(other) => anyhow::bail!("unknown argument '{other}' (usage: lite-harness-tui [--primary <agent>])"),
    }
}

/// Free-form: an agent name the daemon doesn't recognize is a normal,
/// reportable `session/delegate` error (via the agent registry), not
/// something worth a second, duplicated allowlist on the client side.
/// `pub` (not `pub(crate)`) so `main.rs`'s `--primary` flag parsing --
/// a separate binary crate, not just a separate module -- can reuse it
/// instead of duplicating the same name-to-`AgentKind` mapping.
pub fn parse_agent_kind(name: &str) -> lh_event::AgentKind {
    match name {
        "claude-code" => lh_event::AgentKind::ClaudeCode,
        "codex" => lh_event::AgentKind::Codex,
        "gemini-cli" => lh_event::AgentKind::GeminiCli,
        "goose" => lh_event::AgentKind::Goose,
        "opencode" => lh_event::AgentKind::OpenCode,
        other => lh_event::AgentKind::Custom { name: other.to_string() },
    }
}

fn describe_available_agents(agents: &[lh_protocol::AgentInfo]) -> String {
    if agents.is_empty() {
        return "no delegated agents registered (see LITE_HARNESS_AGENTS_FILE)".to_string();
    }
    let listed = agents
        .iter()
        .map(|a| {
            if a.can_be_primary {
                format!("{:?} (primary-capable)", a.kind)
            } else {
                format!("{:?}", a.kind)
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("registered agents: {listed}")
}

fn describe_child_outcome(outcome: &lh_event::ChildOutcome) -> String {
    match outcome {
        lh_event::ChildOutcome::Success { summary } => format!("success: {summary}"),
        lh_event::ChildOutcome::Failed { message } => format!("failed: {message}"),
        lh_event::ChildOutcome::Cancelled => "cancelled".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use lh_protocol::{Response, RpcError};
    use tokio::net::UnixListener;

    use super::*;
    use crate::app::App;

    /// A `DaemonClient` connected to a live but otherwise-inert socket --
    /// these tests exercise `App` state transitions from synthetic
    /// `ClientEvent`/`KeyEvent`s directly, they don't need the peer to
    /// actually respond to anything `handle_key`/`handle_client_event`
    /// sends on `client`.
    async fn inert_client() -> DaemonClient {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("fake.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();
        tokio::spawn(async move {
            let _ = listener.accept().await;
            std::mem::forget(dir);
        });
        let (client, _events) = DaemonClient::connect(&sock_path).await.unwrap();
        client
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Types `text` one character at a time through the real `handle_key`
    /// path (matching how a real terminal delivers keystrokes) without
    /// submitting -- the counterpart to `submit_line` below for tests that
    /// need to inspect state mid-typing, before any Enter.
    async fn type_text(app: &mut App, client: &mut DaemonClient, text: &str) {
        for c in text.chars() {
            handle_key(app, client, key(KeyCode::Char(c))).await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_permission_ask_event_defaults_the_modal_to_deny_and_logs_a_summary() {
        let mut app = App::new();
        let mut client = inert_client().await;
        let request = crate::app::fake_permission_request();

        handle_client_event(&mut app, &mut client, std::path::Path::new("."), ClientEvent::PermissionAsk { request_id: 7, request })
            .await
            .unwrap();

        let pending = app.pending_permission.as_ref().expect("should now be pending");
        assert_eq!(pending.request_id, 7);
        assert_eq!(app.selected_permission_choice(), Some(crate::app::PermissionChoice::Deny));
        assert!(app.transcript.iter().any(|i| matches!(i, crate::app::TranscriptItem::System(t) if t.contains("execute: echo hi"))));
    }

    #[tokio::test]
    async fn a_response_that_doesnt_match_the_pending_id_is_ignored() {
        let mut app = App::new();
        let mut client = inert_client().await;
        app.pending = Some((5, PendingKind::Prompt));

        handle_client_event(&mut app, &mut client, std::path::Path::new("."), ClientEvent::Response(Response::ok(6, serde_json::json!({}))))
            .await
            .unwrap();

        assert_eq!(app.pending, Some((5, PendingKind::Prompt)), "unrelated response must not clear pending");
    }

    #[tokio::test]
    async fn an_error_response_to_initialize_quits_the_app() {
        let mut app = App::new();
        let mut client = inert_client().await;
        app.pending = Some((1, PendingKind::Initialize));

        let mut resp = Response::ok(1, serde_json::json!({}));
        resp.result = None;
        resp.error = Some(RpcError { code: -1, message: "boom".to_string() });
        handle_client_event(&mut app, &mut client, std::path::Path::new("."), ClientEvent::Response(resp)).await.unwrap();

        assert!(app.should_quit);
        assert!(app.transcript.iter().any(|i| matches!(i, crate::app::TranscriptItem::Error(t) if t.contains("initialize failed"))));
    }

    #[tokio::test]
    async fn an_error_response_to_a_prompt_does_not_quit_the_app() {
        let mut app = App::new();
        let mut client = inert_client().await;
        app.pending = Some((2, PendingKind::Prompt));

        let mut resp = Response::ok(2, serde_json::json!({}));
        resp.result = None;
        resp.error = Some(RpcError { code: -1, message: "turn failed".to_string() });
        handle_client_event(&mut app, &mut client, std::path::Path::new("."), ClientEvent::Response(resp)).await.unwrap();

        assert!(!app.should_quit, "a failed turn shouldn't kill the whole app");
        assert!(app.transcript.iter().any(|i| matches!(i, crate::app::TranscriptItem::Error(t) if t.contains("session/prompt failed"))));
    }

    #[tokio::test]
    async fn a_closed_client_event_pushes_an_error_and_quits() {
        let mut app = App::new();
        let mut client = inert_client().await;

        handle_client_event(&mut app, &mut client, std::path::Path::new("."), ClientEvent::Closed).await.unwrap();

        assert!(app.should_quit);
        assert!(app.transcript.iter().any(|i| matches!(i, crate::app::TranscriptItem::Error(_))));
    }

    #[tokio::test]
    async fn ctrl_c_quits_regardless_of_app_state() {
        let mut app = App::new();
        let mut client = inert_client().await;

        handle_key(&mut app, &mut client, KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)).await.unwrap();

        assert!(app.should_quit);
    }

    #[tokio::test]
    async fn ctrl_b_toggles_the_sidebar_regardless_of_app_state() {
        let mut app = App::new(); // still Connecting -- toggling shouldn't require a ready session
        let mut client = inert_client().await;
        assert!(app.sidebar_visible, "visible by default");

        handle_key(&mut app, &mut client, KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)).await.unwrap();
        assert!(!app.sidebar_visible);

        handle_key(&mut app, &mut client, KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)).await.unwrap();
        assert!(app.sidebar_visible, "toggles back on the second press");
    }

    fn pending_permission_app() -> App {
        let mut app = App::new();
        app.pending_permission =
            Some(PendingPermission { request_id: -1, request: crate::app::fake_permission_request(), selected: 0 });
        app
    }

    const TEST_TERMINAL: Rect = Rect { x: 0, y: 0, width: 120, height: 40 };

    #[tokio::test]
    async fn a_mouse_click_on_a_permission_option_answers_it_the_same_way_the_keyboard_does() {
        use crossterm::event::{MouseButton, MouseEventKind};

        let mut app = pending_permission_app();
        let mut client = inert_client().await;

        let popup = crate::ui::permission_popup_rect(TEST_TERMINAL);
        let (_, options_row, _) = crate::ui::permission_modal_rows(popup);
        let deny_rect = crate::ui::permission_option_rects(options_row)
            [PermissionChoice::ALL.iter().position(|c| *c == PermissionChoice::Deny).unwrap()];
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: deny_rect.x,
            row: deny_rect.y,
            modifiers: KeyModifiers::NONE,
        };

        handle_mouse_event(&mut app, &mut client, click, TEST_TERMINAL).await.unwrap();

        assert!(app.pending_permission.is_none(), "the click should have answered the request");
    }

    #[tokio::test]
    async fn a_mouse_scroll_with_nothing_pending_scrolls_the_transcript() {
        use crossterm::event::MouseEventKind;

        let mut app = App::new();
        let mut client = inert_client().await;
        let scroll = MouseEvent { kind: MouseEventKind::ScrollUp, column: 5, row: 5, modifiers: KeyModifiers::NONE };

        handle_mouse_event(&mut app, &mut client, scroll, TEST_TERMINAL).await.unwrap();

        assert!(app.transcript_scroll > 0);
    }

    #[tokio::test]
    async fn answering_a_pending_permission_with_n_denies_it() {
        let mut app = pending_permission_app();
        let mut client = inert_client().await;

        handle_key(&mut app, &mut client, key(KeyCode::Char('n'))).await.unwrap();

        assert!(app.pending_permission.is_none());
    }

    #[tokio::test]
    async fn an_unrecognized_key_while_answering_a_permission_is_a_no_op() {
        let mut app = pending_permission_app();
        let mut client = inert_client().await;

        handle_key(&mut app, &mut client, key(KeyCode::Tab)).await.unwrap();

        assert!(app.pending_permission.is_some(), "an unrelated key must not answer the pending permission");
    }

    #[tokio::test]
    async fn arrow_keys_cycle_the_permission_modals_selection_without_answering_it() {
        let mut app = pending_permission_app();
        let mut client = inert_client().await;

        handle_key(&mut app, &mut client, key(KeyCode::Right)).await.unwrap();

        assert!(app.pending_permission.is_some(), "cycling must not answer the request");
        assert_eq!(app.selected_permission_choice(), Some(crate::app::PermissionChoice::Allow));
    }

    #[tokio::test]
    async fn enter_confirms_the_currently_selected_option_which_defaults_to_deny() {
        let mut app = pending_permission_app();
        let mut client = inert_client().await;

        handle_key(&mut app, &mut client, key(KeyCode::Enter)).await.unwrap();

        assert!(app.pending_permission.is_none(), "must answer once confirmed, not just close the modal");
    }

    #[tokio::test]
    async fn a_key_after_the_permission_is_already_answered_is_a_no_op() {
        let mut app = pending_permission_app();
        let mut client = inert_client().await;

        handle_key(&mut app, &mut client, key(KeyCode::Char('n'))).await.unwrap();
        assert!(app.pending_permission.is_none());

        // A second key event (e.g. a stray repeat) after the modal already
        // closed must not panic or try to answer a nonexistent request.
        handle_key(&mut app, &mut client, key(KeyCode::Char('y'))).await.unwrap();
        assert!(app.pending_permission.is_none());
    }

    #[tokio::test]
    async fn a_and_d_answer_with_the_always_scoped_decisions() {
        let mut app = pending_permission_app();
        let mut client = inert_client().await;

        handle_key(&mut app, &mut client, key(KeyCode::Char('a'))).await.unwrap();
        assert!(app.pending_permission.is_none());

        app = pending_permission_app();
        handle_key(&mut app, &mut client, key(KeyCode::Char('d'))).await.unwrap();
        assert!(app.pending_permission.is_none());
    }

    #[tokio::test]
    async fn keys_are_ignored_entirely_while_input_is_disabled() {
        let mut app = App::new(); // still Connecting -> input_enabled() is false
        let mut client = inert_client().await;

        handle_key(&mut app, &mut client, key(KeyCode::Char('x'))).await.unwrap();

        assert!(app.input.is_empty());
    }

    #[tokio::test]
    async fn enter_on_empty_input_does_not_send_a_prompt() {
        let mut app = App::new();
        app.phase = ConnPhase::Ready;
        let mut client = inert_client().await;

        handle_key(&mut app, &mut client, key(KeyCode::Enter)).await.unwrap();

        assert!(app.pending.is_none());
        assert!(app.transcript.is_empty());
    }

    #[tokio::test]
    async fn an_unrecognized_key_while_typing_is_a_no_op() {
        let mut app = App::new();
        app.phase = ConnPhase::Ready;
        app.input.insert_char('x');
        let mut client = inert_client().await;

        handle_key(&mut app, &mut client, key(KeyCode::Tab)).await.unwrap();

        assert_eq!(app.input.text(), "x");
        assert!(app.pending.is_none());
    }

    #[tokio::test]
    async fn backspace_removes_the_character_before_the_cursor() {
        let mut app = App::new();
        app.phase = ConnPhase::Ready;
        app.input.insert_char('h');
        app.input.insert_char('i');
        let mut client = inert_client().await;

        handle_key(&mut app, &mut client, key(KeyCode::Backspace)).await.unwrap();

        assert_eq!(app.input.text(), "h");
    }

    #[tokio::test]
    async fn left_and_right_arrows_move_the_cursor_without_changing_the_text() {
        let mut app = App::new();
        app.phase = ConnPhase::Ready;
        app.input.insert_char('a');
        app.input.insert_char('c');
        let mut client = inert_client().await;

        handle_key(&mut app, &mut client, key(KeyCode::Left)).await.unwrap();
        app.input.insert_char('b');

        assert_eq!(app.input.text(), "abc", "typed in the middle after moving left");

        handle_key(&mut app, &mut client, key(KeyCode::Right)).await.unwrap();
        assert_eq!(app.input.cursor(), 3, "moved right past the inserted 'b' to the end");
    }

    #[tokio::test]
    async fn ctrl_backspace_also_deletes_the_word_before_the_cursor() {
        let mut app = App::new();
        app.phase = ConnPhase::Ready;
        for c in "run echo".chars() {
            app.input.insert_char(c);
        }
        let mut client = inert_client().await;

        handle_key(&mut app, &mut client, KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL)).await.unwrap();

        assert_eq!(app.input.text(), "run ");
    }

    #[tokio::test]
    async fn ctrl_w_deletes_the_word_before_the_cursor() {
        let mut app = App::new();
        app.phase = ConnPhase::Ready;
        for c in "run echo".chars() {
            app.input.insert_char(c);
        }
        let mut client = inert_client().await;

        handle_key(&mut app, &mut client, KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL)).await.unwrap();

        assert_eq!(app.input.text(), "run ");
    }

    #[tokio::test]
    async fn alt_enter_inserts_a_newline_instead_of_submitting() {
        let mut app = App::new();
        app.phase = ConnPhase::Ready;
        app.input.insert_char('a');
        let mut client = inert_client().await;

        handle_key(&mut app, &mut client, KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)).await.unwrap();

        assert_eq!(app.input.text(), "a\n");
        assert!(app.pending.is_none(), "must not have submitted a prompt");
    }

    #[tokio::test]
    async fn home_and_end_move_the_cursor_to_the_line_boundaries() {
        let mut app = App::new();
        app.phase = ConnPhase::Ready;
        for c in "hi".chars() {
            app.input.insert_char(c);
        }
        let mut client = inert_client().await;

        handle_key(&mut app, &mut client, key(KeyCode::Home)).await.unwrap();
        assert_eq!(app.input.cursor(), 0);

        handle_key(&mut app, &mut client, key(KeyCode::End)).await.unwrap();
        assert_eq!(app.input.cursor(), 2);
    }

    #[tokio::test]
    async fn arrow_and_page_keys_scroll_the_transcript_even_while_input_is_disabled() {
        let mut app = App::new(); // Connecting -> input disabled
        let mut client = inert_client().await;

        handle_key(&mut app, &mut client, key(KeyCode::Up)).await.unwrap();
        assert_eq!(app.transcript_scroll, 1);

        handle_key(&mut app, &mut client, key(KeyCode::PageUp)).await.unwrap();
        assert_eq!(app.transcript_scroll, 11);

        handle_key(&mut app, &mut client, key(KeyCode::Down)).await.unwrap();
        assert_eq!(app.transcript_scroll, 10);

        handle_key(&mut app, &mut client, key(KeyCode::PageDown)).await.unwrap();
        assert_eq!(app.transcript_scroll, 0);
    }

    #[tokio::test]
    async fn up_down_navigate_the_autocomplete_dropdown_instead_of_scrolling_while_it_is_open() {
        let mut app = ready_app();
        let mut client = inert_client().await;
        type_text(&mut app, &mut client, "/").await; // matches all 4 registered commands

        handle_key(&mut app, &mut client, key(KeyCode::Down)).await.unwrap();
        assert_eq!(app.autocomplete_selected, 1, "Down should cycle the dropdown");
        assert_eq!(app.transcript_scroll, 0, "and must not also scroll the transcript");

        handle_key(&mut app, &mut client, key(KeyCode::Up)).await.unwrap();
        assert_eq!(app.autocomplete_selected, 0);
        assert_eq!(app.transcript_scroll, 0);
    }

    #[tokio::test]
    async fn tab_completes_the_highlighted_command_without_submitting_it() {
        let mut app = ready_app();
        let mut client = inert_client().await;
        type_text(&mut app, &mut client, "/de").await;

        handle_key(&mut app, &mut client, key(KeyCode::Tab)).await.unwrap();

        assert_eq!(app.input.text(), "/delegate ");
        assert!(app.pending.is_none(), "Tab must only fill the input, never submit");
        assert!(app.transcript.is_empty(), "nothing should reach the transcript yet");
    }

    #[tokio::test]
    async fn enter_on_a_narrowed_prefix_completes_and_submits_the_command() {
        let mut app = ready_app();
        let mut client = inert_client().await;
        type_text(&mut app, &mut client, "/ag").await; // "agents" is the only match

        handle_key(&mut app, &mut client, key(KeyCode::Enter)).await.unwrap();

        assert!(
            app.transcript.iter().any(|i| matches!(i, crate::app::TranscriptItem::User(t) if t == "/agents ")),
            "Enter should have snapped the input to the full command before submitting"
        );
        assert!(app.transcript.iter().any(|i| matches!(i, crate::app::TranscriptItem::System(t) if t.contains("no delegated agents"))));
    }

    #[tokio::test]
    async fn enter_on_an_already_exact_command_submits_it_unchanged() {
        let mut app = ready_app();
        let mut client = inert_client().await;
        type_text(&mut app, &mut client, "/quit").await;

        handle_key(&mut app, &mut client, key(KeyCode::Enter)).await.unwrap();

        assert!(app.should_quit);
    }

    #[tokio::test]
    async fn escape_dismisses_the_dropdown_without_changing_the_input() {
        let mut app = ready_app();
        let mut client = inert_client().await;
        type_text(&mut app, &mut client, "/de").await;

        handle_key(&mut app, &mut client, key(KeyCode::Esc)).await.unwrap();
        assert!(app.autocomplete_dismissed);
        assert_eq!(app.input.text(), "/de", "Esc must not touch what was typed");

        // With the dropdown dismissed, Up falls through to the normal scroll
        // handling instead of being claimed by the (now-hidden) dropdown.
        handle_key(&mut app, &mut client, key(KeyCode::Up)).await.unwrap();
        assert_eq!(app.transcript_scroll, 1);
    }

    #[tokio::test]
    async fn typing_again_after_dismissing_the_dropdown_clears_the_dismissal() {
        let mut app = ready_app();
        let mut client = inert_client().await;
        type_text(&mut app, &mut client, "/de").await;
        handle_key(&mut app, &mut client, key(KeyCode::Esc)).await.unwrap();
        assert!(app.autocomplete_dismissed);

        handle_key(&mut app, &mut client, key(KeyCode::Char('l'))).await.unwrap();
        assert!(!app.autocomplete_dismissed);

        // Tab is a no-op for plain text editing (not handled by the normal
        // arm below), so this only succeeds if the dropdown is intercepting
        // again -- an unambiguous signal, unlike re-checking Up/Down.
        handle_key(&mut app, &mut client, key(KeyCode::Tab)).await.unwrap();
        assert_eq!(app.input.text(), "/delegate ", "autocomplete should be active again after the dismissal was cleared");
    }

    #[tokio::test]
    async fn a_ledger_query_response_updates_last_ledger_without_touching_pending() {
        let mut app = App::new();
        let mut client = inert_client().await;
        app.pending = Some((5, PendingKind::Prompt)); // a real prompt is still in flight
        app.pending_ledger_query = Some(42);

        let rollup = lh_ledger::LedgerRollup {
            session_id: lh_event::SessionId::now_v7(),
            input_tokens: Some(10),
            output_tokens: Some(4),
            cache_read_tokens: None,
            cache_write_tokens: None,
            cost_usd: Some(0.0012),
            turns: 1,
            confidence: lh_event::UsageConfidence::Exact,
            children: Vec::new(),
        };
        let resp = Response::ok(42, serde_json::to_value(lh_protocol::LedgerQueryResult { rollup }).unwrap());
        handle_client_event(&mut app, &mut client, std::path::Path::new("."), ClientEvent::Response(resp)).await.unwrap();

        assert!(app.pending_ledger_query.is_none());
        assert_eq!(app.pending, Some((5, PendingKind::Prompt)), "the real pending prompt must be untouched");
        assert_eq!(app.last_ledger.unwrap().cost_usd, Some(0.0012));
    }

    #[tokio::test]
    async fn a_ledger_query_response_that_fails_to_deserialize_is_ignored_not_fatal() {
        let mut app = App::new();
        let mut client = inert_client().await;
        app.pending_ledger_query = Some(42);

        let resp = Response::ok(42, serde_json::json!({ "not": "a rollup" }));
        handle_client_event(&mut app, &mut client, std::path::Path::new("."), ClientEvent::Response(resp)).await.unwrap();

        assert!(app.pending_ledger_query.is_none(), "still clears the pending marker even on a malformed reply");
        assert!(app.last_ledger.is_none());
    }

    #[tokio::test]
    async fn a_ledger_query_response_with_an_error_and_no_result_is_ignored_not_fatal() {
        let mut app = App::new();
        let mut client = inert_client().await;
        app.pending_ledger_query = Some(42);

        let mut resp = Response::ok(42, serde_json::json!({}));
        resp.result = None;
        resp.error = Some(RpcError { code: -1, message: "session not found".to_string() });
        handle_client_event(&mut app, &mut client, std::path::Path::new("."), ClientEvent::Response(resp)).await.unwrap();

        assert!(app.pending_ledger_query.is_none());
        assert!(app.last_ledger.is_none());
    }

    #[test]
    fn kind_label_names_every_variant() {
        assert_eq!(kind_label(PendingKind::Initialize), "initialize");
        assert_eq!(kind_label(PendingKind::AgentsList), "agents/list");
        assert_eq!(kind_label(PendingKind::SessionCreate), "session/create");
        assert_eq!(kind_label(PendingKind::Prompt), "session/prompt");
        assert_eq!(kind_label(PendingKind::Delegate), "session/delegate");
    }

    #[tokio::test]
    async fn initialize_completing_moves_on_to_agents_list_not_session_create_directly() {
        let mut app = App::new();
        let mut client = inert_client().await;
        app.pending = Some((1, PendingKind::Initialize));

        let resp = Response::ok(1, serde_json::json!({ "protocol_version": 1 }));
        handle_client_event(&mut app, &mut client, std::path::Path::new("."), ClientEvent::Response(resp)).await.unwrap();

        assert_eq!(app.pending.map(|(_, kind)| kind), Some(PendingKind::AgentsList));
    }

    #[tokio::test]
    async fn agents_list_completing_stores_the_result_and_moves_on_to_session_create() {
        let mut app = App::new();
        let mut client = inert_client().await;
        app.pending = Some((1, PendingKind::AgentsList));

        let agents = lh_protocol::AgentsListResult {
            agents: vec![lh_protocol::AgentInfo { kind: lh_event::AgentKind::ClaudeCode, can_be_primary: true }],
        };
        let resp = Response::ok(1, serde_json::to_value(agents).unwrap());
        handle_client_event(&mut app, &mut client, std::path::Path::new("."), ClientEvent::Response(resp)).await.unwrap();

        assert_eq!(app.available_agents.len(), 1);
        assert_eq!(app.pending.map(|(_, kind)| kind), Some(PendingKind::SessionCreate));
    }

    #[tokio::test]
    async fn session_create_completing_captures_the_configured_context_window() {
        let mut app = App::new();
        let mut client = inert_client().await;
        app.pending = Some((1, PendingKind::SessionCreate));

        let create = SessionCreateResult {
            session_id: lh_event::SessionId::now_v7(),
            context_window: Some(200_000),
            current_model: Some("claude-sonnet-5".to_string()),
        };
        let resp = Response::ok(1, serde_json::to_value(create).unwrap());
        handle_client_event(&mut app, &mut client, std::path::Path::new("."), ClientEvent::Response(resp)).await.unwrap();

        assert_eq!(app.context_window, Some(200_000));
        assert_eq!(app.current_model, Some("claude-sonnet-5".to_string()));
        assert_eq!(app.phase, ConnPhase::Ready);
    }

    #[tokio::test]
    async fn session_create_completing_with_no_context_window_configured_leaves_it_none() {
        let mut app = App::new();
        let mut client = inert_client().await;
        app.pending = Some((1, PendingKind::SessionCreate));

        let create =
            SessionCreateResult { session_id: lh_event::SessionId::now_v7(), context_window: None, current_model: None };
        let resp = Response::ok(1, serde_json::to_value(create).unwrap());
        handle_client_event(&mut app, &mut client, std::path::Path::new("."), ClientEvent::Response(resp)).await.unwrap();

        assert_eq!(app.context_window, None);
        assert_eq!(app.current_model, None);
    }

    #[tokio::test]
    async fn an_error_response_to_agents_list_quits_the_app() {
        let mut app = App::new();
        let mut client = inert_client().await;
        app.pending = Some((1, PendingKind::AgentsList));

        let mut resp = Response::ok(1, serde_json::json!({}));
        resp.result = None;
        resp.error = Some(RpcError { code: -1, message: "boom".to_string() });
        handle_client_event(&mut app, &mut client, std::path::Path::new("."), ClientEvent::Response(resp)).await.unwrap();

        assert!(app.should_quit, "the handshake can't proceed without it, so this is fatal like initialize/session-create");
    }

    #[tokio::test]
    async fn a_delegate_response_reports_success_and_failure_distinctly() {
        let mut app = App::new();
        let mut client = inert_client().await;
        app.pending = Some((1, PendingKind::Delegate));

        let ok_result = lh_protocol::SessionDelegateResult {
            child_session_id: lh_event::SessionId::now_v7(),
            outcome: lh_event::ChildOutcome::Success { summary: "did it".to_string() },
        };
        let resp = Response::ok(1, serde_json::to_value(ok_result).unwrap());
        handle_client_event(&mut app, &mut client, std::path::Path::new("."), ClientEvent::Response(resp)).await.unwrap();
        assert!(app.transcript.iter().any(|i| matches!(i, crate::app::TranscriptItem::System(t) if t.contains("success: did it"))));

        app.pending = Some((2, PendingKind::Delegate));
        let fail_result = lh_protocol::SessionDelegateResult {
            child_session_id: lh_event::SessionId::now_v7(),
            outcome: lh_event::ChildOutcome::Failed { message: "nope".to_string() },
        };
        let resp = Response::ok(2, serde_json::to_value(fail_result).unwrap());
        handle_client_event(&mut app, &mut client, std::path::Path::new("."), ClientEvent::Response(resp)).await.unwrap();
        assert!(app.transcript.iter().any(|i| matches!(i, crate::app::TranscriptItem::System(t) if t.contains("failed: nope"))));
    }

    async fn submit_line(app: &mut App, client: &mut DaemonClient, text: &str) {
        for c in text.chars() {
            handle_key(app, client, key(KeyCode::Char(c))).await.unwrap();
        }
        handle_key(app, client, key(KeyCode::Enter)).await.unwrap();
    }

    fn ready_app() -> App {
        let mut app = App::new();
        app.phase = ConnPhase::Ready;
        app
    }

    #[tokio::test]
    async fn slash_help_prints_the_command_list_without_sending_a_prompt() {
        let mut app = ready_app();
        let mut client = inert_client().await;

        submit_line(&mut app, &mut client, "/help").await;

        assert!(app.pending.is_none(), "a slash command must never start a session/prompt round trip");
        assert!(app.transcript.iter().any(|i| matches!(i, crate::app::TranscriptItem::System(t) if t.contains("/agents"))));
    }

    #[tokio::test]
    async fn slash_quit_sets_should_quit() {
        let mut app = ready_app();
        let mut client = inert_client().await;

        submit_line(&mut app, &mut client, "/quit").await;

        assert!(app.should_quit);
    }

    #[tokio::test]
    async fn slash_agents_with_none_registered_says_so() {
        let mut app = ready_app();
        let mut client = inert_client().await;

        submit_line(&mut app, &mut client, "/agents").await;

        assert!(app.transcript.iter().any(|i| matches!(i, crate::app::TranscriptItem::System(t) if t.contains("no delegated agents"))));
    }

    #[tokio::test]
    async fn slash_agents_lists_registered_agents_and_flags_primary_capable_ones() {
        let mut app = ready_app();
        app.available_agents = vec![
            lh_protocol::AgentInfo { kind: lh_event::AgentKind::ClaudeCode, can_be_primary: true },
            lh_protocol::AgentInfo { kind: lh_event::AgentKind::Codex, can_be_primary: false },
        ];
        let mut client = inert_client().await;

        submit_line(&mut app, &mut client, "/agents").await;

        let line = app
            .transcript
            .iter()
            .find_map(|i| match i {
                crate::app::TranscriptItem::System(t) if t.contains("registered agents") => Some(t.clone()),
                _ => None,
            })
            .expect("should have printed the agent list");
        assert!(line.contains("ClaudeCode (primary-capable)"));
        assert!(line.contains("Codex"));
        assert!(!line.contains("Codex (primary-capable)"));
    }

    #[tokio::test]
    async fn slash_delegate_with_missing_args_is_a_reported_error_not_a_request() {
        let mut app = ready_app();
        let mut client = inert_client().await;

        submit_line(&mut app, &mut client, "/delegate").await;
        assert!(app.pending.is_none());
        assert!(app.transcript.iter().any(|i| matches!(i, crate::app::TranscriptItem::Error(t) if t.contains("usage: /delegate"))));

        submit_line(&mut app, &mut client, "/delegate claude-code").await;
        assert!(app.pending.is_none(), "an agent with no task summary must not fire a request either");
    }

    #[tokio::test]
    async fn slash_delegate_with_a_task_sends_session_delegate_and_tracks_it() {
        let mut app = ready_app();
        let mut client = inert_client().await;

        submit_line(&mut app, &mut client, "/delegate claude-code fix the bug").await;

        assert_eq!(app.pending.map(|(_, kind)| kind), Some(PendingKind::Delegate));
    }

    #[tokio::test]
    async fn slash_model_sends_models_list_and_tracks_it() {
        let mut app = ready_app();
        let mut client = inert_client().await;

        submit_line(&mut app, &mut client, "/model").await;

        assert_eq!(app.pending.map(|(_, kind)| kind), Some(PendingKind::ModelsList));
    }

    #[tokio::test]
    async fn models_list_completing_opens_the_picker_with_the_current_model_preselected() {
        let mut app = ready_app();
        let mut client = inert_client().await;
        app.pending = Some((1, PendingKind::ModelsList));

        let result = lh_protocol::ModelsListResult {
            models: vec![
                lh_model_provider::ModelInfo { id: "claude-sonnet-5".to_string() },
                lh_model_provider::ModelInfo { id: "claude-opus-5".to_string() },
            ],
            current: "claude-opus-5".to_string(),
        };
        let resp = Response::ok(1, serde_json::to_value(result).unwrap());
        handle_client_event(&mut app, &mut client, std::path::Path::new("."), ClientEvent::Response(resp)).await.unwrap();

        assert!(app.model_picker_visible);
        assert_eq!(app.available_models, vec!["claude-sonnet-5".to_string(), "claude-opus-5".to_string()]);
        assert_eq!(app.current_model, Some("claude-opus-5".to_string()));
        assert_eq!(app.model_picker_selected, 1, "the currently-active model should be preselected");
    }

    #[tokio::test]
    async fn arrow_keys_cycle_the_model_pickers_selection_without_closing_it() {
        let mut app = ready_app();
        let mut client = inert_client().await;
        app.model_picker_visible = true;
        app.available_models = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        app.model_picker_selected = 0;

        handle_key(&mut app, &mut client, key(KeyCode::Down)).await.unwrap();
        assert_eq!(app.model_picker_selected, 1);
        assert!(app.model_picker_visible);

        handle_key(&mut app, &mut client, key(KeyCode::Up)).await.unwrap();
        assert_eq!(app.model_picker_selected, 0);
        assert!(app.model_picker_visible);
    }

    #[tokio::test]
    async fn escape_closes_the_model_picker_without_sending_a_selection() {
        let mut app = ready_app();
        let mut client = inert_client().await;
        app.model_picker_visible = true;
        app.available_models = vec!["a".to_string(), "b".to_string()];

        handle_key(&mut app, &mut client, key(KeyCode::Esc)).await.unwrap();

        assert!(!app.model_picker_visible);
        assert!(app.pending_model_select.is_none());
    }

    #[tokio::test]
    async fn enter_sends_model_select_for_the_highlighted_model_and_closes_the_picker() {
        let mut app = ready_app();
        let mut client = inert_client().await;
        app.model_picker_visible = true;
        app.available_models = vec!["a".to_string(), "b".to_string()];
        app.model_picker_selected = 1;

        handle_key(&mut app, &mut client, key(KeyCode::Enter)).await.unwrap();

        assert!(!app.model_picker_visible, "the picker closes immediately, before the response arrives");
        assert!(app.pending_model_select.is_some());
    }

    #[tokio::test]
    async fn a_successful_model_select_response_updates_current_model_and_reports_it() {
        let mut app = ready_app();
        let mut client = inert_client().await;
        app.pending_model_select = Some(7);

        let result = lh_protocol::ModelSelectResult { model: "claude-opus-5".to_string() };
        let resp = Response::ok(7, serde_json::to_value(result).unwrap());
        handle_client_event(&mut app, &mut client, std::path::Path::new("."), ClientEvent::Response(resp)).await.unwrap();

        assert!(app.pending_model_select.is_none());
        assert_eq!(app.current_model, Some("claude-opus-5".to_string()));
        assert!(app.transcript.iter().any(|i| matches!(i, crate::app::TranscriptItem::System(t) if t.contains("switched model to claude-opus-5"))));
    }

    #[tokio::test]
    async fn a_failed_model_select_response_reports_the_error_and_leaves_current_model_untouched() {
        let mut app = ready_app();
        app.current_model = Some("claude-sonnet-5".to_string());
        let mut client = inert_client().await;
        app.pending_model_select = Some(7);

        let mut resp = Response::ok(7, serde_json::json!({}));
        resp.result = None;
        resp.error = Some(RpcError { code: -1, message: "no such model".to_string() });
        handle_client_event(&mut app, &mut client, std::path::Path::new("."), ClientEvent::Response(resp)).await.unwrap();

        assert!(app.pending_model_select.is_none());
        assert_eq!(app.current_model, Some("claude-sonnet-5".to_string()), "a failed switch must not clobber the last-known model");
        assert!(app.transcript.iter().any(|i| matches!(i, crate::app::TranscriptItem::Error(t) if t.contains("model/select failed"))));
    }

    #[tokio::test]
    async fn a_model_select_response_never_touches_the_main_pending_gate() {
        let mut app = ready_app();
        let mut client = inert_client().await;
        app.pending = Some((5, PendingKind::Prompt)); // a real prompt is still in flight
        app.pending_model_select = Some(7);

        let result = lh_protocol::ModelSelectResult { model: "claude-opus-5".to_string() };
        let resp = Response::ok(7, serde_json::to_value(result).unwrap());
        handle_client_event(&mut app, &mut client, std::path::Path::new("."), ClientEvent::Response(resp)).await.unwrap();

        assert_eq!(app.pending, Some((5, PendingKind::Prompt)), "the real pending prompt must be untouched");
    }

    #[tokio::test]
    async fn an_unknown_slash_command_is_a_reported_error() {
        let mut app = ready_app();
        let mut client = inert_client().await;

        submit_line(&mut app, &mut client, "/nonsense").await;

        assert!(app.pending.is_none());
        assert!(app.transcript.iter().any(|i| matches!(i, crate::app::TranscriptItem::Error(t) if t.contains("unknown command '/nonsense'"))));
    }

    #[test]
    fn parse_primary_arg_with_no_args_defaults_to_native() {
        assert!(matches!(parse_primary_arg(&[]).unwrap(), lh_protocol::PrimarySelector::Native));
    }

    #[test]
    fn parse_primary_arg_with_primary_and_a_name_delegates_to_it() {
        let args = vec!["--primary".to_string(), "claude-code".to_string()];
        let result = parse_primary_arg(&args).unwrap();
        assert!(matches!(result, lh_protocol::PrimarySelector::Delegated { agent: lh_event::AgentKind::ClaudeCode }));
    }

    #[test]
    fn parse_primary_arg_with_primary_and_no_value_is_an_error() {
        let args = vec!["--primary".to_string()];
        assert!(parse_primary_arg(&args).is_err());
    }

    #[test]
    fn parse_primary_arg_with_an_unknown_flag_is_an_error() {
        let args = vec!["--bogus".to_string()];
        assert!(parse_primary_arg(&args).is_err());
    }

    #[test]
    fn parse_agent_kind_recognizes_the_known_names_and_falls_back_to_custom() {
        assert!(matches!(parse_agent_kind("claude-code"), lh_event::AgentKind::ClaudeCode));
        assert!(matches!(parse_agent_kind("codex"), lh_event::AgentKind::Codex));
        assert!(matches!(parse_agent_kind("gemini-cli"), lh_event::AgentKind::GeminiCli));
        assert!(matches!(parse_agent_kind("goose"), lh_event::AgentKind::Goose));
        assert!(matches!(parse_agent_kind("opencode"), lh_event::AgentKind::OpenCode));
        assert!(matches!(parse_agent_kind("my-custom-thing"), lh_event::AgentKind::Custom { name } if name == "my-custom-thing"));
    }
}
