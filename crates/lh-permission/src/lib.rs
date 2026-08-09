//! The permission engine (architecture §6) — the one interception point
//! every tool call, native or delegated, must pass through.
//!
//! Phase 2 scope: `DefaultPermissionEngine` runs in "Ask-only" mode -- no
//! policy store yet (that's Phase 3's `.lite-harness/policy.toml` layering),
//! every single request is forwarded to a `PermissionPrompter`. The engine
//! itself stays transport-agnostic: it doesn't know or care whether the
//! prompter is asking over a Unix socket, a websocket, or a test double --
//! that decoupling is what lets the same engine gate both native tool calls
//! and (from Phase 4 on) ACP-delegated ones.

use std::sync::Arc;

use async_trait::async_trait;
use lh_event::{PermissionDecision, PermissionRequest};

pub type Result<T> = std::result::Result<T, PermissionError>;

#[derive(Debug, thiserror::Error)]
pub enum PermissionError {
    #[error("permission prompt failed: {0}")]
    PromptFailed(String),
}

/// Abstracts "ask a human/UI and wait for a decision" away from any
/// specific transport. The daemon implements this over the Harness
/// Protocol (§4); tests implement it however is convenient.
#[async_trait]
pub trait PermissionPrompter: Send + Sync {
    async fn ask(&self, request: &PermissionRequest) -> Result<PermissionDecision>;
}

/// The single entry point every tool call must go through (architecture
/// §6.2). Called by (a) the native tool executor before any built-in or
/// MCP-sourced tool call, and (b) — from Phase 4 — the ACP client's
/// `request_permission` handler for delegated agents' tool calls.
#[async_trait]
pub trait PermissionEngine: Send + Sync {
    async fn decide(&self, request: &PermissionRequest) -> Result<PermissionDecision>;
}

pub struct DefaultPermissionEngine {
    prompter: Arc<dyn PermissionPrompter>,
}

impl DefaultPermissionEngine {
    pub fn new(prompter: Arc<dyn PermissionPrompter>) -> Self {
        Self { prompter }
    }
}

#[async_trait]
impl PermissionEngine for DefaultPermissionEngine {
    async fn decide(&self, request: &PermissionRequest) -> Result<PermissionDecision> {
        // Phase 2: no policy store to consult yet -- always ask.
        // Phase 3 adds a layered policy_store lookup here, short-circuiting
        // to Allow/Deny without a round-trip when a rule already matches;
        // this method signature doesn't need to change for that.
        self.prompter.ask(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lh_event::{PermissionAction, RiskTier};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ScriptedPrompter {
        decision: PermissionDecision,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl PermissionPrompter for ScriptedPrompter {
        async fn ask(&self, _request: &PermissionRequest) -> Result<PermissionDecision> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.decision.clone())
        }
    }

    fn sample_request() -> PermissionRequest {
        PermissionRequest {
            session_id: lh_event::SessionId::now_v7(),
            tool_source: lh_event::ToolSource::Native {
                tool_id: "write_file".to_string(),
            },
            action: PermissionAction::FileWrite {
                path: PathBuf::from("foo.txt"),
                diff_summary: None,
            },
            risk_tier: RiskTier::Write,
        }
    }

    #[tokio::test]
    async fn every_request_is_forwarded_to_the_prompter() {
        let prompter = Arc::new(ScriptedPrompter {
            decision: PermissionDecision::Allow,
            calls: AtomicUsize::new(0),
        });
        let engine = DefaultPermissionEngine::new(prompter.clone());

        let decision = engine.decide(&sample_request()).await.unwrap();
        assert!(matches!(decision, PermissionDecision::Allow));
        assert_eq!(prompter.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_deny_decision_passes_through_unchanged() {
        let prompter = Arc::new(ScriptedPrompter {
            decision: PermissionDecision::Deny,
            calls: AtomicUsize::new(0),
        });
        let engine = DefaultPermissionEngine::new(prompter);

        let decision = engine.decide(&sample_request()).await.unwrap();
        assert!(matches!(decision, PermissionDecision::Deny));
    }
}
