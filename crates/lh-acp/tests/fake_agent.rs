//! Integration test: `lh-acp` against a hand-rolled fake ACP agent
//! (`tests/fixtures/fake_acp_agent.py`) speaking just enough of the wire
//! protocol to exercise `AcpConnection` + `HarnessAcpClient` end to end,
//! with zero real dependencies (no network, no real Claude Code, no API
//! key needed to reach a model) -- this is the real bar for "does the
//! mechanism work" per the plan, since no live Anthropic API key is
//! available to test against the actual `claude-agent-acp` adapter.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use lh_acp::registry::{DelegatedAgentAdapter, SpawnSpec};
use lh_event::{
    AgentKind, ChildOutcome, EventPayload, PermissionDecision, PermissionRequest, SessionId,
    UsageConfidence,
};
use lh_execution::LocalExecutionPlane;
use lh_permission::{DefaultPermissionEngine, PermissionPrompter};
use lh_store::{SessionStore, SqliteSessionStore};

struct AlwaysAllow {
    calls: AtomicUsize,
}

#[async_trait]
impl PermissionPrompter for AlwaysAllow {
    async fn ask(&self, _request: &PermissionRequest) -> lh_permission::Result<PermissionDecision> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(PermissionDecision::Allow)
    }
}

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_acp_agent.py")
}

#[tokio::test]
async fn a_delegated_task_round_trips_through_the_fake_agent() {
    std::env::set_var("LH_ACP_TEST_FAKE_KEY", "unused-by-the-fake-agent");

    let workspace = tempfile::tempdir().unwrap();
    let store: Arc<dyn SessionStore> = Arc::new(SqliteSessionStore::open_in_memory().unwrap());
    let prompter = Arc::new(AlwaysAllow { calls: AtomicUsize::new(0) });
    let permission_engine: Arc<dyn lh_permission::PermissionEngine> =
        Arc::new(DefaultPermissionEngine::new(prompter.clone()));
    let execution_plane: Arc<dyn lh_execution::ExecutionPlane> =
        Arc::new(LocalExecutionPlane::new(workspace.path().to_path_buf()).await.unwrap());

    let adapter = DelegatedAgentAdapter {
        kind: AgentKind::ClaudeCode,
        spawn: SpawnSpec {
            command: "python3".to_string(),
            args: vec![fixture_path().to_string_lossy().to_string()],
            api_key_env: "LH_ACP_TEST_FAKE_KEY".to_string(),
        },
        can_be_primary: false,
    };

    let parent_session_id = SessionId::now_v7();
    let child_session_id = SessionId::now_v7();

    let outcome = lh_acp::delegate::run_delegation(
        parent_session_id,
        child_session_id,
        workspace.path(),
        &adapter,
        "please run the diagnostic",
        store.clone(),
        permission_engine,
        execution_plane,
    )
    .await
    .unwrap();

    assert!(matches!(outcome, ChildOutcome::Success { .. }), "expected Success, got {outcome:?}");
    assert_eq!(prompter.calls.load(Ordering::SeqCst), 1, "exactly one live permission ask");

    let child_events = store.read_from(child_session_id, 0).await.unwrap();
    let kinds: Vec<&str> = child_events.iter().map(payload_kind).collect();
    assert_eq!(
        kinds,
        vec![
            "SessionDriverSet",
            "UserMessage",
            "AgentMessageChunk",
            "ToolCallRequested",
            "PermissionRequested",
            "PermissionDecided",
            "ToolCallUpdated",
            "UsageReported",
        ]
    );

    let EventPayload::UsageReported { usage } = &child_events.last().unwrap().payload else {
        panic!("expected UsageReported");
    };
    assert_eq!(usage.cost_usd, Some(0.01));
    assert_eq!(usage.confidence, UsageConfidence::Estimated);
    assert_eq!(usage.input_tokens, None, "v1 schema gives no per-turn token split -- honest gap");

    let parent_events = store.read_from(parent_session_id, 0).await.unwrap();
    let parent_kinds: Vec<&str> = parent_events.iter().map(payload_kind).collect();
    assert_eq!(parent_kinds, vec!["ChildSessionSpawned", "ChildSessionEnded"]);
}

#[tokio::test]
async fn a_root_session_driven_by_a_delegated_primary_round_trips_through_the_fake_agent() {
    // Architecture §12 / Phase 6: root substitution -- `session_id` here
    // has no parent, unlike `run_delegation`'s child case above. Same fake
    // agent, same ACP flow (spawn/init/session-new/prompt/usage); the only
    // difference is no `ChildSessionSpawned`/`ChildSessionEnded` wrapping,
    // because nothing about ACP's spawn/connect flow cares whether the
    // resulting session is attached as a child or *is* the tree's root.
    std::env::set_var("LH_ACP_TEST_FAKE_KEY", "unused-by-the-fake-agent");

    let workspace = tempfile::tempdir().unwrap();
    let store: Arc<dyn SessionStore> = Arc::new(SqliteSessionStore::open_in_memory().unwrap());
    let prompter = Arc::new(AlwaysAllow { calls: AtomicUsize::new(0) });
    let permission_engine: Arc<dyn lh_permission::PermissionEngine> =
        Arc::new(DefaultPermissionEngine::new(prompter.clone()));
    let execution_plane: Arc<dyn lh_execution::ExecutionPlane> =
        Arc::new(LocalExecutionPlane::new(workspace.path().to_path_buf()).await.unwrap());

    let adapter = DelegatedAgentAdapter {
        kind: AgentKind::ClaudeCode,
        spawn: SpawnSpec {
            command: "python3".to_string(),
            args: vec![fixture_path().to_string_lossy().to_string()],
            api_key_env: "LH_ACP_TEST_FAKE_KEY".to_string(),
        },
        can_be_primary: true,
    };

    // The root session itself -- a real deployment would have appended
    // `SessionDriverSet { driver: Delegated }` here at `session/create`
    // time (lh-daemon); the test only exercises `run_primary_turn` itself.
    let session_id = SessionId::now_v7();

    let outcome = lh_acp::delegate::run_primary_turn(
        session_id,
        workspace.path(),
        &adapter,
        "please run the diagnostic",
        store.clone(),
        permission_engine,
        execution_plane,
    )
    .await
    .unwrap();

    assert!(matches!(outcome, ChildOutcome::Success { .. }), "expected Success, got {outcome:?}");
    assert_eq!(prompter.calls.load(Ordering::SeqCst), 1, "exactly one live permission ask");

    let events = store.read_from(session_id, 0).await.unwrap();
    let kinds: Vec<&str> = events.iter().map(payload_kind).collect();
    assert_eq!(
        kinds,
        vec![
            "UserMessage",
            "AgentMessageChunk",
            "ToolCallRequested",
            "PermissionRequested",
            "PermissionDecided",
            "ToolCallUpdated",
            "UsageReported",
        ],
        "no ChildSessionSpawned/Ended -- this session has no parent, it IS the root"
    );

    let EventPayload::UsageReported { usage } = &events.last().unwrap().payload else {
        panic!("expected UsageReported");
    };
    assert_eq!(usage.cost_usd, Some(0.01));
    assert_eq!(usage.confidence, UsageConfidence::Estimated);
}

fn payload_kind(event: &lh_event::Event) -> &'static str {
    match &event.payload {
        EventPayload::UserMessage { .. } => "UserMessage",
        EventPayload::AgentMessageChunk { .. } => "AgentMessageChunk",
        EventPayload::AgentThoughtChunk { .. } => "AgentThoughtChunk",
        EventPayload::ToolCallRequested { .. } => "ToolCallRequested",
        EventPayload::ToolCallUpdated { .. } => "ToolCallUpdated",
        EventPayload::PermissionRequested { .. } => "PermissionRequested",
        EventPayload::PermissionDecided { .. } => "PermissionDecided",
        EventPayload::UsageReported { .. } => "UsageReported",
        EventPayload::ChildSessionSpawned { .. } => "ChildSessionSpawned",
        EventPayload::ChildSessionEnded { .. } => "ChildSessionEnded",
        EventPayload::SessionForked { .. } => "SessionForked",
        EventPayload::SessionResumed { .. } => "SessionResumed",
        EventPayload::SessionDriverSet { .. } => "SessionDriverSet",
        EventPayload::PlanUpdated { .. } => "PlanUpdated",
        EventPayload::Error { .. } => "Error",
    }
}
