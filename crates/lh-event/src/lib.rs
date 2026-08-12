//! Core event/session types for lite-harness (architecture §3, §6, §7, §12).
//!
//! This crate is the base of the dependency graph: everything else depends
//! on it, it depends on nothing lite-harness-specific. Value types that get
//! embedded in the event log (permission requests/decisions, usage deltas)
//! live here even though the *logic* that produces them (the permission
//! engine, the cost ledger) lives in their own crates later.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub type SessionId = uuid::Uuid;
pub type EventId = uuid::Uuid;
pub type ToolCallId = String;

/// One entry in a session's append-only event log (architecture §3.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// Monotonic within a session. Callers should pass `0`; the
    /// `SessionStore` implementation assigns and returns the real value.
    pub seq: u64,
    pub event_id: EventId,
    pub session_id: SessionId,
    pub parent_session_id: Option<SessionId>,
    pub ts: DateTime<Utc>,
    pub actor: Actor,
    pub payload: EventPayload,
}

impl Event {
    pub fn new(
        session_id: SessionId,
        parent_session_id: Option<SessionId>,
        actor: Actor,
        payload: EventPayload,
    ) -> Self {
        Self {
            seq: 0,
            event_id: uuid::Uuid::now_v7(),
            session_id,
            parent_session_id,
            ts: Utc::now(),
            actor,
            payload,
        }
    }
}

/// Who produced an event. "Which agent kind" is recorded once per session
/// via `SessionDriverSet`, not repeated per event (§12.1 refinement).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Actor {
    User,
    Agent,
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum EventPayload {
    UserMessage {
        content: Vec<ContentBlock>,
    },
    AgentMessageChunk {
        content: ContentBlock,
    },
    AgentThoughtChunk {
        content: ContentBlock,
    },
    ToolCallRequested {
        call: ToolCall,
    },
    ToolCallUpdated {
        call_id: ToolCallId,
        status: ToolCallStatus,
        output: Option<ContentBlock>,
    },
    PermissionRequested {
        call_id: ToolCallId,
        request: PermissionRequest,
    },
    PermissionDecided {
        call_id: ToolCallId,
        decision: PermissionDecision,
        decided_by: DecisionSource,
    },
    UsageReported {
        usage: UsageDelta,
    },
    ChildSessionSpawned {
        child: SessionId,
        kind: ChildKind,
        spec: ChildSpec,
    },
    ChildSessionEnded {
        child: SessionId,
        outcome: ChildOutcome,
    },
    SessionForked {
        from_seq: u64,
        new_session: SessionId,
    },
    SessionResumed {
        at_seq: u64,
    },
    /// Exactly one per session, appended at creation (§12.1).
    SessionDriverSet {
        driver: SessionDriver,
    },
    PlanUpdated {
        steps: Vec<PlanStep>,
    },
    Error {
        message: String,
        recoverable: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    Text { text: String },
    Other { kind: String, value: serde_json::Value },
}

impl ContentBlock {
    pub fn text(s: impl Into<String>) -> Self {
        ContentBlock::Text { text: s.into() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub call_id: ToolCallId,
    pub tool_name: String,
    pub source: ToolSource,
    pub args_summary: serde_json::Value,
    pub raw_args: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ToolSource {
    Native { tool_id: String },
    Mcp { server: String, tool: String },
    Acp { agent: AgentKind },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolCallStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
    Cancelled,
}

/// Which underlying agent kind a session/tool-call is associated with
/// (architecture §5.2, §12.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AgentKind {
    ClaudeCode,
    Codex,
    GeminiCli,
    Goose,
    OpenCode,
    Custom { name: String },
}

// --- Permission types (architecture §6; logic lives in lh-permission,
// these are just the data shapes embedded in the event log) ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRequest {
    pub session_id: SessionId,
    pub tool_source: ToolSource,
    pub action: PermissionAction,
    pub risk_tier: RiskTier,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum PermissionAction {
    FileRead { path: PathBuf },
    FileWrite { path: PathBuf, diff_summary: Option<String> },
    Exec { command: String, args: Vec<String>, cwd: PathBuf },
    NetworkFetch { url: String },
    McpToolCall { server: String, tool: String, args_summary: serde_json::Value },
    DelegatedAgentToolCall { agent: AgentKind, acp_tool_call: Box<ToolCall> },
    /// §12.2 — delegation itself flows through the same permission gate.
    DelegateAgent { target: AgentKind, task_summary: String },
    /// §9 — spawning a native subagent is itself a gated action, with its
    /// own dedicated policy lever (e.g. "always ask before spawning
    /// subagents"), independent of the risk tier of whatever the subagent
    /// ends up doing internally.
    SpawnSubagent { role: String, task_summary: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RiskTier {
    Read,
    Write,
    Execute,
    Network,
    Destructive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyScope {
    Session,
    Project,
    Global,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum PermissionDecision {
    Allow,
    AllowAlways { scope: PolicyScope },
    Deny,
    DenyAlways { scope: PolicyScope },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DecisionSource {
    User,
    PolicyRule,
    System,
}

// --- Cost ledger types (architecture §7) ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UsageConfidence {
    Exact,
    Estimated,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageDelta {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// Tokens served from / written to a prompt cache -- see
    /// `lh_model_provider::ModelUsage`'s matching fields for the per-protocol
    /// story on why `cache_write_tokens` is always `None` for OpenAI-shaped
    /// providers. Purely informational: `cost_usd` below is not cache-aware.
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    /// Money as f64 for the Phase 1 skeleton; a fixed-point type
    /// (e.g. `rust_decimal::Decimal`) is a drop-in swap later without
    /// changing the event schema's shape or call sites' logic.
    pub cost_usd: Option<f64>,
    pub wall_ms: u64,
    pub confidence: UsageConfidence,
}

// --- Session tree / child types (architecture §3.2, §9, §12) ---

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ChildKind {
    NativeSubagent { role: String },
    Delegated { agent: AgentKind },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildSpec {
    pub task_summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ChildOutcome {
    Success { summary: String },
    Failed { message: String },
    Cancelled,
}

/// §12.1 — applies uniformly to root and child sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SessionDriver {
    Native,
    Delegated { agent: AgentKind },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanStepStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStep {
    pub description: String,
    pub status: PlanStepStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_round_trips_through_json() {
        let session_id = SessionId::now_v7();
        let event = Event::new(
            session_id,
            None,
            Actor::User,
            EventPayload::UserMessage {
                content: vec![ContentBlock::text("hello")],
            },
        );
        let json = serde_json::to_string(&event).unwrap();
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(back.session_id, session_id);
    }
}
