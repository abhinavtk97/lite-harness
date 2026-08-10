//! Built-in tool metadata: specs shown to the model, and the mapping from
//! a raw tool call to the `PermissionAction`/`RiskTier` the permission
//! engine gates it on. Actual execution -- including sandboxing -- lives
//! in `lh-execution`'s `ExecutionPlane` (architecture §8); this module
//! only describes the tools, it doesn't run them.

use std::path::PathBuf;

use lh_event::{PermissionAction, RiskTier};
use lh_model_provider::ToolSpec;

pub fn builtin_tool_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "read_file".to_string(),
            description: "Read a UTF-8 text file, path relative to the workspace root."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
        },
        ToolSpec {
            name: "write_file".to_string(),
            description: "Write (overwrite) a UTF-8 text file, path relative to the workspace \
                root, creating parent directories as needed."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
        },
        ToolSpec {
            name: "bash".to_string(),
            description: "Execute a shell command in the workspace root. UNSANDBOXED in this \
                build -- real sandboxing lands in a later phase."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "command": { "type": "string" } },
                "required": ["command"]
            }),
        },
        ToolSpec {
            name: "spawn_subagent".to_string(),
            description: "Delegate a focused sub-task to a fresh native subagent with its own \
                conversation (it does not see this conversation's history, only the instructions \
                given here). Returns the subagent's final answer as text. Optionally restrict \
                which tools it may use."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "role": { "type": "string", "description": "short label for what this subagent is for" },
                    "instructions": { "type": "string", "description": "the task, self-contained" },
                    "tool_allowlist": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "optional subset of tool names the subagent may use"
                    },
                    "max_turns": { "type": "integer", "description": "optional cap on model<->tool round-trips" }
                },
                "required": ["role", "instructions"]
            }),
        },
    ]
}

pub fn permission_action_for(tool_name: &str, input: &serde_json::Value) -> PermissionAction {
    match tool_name {
        "read_file" => PermissionAction::FileRead {
            path: PathBuf::from(str_arg(input, "path").unwrap_or_default()),
        },
        "write_file" => PermissionAction::FileWrite {
            path: PathBuf::from(str_arg(input, "path").unwrap_or_default()),
            diff_summary: None,
        },
        "bash" => PermissionAction::Exec {
            command: str_arg(input, "command").unwrap_or_default().to_string(),
            args: vec![],
            cwd: PathBuf::from("."),
        },
        "spawn_subagent" => PermissionAction::SpawnSubagent {
            role: str_arg(input, "role").unwrap_or_default().to_string(),
            task_summary: str_arg(input, "instructions").unwrap_or_default().to_string(),
        },
        other => PermissionAction::Exec {
            command: other.to_string(),
            args: vec![],
            cwd: PathBuf::from("."),
        },
    }
}

pub fn risk_tier_for(tool_name: &str) -> RiskTier {
    match tool_name {
        "read_file" => RiskTier::Read,
        "write_file" => RiskTier::Write,
        "bash" => RiskTier::Execute,
        "spawn_subagent" => RiskTier::Execute,
        _ => RiskTier::Execute,
    }
}

fn str_arg<'a>(input: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    input.get(key)?.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_action_reads_the_expected_argument_per_tool() {
        let read = permission_action_for("read_file", &serde_json::json!({"path": "a.txt"}));
        assert!(matches!(read, PermissionAction::FileRead { path } if path == PathBuf::from("a.txt")));

        let exec = permission_action_for("bash", &serde_json::json!({"command": "ls"}));
        assert!(matches!(exec, PermissionAction::Exec { command, .. } if command == "ls"));
    }

    #[test]
    fn risk_tiers_match_the_architecture_defaults() {
        assert_eq!(risk_tier_for("read_file"), RiskTier::Read);
        assert_eq!(risk_tier_for("write_file"), RiskTier::Write);
        assert_eq!(risk_tier_for("bash"), RiskTier::Execute);
        assert_eq!(risk_tier_for("spawn_subagent"), RiskTier::Execute);
    }

    #[test]
    fn spawn_subagent_gets_its_own_permission_action() {
        let action = permission_action_for(
            "spawn_subagent",
            &serde_json::json!({"role": "researcher", "instructions": "find the bug"}),
        );
        assert!(matches!(
            action,
            PermissionAction::SpawnSubagent { role, task_summary }
                if role == "researcher" && task_summary == "find the bug"
        ));
    }
}
