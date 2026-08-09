//! Fallback for any OS that isn't Linux or macOS (architecture §8 marks
//! Windows explicitly out of scope for v1; this also covers other Unix
//! variants nobody has asked for yet). No kernel sandbox is attempted --
//! `bash` still runs, pinned to the workspace root with a scrubbed
//! environment, but `describe()` reports `sandboxed: false` honestly
//! rather than pretending.

use std::path::Path;

use crate::ExecutionPlaneCapabilities;

pub(crate) async fn probe() -> ExecutionPlaneCapabilities {
    ExecutionPlaneCapabilities {
        sandboxed: false,
        mechanism: "none (unsupported OS, path-boundary check only)",
        network_restricted: false,
    }
}

pub(crate) fn build_command(
    command: &str,
    workspace_root: &Path,
    scratch_dir: &Path,
) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(workspace_root)
        .env_clear()
        .envs(super::scrubbed_env(scratch_dir));
    cmd
}
