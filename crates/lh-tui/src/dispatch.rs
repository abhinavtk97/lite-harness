//! Wires `ClientEvent`s (from the daemon) and `KeyEvent`s (from the
//! terminal) into `App` state transitions. Pure glue -- no rendering, no
//! terminal setup -- which is what lets `tests/e2e.rs` drive a real daemon
//! through these same functions with synthetic key events and no real tty.

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use lh_protocol::{
    methods, InitializeParams, LedgerQueryParams, PermissionAskResult, PrimarySelector, SessionCreateParams,
    SessionCreateResult, SessionPromptParams, SessionPromptResult, PROTOCOL_VERSION,
};

use crate::app::{describe_permission_action, App, ConnPhase, PendingKind, PendingPermission, PermissionChoice};
use crate::client::{ClientEvent, DaemonClient};

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

            let Some((pending_id, kind)) = app.pending else { return Ok(()) };
            if resp.id != pending_id {
                // Not what we're waiting on (e.g. a stray response to a
                // fire-and-forget request) -- leave `pending` alone.
                return Ok(());
            }
            app.pending = None;

            if let Some(err) = resp.error {
                app.push_error(format!("{} failed: {}", kind_label(kind), err.message));
                if kind == PendingKind::Initialize || kind == PendingKind::SessionCreate {
                    app.should_quit = true;
                }
                return Ok(());
            }
            let result = resp.result.unwrap_or_default();

            match kind {
                PendingKind::Initialize => {
                    app.status = "connected".to_string();
                    let create_id = client
                        .send_request(
                            methods::SESSION_CREATE,
                            serde_json::to_value(SessionCreateParams {
                                cwd: cwd.to_string_lossy().to_string(),
                                primary: PrimarySelector::Native,
                            })?,
                        )
                        .await?;
                    app.pending = Some((create_id, PendingKind::SessionCreate));
                }
                PendingKind::SessionCreate => {
                    let result: SessionCreateResult = serde_json::from_value(result)?;
                    app.session_id = Some(result.session_id);
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
            let prompt_id = client
                .send_request(methods::SESSION_PROMPT, serde_json::to_value(SessionPromptParams { text })?)
                .await?;
            app.pending = Some((prompt_id, PendingKind::Prompt));
        }
        // Ctrl+Backspace and Ctrl+W both do readline's word-delete --
        // terminals vary in whether they send a distinguishable
        // Ctrl+Backspace at all, so Ctrl+W is the reliable one of the two.
        KeyCode::Backspace if ctrl => app.input.delete_word_before_cursor(),
        KeyCode::Char('w') if ctrl => app.input.delete_word_before_cursor(),
        KeyCode::Backspace => app.input.backspace(),
        KeyCode::Left => app.input.move_left(),
        KeyCode::Right => app.input.move_right(),
        KeyCode::Home => app.input.move_line_start(),
        KeyCode::End => app.input.move_line_end(),
        KeyCode::Char(c) => app.input.insert_char(c),
        _ => {}
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
        PendingKind::SessionCreate => "session/create",
        PendingKind::Prompt => "session/prompt",
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

    fn pending_permission_app() -> App {
        let mut app = App::new();
        app.pending_permission =
            Some(PendingPermission { request_id: -1, request: crate::app::fake_permission_request(), selected: 0 });
        app
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
    async fn a_ledger_query_response_updates_last_ledger_without_touching_pending() {
        let mut app = App::new();
        let mut client = inert_client().await;
        app.pending = Some((5, PendingKind::Prompt)); // a real prompt is still in flight
        app.pending_ledger_query = Some(42);

        let rollup = lh_ledger::LedgerRollup {
            session_id: lh_event::SessionId::now_v7(),
            input_tokens: Some(10),
            output_tokens: Some(4),
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
        assert_eq!(kind_label(PendingKind::SessionCreate), "session/create");
        assert_eq!(kind_label(PendingKind::Prompt), "session/prompt");
    }
}
