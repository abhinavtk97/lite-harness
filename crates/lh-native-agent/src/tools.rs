//! Minimal built-in tools for Phase 2: `read_file`, `write_file`, `bash`.
//! Deliberately unsandboxed and flagged as such -- real OS-level
//! sandboxing (Landlock/Seatbelt, architecture §8) is Phase 3's job, not
//! this one's. What *is* in scope here: a bare-minimum path-traversal
//! guard, so the dev-only nature of this phase doesn't mean "no guard rails
//! at all."

use std::path::{Component, Path, PathBuf};

use lh_event::{PermissionAction, RiskTier};
use lh_model_provider::ToolSpec;

const MAX_OUTPUT_CHARS: usize = 4000;

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("path '{0}' escapes the workspace root")]
    PathEscapesWorkspace(String),
    #[error("missing or invalid '{0}' argument")]
    BadArgument(&'static str),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("unknown tool: {0}")]
    UnknownTool(String),
}

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

pub async fn execute_tool(
    tool_name: &str,
    input: &serde_json::Value,
    workspace_root: &Path,
) -> Result<String, ToolError> {
    match tool_name {
        "read_file" => {
            let rel = str_arg(input, "path").ok_or(ToolError::BadArgument("path"))?;
            let full = resolve_within(workspace_root, rel)?;
            let content = tokio::fs::read_to_string(&full).await?;
            Ok(truncate(&content))
        }
        "write_file" => {
            let rel = str_arg(input, "path").ok_or(ToolError::BadArgument("path"))?;
            let content = str_arg(input, "content").ok_or(ToolError::BadArgument("content"))?;
            let full = resolve_within(workspace_root, rel)?;
            if let Some(parent) = full.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(&full, content).await?;
            Ok(format!("wrote {} bytes to {rel}", content.len()))
        }
        "bash" => {
            let command = str_arg(input, "command").ok_or(ToolError::BadArgument("command"))?;
            let output = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(command)
                .current_dir(workspace_root)
                .output()
                .await?;
            let mut combined = String::from_utf8_lossy(&output.stdout).to_string();
            if !output.stderr.is_empty() {
                combined.push_str("\n[stderr]\n");
                combined.push_str(&String::from_utf8_lossy(&output.stderr));
            }
            combined.push_str(&format!("\n[exit status: {}]", output.status));
            Ok(truncate(&combined))
        }
        other => Err(ToolError::UnknownTool(other.to_string())),
    }
}

fn str_arg<'a>(input: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    input.get(key)?.as_str()
}

fn truncate(s: &str) -> String {
    if s.chars().count() <= MAX_OUTPUT_CHARS {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(MAX_OUTPUT_CHARS).collect();
        format!("{truncated}\n... [truncated, {} more chars]", s.chars().count() - MAX_OUTPUT_CHARS)
    }
}

/// Rejects any relative path with a `..` or absolute-path component before
/// joining it to `root`. Not a substitute for the OS-level sandbox
/// (§8) -- just enough to stop the most obvious accidents in a phase that
/// is explicitly unsandboxed otherwise.
fn resolve_within(root: &Path, rel: &str) -> Result<PathBuf, ToolError> {
    let rel_path = Path::new(rel);
    for component in rel_path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            _ => return Err(ToolError::PathEscapesWorkspace(rel.to_string())),
        }
    }
    Ok(root.join(rel_path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_file_round_trips_write_file() {
        let dir = tempfile::tempdir().unwrap();
        execute_tool(
            "write_file",
            &serde_json::json!({"path": "a.txt", "content": "hi"}),
            dir.path(),
        )
        .await
        .unwrap();

        let out = execute_tool("read_file", &serde_json::json!({"path": "a.txt"}), dir.path())
            .await
            .unwrap();
        assert_eq!(out, "hi");
    }

    #[tokio::test]
    async fn write_file_creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        execute_tool(
            "write_file",
            &serde_json::json!({"path": "nested/dir/b.txt", "content": "nested"}),
            dir.path(),
        )
        .await
        .unwrap();
        assert!(dir.path().join("nested/dir/b.txt").exists());
    }

    #[tokio::test]
    async fn bash_captures_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let out = execute_tool("bash", &serde_json::json!({"command": "echo hi"}), dir.path())
            .await
            .unwrap();
        assert!(out.contains("hi"));
    }

    #[tokio::test]
    async fn path_traversal_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let err = execute_tool(
            "read_file",
            &serde_json::json!({"path": "../escape.txt"}),
            dir.path(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::PathEscapesWorkspace(_)));
    }
}
