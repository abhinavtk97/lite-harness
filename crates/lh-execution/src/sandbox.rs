//! OS-specific sandboxing backends for `bash` tool execution. Each backend
//! exposes the same two functions: `probe()` (a one-time, out-of-process
//! capability check) and `build_command()` (constructs the actual
//! `tokio::process::Command` to run, sandboxed by whatever mechanism this
//! OS supports -- falling back to an unsandboxed, env-scrubbed command
//! pinned to the workspace root if it supports none).

use std::path::Path;

use crate::ExecutionPlaneCapabilities;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as backend;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as backend;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod unsupported;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
use unsupported as backend;

pub(crate) async fn probe() -> ExecutionPlaneCapabilities {
    backend::probe().await
}

pub(crate) fn build_command(
    command: &str,
    workspace_root: &Path,
    scratch_dir: &Path,
) -> tokio::process::Command {
    backend::build_command(command, workspace_root, scratch_dir)
}

/// Shared by every backend: `bash`'s child process only ever sees an
/// allowlisted environment (plus `TMPDIR` pointed at the scratch dir),
/// never the daemon's full environment (which may carry provider API
/// keys, etc.).
pub(crate) fn scrubbed_env(scratch_dir: &Path) -> Vec<(String, String)> {
    let allowlist = ["PATH", "HOME", "LANG", "LC_ALL", "TERM"];
    let mut env: Vec<(String, String)> = allowlist
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect();
    env.push(("TMPDIR".to_string(), scratch_dir.display().to_string()));
    env
}
