//! In-process integration tests for `lite-harness-web`: binds
//! `lh_web_backend::build_router` to an ephemeral port within the test's
//! own process (no subprocess -- see the crate doc for why: the same
//! async-task coverage-flushing gap `lh-daemon` hit with subprocesses
//! doesn't apply to a task spawned inside the test's own process), drives
//! a real WebSocket client against it, and bridges through to a real
//! `lite-harnessd` daemon.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use lh_protocol::{default_socket_path, methods, InitializeParams, InitializeResult, Message as ProtoMessage, PROTOCOL_VERSION};
use lh_web_backend::AppState;
use tokio_tungstenite::tungstenite::Message as WsMessage;

fn daemon_bin() -> PathBuf {
    // lite-harnessd is a sibling workspace binary, not a dependency of
    // this crate -- same derivation `lh_protocol::daemon_binary_path()`
    // uses in production. Derived from this crate's *own* bin target's
    // `CARGO_BIN_EXE_*` (guaranteed correct regardless of which target
    // subdirectory the build tool used -- plain `cargo test` and
    // `cargo-llvm-cov` versions disagree on this, see lh-protocol's own
    // `real_daemon_bin` test helper for the version-drift details) rather
    // than reconstructing the path by hand. Requires `cargo test
    // --workspace`.
    PathBuf::from(env!("CARGO_BIN_EXE_lite-harness-web"))
        .with_file_name(if cfg!(windows) { "lite-harnessd.exe" } else { "lite-harnessd" })
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

async fn start_daemon(workspace: &Path, home: &Path) -> (DaemonGuard, PathBuf) {
    let canonical = workspace.canonicalize().unwrap();
    let sock_path = default_socket_path(&canonical);
    let child = std::process::Command::new(daemon_bin())
        .current_dir(&canonical)
        .env("HOME", home)
        .env_remove("LITE_HARNESS_PROVIDERS_FILE")
        .env_remove("LITE_HARNESS_AGENTS_FILE")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("failed to spawn lite-harnessd");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if std::os::unix::net::UnixStream::connect(&sock_path).is_ok() {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "daemon never started listening");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    (DaemonGuard { child }, sock_path)
}

/// Binds `lh_web_backend::build_router` to an ephemeral localhost port
/// in-process and returns its base URL, keeping the serve task alive for
/// the caller's lifetime by just leaking the JoinHandle (fine for a
/// short-lived test process).
async fn start_web_backend(sock_path: PathBuf, cwd: &str, static_dir: &str) -> String {
    let state = Arc::new(AppState { sock_path, cwd: cwd.to_string() });
    let app = lh_web_backend::build_router(state, static_dir);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn api_cwd_reflects_the_backends_own_workspace() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let (_daemon, sock_path) = start_daemon(workspace.path(), home.path()).await;
    let base = start_web_backend(sock_path, "/some/workspace/path", "/nonexistent-static-dir").await;

    let resp = reqwest::get(format!("{base}/api/cwd")).await.unwrap();
    assert!(resp.status().is_success());
    let body = resp.text().await.unwrap();
    assert_eq!(body, "/some/workspace/path");
}

#[tokio::test]
async fn static_files_are_served_from_the_configured_directory() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let (_daemon, sock_path) = start_daemon(workspace.path(), home.path()).await;

    let static_dir = tempfile::tempdir().unwrap();
    std::fs::write(static_dir.path().join("index.html"), "<!doctype html><title>test</title>").unwrap();

    let base = start_web_backend(sock_path, "/cwd", static_dir.path().to_str().unwrap()).await;

    let resp = reqwest::get(format!("{base}/index.html")).await.unwrap();
    assert!(resp.status().is_success());
    assert!(resp.text().await.unwrap().contains("test"));

    // A path that doesn't exist under the static dir -- ServeDir's own
    // fallback behavior, not something this crate implements itself, but
    // worth pinning so a future dependency bump can't silently change it
    // to a 200 or a panic.
    let missing = reqwest::get(format!("{base}/does-not-exist.txt")).await.unwrap();
    assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn websocket_bridges_a_full_initialize_and_session_create_round_trip() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let (_daemon, sock_path) = start_daemon(workspace.path(), home.path()).await;
    let base = start_web_backend(sock_path, workspace.path().to_str().unwrap(), "/nonexistent-static-dir").await;
    let ws_url = base.replacen("http://", "ws://", 1) + "/ws";

    let (mut ws, _resp) = tokio_tungstenite::connect_async(ws_url).await.unwrap();

    // initialize
    ws.send(WsMessage::Text(
        serde_json::to_string(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": methods::INITIALIZE,
            "params": InitializeParams { protocol_version: PROTOCOL_VERSION },
        }))
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();
    let reply = ws.next().await.unwrap().unwrap();
    let WsMessage::Text(text) = reply else { panic!("expected a text frame, got {reply:?}") };
    let msg: ProtoMessage = serde_json::from_str(&text).unwrap();
    let ProtoMessage::Response(resp) = msg else { panic!("expected a Response, got {msg:?}") };
    let result: InitializeResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(result.protocol_version, PROTOCOL_VERSION);

    // agents/list -- proves the bridge carries an arbitrary second request
    // over the same connection, not just the first byte-for-byte.
    ws.send(WsMessage::Text(
        serde_json::to_string(&serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": methods::AGENTS_LIST, "params": {}}))
            .unwrap()
            .into(),
    ))
    .await
    .unwrap();
    let reply2 = ws.next().await.unwrap().unwrap();
    let WsMessage::Text(text2) = reply2 else { panic!("expected a text frame, got {reply2:?}") };
    assert!(text2.contains("\"agents\""), "got: {text2}");

    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn websocket_closing_the_browser_side_does_not_hang_the_bridge_task() {
    // A regression guard for the bridge's tokio::select! shutdown path:
    // closing the client side must make the bridge task end (and thus the
    // daemon connection close) rather than leaking forever. Verified
    // indirectly: connecting again afterwards must still work fine.
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let (_daemon, sock_path) = start_daemon(workspace.path(), home.path()).await;
    let base = start_web_backend(sock_path, workspace.path().to_str().unwrap(), "/nonexistent-static-dir").await;
    let ws_url = base.replacen("http://", "ws://", 1) + "/ws";

    {
        let (mut ws, _resp) = tokio_tungstenite::connect_async(&ws_url).await.unwrap();
        ws.close(None).await.unwrap();
    }

    let (mut ws2, _resp2) = tokio_tungstenite::connect_async(&ws_url).await.unwrap();
    ws2.send(WsMessage::Text(
        serde_json::to_string(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": methods::INITIALIZE,
            "params": InitializeParams { protocol_version: PROTOCOL_VERSION },
        }))
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();
    let reply = ws2.next().await.unwrap().unwrap();
    assert!(matches!(reply, WsMessage::Text(_)));
}
