//! One real end-to-end test: a real `lite-harnessd` + scripted mock model
//! server, driven through `lh_tui::client::DaemonClient` +
//! `lh_tui::dispatch` + `lh_tui::app::App` exactly the way `main.rs`'s
//! event loop does, but in-process with synthetic `KeyEvent`s instead of a
//! real terminal or a subprocess for the client half -- proves the whole
//! plumbing (connect -> initialize -> session/create -> type a prompt ->
//! live permission ask -> allow -> turn completes) actually works end to
//! end, matching this project's live-e2e-verification discipline.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use lh_protocol::default_socket_path;
use lh_tui::app::{App, ConnPhase, TranscriptItem};
use lh_tui::client::{ClientEvent, DaemonClient};
use lh_tui::dispatch;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn daemon_bin() -> PathBuf {
    // lite-harnessd is a sibling workspace binary, not a dependency of
    // this crate -- derived from this crate's own CARGO_BIN_EXE (correct
    // regardless of which target subdirectory the build tool used; see
    // lh-web-backend's e2e.rs for why a hand-built "target/debug/..." path
    // would be wrong under some cargo-llvm-cov versions).
    PathBuf::from(env!("CARGO_BIN_EXE_lite-harness-tui")).with_file_name("lite-harnessd")
}

fn write_file(dir: &std::path::Path, rel: &str, contents: &str) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

fn providers_toml(base_url: &str) -> String {
    format!(
        "default = \"mock\"\n\n[[provider]]\nname = \"mock\"\nprotocol = \"open-ai-compatible\"\n\
         base_url = \"{base_url}/v1\"\napi_key_env = \"MOCK_API_KEY\"\ndefault_model = \"mock-model\"\n"
    )
}

fn text_response(text: &str) -> serde_json::Value {
    serde_json::json!({
        "choices": [{"message": {"role": "assistant", "content": text}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5}
    })
}

fn bash_tool_call_response(call_id: &str, command: &str) -> serde_json::Value {
    serde_json::json!({
        "choices": [{
            "message": {
                "role": "assistant", "content": null,
                "tool_calls": [{
                    "id": call_id, "type": "function",
                    "function": {"name": "bash", "arguments": serde_json::to_string(&serde_json::json!({"command": command})).unwrap()},
                }],
            },
            "finish_reason": "tool_calls",
        }],
        "usage": {"prompt_tokens": 15, "completion_tokens": 8},
    })
}

struct ScriptedResponder {
    bodies: Mutex<VecDeque<serde_json::Value>>,
}

impl wiremock::Respond for ScriptedResponder {
    fn respond(&self, _req: &wiremock::Request) -> ResponseTemplate {
        let mut q = self.bodies.lock().unwrap();
        let body = q.pop_front().unwrap_or_else(|| text_response("done"));
        ResponseTemplate::new(200).set_body_json(body)
    }
}

async fn scripted_mock_server(bodies: Vec<serde_json::Value>) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/chat/completions"))
        .respond_with(ScriptedResponder { bodies: Mutex::new(bodies.into()) })
        .mount(&server)
        .await;
    server
}

struct DaemonGuard {
    child: std::process::Child,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = std::process::Command::new("kill").args(["-TERM", &self.child.id().to_string()]).status();
        let _ = self.child.wait();
    }
}

async fn start_daemon(workspace: &std::path::Path, home: &std::path::Path, env: &[(&str, &str)]) -> DaemonGuard {
    let canonical = workspace.canonicalize().unwrap();
    let sock_path = default_socket_path(&canonical);
    let mut cmd = std::process::Command::new(daemon_bin());
    cmd.current_dir(&canonical)
        .env("HOME", home)
        .env_remove("LITE_HARNESS_PROVIDERS_FILE")
        .env_remove("LITE_HARNESS_AGENTS_FILE")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let child = cmd.spawn().expect("failed to spawn lite-harnessd");

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if std::os::unix::net::UnixStream::connect(&sock_path).is_ok() {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "daemon never started listening");
        std::thread::sleep(Duration::from_millis(50));
    }
    DaemonGuard { child }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// Drives `app`/`client` against real daemon events until `done` returns
/// true, applying every event through the exact same `dispatch` functions
/// `main.rs` uses. Bounded by a deadline so a broken assumption fails the
/// test instead of hanging CI forever.
async fn pump_until(
    app: &mut App,
    client: &mut DaemonClient,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<ClientEvent>,
    cwd: &std::path::Path,
    mut done: impl FnMut(&App) -> bool,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !done(app) {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "pump_until timed out; transcript so far: {:?}", app.transcript);
        let event = tokio::time::timeout(remaining, events.recv())
            .await
            .expect("timed out waiting for the next daemon event")
            .expect("daemon closed the connection unexpectedly");
        dispatch::handle_client_event(app, client, cwd, event).await.unwrap();
    }
}

#[tokio::test]
async fn a_full_turn_with_a_live_permission_ask_round_trips_through_the_real_daemon() {
    let server = scripted_mock_server(vec![bash_tool_call_response("call_1", "echo hi"), text_response("done")]).await;
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    write_file(workspace.path(), "providers.toml", &providers_toml(&server.uri()));
    let _daemon = start_daemon(
        workspace.path(),
        home.path(),
        &[
            ("LITE_HARNESS_PROVIDERS_FILE", workspace.path().join("providers.toml").to_str().unwrap()),
            ("MOCK_API_KEY", "test"),
        ],
    )
    .await;

    let canonical = workspace.path().canonicalize().unwrap();
    let sock_path = default_socket_path(&canonical);
    let (mut client, mut events) = DaemonClient::connect(&sock_path).await.unwrap();

    let mut app = App::new();
    dispatch::start_handshake(&mut client, &mut app).await.unwrap();
    pump_until(&mut app, &mut client, &mut events, &canonical, |a| a.phase == ConnPhase::Ready).await;
    assert!(app.session_id.is_some());

    for c in "run echo".chars() {
        dispatch::handle_key(&mut app, &mut client, key(KeyCode::Char(c))).await.unwrap();
    }
    dispatch::handle_key(&mut app, &mut client, key(KeyCode::Enter)).await.unwrap();
    assert!(matches!(app.transcript.first(), Some(TranscriptItem::User(t)) if t == "run echo"));

    pump_until(&mut app, &mut client, &mut events, &canonical, |a| a.pending_permission.is_some()).await;

    dispatch::handle_key(&mut app, &mut client, key(KeyCode::Char('y'))).await.unwrap();
    assert!(app.pending_permission.is_none(), "answering should clear the pending permission immediately");

    pump_until(&mut app, &mut client, &mut events, &canonical, |a| {
        a.transcript.iter().any(|item| matches!(item, TranscriptItem::System(t) if t.contains("turn complete")))
    })
    .await;

    assert!(app.transcript.iter().any(|item| matches!(item, TranscriptItem::ToolCall { tool_name, .. } if tool_name == "bash")));
    assert!(app.transcript.iter().any(|item| matches!(item, TranscriptItem::ToolCallUpdate { .. })));
}
