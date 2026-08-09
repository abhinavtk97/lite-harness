//! Seatbelt (`sandbox-exec`) based sandboxing for macOS (architecture §8).
//! No well-maintained Rust crate wraps Seatbelt, so this shells out to
//! `/usr/bin/sandbox-exec` with a generated `.sb` profile, the same
//! approach the architecture doc calls for.
//!
//! **Not runtime-verified.** This project has only ever been built and run
//! on Linux (including in the dev container this phase was implemented
//! in) -- this file is written to match Seatbelt's documented profile
//! syntax and `sandbox-exec`'s CLI, but has not been exercised against a
//! real macOS host. Treat it as a best-effort starting point, not a
//! battle-tested sandbox.

use std::path::Path;

use crate::ExecutionPlaneCapabilities;

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

fn profile(workspace_root: &Path, scratch_dir: &Path) -> String {
    format!(
        r#"(version 1)
(deny default)
(allow process-exec)
(allow process-fork)
(allow signal (target self))
(allow sysctl-read)
(allow file-read*)
(allow file-write* (subpath {ws}))
(allow file-write* (subpath {scratch}))
"#,
        ws = quote_sb_path(workspace_root),
        scratch = quote_sb_path(scratch_dir),
    )
}

/// Seatbelt profiles take Scheme-style string literals; escape backslashes
/// and double quotes so a path can't break out of the literal.
fn quote_sb_path(p: &Path) -> String {
    let escaped = p.display().to_string().replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

pub(crate) async fn probe() -> ExecutionPlaneCapabilities {
    let sandboxed = if Path::new(SANDBOX_EXEC).exists() {
        let tmp_profile = std::env::temp_dir().join(format!("lite-harness-probe-{}.sb", std::process::id()));
        let minimal = "(version 1)\n(deny default)\n(allow process-exec)\n(allow file-read*)\n";
        let write_ok = std::fs::write(&tmp_profile, minimal).is_ok();
        let status = if write_ok {
            tokio::process::Command::new(SANDBOX_EXEC)
                .arg("-f")
                .arg(&tmp_profile)
                .arg("/bin/sh")
                .arg("-c")
                .arg("true")
                .status()
                .await
        } else {
            Err(std::io::Error::other("could not write probe profile"))
        };
        let _ = std::fs::remove_file(&tmp_profile);
        matches!(status, Ok(s) if s.success())
    } else {
        false
    };

    ExecutionPlaneCapabilities {
        sandboxed,
        mechanism: if sandboxed {
            "sandbox-exec (unverified on this build)"
        } else {
            "none (path-boundary check only)"
        },
        network_restricted: false,
    }
}

pub(crate) fn build_command(
    command: &str,
    workspace_root: &Path,
    scratch_dir: &Path,
) -> tokio::process::Command {
    if !Path::new(SANDBOX_EXEC).exists() {
        // Best-effort fallback: no sandbox-exec on this system.
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg(command)
            .current_dir(workspace_root)
            .env_clear()
            .envs(super::scrubbed_env(scratch_dir));
        return cmd;
    }

    let profile_path = scratch_dir.join(format!(".sandbox-{}.sb", uuid_like()));
    let _ = std::fs::write(&profile_path, profile(workspace_root, scratch_dir));

    let mut cmd = tokio::process::Command::new(SANDBOX_EXEC);
    cmd.arg("-f")
        .arg(&profile_path)
        .arg("/bin/sh")
        .arg("-c")
        .arg(command)
        .current_dir(workspace_root)
        .env_clear()
        .envs(super::scrubbed_env(scratch_dir));
    cmd
}

/// A short, dependency-free unique-ish suffix for the per-call profile
/// file -- doesn't need to be cryptographically unique, just distinct
/// enough to avoid concurrent `bash` calls colliding.
fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{nanos:x}")
}
