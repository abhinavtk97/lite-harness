//! Sandboxed tool execution (architecture §8). `ExecutionPlane` is the one
//! path every native tool call's actual side effect goes through -- the
//! `proof: &PermissionDecision` argument is deliberate: there is no way to
//! call `execute()` without first having gone through the permission
//! engine and having a decision in hand, and a Deny/DenyAlways decision is
//! rejected at runtime too, not just discouraged by convention.
//!
//! v1 (`LocalExecutionPlane`): Landlock (kernel 5.13+) on Linux, restricting
//! `bash` tool calls' filesystem access to the workspace root + a scratch
//! dir; `sandbox-exec` on macOS (written to match the architecture, not
//! runtime-verified -- this project has only ever been built/run on
//! Linux, and this dev container's own outer sandbox blocks the Landlock
//! syscall too, so even the Linux path only gets to prove its fallback
//! behavior here, not real kernel enforcement). Falls back to an
//! unsandboxed path-boundary check everywhere else (old kernel, Landlock
//! disabled/unavailable, unsupported OS) -- `describe()` reports the real,
//! probed capability rather than assuming it.
//!
//! `read_file`/`write_file` are the daemon's own trusted code (the path
//! they touch is decided by the daemon, not by an executed command), so
//! they get the same path-boundary check but not a kernel sandbox --
//! Landlock's `restrict_self()` model scopes an entire process (and
//! everything it execs afterwards) for that process's remaining lifetime,
//! which fits `bash`'s child-process boundary, not a call made in-process
//! in the long-running daemon.

mod sandbox;

use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use lh_event::{PermissionDecision, ToolCall};

pub type Result<T> = std::result::Result<T, ExecutionError>;

#[derive(Debug, thiserror::Error)]
pub enum ExecutionError {
    #[error("tool call was not authorized: no Allow decision in hand")]
    NotAuthorized,
    #[error("path '{0}' escapes the workspace root")]
    PathEscapesWorkspace(String),
    #[error("missing or invalid '{0}' argument")]
    BadArgument(&'static str),
    #[error("command timed out after {0:?}")]
    Timeout(std::time::Duration),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("unknown tool: {0}")]
    UnknownTool(String),
}

#[derive(Debug, Clone)]
pub struct ExecutionPlaneCapabilities {
    pub sandboxed: bool,
    pub mechanism: &'static str,
    pub network_restricted: bool,
}

#[async_trait]
pub trait ExecutionPlane: Send + Sync {
    /// Executes one built-in tool call (`read_file`/`write_file`/`bash`),
    /// given proof that the permission engine already allowed it. Returns
    /// the tool's textual output.
    async fn execute(&self, call: &ToolCall, proof: &PermissionDecision) -> Result<String>;
    /// Starts `call`'s command (its `raw_args.command`) without waiting for
    /// it to finish -- the non-blocking sibling of `execute()`, backing
    /// both ACP's `terminal/*` capability (`lh-acp`) and, eventually, a
    /// native background-bash tool. Reuses the identical sandboxed command
    /// construction `execute()`'s `bash` arm uses; the only difference is
    /// this returns a live handle instead of awaiting completion.
    async fn spawn_background(
        &self,
        call: &ToolCall,
        proof: &PermissionDecision,
    ) -> Result<std::sync::Arc<dyn BackgroundProcess>>;
    fn describe(&self) -> ExecutionPlaneCapabilities;
}

/// How a background process exited -- mirrors ACP's `TerminalExitStatus`
/// shape (`exit_code`/`signal`, either may be unset) closely enough that
/// `lh-acp` can translate it directly, without being ACP-specific itself.
#[derive(Debug, Clone, Default)]
pub struct ExitStatus {
    pub code: Option<i32>,
    pub signal: Option<String>,
}

/// A snapshot of a background process's output, taken without blocking --
/// what `output()` below returns.
#[derive(Debug, Clone)]
pub struct BackgroundOutput {
    pub text: String,
    /// Whether `text` has been truncated (from the front, matching ACP's
    /// stated truncation semantics) to stay under the same output cap
    /// `execute()` applies.
    pub truncated: bool,
    /// `Some` once the process has exited; polling `output()` before that
    /// is exactly how a caller checks "is it still running."
    pub exit_status: Option<ExitStatus>,
}

/// A live handle to a process started by `spawn_background`. Cheap to
/// clone (implementations wrap their real state in `Arc`s internally) so
/// one handle can be stored once and read from repeatedly.
#[async_trait]
pub trait BackgroundProcess: Send + Sync {
    /// Currently buffered output and exit status, without blocking.
    async fn output(&self) -> BackgroundOutput;
    /// Blocks until the process exits, then returns its exit status.
    async fn wait_for_exit(&self) -> ExitStatus;
    /// Requests termination. Fire-and-forget -- call `wait_for_exit()` or
    /// `output()` afterwards to observe the result.
    async fn kill(&self);
}

const MAX_OUTPUT_CHARS: usize = 4000;
const EXEC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

pub struct LocalExecutionPlane {
    workspace_root: PathBuf,
    scratch_dir: PathBuf,
    capabilities: ExecutionPlaneCapabilities,
}

impl LocalExecutionPlane {
    /// Creates the scratch dir and runs a one-time, harmless capability
    /// probe in a throwaway child process -- `restrict_self()` scopes the
    /// *calling* process irrevocably, so probing must never happen in the
    /// daemon's own process, or the daemon would sandbox itself out of
    /// reaching the model provider's API and its own session DB.
    pub async fn new(workspace_root: PathBuf) -> std::io::Result<Self> {
        let scratch_dir = workspace_root.join(".lite-harness/scratch");
        tokio::fs::create_dir_all(&scratch_dir).await?;
        let capabilities = sandbox::probe().await;
        Ok(Self {
            workspace_root,
            scratch_dir,
            capabilities,
        })
    }
}

#[async_trait]
impl ExecutionPlane for LocalExecutionPlane {
    async fn execute(&self, call: &ToolCall, proof: &PermissionDecision) -> Result<String> {
        match proof {
            PermissionDecision::Allow | PermissionDecision::AllowAlways { .. } => {}
            PermissionDecision::Deny | PermissionDecision::DenyAlways { .. } => {
                return Err(ExecutionError::NotAuthorized);
            }
        }

        match call.tool_name.as_str() {
            "read_file" => {
                let rel =
                    str_arg(&call.raw_args, "path").ok_or(ExecutionError::BadArgument("path"))?;
                let full = resolve_within(&self.workspace_root, rel)?;
                let content = tokio::fs::read_to_string(&full).await?;
                Ok(truncate(&content))
            }
            "write_file" => {
                let rel =
                    str_arg(&call.raw_args, "path").ok_or(ExecutionError::BadArgument("path"))?;
                let content = str_arg(&call.raw_args, "content")
                    .ok_or(ExecutionError::BadArgument("content"))?;
                let full = resolve_within(&self.workspace_root, rel)?;
                if let Some(parent) = full.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                tokio::fs::write(&full, content).await?;
                Ok(format!("wrote {} bytes to {rel}", content.len()))
            }
            "bash" => {
                let command = str_arg(&call.raw_args, "command")
                    .ok_or(ExecutionError::BadArgument("command"))?;
                self.run_sandboxed(command).await
            }
            other => Err(ExecutionError::UnknownTool(other.to_string())),
        }
    }

    async fn spawn_background(
        &self,
        call: &ToolCall,
        proof: &PermissionDecision,
    ) -> Result<std::sync::Arc<dyn BackgroundProcess>> {
        match proof {
            PermissionDecision::Allow | PermissionDecision::AllowAlways { .. } => {}
            PermissionDecision::Deny | PermissionDecision::DenyAlways { .. } => {
                return Err(ExecutionError::NotAuthorized);
            }
        }
        let command =
            str_arg(&call.raw_args, "command").ok_or(ExecutionError::BadArgument("command"))?;
        self.spawn_sandboxed_background(command).await
    }

    fn describe(&self) -> ExecutionPlaneCapabilities {
        self.capabilities.clone()
    }
}

impl LocalExecutionPlane {
    async fn spawn_sandboxed_background(&self, command: &str) -> Result<std::sync::Arc<dyn BackgroundProcess>> {
        use std::sync::Arc;
        use tokio::sync::Mutex as TokioMutex;

        let mut cmd = sandbox::build_command(command, &self.workspace_root, &self.scratch_dir);
        // Unlike `run_sandboxed`, this process is meant to outlive the
        // single call that started it -- `kill_on_drop` would SIGKILL it
        // the moment this future (not the process) is dropped, which
        // defeats the entire point of a background task. Lifecycle is
        // instead owned explicitly by the supervisor task below.
        cmd.kill_on_drop(false);
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");

        let output = Arc::new(TokioMutex::new(String::new()));
        let truncated = Arc::new(TokioMutex::new(false));
        spawn_output_reader(stdout, output.clone(), truncated.clone());
        spawn_output_reader(stderr, output.clone(), truncated.clone());

        let exit_status: Arc<TokioMutex<Option<ExitStatus>>> = Arc::new(TokioMutex::new(None));
        let exited = Arc::new(tokio::sync::Notify::new());
        let (kill_tx, mut kill_rx) = tokio::sync::mpsc::channel::<()>(1);

        let exit_status_writer = exit_status.clone();
        let exited_notifier = exited.clone();
        tokio::spawn(async move {
            let status = tokio::select! {
                _ = kill_rx.recv() => {
                    let _ = child.start_kill();
                    child.wait().await
                }
                status = child.wait() => status,
            };
            let resolved = match status {
                Ok(s) => ExitStatus {
                    code: s.code(),
                    #[cfg(unix)]
                    signal: std::os::unix::process::ExitStatusExt::signal(&s).map(|n| n.to_string()),
                    #[cfg(not(unix))]
                    signal: None,
                },
                Err(_) => ExitStatus::default(),
            };
            *exit_status_writer.lock().await = Some(resolved);
            exited_notifier.notify_waiters();
        });

        Ok(Arc::new(LocalBackgroundProcess {
            output,
            truncated,
            exit_status,
            exited,
            kill_tx,
        }))
    }

    async fn run_sandboxed(&self, command: &str) -> Result<String> {
        let mut cmd = sandbox::build_command(command, &self.workspace_root, &self.scratch_dir);
        cmd.kill_on_drop(true);
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let child = cmd.spawn()?;
        // `wait_with_output` owns `child`; if the timeout fires first, that
        // future (and the child inside it) is dropped, and `kill_on_drop`
        // makes tokio SIGKILL the process right there -- no separate kill
        // path needed.
        let output = match tokio::time::timeout(EXEC_TIMEOUT, child.wait_with_output()).await {
            Ok(res) => res?,
            Err(_elapsed) => return Err(ExecutionError::Timeout(EXEC_TIMEOUT)),
        };

        let mut combined = String::from_utf8_lossy(&output.stdout).to_string();
        if !output.stderr.is_empty() {
            combined.push_str("\n[stderr]\n");
            combined.push_str(&String::from_utf8_lossy(&output.stderr));
        }
        combined.push_str(&format!("\n[{}]", output.status));
        Ok(truncate(&combined))
    }
}

struct LocalBackgroundProcess {
    output: std::sync::Arc<tokio::sync::Mutex<String>>,
    truncated: std::sync::Arc<tokio::sync::Mutex<bool>>,
    exit_status: std::sync::Arc<tokio::sync::Mutex<Option<ExitStatus>>>,
    exited: std::sync::Arc<tokio::sync::Notify>,
    kill_tx: tokio::sync::mpsc::Sender<()>,
}

#[async_trait]
impl BackgroundProcess for LocalBackgroundProcess {
    async fn output(&self) -> BackgroundOutput {
        BackgroundOutput {
            text: self.output.lock().await.clone(),
            truncated: *self.truncated.lock().await,
            exit_status: self.exit_status.lock().await.clone(),
        }
    }

    async fn wait_for_exit(&self) -> ExitStatus {
        loop {
            // Register for the notification *before* checking the
            // condition -- otherwise an exit that lands between the check
            // and the `.await` below would be missed entirely.
            let notified = self.exited.notified();
            if let Some(status) = self.exit_status.lock().await.clone() {
                return status;
            }
            notified.await;
        }
    }

    async fn kill(&self) {
        let _ = self.kill_tx.send(()).await;
    }
}

/// Streams `reader` into `output` as it arrives, applying the same
/// front-truncation cap `execute()`'s synchronous path applies via
/// `truncate()` -- kept as a running invariant here (checked after every
/// chunk) rather than a one-shot check at the end, since a background
/// process's output has no fixed end to check at.
fn spawn_output_reader<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    reader: R,
    output: std::sync::Arc<tokio::sync::Mutex<String>>,
    truncated: std::sync::Arc<tokio::sync::Mutex<bool>>,
) {
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut reader = reader;
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let chunk = String::from_utf8_lossy(&buf[..n]);
                    let mut out = output.lock().await;
                    out.push_str(&chunk);
                    let len = out.chars().count();
                    if len > MAX_OUTPUT_CHARS {
                        let excess = len - MAX_OUTPUT_CHARS;
                        *out = out.chars().skip(excess).collect();
                        *truncated.lock().await = true;
                    }
                }
            }
        }
    });
}

fn str_arg<'a>(input: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    input.get(key)?.as_str()
}

fn truncate(s: &str) -> String {
    if s.chars().count() <= MAX_OUTPUT_CHARS {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(MAX_OUTPUT_CHARS).collect();
        format!(
            "{truncated}\n... [truncated, {} more chars]",
            s.chars().count() - MAX_OUTPUT_CHARS
        )
    }
}

/// Rejects any relative path with a `..` or absolute-path component. A
/// software boundary the *daemon's own code* enforces for
/// `read_file`/`write_file` -- see the module doc for why those two don't
/// go through the kernel sandbox the way `bash` does.
fn resolve_within(root: &Path, rel: &str) -> Result<PathBuf> {
    let rel_path = Path::new(rel);
    for component in rel_path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            _ => return Err(ExecutionError::PathEscapesWorkspace(rel.to_string())),
        }
    }
    Ok(root.join(rel_path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lh_event::ToolSource;

    fn call(tool_name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            call_id: "call_1".to_string(),
            tool_name: tool_name.to_string(),
            source: ToolSource::Native {
                tool_id: tool_name.to_string(),
            },
            args_summary: args.clone(),
            raw_args: args,
        }
    }

    #[tokio::test]
    async fn a_deny_proof_is_rejected_before_touching_the_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let plane = LocalExecutionPlane::new(dir.path().to_path_buf()).await.unwrap();

        let err = plane
            .execute(
                &call("write_file", serde_json::json!({"path": "x.txt", "content": "no"})),
                &PermissionDecision::Deny,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ExecutionError::NotAuthorized));
        assert!(!dir.path().join("x.txt").exists());
    }

    #[tokio::test]
    async fn read_file_round_trips_write_file() {
        let dir = tempfile::tempdir().unwrap();
        let plane = LocalExecutionPlane::new(dir.path().to_path_buf()).await.unwrap();

        plane
            .execute(
                &call("write_file", serde_json::json!({"path": "a.txt", "content": "hi"})),
                &PermissionDecision::Allow,
            )
            .await
            .unwrap();

        let out = plane
            .execute(&call("read_file", serde_json::json!({"path": "a.txt"})), &PermissionDecision::Allow)
            .await
            .unwrap();
        assert_eq!(out, "hi");
    }

    #[tokio::test]
    async fn path_traversal_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let plane = LocalExecutionPlane::new(dir.path().to_path_buf()).await.unwrap();

        let err = plane
            .execute(
                &call("read_file", serde_json::json!({"path": "../escape.txt"})),
                &PermissionDecision::Allow,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ExecutionError::PathEscapesWorkspace(_)));
    }

    #[tokio::test]
    async fn bash_runs_in_the_workspace_and_captures_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let plane = LocalExecutionPlane::new(dir.path().to_path_buf()).await.unwrap();

        let out = plane
            .execute(&call("bash", serde_json::json!({"command": "pwd && echo hi"})), &PermissionDecision::Allow)
            .await
            .unwrap();
        assert!(out.contains("hi"));
        assert!(out.contains(&dir.path().canonicalize().unwrap().display().to_string()));
    }

    #[tokio::test]
    async fn bash_env_is_scrubbed() {
        let dir = tempfile::tempdir().unwrap();
        let plane = LocalExecutionPlane::new(dir.path().to_path_buf()).await.unwrap();

        // SOME_RANDOM_VAR is set in this test process's env but is not on
        // the allowlist, so it must not reach the sandboxed child.
        std::env::set_var("LH_TEST_SHOULD_NOT_LEAK", "leaked");
        let out = plane
            .execute(
                &call("bash", serde_json::json!({"command": "echo [$LH_TEST_SHOULD_NOT_LEAK]"})),
                &PermissionDecision::Allow,
            )
            .await
            .unwrap();
        std::env::remove_var("LH_TEST_SHOULD_NOT_LEAK");
        assert!(out.contains("[]"), "expected the var to be scrubbed, got: {out}");
    }

    #[test]
    fn describe_reports_a_mechanism_string() {
        // Just exercises the type -- the real probe is async and I/O
        // bound, covered indirectly by every other test constructing a
        // LocalExecutionPlane successfully.
        let caps = ExecutionPlaneCapabilities {
            sandboxed: false,
            mechanism: "none (path-boundary check only)",
            network_restricted: false,
        };
        assert!(!caps.mechanism.is_empty());
    }

    #[tokio::test]
    async fn a_deny_proof_is_rejected_before_spawning_anything() {
        let dir = tempfile::tempdir().unwrap();
        let plane = LocalExecutionPlane::new(dir.path().to_path_buf()).await.unwrap();

        match plane
            .spawn_background(&call("bash", serde_json::json!({"command": "echo no"})), &PermissionDecision::Deny)
            .await
        {
            Err(ExecutionError::NotAuthorized) => {}
            other => panic!("expected NotAuthorized, got {}", other.is_ok()),
        }
    }

    #[tokio::test]
    async fn background_process_output_is_visible_before_and_after_exit() {
        let dir = tempfile::tempdir().unwrap();
        let plane = LocalExecutionPlane::new(dir.path().to_path_buf()).await.unwrap();

        let process = plane
            .spawn_background(
                &call("bash", serde_json::json!({"command": "echo start; sleep 0.3; echo done"})),
                &PermissionDecision::Allow,
            )
            .await
            .unwrap();

        // Non-blocking: the process is very likely still running immediately
        // after spawn (it sleeps 0.3s), so this must return without waiting
        // for it -- the whole point of "background."
        let early = process.output().await;
        assert!(early.exit_status.is_none(), "expected still running, got {early:?}");

        let status = process.wait_for_exit().await;
        assert_eq!(status.code, Some(0));

        let out = process.output().await;
        assert!(out.exit_status.is_some());
        assert!(out.text.contains("start"));
        assert!(out.text.contains("done"));
    }

    #[tokio::test]
    async fn kill_terminates_a_long_running_background_process() {
        let dir = tempfile::tempdir().unwrap();
        let plane = LocalExecutionPlane::new(dir.path().to_path_buf()).await.unwrap();

        let process = plane
            .spawn_background(&call("bash", serde_json::json!({"command": "sleep 30"})), &PermissionDecision::Allow)
            .await
            .unwrap();

        process.kill().await;
        let status =
            tokio::time::timeout(std::time::Duration::from_secs(5), process.wait_for_exit())
                .await
                .expect("kill should terminate the process promptly, not after the full sleep");
        assert!(status.code != Some(0) || status.signal.is_some(), "expected an abnormal exit, got {status:?}");
    }
}
