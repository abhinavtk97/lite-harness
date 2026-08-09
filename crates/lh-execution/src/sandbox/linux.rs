//! Landlock-based sandboxing (architecture §8) -- restricts the `bash`
//! child process's filesystem access to the workspace root + scratch dir
//! (read-write) and a minimal set of read-only system paths it needs to
//! actually run a shell (`/usr`, `/bin`, `/sbin`, `/lib`, `/lib64`,
//! `/etc`). `restrict_self()` scopes the *calling* process and everything
//! it execs afterwards -- irrevocably, for that process's whole remaining
//! lifetime -- which is exactly the child-process boundary `bash` already
//! has, and the daemon itself must never cross into (a sandboxed daemon
//! couldn't reach the model provider's API or its own session DB outside
//! the workspace).
//!
//! Best-effort throughout: on an unsupported/disabled kernel, every
//! Landlock call here is a no-op (the crate's default
//! `CompatLevel::BestEffort` degrades gracefully instead of erroring), so
//! `bash` still runs -- just unsandboxed, which `probe()` reports
//! honestly via `describe()` rather than assuming success.

use std::path::{Path, PathBuf};

use landlock::{
    path_beneath_rules, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr,
    RulesetCreatedAttr, RulesetStatus, ABI,
};

use crate::ExecutionPlaneCapabilities;

const ABI_LEVEL: ABI = ABI::V1;

/// System paths a POSIX shell + coreutils need read+execute access to just
/// to run at all. Only paths that actually exist on this host are added.
const SYSTEM_RO_PATHS: &[&str] = &["/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc"];

/// Restricts the *calling* process to read-only access under
/// `SYSTEM_RO_PATHS` and read-write access under `rw_paths`, denying
/// everything else. Returns whether the restriction actually took effect
/// (fully or partially) -- `false` on any error or an unsupported kernel,
/// never panics or propagates, since callers treat this as best-effort.
fn apply_ruleset(rw_paths: &[PathBuf]) -> bool {
    let ro_paths: Vec<&str> = SYSTEM_RO_PATHS
        .iter()
        .copied()
        .filter(|p| Path::new(p).exists())
        .collect();

    let result = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::from_all(ABI_LEVEL))
        .and_then(|r| r.create())
        .and_then(|r| r.add_rules(path_beneath_rules(&ro_paths, AccessFs::from_read(ABI_LEVEL))))
        .and_then(|r| r.add_rules(path_beneath_rules(rw_paths, AccessFs::from_all(ABI_LEVEL))))
        .and_then(|r| r.restrict_self());

    matches!(
        result.map(|s| s.ruleset),
        Ok(RulesetStatus::FullyEnforced) | Ok(RulesetStatus::PartiallyEnforced)
    )
}

pub(crate) async fn probe() -> ExecutionPlaneCapabilities {
    // `restrict_self()` scopes the calling process, so probing must run in
    // a throwaway child, never the daemon itself. That child never
    // actually execs anything: the pre_exec hook applies a harmless
    // ruleset (scoped to /tmp) and exits directly with a status the
    // parent can read -- no IPC needed.
    let mut cmd = tokio::process::Command::new("/bin/sh");
    cmd.arg("-c").arg("true");
    unsafe {
        cmd.pre_exec(|| {
            if apply_ruleset(&[PathBuf::from("/tmp")]) {
                std::process::exit(0)
            } else {
                std::process::exit(1)
            }
        });
    }

    let sandboxed = matches!(cmd.status().await, Ok(status) if status.success());
    ExecutionPlaneCapabilities {
        sandboxed,
        mechanism: if sandboxed {
            "landlock"
        } else {
            "none (path-boundary check only)"
        },
        // Landlock's TCP bind/connect restriction (ABI v4+) isn't wired up
        // yet -- an honest gap, not a silent one (architecture §8).
        network_restricted: false,
    }
}

pub(crate) fn build_command(
    command: &str,
    workspace_root: &Path,
    scratch_dir: &Path,
) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(workspace_root)
        .env_clear()
        .envs(super::scrubbed_env(scratch_dir));

    let rw_paths = vec![workspace_root.to_path_buf(), scratch_dir.to_path_buf()];
    unsafe {
        cmd.pre_exec(move || {
            // Best-effort: never block the actual command on a Landlock
            // failure (an unsupported/disabled kernel is expected and
            // fine -- `probe()` already reported that honestly).
            let _ = apply_ruleset(&rw_paths);
            Ok(())
        });
    }
    cmd
}
