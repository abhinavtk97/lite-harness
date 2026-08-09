//! `HarnessAcpClient` (architecture §5): translates an ACP agent's
//! `session/update` notifications into the same `Event`/`EventPayload`
//! shapes native tool calls use (§3 -- "a translator, not a special
//! case"), and routes its `session/request_permission` /
//! `fs/read_text_file` / `fs/write_text_file` requests through the
//! *identical* `PermissionEngine` instance the native loop uses -- no
//! parallel trust boundary (§6).

use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol_schema::v1 as acp;
use lh_event::{
    Actor, AgentKind, ContentBlock, Event, EventPayload, PermissionAction, PermissionDecision,
    PermissionRequest, RiskTier, SessionId, ToolCall, ToolCallStatus, ToolSource,
};
use lh_execution::ExecutionPlane;
use lh_permission::PermissionEngine;
use lh_store::SessionStore;
use tokio::sync::Mutex as AsyncMutex;

pub type Result<T> = std::result::Result<T, anyhow::Error>;

pub struct HarnessAcpClient {
    child_session_id: SessionId,
    agent_kind: AgentKind,
    store: Arc<dyn SessionStore>,
    permission_engine: Arc<dyn PermissionEngine>,
    execution_plane: Arc<dyn ExecutionPlane>,
    workspace_root: PathBuf,
    /// Last cumulative session cost observed via `UsageUpdate.cost`
    /// (architecture §7/B.5 -- ACP reports a *cumulative* figure, not a
    /// per-turn delta; `delegate::run_delegation` diffs successive
    /// snapshots of this into a per-turn `UsageDelta`).
    last_reported_cost: AsyncMutex<Option<f64>>,
}

impl HarnessAcpClient {
    pub fn new(
        child_session_id: SessionId,
        agent_kind: AgentKind,
        store: Arc<dyn SessionStore>,
        permission_engine: Arc<dyn PermissionEngine>,
        execution_plane: Arc<dyn ExecutionPlane>,
        workspace_root: PathBuf,
    ) -> Self {
        Self {
            child_session_id,
            agent_kind,
            store,
            permission_engine,
            execution_plane,
            workspace_root,
            last_reported_cost: AsyncMutex::new(None),
        }
    }

    pub async fn last_reported_cost(&self) -> Option<f64> {
        *self.last_reported_cost.lock().await
    }

    async fn emit(&self, actor: Actor, payload: EventPayload) -> Result<()> {
        self.store
            .append(Event::new(self.child_session_id, None, actor, payload))
            .await?;
        Ok(())
    }

    pub async fn handle_session_update(&self, notif: acp::SessionNotification) -> Result<()> {
        if let acp::SessionUpdate::UsageUpdate(u) = &notif.update {
            *self.last_reported_cost.lock().await = u.cost.as_ref().map(|c| c.amount);
        }
        if let Some(payload) = translate_acp_update(&notif.update) {
            self.emit(Actor::Agent, payload).await?;
        }
        Ok(())
    }

    pub async fn handle_request_permission(
        &self,
        req: acp::RequestPermissionRequest,
    ) -> Result<acp::RequestPermissionResponse> {
        let call_id = req.tool_call.tool_call_id.0.to_string();
        let perm_request = translate_acp_permission_request(self.child_session_id, &self.agent_kind, &req);

        self.emit(
            Actor::System,
            EventPayload::PermissionRequested {
                call_id: call_id.clone(),
                request: perm_request.clone(),
            },
        )
        .await?;

        // The exact same PermissionEngine instance that gates native tool
        // calls (architecture §6's "one interception point") -- passed in
        // at construction, never a parallel engine.
        let resolution = self.permission_engine.decide(&perm_request).await?;

        self.emit(
            Actor::System,
            EventPayload::PermissionDecided {
                call_id,
                decision: resolution.decision.clone(),
                decided_by: resolution.source,
            },
        )
        .await?;

        Ok(translate_decision_to_acp_response(resolution.decision, &req))
    }

    pub async fn handle_read_text_file(
        &self,
        req: acp::ReadTextFileRequest,
    ) -> Result<acp::ReadTextFileResponse> {
        // Native reads go through the permission engine too
        // (handle_one_tool_call), so ACP-originated reads get identical
        // treatment -- no special-cased "reads are always fine" path.
        let rel_path = relativize(&req.path, &self.workspace_root);
        let perm_request = PermissionRequest {
            session_id: self.child_session_id,
            tool_source: ToolSource::Acp { agent: self.agent_kind.clone() },
            action: PermissionAction::FileRead { path: rel_path.clone() },
            risk_tier: RiskTier::Read,
        };
        let resolution = self.permission_engine.decide(&perm_request).await?;
        match &resolution.decision {
            PermissionDecision::Allow | PermissionDecision::AllowAlways { .. } => {
                let call = ToolCall {
                    call_id: format!("acp-read:{}", req.path.display()),
                    tool_name: "read_file".to_string(),
                    source: ToolSource::Acp { agent: self.agent_kind.clone() },
                    args_summary: serde_json::json!({ "path": rel_path }),
                    raw_args: serde_json::json!({ "path": rel_path }),
                };
                let content = self
                    .execution_plane
                    .execute(&call, &resolution.decision)
                    .await?;
                Ok(acp::ReadTextFileResponse::new(content))
            }
            PermissionDecision::Deny | PermissionDecision::DenyAlways { .. } => {
                anyhow::bail!("read denied by permission policy")
            }
        }
    }

    pub async fn handle_write_text_file(
        &self,
        req: acp::WriteTextFileRequest,
    ) -> Result<acp::WriteTextFileResponse> {
        let rel_path = relativize(&req.path, &self.workspace_root);
        let perm_request = PermissionRequest {
            session_id: self.child_session_id,
            tool_source: ToolSource::Acp { agent: self.agent_kind.clone() },
            action: PermissionAction::FileWrite { path: rel_path.clone(), diff_summary: None },
            risk_tier: RiskTier::Write,
        };
        let resolution = self.permission_engine.decide(&perm_request).await?;
        match &resolution.decision {
            PermissionDecision::Allow | PermissionDecision::AllowAlways { .. } => {
                let call = ToolCall {
                    call_id: format!("acp-write:{}", req.path.display()),
                    tool_name: "write_file".to_string(),
                    source: ToolSource::Acp { agent: self.agent_kind.clone() },
                    args_summary: serde_json::json!({ "path": rel_path }),
                    raw_args: serde_json::json!({ "path": rel_path, "content": req.content }),
                };
                self.execution_plane.execute(&call, &resolution.decision).await?;
                Ok(acp::WriteTextFileResponse::new())
            }
            PermissionDecision::Deny | PermissionDecision::DenyAlways { .. } => {
                anyhow::bail!("write denied by permission policy")
            }
        }
    }
}

/// ACP paths are absolute (`session/new`'s `cwd` contract); our native
/// tools take workspace-relative paths. Best-effort strip of the
/// workspace root prefix -- falls back to the absolute path if it isn't
/// actually under the workspace (the `ExecutionPlane`'s own
/// path-boundary check is the real enforcement either way).
fn relativize(path: &std::path::Path, workspace_root: &std::path::Path) -> PathBuf {
    path.strip_prefix(workspace_root).unwrap_or(path).to_path_buf()
}

/// `session/update` -> `Event` (§3's "a translator, not a special case").
/// Returns `None` for update kinds with no v1 mapping (e.g. mode/config
/// updates) -- `UsageUpdate` is handled separately by the caller (it
/// updates `last_reported_cost`, it doesn't itself become an `Event`).
pub fn translate_acp_update(update: &acp::SessionUpdate) -> Option<EventPayload> {
    match update {
        acp::SessionUpdate::AgentMessageChunk(c) => Some(EventPayload::AgentMessageChunk {
            content: translate_acp_content_block(&c.content),
        }),
        acp::SessionUpdate::AgentThoughtChunk(c) => Some(EventPayload::AgentThoughtChunk {
            content: translate_acp_content_block(&c.content),
        }),
        acp::SessionUpdate::ToolCall(tc) => Some(EventPayload::ToolCallRequested {
            call: translate_acp_tool_call(tc),
        }),
        acp::SessionUpdate::ToolCallUpdate(tcu) => Some(EventPayload::ToolCallUpdated {
            call_id: tcu.tool_call_id.0.to_string(),
            status: translate_acp_tool_call_status(tcu.fields.status),
            output: tcu
                .fields
                .content
                .as_ref()
                .and_then(|blocks| blocks.first())
                .and_then(translate_acp_tool_call_content),
        }),
        _ => None,
    }
}

fn translate_acp_content_block(block: &acp::ContentBlock) -> ContentBlock {
    match block {
        acp::ContentBlock::Text(t) => ContentBlock::text(t.text.clone()),
        other => ContentBlock::Other {
            kind: "acp".to_string(),
            value: serde_json::to_value(other).unwrap_or(serde_json::Value::Null),
        },
    }
}

fn translate_acp_tool_call_content(content: &acp::ToolCallContent) -> Option<ContentBlock> {
    match content {
        acp::ToolCallContent::Content(c) => Some(translate_acp_content_block(&c.content)),
        other => Some(ContentBlock::Other {
            kind: "acp_tool_call_content".to_string(),
            value: serde_json::to_value(other).unwrap_or(serde_json::Value::Null),
        }),
    }
}

fn translate_acp_tool_call(tc: &acp::ToolCall) -> ToolCall {
    let args = serde_json::json!({ "title": tc.title, "kind": tc.kind });
    ToolCall {
        call_id: tc.tool_call_id.0.to_string(),
        tool_name: tc.title.clone(),
        source: ToolSource::Acp { agent: AgentKind::ClaudeCode },
        args_summary: args.clone(),
        raw_args: args,
    }
}

fn translate_acp_tool_call_status(status: Option<acp::ToolCallStatus>) -> ToolCallStatus {
    match status.unwrap_or_default() {
        acp::ToolCallStatus::Pending => ToolCallStatus::Pending,
        acp::ToolCallStatus::InProgress => ToolCallStatus::InProgress,
        acp::ToolCallStatus::Completed => ToolCallStatus::Completed,
        acp::ToolCallStatus::Failed => ToolCallStatus::Failed,
        _ => ToolCallStatus::Pending,
    }
}

/// ACP's `ToolKind` -> our `RiskTier`. The one correctness-critical
/// mapping in this whole slice: `Execute`/`Delete` must land on
/// `Execute`/`Destructive` so the structural gate on destructive actions
/// (architecture §6 -- `RiskTier::Destructive` can never be satisfied by a
/// stored "always" rule) actually engages for ACP-originated destructive
/// calls exactly as it does for native ones.
pub fn infer_risk_tier(kind: acp::ToolKind) -> RiskTier {
    match kind {
        acp::ToolKind::Read | acp::ToolKind::Search => RiskTier::Read,
        acp::ToolKind::Edit | acp::ToolKind::Move => RiskTier::Write,
        acp::ToolKind::Delete => RiskTier::Destructive,
        acp::ToolKind::Execute => RiskTier::Execute,
        acp::ToolKind::Fetch => RiskTier::Network,
        acp::ToolKind::Think | acp::ToolKind::SwitchMode | acp::ToolKind::Other => RiskTier::Read,
        _ => RiskTier::Read,
    }
}

pub fn translate_acp_permission_request(
    child_session_id: SessionId,
    agent: &AgentKind,
    req: &acp::RequestPermissionRequest,
) -> PermissionRequest {
    let acp_tool_call = translate_acp_tool_call_update_to_tool_call(&req.tool_call, agent);
    let risk_tier = infer_risk_tier(req.tool_call.fields.kind.unwrap_or_default());
    PermissionRequest {
        session_id: child_session_id,
        tool_source: ToolSource::Acp { agent: agent.clone() },
        action: PermissionAction::DelegatedAgentToolCall {
            agent: agent.clone(),
            acp_tool_call: Box::new(acp_tool_call),
        },
        risk_tier,
    }
}

fn translate_acp_tool_call_update_to_tool_call(tcu: &acp::ToolCallUpdate, agent: &AgentKind) -> ToolCall {
    let args = serde_json::json!({
        "title": tcu.fields.title,
        "kind": tcu.fields.kind,
    });
    ToolCall {
        call_id: tcu.tool_call_id.0.to_string(),
        tool_name: tcu.fields.title.clone().unwrap_or_else(|| tcu.tool_call_id.0.to_string()),
        source: ToolSource::Acp { agent: agent.clone() },
        args_summary: args.clone(),
        raw_args: args,
    }
}

/// Our `PermissionDecision` -> the specific option the agent itself
/// offered in `req.options` -- ACP requires selecting one of the agent's
/// own listed options, not synthesizing an arbitrary outcome. Falls back
/// to the nearest same-polarity option, and to `Cancelled` -- never
/// guesses -- if the agent offers neither polarity (a spec violation).
pub fn translate_decision_to_acp_response(
    decision: PermissionDecision,
    req: &acp::RequestPermissionRequest,
) -> acp::RequestPermissionResponse {
    use acp::PermissionOptionKind::*;
    let wanted: [acp::PermissionOptionKind; 2] = match &decision {
        PermissionDecision::Allow => [AllowOnce, AllowAlways],
        PermissionDecision::AllowAlways { .. } => [AllowAlways, AllowOnce],
        PermissionDecision::Deny => [RejectOnce, RejectAlways],
        PermissionDecision::DenyAlways { .. } => [RejectAlways, RejectOnce],
    };
    let chosen = wanted
        .iter()
        .find_map(|k| req.options.iter().find(|o| o.kind == *k));
    match chosen {
        Some(opt) => acp::RequestPermissionResponse::new(acp::RequestPermissionOutcome::Selected(
            acp::SelectedPermissionOutcome::new(opt.option_id.clone()),
        )),
        None => acp::RequestPermissionResponse::new(acp::RequestPermissionOutcome::Cancelled),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use lh_permission::{DefaultPermissionEngine, PermissionPrompter};
    use lh_store::SqliteSessionStore;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct AlwaysAllowAlways {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl PermissionPrompter for AlwaysAllowAlways {
        async fn ask(&self, _request: &PermissionRequest) -> lh_permission::Result<PermissionDecision> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(PermissionDecision::AllowAlways { scope: lh_event::PolicyScope::Session })
        }
    }

    fn destructive_permission_request(call_id: &str) -> acp::RequestPermissionRequest {
        let fields = acp::ToolCallUpdateFields::new()
            .kind(Some(acp::ToolKind::Delete))
            .status(Some(acp::ToolCallStatus::Pending));
        let tool_call = acp::ToolCallUpdate::new(call_id.to_string(), fields);
        let options = vec![
            acp::PermissionOption::new("allow_once", "Allow", acp::PermissionOptionKind::AllowOnce),
            acp::PermissionOption::new("allow_always", "Always Allow", acp::PermissionOptionKind::AllowAlways),
            acp::PermissionOption::new("reject_once", "Reject", acp::PermissionOptionKind::RejectOnce),
        ];
        acp::RequestPermissionRequest::new("fake-session", tool_call, options)
    }

    /// The single highest-value correctness check for this whole slice's
    /// "no parallel trust boundary" claim: an ACP-originated tool call
    /// whose `ToolKind` maps to `RiskTier::Destructive` must hit the
    /// structural gate exactly like a native one does (architecture §6,
    /// proven for the native path by
    /// `lh-permission`'s `destructive_requests_never_consult_or_write_policy`)
    /// -- an `AllowAlways` response must never get persisted and must
    /// never short-circuit a second identical request.
    #[tokio::test]
    async fn a_destructive_acp_tool_call_is_never_short_circuited_by_an_always_rule() {
        let workspace = tempfile::tempdir().unwrap();
        let store: Arc<dyn SessionStore> = Arc::new(SqliteSessionStore::open_in_memory().unwrap());
        let prompter = Arc::new(AlwaysAllowAlways { calls: AtomicUsize::new(0) });
        let permission_engine: Arc<dyn PermissionEngine> =
            Arc::new(DefaultPermissionEngine::new(prompter.clone()));
        let execution_plane: Arc<dyn ExecutionPlane> =
            Arc::new(lh_execution::LocalExecutionPlane::new(workspace.path().to_path_buf()).await.unwrap());

        let client = HarnessAcpClient::new(
            SessionId::now_v7(),
            AgentKind::ClaudeCode,
            store,
            permission_engine,
            execution_plane,
            workspace.path().to_path_buf(),
        );

        client.handle_request_permission(destructive_permission_request("call_1")).await.unwrap();
        assert_eq!(prompter.calls.load(Ordering::SeqCst), 1);

        // Same class of request again -- if the AllowAlways from the first
        // call had (incorrectly) been persisted, this would short-circuit
        // and the prompter would NOT be asked a second time.
        client.handle_request_permission(destructive_permission_request("call_2")).await.unwrap();
        assert_eq!(prompter.calls.load(Ordering::SeqCst), 2, "destructive requests must always re-ask");
    }

    #[test]
    fn infer_risk_tier_maps_execute_and_delete_correctly() {
        assert_eq!(infer_risk_tier(acp::ToolKind::Execute), RiskTier::Execute);
        assert_eq!(infer_risk_tier(acp::ToolKind::Delete), RiskTier::Destructive);
        assert_eq!(infer_risk_tier(acp::ToolKind::Read), RiskTier::Read);
    }
}
