//! End-to-end delegation: spawn -> `initialize` -> `session/new` ->
//! `session/prompt` (architecture §5's flow). Two entry points share the
//! same inner turn logic (`run_delegation_inner`):
//! `run_delegation` wraps it with `ChildSessionSpawned`/`ChildSessionEnded`
//! bookkeeping for a *child* session; `run_primary_turn` (architecture
//! §12, Phase 6) calls it directly for a session that is itself the root
//! -- there's no parent to wrap, since nothing about ACP's spawn/connect
//! flow cares whether the resulting session is attached as a child or
//! *is* the tree's root (§12's key insight).

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use agent_client_protocol_schema::v1 as acp;
use lh_event::{
    Actor, ChildKind, ChildOutcome, ChildSpec, ContentBlock, Event, EventPayload, SessionDriver,
    SessionId, UsageConfidence, UsageDelta,
};
use lh_execution::ExecutionPlane;
use lh_permission::PermissionEngine;
use lh_store::SessionStore;

use crate::client::HarnessAcpClient;
use crate::registry::DelegatedAgentAdapter;
use crate::transport::AcpConnection;

pub type Result<T> = std::result::Result<T, anyhow::Error>;

/// Runs one delegated task end to end against `adapter`, appending a full
/// `ChildSessionSpawned` .. `ChildSessionEnded` event sequence to
/// `parent_session_id`'s child (`child_session_id`, minted by the caller
/// so it can respond to the client before this completes, mirroring how
/// `handle_session_prompt` already works).
#[allow(clippy::too_many_arguments)]
pub async fn run_delegation(
    parent_session_id: SessionId,
    child_session_id: SessionId,
    workspace_root: &Path,
    adapter: &DelegatedAgentAdapter,
    task_summary: &str,
    store: Arc<dyn SessionStore>,
    permission_engine: Arc<dyn PermissionEngine>,
    execution_plane: Arc<dyn ExecutionPlane>,
) -> Result<ChildOutcome> {
    // ChildSessionSpawned on the parent must land *before* the child's own
    // first event: the daemon's event forwarder (lh-daemon/src/main.rs)
    // discovers session-tree membership live by watching for this event,
    // so a client attached to the parent needs to see it before it can
    // recognize the child's events as ones to forward.
    store
        .append(Event::new(
            parent_session_id,
            None,
            Actor::System,
            EventPayload::ChildSessionSpawned {
                child: child_session_id,
                kind: ChildKind::Delegated { agent: adapter.kind.clone() },
                spec: ChildSpec { task_summary: task_summary.to_string() },
            },
        ))
        .await?;
    store
        .append(Event::new(
            child_session_id,
            Some(parent_session_id),
            Actor::System,
            EventPayload::SessionDriverSet {
                driver: SessionDriver::Delegated { agent: adapter.kind.clone() },
            },
        ))
        .await?;
    store
        .append(Event::new(
            child_session_id,
            Some(parent_session_id),
            Actor::User,
            EventPayload::UserMessage { content: vec![ContentBlock::text(task_summary)] },
        ))
        .await?;

    let outcome = run_delegation_inner(
        child_session_id,
        workspace_root,
        adapter,
        task_summary,
        store.clone(),
        permission_engine,
        execution_plane,
    )
    .await;

    let outcome = outcome.unwrap_or_else(|e| ChildOutcome::Failed { message: e.to_string() });

    store
        .append(Event::new(
            parent_session_id,
            None,
            Actor::System,
            EventPayload::ChildSessionEnded { child: child_session_id, outcome: outcome.clone() },
        ))
        .await?;

    Ok(outcome)
}

/// Runs one prompt turn for a session whose *own* driver is
/// `SessionDriver::Delegated` (root substitution, architecture §12) --
/// unlike `run_delegation`, `session_id` here has no parent and there is
/// no `ChildSessionSpawned`/`ChildSessionEnded` wrapping to do; the
/// session itself already got its `SessionDriverSet { driver: Delegated }`
/// at `session/create` time (`lh-daemon`). A failure is recorded as an
/// `Error` event directly on this session -- the same role
/// `ChildSessionEnded { outcome: Failed }` plays for a child, since there's
/// no parent event to carry that information here.
pub async fn run_primary_turn(
    session_id: SessionId,
    workspace_root: &Path,
    adapter: &DelegatedAgentAdapter,
    task_summary: &str,
    store: Arc<dyn SessionStore>,
    permission_engine: Arc<dyn PermissionEngine>,
    execution_plane: Arc<dyn ExecutionPlane>,
) -> Result<ChildOutcome> {
    store
        .append(Event::new(
            session_id,
            None,
            Actor::User,
            EventPayload::UserMessage { content: vec![ContentBlock::text(task_summary)] },
        ))
        .await?;

    let outcome = run_delegation_inner(
        session_id,
        workspace_root,
        adapter,
        task_summary,
        store.clone(),
        permission_engine,
        execution_plane,
    )
    .await;

    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(e) => {
            store
                .append(Event::new(
                    session_id,
                    None,
                    Actor::System,
                    EventPayload::Error { message: e.to_string(), recoverable: true },
                ))
                .await?;
            ChildOutcome::Failed { message: e.to_string() }
        }
    };

    Ok(outcome)
}

async fn run_delegation_inner(
    child_session_id: SessionId,
    workspace_root: &Path,
    adapter: &DelegatedAgentAdapter,
    task_summary: &str,
    store: Arc<dyn SessionStore>,
    permission_engine: Arc<dyn PermissionEngine>,
    execution_plane: Arc<dyn ExecutionPlane>,
) -> Result<ChildOutcome> {
    let client = Arc::new(HarnessAcpClient::new(
        child_session_id,
        adapter.kind.clone(),
        store.clone(),
        permission_engine,
        execution_plane,
        workspace_root.to_path_buf(),
    ));

    let mut conn = AcpConnection::spawn(adapter, workspace_root, client.clone()).await?;

    let init_params = serde_json::to_value(acp::InitializeRequest::new(
        agent_client_protocol_schema::ProtocolVersion::V1,
    ))?;
    let _init: acp::InitializeResponse = conn.call("initialize", init_params).await?;

    let new_session_params = serde_json::to_value(acp::NewSessionRequest::new(workspace_root))?;
    let new_session: acp::NewSessionResponse = conn.call("session/new", new_session_params).await?;
    let acp_session_id = new_session.session_id;

    let started = Instant::now();
    let before_cost = client.last_reported_cost().await;

    let prompt = acp::PromptRequest::new(
        acp_session_id,
        vec![acp::ContentBlock::Text(acp::TextContent::new(task_summary))],
    );
    let prompt_params = serde_json::to_value(prompt)?;
    let prompt_response: acp::PromptResponse = conn.call("session/prompt", prompt_params).await?;

    let after_cost = client.last_reported_cost().await;
    let wall_ms = started.elapsed().as_millis() as u64;
    let usage = match (before_cost, after_cost) {
        (_, None) => UsageDelta {
            input_tokens: None,
            output_tokens: None,
            cost_usd: None,
            wall_ms,
            confidence: UsageConfidence::Unknown,
        },
        (before, Some(after)) => UsageDelta {
            input_tokens: None,
            output_tokens: None,
            cost_usd: Some(after - before.unwrap_or(0.0)),
            wall_ms,
            // A diffed cumulative figure, not a true per-turn number --
            // never claimed Exact (architecture §7/B.5's honest-gap
            // pattern, same reasoning as an unpriced native model).
            confidence: UsageConfidence::Estimated,
        },
    };
    store
        .append(Event::new(
            child_session_id,
            None,
            Actor::System,
            EventPayload::UsageReported { usage },
        ))
        .await?;

    let _ = conn.kill().await;

    let summary = format!("stop_reason: {:?}", prompt_response.stop_reason);
    Ok(match prompt_response.stop_reason {
        acp::StopReason::EndTurn => ChildOutcome::Success { summary },
        acp::StopReason::Cancelled => ChildOutcome::Cancelled,
        _ => ChildOutcome::Failed { message: summary },
    })
}
