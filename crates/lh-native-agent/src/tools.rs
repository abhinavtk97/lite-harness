//! Built-in tool metadata: specs shown to the model, and the mapping from
//! a raw tool call to the `PermissionAction`/`RiskTier` the permission
//! engine gates it on. Actual execution -- including sandboxing -- lives
//! in `lh-execution`'s `ExecutionPlane` (architecture §8); this module
//! only describes the tools, it doesn't run them.

use std::path::PathBuf;

use lh_event::{PermissionAction, PlanStep, PlanStepStatus, RiskTier};
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
            name: "bash_background".to_string(),
            description: "Start a shell command running in the background and return a bash_id \
                immediately, without waiting for it to finish. Use bash_output to check on it \
                later (safe to call repeatedly while it's still running) and bash_kill to stop it."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "command": { "type": "string" } },
                "required": ["command"]
            }),
        },
        ToolSpec {
            name: "bash_output".to_string(),
            description: "Check the output and exit status of a command started with \
                bash_background, given its bash_id."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "bash_id": { "type": "string" } },
                "required": ["bash_id"]
            }),
        },
        ToolSpec {
            name: "bash_kill".to_string(),
            description: "Stop a command started with bash_background, given its bash_id."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "bash_id": { "type": "string" } },
                "required": ["bash_id"]
            }),
        },
        ToolSpec {
            name: "bash_wait".to_string(),
            description: "Block until a command started with bash_background finishes, then \
                return its final output and exit status, given its bash_id. Use this instead of \
                polling bash_output in a loop when you actually need to wait for completion \
                before continuing."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "bash_id": { "type": "string" } },
                "required": ["bash_id"]
            }),
        },
        ToolSpec {
            name: "plan_update".to_string(),
            description: "Report or replace your current step-by-step plan for this task, so \
                the user can see progress. Each call replaces the entire plan with the full, \
                current list of steps -- call it again whenever a step's status changes."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "steps": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "description": { "type": "string" },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"]
                                }
                            },
                            "required": ["description", "status"]
                        }
                    }
                },
                "required": ["steps"]
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

/// `bash_output`/`bash_wait`/`bash_kill`/`plan_update` deliberately have no
/// arms here -- unlike every other tool, `NativeAgentLoop::handle_one_tool_call`
/// intercepts them *before* this function is ever called, skipping a fresh
/// live permission ask entirely. For the three `bash_*` ones this mirrors
/// the identical design decision already made for ACP's `terminal/output`/
/// `terminal/wait_for_exit`/`terminal/kill` in `lh-acp`: reading, blocking
/// on, or killing an already-approved background process -- started by a
/// permission-gated `bash_background` call -- creates no new risk to ask
/// about again. `plan_update` is simpler still: it never touches the
/// system at all (no file, no process, no network), just appends a
/// `PlanUpdated` bookkeeping event, so it was never risk-bearing in the
/// first place.
pub fn permission_action_for(tool_name: &str, input: &serde_json::Value) -> PermissionAction {
    match tool_name {
        "read_file" => PermissionAction::FileRead {
            path: PathBuf::from(str_arg(input, "path").unwrap_or_default()),
        },
        "write_file" => PermissionAction::FileWrite {
            path: PathBuf::from(str_arg(input, "path").unwrap_or_default()),
            diff_summary: None,
        },
        "bash" | "bash_background" => PermissionAction::Exec {
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
        "bash" | "bash_background" => RiskTier::Execute,
        "spawn_subagent" => RiskTier::Execute,
        _ => RiskTier::Execute,
    }
}

fn str_arg<'a>(input: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    input.get(key)?.as_str()
}

/// Parses `plan_update`'s `steps` argument into `PlanStep`s. `None` means
/// the input was malformed (missing/wrong-shaped `steps`, or an
/// unrecognized `status` string) -- the caller turns that into a `Failed`
/// tool result rather than silently emitting a partial or empty plan.
pub fn parse_plan_steps(input: &serde_json::Value) -> Option<Vec<PlanStep>> {
    let steps = input.get("steps")?.as_array()?;
    steps
        .iter()
        .map(|s| {
            let description = s.get("description")?.as_str()?.to_string();
            let status = match s.get("status")?.as_str()? {
                "pending" => PlanStepStatus::Pending,
                "in_progress" => PlanStepStatus::InProgress,
                "completed" => PlanStepStatus::Completed,
                _ => return None,
            };
            Some(PlanStep { description, status })
        })
        .collect()
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
        assert_eq!(risk_tier_for("bash_background"), RiskTier::Execute);
        assert_eq!(risk_tier_for("spawn_subagent"), RiskTier::Execute);
    }

    #[test]
    fn bash_background_gets_the_same_exec_action_shape_as_bash() {
        let action = permission_action_for("bash_background", &serde_json::json!({"command": "sleep 5"}));
        assert!(matches!(action, PermissionAction::Exec { command, .. } if command == "sleep 5"));
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

    #[test]
    fn parse_plan_steps_reads_every_status_variant() {
        let steps = parse_plan_steps(&serde_json::json!({"steps": [
            {"description": "a", "status": "pending"},
            {"description": "b", "status": "in_progress"},
            {"description": "c", "status": "completed"},
        ]}))
        .unwrap();
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0].status, PlanStepStatus::Pending);
        assert_eq!(steps[1].status, PlanStepStatus::InProgress);
        assert_eq!(steps[2].status, PlanStepStatus::Completed);
    }

    #[test]
    fn parse_plan_steps_rejects_an_unrecognized_status() {
        assert!(parse_plan_steps(&serde_json::json!({"steps": [{"description": "a", "status": "done"}]})).is_none());
    }

    #[test]
    fn parse_plan_steps_rejects_a_missing_steps_field() {
        assert!(parse_plan_steps(&serde_json::json!({})).is_none());
    }
}
