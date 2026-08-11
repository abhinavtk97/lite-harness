//! One real end-to-end smoke test: spawns the actual compiled
//! `lite-harness-web` binary (not `lh_web_backend::build_router` in-process)
//! to confirm the thin `main.rs` wiring (env parsing, static dir default vs
//! override, port default vs override, socket bind) actually works.
//! `tests/e2e.rs` carries the scenario/coverage weight in-process; this
//! file exists purely for "the real binary still works" confidence,
//! matching `lh-daemon/tests/binary_smoke.rs`.

use std::path::PathBuf;

fn web_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_lite-harness-web"))
}

/// Reserves a free localhost port by binding then immediately releasing it
/// -- `lite-harness-web`'s own `main.rs` has no port-0-and-report-back
/// mechanism (it `eprintln!`s the configured address *before* binding), so
/// the caller has to pick a concrete port up front.
fn reserve_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

#[tokio::test]
async fn the_real_binary_starts_and_serves_cwd_and_static_files() {
    let workspace = tempfile::tempdir().unwrap();
    let static_dir = tempfile::tempdir().unwrap();
    std::fs::write(static_dir.path().join("index.html"), "<!doctype html><title>smoke</title>").unwrap();

    let port = reserve_port();
    let mut child = tokio::process::Command::new(web_bin())
        .current_dir(workspace.path())
        .env("LITE_HARNESS_WEB_PORT", port.to_string())
        .env("LITE_HARNESS_WEB_STATIC_DIR", static_dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("failed to spawn lite-harness-web");

    let base = format!("http://127.0.0.1:{port}");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if reqwest::get(format!("{base}/api/cwd")).await.is_ok() {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "lite-harness-web never started listening");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let cwd_resp = reqwest::get(format!("{base}/api/cwd")).await.unwrap();
    assert!(cwd_resp.status().is_success());
    let canonical = workspace.path().canonicalize().unwrap();
    assert_eq!(cwd_resp.text().await.unwrap(), canonical.to_string_lossy());

    let static_resp = reqwest::get(format!("{base}/index.html")).await.unwrap();
    assert!(static_resp.status().is_success());
    assert!(static_resp.text().await.unwrap().contains("smoke"));

    // Graceful shutdown: SIGTERM should make the real process exit on its
    // own (not require a SIGKILL), per main.rs's signal handling.
    if let Some(pid) = child.id() {
        let _ = std::process::Command::new("kill").args(["-TERM", &pid.to_string()]).status();
    }
    let status = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
        .await
        .expect("lite-harness-web did not exit within 5s of SIGTERM")
        .unwrap();
    assert!(status.success(), "should exit 0 on graceful SIGTERM shutdown, got {status:?}");
}
