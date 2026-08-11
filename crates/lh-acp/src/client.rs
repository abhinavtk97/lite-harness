//! `HarnessAcpClient` (architecture §5): translates an ACP agent's
//! `session/update` notifications into the same `Event`/`EventPayload`
//! shapes native tool calls use (§3 -- "a translator, not a special
//! case"), and routes its `session/request_permission` /
//! `fs/read_text_file` / `fs/write_text_file` requests through the
//! *identical* `PermissionEngine` instance the native loop uses -- no
//! parallel trust boundary (§6).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol_schema::v1 as acp;
use lh_event::{
    Actor, AgentKind, ContentBlock, Event, EventPayload, PermissionAction, PermissionDecision,
    PermissionRequest, PlanStep, PlanStepStatus, RiskTier, SessionId, ToolCall, ToolCallStatus,
    ToolSource,
};
use lh_execution::{BackgroundProcess, ExecutionPlane};
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
    /// Live background processes created via `terminal/create`, keyed by
    /// the id we minted for them. `terminal/output`/`wait_for_exit`/`kill`
    /// all just look a handle up here; `terminal/release` removes it.
    terminals: AsyncMutex<HashMap<String, Arc<dyn BackgroundProcess>>>,
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
            terminals: AsyncMutex::new(HashMap::new()),
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

    /// `terminal/create` (architecture §12.2's neighbor concern -- ACP's
    /// own background-task capability): gated through the identical
    /// `PermissionEngine` and audited with the same `PermissionRequested`/
    /// `PermissionDecided`/`ToolCallRequested` events a native tool call
    /// gets, then handed to `ExecutionPlane::spawn_background` -- the same
    /// sandboxed command construction `execute()`'s `bash` arm uses,
    /// just non-blocking. `command`/`args` are joined into one
    /// POSIX-quoted shell word each (never string-concatenated
    /// unescaped) since `ExecutionPlane` only takes a shell command
    /// string today, matching the native `bash` tool's own shape.
    pub async fn handle_terminal_create(
        &self,
        req: acp::CreateTerminalRequest,
    ) -> Result<acp::CreateTerminalResponse> {
        let cwd = req.cwd.clone().unwrap_or_else(|| self.workspace_root.clone());
        let shell_command = shell_quote_command(&req.command, &req.args);

        let perm_request = PermissionRequest {
            session_id: self.child_session_id,
            tool_source: ToolSource::Acp { agent: self.agent_kind.clone() },
            action: PermissionAction::Exec { command: req.command.clone(), args: req.args.clone(), cwd },
            risk_tier: RiskTier::Execute,
        };
        let terminal_id = lh_event::EventId::now_v7().to_string();

        self.emit(
            Actor::System,
            EventPayload::PermissionRequested { call_id: terminal_id.clone(), request: perm_request.clone() },
        )
        .await?;
        let resolution = self.permission_engine.decide(&perm_request).await?;
        self.emit(
            Actor::System,
            EventPayload::PermissionDecided {
                call_id: terminal_id.clone(),
                decision: resolution.decision.clone(),
                decided_by: resolution.source,
            },
        )
        .await?;

        if let PermissionDecision::Deny | PermissionDecision::DenyAlways { .. } = &resolution.decision {
            anyhow::bail!("terminal/create denied by permission policy")
        }

        let call = ToolCall {
            call_id: terminal_id.clone(),
            tool_name: "terminal".to_string(),
            source: ToolSource::Acp { agent: self.agent_kind.clone() },
            args_summary: serde_json::json!({ "command": req.command, "args": req.args }),
            raw_args: serde_json::json!({ "command": shell_command }),
        };
        self.emit(Actor::Agent, EventPayload::ToolCallRequested { call: call.clone() }).await?;

        let process = self.execution_plane.spawn_background(&call, &resolution.decision).await?;
        self.terminals.lock().await.insert(terminal_id.clone(), process);

        Ok(acp::CreateTerminalResponse::new(terminal_id))
    }

    pub async fn handle_terminal_output(
        &self,
        req: acp::TerminalOutputRequest,
    ) -> Result<acp::TerminalOutputResponse> {
        let process = self.terminal_handle(&req.terminal_id).await?;
        let out = process.output().await;
        let mut resp = acp::TerminalOutputResponse::new(out.text, out.truncated);
        if let Some(status) = out.exit_status {
            resp = resp.exit_status(Some(translate_exit_status(status)));
        }
        Ok(resp)
    }

    pub async fn handle_wait_for_terminal_exit(
        &self,
        req: acp::WaitForTerminalExitRequest,
    ) -> Result<acp::WaitForTerminalExitResponse> {
        let process = self.terminal_handle(&req.terminal_id).await?;
        let status = process.wait_for_exit().await;
        Ok(acp::WaitForTerminalExitResponse::new(translate_exit_status(status)))
    }

    pub async fn handle_kill_terminal(&self, req: acp::KillTerminalRequest) -> Result<acp::KillTerminalResponse> {
        let process = self.terminal_handle(&req.terminal_id).await?;
        process.kill().await;
        Ok(acp::KillTerminalResponse::new())
    }

    /// "The Agent MUST release the terminal using `terminal/release` when
    /// it's no longer needed" -- the spec's own words for this method, and
    /// in practice the one call in the lifecycle guaranteed to happen, so
    /// it's where a final `ToolCallUpdated` gets recorded for the audit
    /// trail. Kills defensively first (a no-op if already exited) so a
    /// released-while-still-running process doesn't outlive the terminal
    /// that was tracking it, matching "release... terminates any running
    /// process" from the spec.
    pub async fn handle_terminal_release(
        &self,
        req: acp::ReleaseTerminalRequest,
    ) -> Result<acp::ReleaseTerminalResponse> {
        let terminal_id = req.terminal_id.0.to_string();
        if let Some(process) = self.terminals.lock().await.remove(&terminal_id) {
            process.kill().await;
            let out = process.output().await;
            let status = if out.exit_status.is_some() { ToolCallStatus::Completed } else { ToolCallStatus::Cancelled };
            self.emit(
                Actor::System,
                EventPayload::ToolCallUpdated {
                    call_id: terminal_id,
                    status,
                    output: Some(ContentBlock::text(out.text)),
                },
            )
            .await?;
        }
        Ok(acp::ReleaseTerminalResponse::new())
    }

    async fn terminal_handle(&self, terminal_id: &acp::TerminalId) -> Result<Arc<dyn BackgroundProcess>> {
        self.terminals
            .lock()
            .await
            .get(terminal_id.0.as_ref())
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown or already-released terminal id: {}", terminal_id.0))
    }
}

/// Wraps `command` and each of `args` in single quotes (escaping any
/// embedded single quote as `'\''`, the standard POSIX-safe way to turn an
/// arbitrary string into one shell word without a shell parsing it first)
/// and joins them with spaces. `ExecutionPlane` only takes a shell command
/// string today (matching the native `bash` tool); ACP's `command`/`args`
/// are execve-style (never shell-interpreted), so this reproduces that
/// exact semantics through a shell layer instead of letting agent- or
/// attacker-controlled argument text reach the shell parser unescaped.
fn shell_quote_command(command: &str, args: &[String]) -> String {
    let mut words = vec![shell_quote(command)];
    words.extend(args.iter().map(|a| shell_quote(a)));
    words.join(" ")
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn translate_exit_status(status: lh_execution::ExitStatus) -> acp::TerminalExitStatus {
    acp::TerminalExitStatus::new()
        .exit_code(status.code.map(|c| c as u32))
        .signal(status.signal)
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
        acp::SessionUpdate::Plan(plan) => Some(EventPayload::PlanUpdated {
            steps: translate_acp_plan(plan),
        }),
        _ => None,
    }
}

/// Maps ACP's `Plan.entries` (§Agent Plan) onto our own `PlanStep`s -- the
/// native loop's `plan_update` tool produces the identical `PlanUpdated`
/// shape, so a client renders a delegated agent's plan and a native
/// subagent's plan the same way. `priority` has no equivalent in our
/// schema and is dropped, same as any other ACP field we don't model
/// (e.g. `UsageUpdate.size`). `PlanEntryStatus` is `#[non_exhaustive]` in
/// the schema crate, so an unrecognized future variant maps to `Pending`
/// (never falsely claims completion) rather than failing to compile.
fn translate_acp_plan(plan: &acp::Plan) -> Vec<PlanStep> {
    plan.entries
        .iter()
        .map(|entry| PlanStep {
            description: entry.content.clone(),
            status: match entry.status {
                acp::PlanEntryStatus::Pending => PlanStepStatus::Pending,
                acp::PlanEntryStatus::InProgress => PlanStepStatus::InProgress,
                acp::PlanEntryStatus::Completed => PlanStepStatus::Completed,
                _ => PlanStepStatus::Pending,
            },
        })
        .collect()
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

    /// Previously the fallthrough `_ => None` silently dropped an incoming
    /// `session/update` -> `Plan` notification -- an agent's plan would just
    /// never appear anywhere. This pins the fix: `translate_acp_update` must
    /// produce a `PlanUpdated` event with every entry mapped, in order.
    #[test]
    fn translate_acp_update_maps_plan_to_plan_updated() {
        let plan = acp::Plan::new(vec![
            acp::PlanEntry::new("find the bug", acp::PlanEntryPriority::High, acp::PlanEntryStatus::InProgress),
            acp::PlanEntry::new("fix it", acp::PlanEntryPriority::Medium, acp::PlanEntryStatus::Pending),
            acp::PlanEntry::new("done", acp::PlanEntryPriority::Low, acp::PlanEntryStatus::Completed),
        ]);

        let payload = translate_acp_update(&acp::SessionUpdate::Plan(plan)).unwrap();
        let EventPayload::PlanUpdated { steps } = payload else {
            panic!("expected PlanUpdated, got {payload:?}");
        };
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0].description, "find the bug");
        assert_eq!(steps[0].status, PlanStepStatus::InProgress);
        assert_eq!(steps[1].status, PlanStepStatus::Pending);
        assert_eq!(steps[2].status, PlanStepStatus::Completed);
    }
}
