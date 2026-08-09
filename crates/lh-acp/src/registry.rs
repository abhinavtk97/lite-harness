//! Delegated-agent adapter registry (architecture §5.2, §12.5): a small,
//! declarative config the daemon loads at startup describing which ACP
//! agents it knows how to spawn -- mirrors the exact `providers.toml`/
//! `policy.toml` convention already established in
//! `lh-daemon/src/providers.rs`, not a new one.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub type Result<T> = std::result::Result<T, RegistryError>;

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse error: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("env var '{0}' (api_key_env) is not set")]
    MissingApiKey(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnSpec {
    /// Binary/command to spawn -- v1 assumes it's pre-installed/discoverable
    /// on `PATH` (architecture §5.2); auto-install is out of scope.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Env var NAME the daemon reads its own environment for and forwards
    /// into the spawned subprocess -- mirrors
    /// `ModelProviderConfig.api_key_env` (`lh-model-provider`) exactly:
    /// never stored in the config file, read fresh at spawn time.
    pub api_key_env: String,
}

impl SpawnSpec {
    /// Reads `api_key_env` from the daemon's own environment and forwards
    /// it into the subprocess as `ANTHROPIC_API_KEY` -- the credential
    /// shape `claude-agent-acp` actually expects (confirmed against its
    /// README: `ANTHROPIC_API_KEY=sk-... claude-agent-acp`).
    pub fn resolved_env(&self) -> Result<Vec<(String, String)>> {
        let key = std::env::var(&self.api_key_env)
            .map_err(|_| RegistryError::MissingApiKey(self.api_key_env.clone()))?;
        Ok(vec![("ANTHROPIC_API_KEY".to_string(), key)])
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegatedAgentAdapter {
    pub kind: lh_event::AgentKind,
    pub spawn: SpawnSpec,
    /// Reserved for §12.5 (primary agent substitution); unused until
    /// Phase 6, where `agents/list` would surface it to build a "which
    /// agent should drive this session" picker.
    #[serde(default)]
    pub can_be_primary: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentsFile {
    #[serde(rename = "agent", default)]
    pub agents: Vec<DelegatedAgentAdapter>,
}

impl AgentsFile {
    pub fn find(&self, kind: &lh_event::AgentKind) -> Option<&DelegatedAgentAdapter> {
        self.agents.iter().find(|a| &a.kind == kind)
    }
}

pub fn agents_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("LITE_HARNESS_AGENTS_FILE") {
        return Some(PathBuf::from(p));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/lite-harness/agents.toml"))
}

pub fn load_agents_file(path: &std::path::Path) -> Result<AgentsFile> {
    if !path.exists() {
        return Ok(AgentsFile::default());
    }
    let text = std::fs::read_to_string(path)?;
    Ok(toml::from_str(&text)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_an_agents_toml_document() {
        let toml_text = r#"
[[agent]]
kind = { type = "ClaudeCode" }
can_be_primary = false

[agent.spawn]
command = "npx"
args = ["-y", "@agentclientprotocol/claude-agent-acp"]
api_key_env = "ANTHROPIC_API_KEY"
"#;
        let file: AgentsFile = toml::from_str(toml_text).unwrap();
        assert_eq!(file.agents.len(), 1);
        assert_eq!(file.agents[0].spawn.command, "npx");
        assert!(matches!(file.agents[0].kind, lh_event::AgentKind::ClaudeCode));
        assert!(file.find(&lh_event::AgentKind::ClaudeCode).is_some());
        assert!(file.find(&lh_event::AgentKind::Codex).is_none());
    }

    #[test]
    fn missing_api_key_env_is_a_clear_error() {
        let spec = SpawnSpec {
            command: "npx".to_string(),
            args: vec![],
            api_key_env: "LH_ACP_TEST_DEFINITELY_UNSET_VAR".to_string(),
        };
        let err = spec.resolved_env().unwrap_err();
        assert!(matches!(err, RegistryError::MissingApiKey(_)));
    }

    #[test]
    fn missing_agents_file_yields_an_empty_registry_not_an_error() {
        let file = load_agents_file(std::path::Path::new("/nonexistent/agents.toml")).unwrap();
        assert!(file.agents.is_empty());
    }
}
