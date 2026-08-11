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

use crate::app::{App, ConnPhase, PendingKind, PendingPermission};
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
            app.push_system(format!("permission requested: {request:?}"));
            app.pending_permission = Some(PendingPermission { request_id });
        }
        ClientEvent::Response(resp) => {
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
                    // Fire-and-forget: not tracked in `pending` (it doesn't
                    // gate input), so its Response falls through the
                    // `!= pending_id` branch above and is ignored for now.
                    // Rendering the ledger rollup lands with the
                    // status-bar/cost-display phase.
                    if let Some(session_id) = app.session_id {
                        let _ = client
                            .send_request(methods::LEDGER_QUERY, serde_json::to_value(LedgerQueryParams { session_id })?)
                            .await;
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
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if let Some((id, decision)) = app.decide_pending_permission(true) {
                    client.respond(id, serde_json::to_value(PermissionAskResult { decision })?).await?;
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Enter => {
                if let Some((id, decision)) = app.decide_pending_permission(false) {
                    client.respond(id, serde_json::to_value(PermissionAskResult { decision })?).await?;
                }
            }
            _ => {}
        }
        return Ok(());
    }

    if !app.input_enabled() {
        return Ok(());
    }

    match key.code {
        KeyCode::Enter => {
            if app.input.trim().is_empty() {
                return Ok(());
            }
            let text = std::mem::take(&mut app.input);
            app.push_user_message(text.clone());
            let prompt_id = client
                .send_request(methods::SESSION_PROMPT, serde_json::to_value(SessionPromptParams { text })?)
                .await?;
            app.pending = Some((prompt_id, PendingKind::Prompt));
        }
        KeyCode::Backspace => {
            app.input.pop();
        }
        KeyCode::Char(c) => {
            app.input.push(c);
        }
        _ => {}
    }
    Ok(())
}

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
    async fn answering_a_pending_permission_with_n_denies_it() {
        let mut app = App::new();
        let mut client = inert_client().await;
        app.pending_permission = Some(PendingPermission { request_id: -1 });

        handle_key(&mut app, &mut client, key(KeyCode::Char('n'))).await.unwrap();

        assert!(app.pending_permission.is_none());
    }

    #[tokio::test]
    async fn an_unrecognized_key_while_answering_a_permission_is_a_no_op() {
        let mut app = App::new();
        let mut client = inert_client().await;
        app.pending_permission = Some(PendingPermission { request_id: -1 });

        handle_key(&mut app, &mut client, key(KeyCode::Left)).await.unwrap();

        assert!(app.pending_permission.is_some(), "an unrelated key must not answer the pending permission");
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
        app.input = "unchanged".to_string();
        let mut client = inert_client().await;

        handle_key(&mut app, &mut client, key(KeyCode::Left)).await.unwrap();

        assert_eq!(app.input, "unchanged");
        assert!(app.pending.is_none());
    }

    #[tokio::test]
    async fn backspace_removes_the_last_input_character() {
        let mut app = App::new();
        app.phase = ConnPhase::Ready;
        app.input = "hi".to_string();
        let mut client = inert_client().await;

        handle_key(&mut app, &mut client, key(KeyCode::Backspace)).await.unwrap();

        assert_eq!(app.input, "h");
    }

    #[test]
    fn kind_label_names_every_variant() {
        assert_eq!(kind_label(PendingKind::Initialize), "initialize");
        assert_eq!(kind_label(PendingKind::SessionCreate), "session/create");
        assert_eq!(kind_label(PendingKind::Prompt), "session/prompt");
    }
}
