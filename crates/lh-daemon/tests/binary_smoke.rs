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

/// Spawns the real daemon rooted at `workspace` (after letting `configure`
/// customize env vars beyond the defaults below), waits for it to start
/// accepting connections, drives a minimal `initialize` round trip to prove
/// it's actually serving requests (not just that the process exists), then
/// sends SIGTERM and asserts a clean exit. Returns the child's captured
/// stderr so callers can assert a specific "failed to load ..." message was
/// actually logged, not silently swallowed -- covers main.rs's
/// load-and-log-don't-crash convention for each config source it reads at
/// startup: a malformed file must degrade to "feature unavailable", never a
/// startup crash.
async fn assert_daemon_starts_and_shuts_down_cleanly(
    workspace: &std::path::Path,
    configure: impl FnOnce(&mut tokio::process::Command),
) -> String {
    use tokio::io::AsyncReadExt;

    let canonical = workspace.canonicalize().unwrap();
    let sock_path = default_socket_path(&canonical);

    let mut cmd = tokio::process::Command::new(daemon_bin());
    cmd.current_dir(&canonical)
        .env_remove("LITE_HARNESS_PROVIDERS_FILE")
        .env_remove("LITE_HARNESS_AGENTS_FILE")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    configure(&mut cmd);

    let mut child = cmd.spawn().expect("failed to spawn lite-harnessd");
    let mut stderr = child.stderr.take().unwrap();

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
    drop(w);
    drop(reader);

    if let Some(pid) = child.id() {
        let _ = std::process::Command::new("kill").args(["-TERM", &pid.to_string()]).status();
    }
    let status = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
        .await
        .expect("daemon did not exit within 5s of SIGTERM")
        .unwrap();
    assert!(
        status.success(),
        "daemon should exit 0 on graceful SIGTERM shutdown despite bad config, got {status:?}"
    );

    let mut buf = String::new();
    stderr.read_to_string(&mut buf).await.unwrap();
    buf
}

#[tokio::test]
async fn malformed_project_policy_file_logs_and_still_serves() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(workspace.path().join(".lite-harness")).unwrap();
    std::fs::write(workspace.path().join(".lite-harness/policy.toml"), "this is not valid toml [[[").unwrap();

    let stderr = assert_daemon_starts_and_shuts_down_cleanly(workspace.path(), |cmd| {
        cmd.env("HOME", home.path());
    })
    .await;
    assert!(stderr.contains("failed to load project policy store"), "stderr was: {stderr}");
}

#[tokio::test]
async fn malformed_global_policy_file_logs_and_still_serves() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".config/lite-harness")).unwrap();
    std::fs::write(home.path().join(".config/lite-harness/policy.toml"), "this is not valid toml [[[").unwrap();

    let stderr = assert_daemon_starts_and_shuts_down_cleanly(workspace.path(), |cmd| {
        cmd.env("HOME", home.path());
    })
    .await;
    assert!(stderr.contains("failed to load global policy store"), "stderr was: {stderr}");
}

#[tokio::test]
async fn malformed_pricing_overrides_file_logs_and_still_serves() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".config/lite-harness")).unwrap();
    std::fs::write(home.path().join(".config/lite-harness/pricing.toml"), "this is not valid toml [[[").unwrap();

    let stderr = assert_daemon_starts_and_shuts_down_cleanly(workspace.path(), |cmd| {
        cmd.env("HOME", home.path());
    })
    .await;
    assert!(stderr.contains("failed to load pricing overrides"), "stderr was: {stderr}");
}

#[tokio::test]
async fn malformed_agent_registry_file_logs_and_still_serves() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let agents_path = workspace.path().join("agents.toml");
    std::fs::write(&agents_path, "this is not valid toml [[[").unwrap();

    let stderr = assert_daemon_starts_and_shuts_down_cleanly(workspace.path(), |cmd| {
        cmd.env("HOME", home.path()).env("LITE_HARNESS_AGENTS_FILE", &agents_path);
    })
    .await;
    assert!(stderr.contains("failed to load agent registry"), "stderr was: {stderr}");
}

#[tokio::test]
async fn malformed_provider_config_logs_and_still_serves() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let providers_path = workspace.path().join("providers.toml");
    std::fs::write(&providers_path, "this is not valid toml [[[").unwrap();

    let stderr = assert_daemon_starts_and_shuts_down_cleanly(workspace.path(), |cmd| {
        cmd.env("HOME", home.path()).env("LITE_HARNESS_PROVIDERS_FILE", &providers_path);
    })
    .await;
    assert!(stderr.contains("failed to load model provider config"), "stderr was: {stderr}");
}

#[tokio::test]
async fn valid_provider_config_resolves_and_logs_ready() {
    // Covers the `Ok(Some(p))` arm of main.rs's provider-resolution match --
    // every other provider-related test here exercises the "config missing
    // or broken" arms, either through this file or through
    // `providers.rs`'s own unit tests (which call `resolve_default_provider`
    // directly, never through the real binary's main.rs wiring).
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let providers_path = workspace.path().join("providers.toml");
    std::fs::write(
        &providers_path,
        "default = \"mock\"\n\n[[provider]]\nname = \"mock\"\nprotocol = \"open-ai-compatible\"\n\
         base_url = \"http://127.0.0.1:1\"\napi_key_env = \"BINARY_SMOKE_TEST_PROVIDER_KEY\"\n\
         default_model = \"mock-model\"\n",
    )
    .unwrap();

    let stderr = assert_daemon_starts_and_shuts_down_cleanly(workspace.path(), |cmd| {
        cmd.env("HOME", home.path())
            .env("LITE_HARNESS_PROVIDERS_FILE", &providers_path)
            .env("BINARY_SMOKE_TEST_PROVIDER_KEY", "sk-test");
    })
    .await;
    assert!(stderr.contains("model provider ready (model=mock-model)"), "stderr was: {stderr}");
}

#[tokio::test]
async fn no_home_env_var_set_is_handled_gracefully() {
    // No HOME at all -- global policy and pricing overrides both skip
    // straight to "nothing to load" (the `None => None` / no-op branches),
    // rather than erroring or panicking on an unset var.
    let workspace = tempfile::tempdir().unwrap();
    let stderr = assert_daemon_starts_and_shuts_down_cleanly(workspace.path(), |cmd| {
        cmd.env_remove("HOME");
    })
    .await;
    assert!(!stderr.contains("failed to load"), "stderr was: {stderr}");
}

#[tokio::test]
async fn a_stale_socket_file_left_over_from_a_previous_run_is_removed_and_rebound() {
    // Covers main.rs's `if sock_path.exists() { remove_file(...) }` branch:
    // simulate a daemon that died without cleaning up its socket file (e.g.
    // SIGKILL) by pre-creating a real listener at the exact path a fresh
    // daemon in this workspace would use, then dropping it -- the file
    // stays on disk (Unix domain sockets aren't removed on listener drop),
    // exactly the stale-socket scenario the real world produces.
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let canonical = workspace.path().canonicalize().unwrap();
    let sock_path = default_socket_path(&canonical);
    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    {
        let _stale_listener = tokio::net::UnixListener::bind(&sock_path).unwrap();
        assert!(sock_path.exists());
        // dropped here, leaving the socket file behind on disk
    }
    assert!(sock_path.exists(), "the stale socket file should still be on disk after the listener is dropped");

    // The real daemon must still start cleanly against that same path.
    assert_daemon_starts_and_shuts_down_cleanly(workspace.path(), |cmd| {
        cmd.env("HOME", home.path());
    })
    .await;
}

#[tokio::test]
async fn a_connection_that_violates_the_protocol_is_logged_by_the_accept_loop_and_the_daemon_keeps_serving() {
    // Covers main.rs's `if let Err(e) = handle_connection(...) { eprintln!("connection
    // error: ...") }` branch: `tests/e2e.rs` exercises `handle_connection`'s own
    // internal error handling in-process, but never through the real
    // accept-loop task that logs a top-level Err from it -- that only
    // happens via the actual spawned binary.
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
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn lite-harnessd");
    let mut stderr = child.stderr.take().unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if UnixStream::connect(&sock_path).await.is_ok() {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "daemon never started listening");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // A well-formed message, but the wrong method where `initialize` is
    // required -- `handle_connection`'s `ensure!` fails and the connection
    // task returns an `Err` that only the accept loop ever sees.
    let stream = UnixStream::connect(&sock_path).await.unwrap();
    let (_r, mut w) = stream.into_split();
    write_message(
        &mut w,
        &Message::Request(Request::new(
            1,
            methods::SESSION_CREATE,
            serde_json::to_value(SessionCreateParams { cwd: ".".to_string(), primary: PrimarySelector::Native }).unwrap(),
        )),
    )
    .await
    .unwrap();
    drop(w);

    // The daemon itself must keep running and accept a fresh, well-formed
    // connection right after -- one bad connection must not take the whole
    // process down.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let stderr_saw_it = loop {
        let mut buf = [0u8; 4096];
        use tokio::io::AsyncReadExt;
        match tokio::time::timeout(std::time::Duration::from_millis(200), stderr.read(&mut buf)).await {
            Ok(Ok(n)) if n > 0 => {
                if String::from_utf8_lossy(&buf[..n]).contains("connection error") {
                    break true;
                }
            }
            _ => {}
        }
        if tokio::time::Instant::now() > deadline {
            break false;
        }
    };
    assert!(stderr_saw_it, "expected a \"connection error: ...\" line on stderr");

    let stream2 = UnixStream::connect(&sock_path).await.unwrap();
    let (r2, mut w2) = stream2.into_split();
    let mut reader2 = buffered(r2);
    write_message(
        &mut w2,
        &Message::Request(Request::new(1, methods::INITIALIZE, serde_json::to_value(InitializeParams { protocol_version: PROTOCOL_VERSION }).unwrap())),
    )
    .await
    .unwrap();
    let Message::Response(resp) = read_message(&mut reader2).await.unwrap().unwrap() else {
        panic!("expected a Response to initialize");
    };
    let result: InitializeResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(result.protocol_version, PROTOCOL_VERSION);

    if let Some(pid) = child.id() {
        let _ = std::process::Command::new("kill").args(["-TERM", &pid.to_string()]).status();
    }
    let status = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
        .await
        .expect("daemon did not exit within 5s of SIGTERM")
        .unwrap();
    assert!(status.success(), "daemon should exit 0 on graceful SIGTERM shutdown, got {status:?}");
}
