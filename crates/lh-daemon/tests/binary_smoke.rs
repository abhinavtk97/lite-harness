//! One real end-to-end smoke test: spawns the actual compiled
//! `lite-harnessd` binary (not `lh_daemon::handle_connection` in-process)
//! and drives a minimal `initialize` -> `session/create` round trip over
//! its real Unix socket, confirming the thin `main.rs` wiring (env/config
//! parsing, socket bind, accept loop, graceful SIGTERM shutdown) actually
//! works end-to-end. `tests/e2e.rs` carries the scenario/coverage weight
//! in-process; this file exists purely for "the real binary still works"
//! confidence, matching this project's established live-e2e-verification
//! discipline.

use std::path::PathBuf;

use lh_protocol::{
    buffered, default_socket_path, methods, read_message, write_message, InitializeParams,
    InitializeResult, Message, PrimarySelector, Request, SessionCreateParams, SessionCreateResult,
    PROTOCOL_VERSION,
};
use tokio::net::UnixStream;

fn daemon_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_lite-harnessd"))
}

#[tokio::test]
async fn the_real_binary_starts_accepts_a_connection_and_shuts_down_on_sigterm() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let canonical = workspace.path().canonicalize().unwrap();
    let sock_path = default_socket_path(&canonical);

    let mut child = tokio::process::Command::new(daemon_bin())
        .current_dir(&canonical)
        .env("HOME", home.path())
        .env_remove("LITE_HARNESS_PROVIDERS_FILE")
        .env_remove("LITE_HARNESS_AGENTS_FILE")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("failed to spawn lite-harnessd");

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if UnixStream::connect(&sock_path).await.is_ok() {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "daemon never started listening");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let stream = UnixStream::connect(&sock_path).await.unwrap();
    let (r, mut w) = stream.into_split();
    let mut reader = buffered(r);

    write_message(
        &mut w,
        &Message::Request(Request::new(1, methods::INITIALIZE, serde_json::to_value(InitializeParams { protocol_version: PROTOCOL_VERSION }).unwrap())),
    )
    .await
    .unwrap();
    let Message::Response(resp) = read_message(&mut reader).await.unwrap().unwrap() else {
        panic!("expected a Response to initialize");
    };
    let result: InitializeResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(result.protocol_version, PROTOCOL_VERSION);

    write_message(
        &mut w,
        &Message::Request(Request::new(
            2,
            methods::SESSION_CREATE,
            serde_json::to_value(SessionCreateParams {
                cwd: workspace.path().to_string_lossy().to_string(),
                primary: PrimarySelector::Native,
            })
            .unwrap(),
        )),
    )
    .await
    .unwrap();
    let Message::Response(resp) = read_message(&mut reader).await.unwrap().unwrap() else {
        panic!("expected a Response to session/create");
    };
    let _result: SessionCreateResult = serde_json::from_value(resp.result.unwrap()).unwrap();

    // Graceful shutdown: SIGTERM should make the real process exit on its
    // own (not require a SIGKILL), per main.rs's signal handling.
    if let Some(pid) = child.id() {
        let _ = std::process::Command::new("kill").args(["-TERM", &pid.to_string()]).status();
    }
    let status = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
        .await
        .expect("daemon did not exit within 5s of SIGTERM")
        .unwrap();
    assert!(status.success(), "daemon should exit 0 on graceful SIGTERM shutdown, got {status:?}");
}
