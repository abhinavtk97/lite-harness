//! Automated end-to-end tests for the `lite-harness` CLI: spawns the real
//! compiled binary with piped stdio against a real `lite-harnessd` +
//! scripted mock model server, driving it exactly the way a human at a
//! terminal would -- exercising `main.rs`'s real branches, previously only
//! ever verified by hand once per phase, never captured as an automated,
//! CI-enforced test.
//!
//! Unlike `lh-daemon`'s equivalent suite, this one stays subprocess-based:
//! `lh-cli`'s `main()` has no `tokio::spawn` calls at all (a single
//! straight-line async flow), so it doesn't hit the async-task coverage-
//! flushing gap that made `lh-daemon`'s `main.rs` need an in-process
//! rewrite (confirmed empirically, not assumed).

use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Mutex;

use lh_protocol::default_socket_path;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn cli_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_lite-harness"))
}

/// `lite-harnessd` isn't a dependency of this crate (it's a sibling
/// binary crate) -- its path is derived the same way `lh_protocol::
/// daemon_binary_path()` does in production: next to this test's own
/// binary in the shared workspace target directory. Requires the whole
/// workspace to have been built (`cargo test --workspace`, which is how
/// this is always actually run), so `lite-harnessd` is guaranteed present.
fn daemon_bin() -> PathBuf {
    cli_bin().with_file_name(if cfg!(windows) { "lite-harnessd.exe" } else { "lite-harnessd" })
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
        let body = q.pop_front().unwrap_or_else(|| {
            serde_json::json!({
                "choices": [{"message": {"role": "assistant", "content": "done"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1}
            })
        });
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
        if let Some(pid) = Some(self.child.id()) {
            let _ = std::process::Command::new("kill").args(["-TERM", &pid.to_string()]).status();
        }
        let _ = self.child.wait();
    }
}

/// Starts the real daemon in `workspace` and waits for its socket to
/// accept connections, so the CLI subprocess (spawned separately, below)
/// connects to an already-running daemon instead of racing its own
/// auto-spawn-on-first-connect logic.
async fn start_daemon(workspace: &std::path::Path, home: &std::path::Path, env: &[(&str, &str)]) -> DaemonGuard {
    let canonical = workspace.canonicalize().unwrap();
    let sock_path = default_socket_path(&canonical);
    let mut cmd = std::process::Command::new(daemon_bin());
    cmd.current_dir(&canonical)
        .env("HOME", home)
        .env_remove("LITE_HARNESS_PROVIDERS_FILE")
        .env_remove("LITE_HARNESS_AGENTS_FILE")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let child = cmd.spawn().expect("failed to spawn lite-harnessd");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if std::os::unix::net::UnixStream::connect(&sock_path).is_ok() {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "daemon never started listening");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    DaemonGuard { child }
}

/// Runs the real `lite-harness` CLI binary against an already-running
/// daemon, writing `stdin_input` up front (for any permission prompts) and
/// collecting all of stdout/stderr after it exits.
fn run_cli(workspace: &std::path::Path, args: &[&str], stdin_input: &str) -> std::process::Output {
    let mut child = std::process::Command::new(cli_bin())
        .current_dir(workspace)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn lite-harness");
    child.stdin.take().unwrap().write_all(stdin_input.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

fn fake_agent_toml(kind_toml: &str, can_be_primary: bool, api_key_env: &str) -> String {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("lh-acp/tests/fixtures/fake_acp_agent.py");
    format!(
        "[[agent]]\nkind = {{ type = \"{kind_toml}\" }}\ncan_be_primary = {can_be_primary}\n\n\
         [agent.spawn]\ncommand = \"python3\"\nargs = [\"{}\"]\napi_key_env = \"{api_key_env}\"\n",
        fixture.to_string_lossy().replace('\\', "\\\\"),
    )
}

// --- tests ---

#[tokio::test]
async fn native_prompt_prints_the_response_and_ledger() {
    let server = scripted_mock_server(vec![text_response("hello from the mock model")]).await;
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

    let output = run_cli(workspace.path(), &["say hi"], "");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("you: say hi"), "got: {stdout}");
    assert!(stdout.contains("hello from the mock model"), "got: {stdout}");
    assert!(stdout.contains("turn complete: EndTurn"), "got: {stdout}");
    assert!(stdout.contains("[ledger]"), "got: {stdout}");
}

#[tokio::test]
async fn list_agents_with_none_configured() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let _daemon = start_daemon(workspace.path(), home.path(), &[]).await;

    let output = run_cli(workspace.path(), &["--list-agents"], "");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    assert!(stdout.contains("no delegated agents configured"), "got: {stdout}");
}

#[tokio::test]
async fn list_agents_with_one_configured() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    write_file(workspace.path(), "agents.toml", &fake_agent_toml("ClaudeCode", true, "LH_CLI_TEST_KEY_LIST"));
    let _daemon = start_daemon(
        workspace.path(),
        home.path(),
        &[("LITE_HARNESS_AGENTS_FILE", workspace.path().join("agents.toml").to_str().unwrap())],
    )
    .await;

    let output = run_cli(workspace.path(), &["--list-agents"], "");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    assert!(stdout.contains("ClaudeCode"), "got: {stdout}");
    assert!(stdout.contains("can be primary"), "got: {stdout}");
}

#[tokio::test]
async fn permission_prompt_allow_runs_the_tool() {
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

    let output = run_cli(workspace.path(), &["run echo"], "y\n");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "got: {stdout}");
    assert!(stdout.contains("[permission]"), "got: {stdout}");
    assert!(stdout.contains("allow?"), "got: {stdout}");
    assert!(stdout.contains("turn complete"), "got: {stdout}");
}

#[tokio::test]
async fn permission_prompt_deny_cancels_the_tool() {
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

    let output = run_cli(workspace.path(), &["run echo"], "N\n");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "got: {stdout}");
    assert!(stdout.contains("decision: Deny"), "got: {stdout}");
}

#[tokio::test]
async fn delegate_agent_flag_round_trips_through_the_fake_agent() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    write_file(workspace.path(), "agents.toml", &fake_agent_toml("ClaudeCode", false, "LH_CLI_TEST_KEY_DELEGATE"));
    let _daemon = start_daemon(
        workspace.path(),
        home.path(),
        &[
            ("LITE_HARNESS_AGENTS_FILE", workspace.path().join("agents.toml").to_str().unwrap()),
            ("LH_CLI_TEST_KEY_DELEGATE", "unused-by-the-fake-agent"),
        ],
    )
    .await;

    let output = run_cli(workspace.path(), &["--agent", "claude-code", "please run the diagnostic"], "");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stderr.contains("delegating to ClaudeCode"), "got: {stderr}");
    assert!(stdout.contains("delegation complete"), "got: {stdout}");
    assert!(stdout.contains("outcome: Success"), "got: {stdout}");
}

#[tokio::test]
async fn primary_flag_round_trips_through_the_fake_agent() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    write_file(workspace.path(), "agents.toml", &fake_agent_toml("ClaudeCode", true, "LH_CLI_TEST_KEY_PRIMARY"));
    let _daemon = start_daemon(
        workspace.path(),
        home.path(),
        &[
            ("LITE_HARNESS_AGENTS_FILE", workspace.path().join("agents.toml").to_str().unwrap()),
            ("LH_CLI_TEST_KEY_PRIMARY", "unused-by-the-fake-agent"),
        ],
    )
    .await;

    let output = run_cli(workspace.path(), &["--primary", "claude-code", "please run the diagnostic"], "");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stderr.contains("root session driven by ClaudeCode"), "got: {stderr}");
    // Specifically EndTurn (real success), not just the generic "turn
    // complete" wrapper -- that phrase also appears for a failed
    // delegated turn (stop_reason becomes "Failed: <message>"), which
    // would make this assertion pass even if the fake-agent spawn failed.
    assert!(stdout.contains("turn complete: EndTurn"), "got: {stdout}");
}

#[test]
fn no_arguments_prints_usage_and_exits_nonzero() {
    let output = std::process::Command::new(cli_bin()).output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("usage: lite-harness"), "got: {stderr}");
}

#[test]
fn unknown_agent_name_is_a_clear_usage_error() {
    let output = std::process::Command::new(cli_bin()).args(["--agent", "nonexistent-agent", "hi"]).output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown agent"), "got: {stderr}");
}

#[test]
fn missing_agent_value_is_a_clear_usage_error() {
    let output = std::process::Command::new(cli_bin()).args(["--agent"]).output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--agent requires a value"), "got: {stderr}");
}
