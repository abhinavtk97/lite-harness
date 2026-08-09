//! The permission engine (architecture §6) — the one interception point
//! every tool call, native or delegated, must pass through.
//!
//! Phase 3 adds layered policy persistence on top of Phase 2's "ask-only"
//! engine: an `AllowAlways`/`DenyAlways` decision is written to a policy
//! store at the chosen scope (session/project/global) and short-circuits
//! future matching requests without a round-trip to the prompter. One rule
//! is structural, not just a default: `RiskTier::Destructive` requests are
//! never looked up in, or written to, any policy layer -- they always
//! reach the prompter, regardless of any "always allow" rule that would
//! otherwise match (architecture §6's "structural gate on destructive
//! actions"). The engine itself stays transport-agnostic throughout: it
//! doesn't know or care whether the prompter is asking over a Unix socket,
//! a websocket, or a test double -- that decoupling is what lets the same
//! engine gate both native tool calls and (from Phase 4 on) ACP-delegated
//! ones.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use lh_event::{
    DecisionSource, PermissionAction, PermissionDecision, PermissionRequest, PolicyScope,
    RiskTier, ToolSource,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as AsyncMutex;

pub type Result<T> = std::result::Result<T, PermissionError>;

#[derive(Debug, thiserror::Error)]
pub enum PermissionError {
    #[error("permission prompt failed: {0}")]
    PromptFailed(String),
    #[error("policy store io error: {0}")]
    PolicyIo(#[from] std::io::Error),
    #[error("policy store parse error: {0}")]
    PolicyParse(#[from] toml::de::Error),
    #[error("policy store serialize error: {0}")]
    PolicySerialize(#[from] toml::ser::Error),
}

/// Abstracts "ask a human/UI and wait for a decision" away from any
/// specific transport. The daemon implements this over the Harness
/// Protocol (§4); tests implement it however is convenient.
#[async_trait]
pub trait PermissionPrompter: Send + Sync {
    async fn ask(&self, request: &PermissionRequest) -> Result<PermissionDecision>;
}

/// What the engine decided, and *why* -- so callers can record an accurate
/// `DecisionSource` on the `PermissionDecided` event instead of assuming
/// every decision came from an interactive prompt.
#[derive(Debug, Clone)]
pub struct PermissionResolution {
    pub decision: PermissionDecision,
    pub source: DecisionSource,
}

/// The single entry point every tool call must go through (architecture
/// §6.2). Called by (a) the native tool executor before any built-in or
/// MCP-sourced tool call, and (b) — from Phase 4 — the ACP client's
/// `request_permission` handler for delegated agents' tool calls.
#[async_trait]
pub trait PermissionEngine: Send + Sync {
    async fn decide(&self, request: &PermissionRequest) -> Result<PermissionResolution>;
}

/// A stable string key identifying "this class of action" for policy
/// matching.
///
/// v1 scope, deliberately: matched at tool + action-kind granularity
/// (e.g. "always allow write_file calls from the native read/write
/// tools"), not per-path or per-command. Finer-grained matching (path
/// prefixes, command allowlists) is a real gap, not a silent one -- it's
/// future work, not implemented here.
pub type PolicyKey = String;

pub fn policy_key(request: &PermissionRequest) -> PolicyKey {
    let source = match &request.tool_source {
        ToolSource::Native { tool_id } => format!("native:{tool_id}"),
        ToolSource::Mcp { server, tool } => format!("mcp:{server}:{tool}"),
        ToolSource::Acp { agent } => format!("acp:{agent:?}"),
    };
    let action = match &request.action {
        PermissionAction::FileRead { .. } => "file_read",
        PermissionAction::FileWrite { .. } => "file_write",
        PermissionAction::Exec { .. } => "exec",
        PermissionAction::NetworkFetch { .. } => "network_fetch",
        PermissionAction::McpToolCall { .. } => "mcp_tool_call",
        PermissionAction::DelegatedAgentToolCall { .. } => "delegated_agent_tool_call",
        PermissionAction::DelegateAgent { .. } => "delegate_agent",
    };
    format!("{source}#{action}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyRuleDecision {
    Allow,
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PolicyRuleEntry {
    key: PolicyKey,
    decision: PolicyRuleDecision,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PolicyFile {
    #[serde(default, rename = "rule")]
    rules: Vec<PolicyRuleEntry>,
}

/// A TOML-backed policy store at one on-disk scope (project or global,
/// architecture §6): `.lite-harness/policy.toml` or
/// `~/.config/lite-harness/policy.toml`. Loaded once at construction,
/// rewritten in full on every `set()` -- policy files are small and
/// change rarely, so incremental writes aren't worth the complexity.
pub struct TomlPolicyStore {
    path: PathBuf,
    rules: AsyncMutex<HashMap<PolicyKey, PolicyRuleDecision>>,
}

impl TomlPolicyStore {
    pub fn load(path: PathBuf) -> Result<Self> {
        let rules = if path.exists() {
            let text = std::fs::read_to_string(&path)?;
            let file: PolicyFile = toml::from_str(&text)?;
            file.rules.into_iter().map(|r| (r.key, r.decision)).collect()
        } else {
            HashMap::new()
        };
        Ok(Self {
            path,
            rules: AsyncMutex::new(rules),
        })
    }

    async fn get(&self, key: &PolicyKey) -> Option<PolicyRuleDecision> {
        self.rules.lock().await.get(key).copied()
    }

    async fn set(&self, key: PolicyKey, decision: PolicyRuleDecision) -> Result<()> {
        let mut guard = self.rules.lock().await;
        guard.insert(key, decision);
        let file = PolicyFile {
            rules: guard
                .iter()
                .map(|(key, decision)| PolicyRuleEntry {
                    key: key.clone(),
                    decision: *decision,
                })
                .collect(),
        };
        drop(guard);

        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.path, toml::to_string_pretty(&file)?)?;
        Ok(())
    }
}

/// In-memory, per-session policy scope -- never touches disk, and is gone
/// as soon as the session/connection that owns it ends.
#[derive(Default)]
pub struct SessionPolicyStore {
    rules: AsyncMutex<HashMap<PolicyKey, PolicyRuleDecision>>,
}

impl SessionPolicyStore {
    pub fn new() -> Self {
        Self::default()
    }

    async fn get(&self, key: &PolicyKey) -> Option<PolicyRuleDecision> {
        self.rules.lock().await.get(key).copied()
    }

    async fn set(&self, key: PolicyKey, decision: PolicyRuleDecision) {
        self.rules.lock().await.insert(key, decision);
    }
}

/// Consults policy layers in order -- session > project > global --
/// before falling back to the prompter; an `AllowAlways`/`DenyAlways`
/// response gets persisted at its chosen scope so the next matching
/// request short-circuits without a round-trip.
pub struct DefaultPermissionEngine {
    prompter: Arc<dyn PermissionPrompter>,
    session_policy: Arc<SessionPolicyStore>,
    project_policy: Option<Arc<TomlPolicyStore>>,
    global_policy: Option<Arc<TomlPolicyStore>>,
}

impl DefaultPermissionEngine {
    /// Ask-only mode (Phase 2 behavior): no on-disk policy layers, but
    /// still tracks in-memory session-scoped rules within this engine's
    /// lifetime.
    pub fn new(prompter: Arc<dyn PermissionPrompter>) -> Self {
        Self {
            prompter,
            session_policy: Arc::new(SessionPolicyStore::new()),
            project_policy: None,
            global_policy: None,
        }
    }

    pub fn with_policy_stores(
        prompter: Arc<dyn PermissionPrompter>,
        session_policy: Arc<SessionPolicyStore>,
        project_policy: Option<Arc<TomlPolicyStore>>,
        global_policy: Option<Arc<TomlPolicyStore>>,
    ) -> Self {
        Self {
            prompter,
            session_policy,
            project_policy,
            global_policy,
        }
    }

    async fn lookup_policy(&self, key: &PolicyKey) -> Option<PolicyRuleDecision> {
        if let Some(d) = self.session_policy.get(key).await {
            return Some(d);
        }
        if let Some(store) = &self.project_policy {
            if let Some(d) = store.get(key).await {
                return Some(d);
            }
        }
        if let Some(store) = &self.global_policy {
            if let Some(d) = store.get(key).await {
                return Some(d);
            }
        }
        None
    }

    /// Persists a rule at `scope`. If `scope` names an on-disk layer this
    /// engine wasn't given a store for, the write is silently skipped --
    /// in the daemon, both on-disk stores are always configured (§13.2's
    /// layering convention), so this only matters for tests that
    /// deliberately construct a narrower engine.
    async fn persist(&self, key: &PolicyKey, decision: PolicyRuleDecision, scope: PolicyScope) -> Result<()> {
        match scope {
            PolicyScope::Session => {
                self.session_policy.set(key.clone(), decision).await;
                Ok(())
            }
            PolicyScope::Project => match &self.project_policy {
                Some(store) => store.set(key.clone(), decision).await,
                None => Ok(()),
            },
            PolicyScope::Global => match &self.global_policy {
                Some(store) => store.set(key.clone(), decision).await,
                None => Ok(()),
            },
        }
    }
}

#[async_trait]
impl PermissionEngine for DefaultPermissionEngine {
    async fn decide(&self, request: &PermissionRequest) -> Result<PermissionResolution> {
        let key = policy_key(request);
        let is_destructive = request.risk_tier == RiskTier::Destructive;

        // Structural gate (architecture §6): a Destructive-tier request
        // never consults policy, no matter what rule might otherwise
        // match.
        if !is_destructive {
            if let Some(rule) = self.lookup_policy(&key).await {
                let decision = match rule {
                    PolicyRuleDecision::Allow => PermissionDecision::Allow,
                    PolicyRuleDecision::Deny => PermissionDecision::Deny,
                };
                return Ok(PermissionResolution {
                    decision,
                    source: DecisionSource::PolicyRule,
                });
            }
        }

        let decision = self.prompter.ask(request).await?;

        // Persist Allow/DenyAlways -- except for Destructive-tier
        // requests, which can never write a blanket rule (architecture
        // §6): the decision still applies to *this* call, it just never
        // reaches the store.
        if !is_destructive {
            match &decision {
                PermissionDecision::AllowAlways { scope } => {
                    self.persist(&key, PolicyRuleDecision::Allow, *scope).await?;
                }
                PermissionDecision::DenyAlways { scope } => {
                    self.persist(&key, PolicyRuleDecision::Deny, *scope).await?;
                }
                PermissionDecision::Allow | PermissionDecision::Deny => {}
            }
        }

        Ok(PermissionResolution {
            decision,
            source: DecisionSource::User,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lh_event::{AgentKind, PermissionAction, RiskTier};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ScriptedPrompter {
        decision: PermissionDecision,
        calls: AtomicUsize,
    }

    impl ScriptedPrompter {
        fn new(decision: PermissionDecision) -> Self {
            Self {
                decision,
                calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl PermissionPrompter for ScriptedPrompter {
        async fn ask(&self, _request: &PermissionRequest) -> Result<PermissionDecision> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.decision.clone())
        }
    }

    fn sample_request(risk_tier: RiskTier) -> PermissionRequest {
        PermissionRequest {
            session_id: lh_event::SessionId::now_v7(),
            tool_source: lh_event::ToolSource::Native {
                tool_id: "write_file".to_string(),
            },
            action: PermissionAction::FileWrite {
                path: PathBuf::from("foo.txt"),
                diff_summary: None,
            },
            risk_tier,
        }
    }

    #[tokio::test]
    async fn every_request_is_forwarded_to_the_prompter_when_no_policy_matches() {
        let prompter = Arc::new(ScriptedPrompter::new(PermissionDecision::Allow));
        let engine = DefaultPermissionEngine::new(prompter.clone());

        let resolution = engine.decide(&sample_request(RiskTier::Write)).await.unwrap();
        assert!(matches!(resolution.decision, PermissionDecision::Allow));
        assert_eq!(resolution.source, DecisionSource::User);
        assert_eq!(prompter.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_deny_decision_passes_through_unchanged() {
        let prompter = Arc::new(ScriptedPrompter::new(PermissionDecision::Deny));
        let engine = DefaultPermissionEngine::new(prompter);

        let resolution = engine.decide(&sample_request(RiskTier::Write)).await.unwrap();
        assert!(matches!(resolution.decision, PermissionDecision::Deny));
    }

    #[tokio::test]
    async fn allow_always_at_session_scope_short_circuits_the_next_matching_request() {
        let prompter = Arc::new(ScriptedPrompter::new(PermissionDecision::AllowAlways {
            scope: PolicyScope::Session,
        }));
        let engine = DefaultPermissionEngine::new(prompter.clone());

        let first = engine.decide(&sample_request(RiskTier::Write)).await.unwrap();
        assert!(matches!(first.decision, PermissionDecision::AllowAlways { .. }));
        assert_eq!(prompter.calls.load(Ordering::SeqCst), 1);

        let second = engine.decide(&sample_request(RiskTier::Write)).await.unwrap();
        assert!(matches!(second.decision, PermissionDecision::Allow));
        assert_eq!(second.source, DecisionSource::PolicyRule);
        // The prompter was not consulted a second time.
        assert_eq!(prompter.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn destructive_requests_never_consult_or_write_policy() {
        let prompter = Arc::new(ScriptedPrompter::new(PermissionDecision::AllowAlways {
            scope: PolicyScope::Session,
        }));
        let engine = DefaultPermissionEngine::new(prompter.clone());

        engine.decide(&sample_request(RiskTier::Destructive)).await.unwrap();
        assert_eq!(prompter.calls.load(Ordering::SeqCst), 1);

        // Even though the response was AllowAlways, a second Destructive
        // request must ask again -- the structural gate means it was
        // never persisted.
        engine.decide(&sample_request(RiskTier::Destructive)).await.unwrap();
        assert_eq!(prompter.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn project_policy_persists_to_disk_and_survives_a_reload() {
        let dir = tempfile::tempdir().unwrap();
        let policy_path = dir.path().join("policy.toml");

        let project_store = Arc::new(TomlPolicyStore::load(policy_path.clone()).unwrap());
        let prompter = Arc::new(ScriptedPrompter::new(PermissionDecision::DenyAlways {
            scope: PolicyScope::Project,
        }));
        let engine = DefaultPermissionEngine::with_policy_stores(
            prompter.clone(),
            Arc::new(SessionPolicyStore::new()),
            Some(project_store),
            None,
        );

        engine.decide(&sample_request(RiskTier::Write)).await.unwrap();
        assert!(policy_path.exists());

        // A brand new engine, loading the same on-disk store, must see
        // the persisted rule without ever asking the prompter.
        let reloaded_store = Arc::new(TomlPolicyStore::load(policy_path).unwrap());
        let fresh_prompter = Arc::new(ScriptedPrompter::new(PermissionDecision::Allow));
        let reloaded_engine = DefaultPermissionEngine::with_policy_stores(
            fresh_prompter.clone(),
            Arc::new(SessionPolicyStore::new()),
            Some(reloaded_store),
            None,
        );

        let resolution = reloaded_engine.decide(&sample_request(RiskTier::Write)).await.unwrap();
        assert!(matches!(resolution.decision, PermissionDecision::Deny));
        assert_eq!(fresh_prompter.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn policy_key_distinguishes_tool_source_and_action_kind() {
        let read = sample_request(RiskTier::Read);
        let mut write = sample_request(RiskTier::Write);
        write.action = PermissionAction::Exec {
            command: "ls".to_string(),
            args: vec![],
            cwd: PathBuf::from("."),
        };
        assert_ne!(policy_key(&read), policy_key(&write));

        let acp = PermissionRequest {
            tool_source: lh_event::ToolSource::Acp {
                agent: AgentKind::ClaudeCode,
            },
            ..sample_request(RiskTier::Write)
        };
        assert_ne!(policy_key(&read), policy_key(&acp));
    }
}
