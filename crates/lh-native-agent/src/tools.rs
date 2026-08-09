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
    }
}
