//! The native agent loop (architecture §11 phase 2, §9's future
//! generalization point). Calls a `ModelProvider`, gates every tool call
//! through a `PermissionEngine`, executes a minimal set of built-in tools,
//! and appends every step to a `SessionStore` -- which is also how the
//! daemon streams events out to clients (§3's "the event log is the sole
//! state-changing primitive" applies here too: this loop never talks to a
//! socket directly, only to the store).
//!
//! Sandboxing (Landlock/Seatbelt) lives behind the `ExecutionPlane` this
//! loop is handed at construction (`lh-execution`, architecture §8).
//!
//! Native subagents (architecture §9) reuse this exact same loop
//! recursively: `spawn_subagent` is a built-in tool gated through the
//! identical `PermissionEngine` path as every other tool, and
//! `run_subagent` just constructs another `NativeAgentLoop` for the child
//! session -- "same machinery," not a parallel mechanism.
//!
//! `NativeAgentLoop` implements `lh_orchestration::ChildRunner` directly
//! (Phase 6, §12.3): the trait's `run()` *is* `run_subagent`'s core logic,
//! just returning the freshly minted child `SessionId` alongside the
//! `ChildOutcome` instead of the pre-formatted tool-feedback string --
//! `run_subagent` itself is now a thin wrapper around it, kept because
//! `handle_one_tool_call` needs that formatted string, not a raw outcome.

mod tools;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use lh_event::{
    Actor, ChildKind, ChildOutcome, ChildSpec, ContentBlock, Event, EventPayload,
    PermissionDecision, PermissionRequest, SessionDriver, SessionId, ToolCall, ToolCallStatus,
    ToolSource, UsageConfidence, UsageDelta,
};
use tools::parse_plan_steps;
use lh_execution::{BackgroundProcess, ExecutionPlane};
use lh_model_provider::{ChatContent, ChatMessage, ChatRole, ModelProvider, ModelRequest, ToolSpec};
use lh_orchestration::{ChildRunner, TaskHandoff};
use lh_permission::{DefaultPermissionEngine, PermissionEngine, PermissionPrompter};
use lh_store::SessionStore;
use tokio::sync::Mutex as AsyncMutex;

/// Shared, per-connection registry of live background processes, keyed by
/// the id minted for them -- owned by whoever constructs `NativeAgentLoop`
/// instances across multiple turns (the daemon), not by the loop itself,
/// since a `session/prompt` call builds a fresh `NativeAgentLoop` every
/// turn but a background task started in one turn must still be pollable
/// in the next (matching Claude Code's own BashOutput UX).
pub type BackgroundProcessRegistry = Arc<AsyncMutex<HashMap<String, Arc<dyn BackgroundProcess>>>>;

pub use tools::builtin_tool_specs;

/// Structural cap on subagent nesting depth (a subagent spawning its own
/// subagent, etc.) -- mirrors the `max_delegation_depth` default already
/// established for ACP delegation (architecture §12.4), same failure
/// class (runaway recursive spawning). Enforced by not even advertising
/// `spawn_subagent` to the model once reached, not just by convention.
pub const MAX_SUBAGENT_DEPTH: usize = 3;

pub type Result<T> = std::result::Result<T, AgentError>;

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("store error: {0}")]
    Store(#[from] lh_store::StoreError),
    #[error("model provider error: {0}")]
    Provider(#[from] lh_model_provider::ProviderError),
    #[error("permission error: {0}")]
    Permission(#[from] lh_permission::PermissionError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOutcome {
    EndTurn,
    MaxTurnsExceeded,
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// The provider's own name (`ModelProviderConfig.name`, §13.2) -- used
    /// only to look up pricing (`lh-ledger`), not to select the provider
    /// itself (that's already fixed by which `ModelProvider` this loop was
    /// constructed with).
    pub provider_name: String,
    pub model: String,
    pub system_prompt: String,
    pub max_tokens: u32,
    /// Safety cap on model<->tool round-trips within a single turn
    /// (architecture's AutoGPT-loop lesson, applied at the smallest scale
    /// that already matters).
    pub max_iterations: usize,
    pub workspace_root: PathBuf,
    /// Restricts which built-in tools this loop advertises to the model --
    /// `None` means all of them. A subagent's allowlist (architecture §9)
    /// must be a subset of its parent's; since v1 has no per-session
    /// notion of "the parent's own allowlist" beyond this same field, the
    /// natural subset check is just: the child's allowlist can only ever
    /// name tools that exist at all, which `available_tool_specs` already
    /// enforces by filtering rather than trusting the list verbatim.
    pub tool_allowlist: Option<Vec<String>>,
    /// 0 for a root session; incremented by one for each level of
    /// `spawn_subagent` nesting (see `MAX_SUBAGENT_DEPTH`).
    pub subagent_depth: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            provider_name: "unset".to_string(),
            model: "unset".to_string(),
            system_prompt: "You are a careful, concise coding assistant. Use the available \
                tools when you need to read or change files or run commands."
                .to_string(),
            max_tokens: 4096,
            max_iterations: 12,
            workspace_root: PathBuf::from("."),
            tool_allowlist: None,
            subagent_depth: 0,
        }
    }
}

/// The tools this loop actually advertises to the model: `builtin_tool_specs()`
/// filtered by `config.tool_allowlist` (if set), with `spawn_subagent`
/// additionally stripped once `MAX_SUBAGENT_DEPTH` is reached -- the
/// structural half of the depth cap (the runtime check in `run_subagent`
/// is the other half, in case a caller ever bypasses this listing).
fn available_tool_specs(config: &AgentConfig) -> Vec<ToolSpec> {
    builtin_tool_specs()
        .into_iter()
        .filter(|spec| {
            config
                .tool_allowlist
                .as_ref()
                .is_none_or(|allowed| allowed.iter().any(|name| name == &spec.name))
        })
        .filter(|spec| spec.name != "spawn_subagent" || config.subagent_depth < MAX_SUBAGENT_DEPTH)
        .collect()
}

/// Parses a `spawn_subagent` tool call's raw JSON input into a
/// `lh_orchestration::TaskHandoff`. An inherent impl can't live on
/// `TaskHandoff` itself now that the type is defined in `lh-orchestration`
/// (Rust's orphan rules only allow inherent methods in the defining
/// crate), so this is a free function instead.
fn task_handoff_from_tool_input(input: &serde_json::Value) -> Option<TaskHandoff> {
    Some(TaskHandoff {
        role: input.get("role")?.as_str()?.to_string(),
        instructions: input.get("instructions")?.as_str()?.to_string(),
        tool_allowlist: input
            .get("tool_allowlist")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()),
        max_turns: input.get("max_turns").and_then(|v| v.as_u64()).map(|n| n as usize),
    })
}

pub struct NativeAgentLoop {
    store: Arc<dyn SessionStore>,
    model_provider: Arc<dyn ModelProvider>,
    permission_engine: Arc<dyn PermissionEngine>,
    execution_plane: Arc<dyn ExecutionPlane>,
    pricing: Arc<lh_ledger::PricingTable>,
    /// Held separately from `permission_engine` (a trait object that
    /// doesn't expose its internals) so `run_subagent` can build a fresh,
    /// *session-scoped-only* `PermissionEngine` for each child -- the
    /// mechanism behind "permission scope typically stricter than parent"
    /// (architecture §9): a child never even gets a reference to the
    /// project/global policy stores, so it structurally cannot read or
    /// write rules at those scopes, not just by runtime convention.
    prompter: Arc<dyn PermissionPrompter>,
    /// Cross-turn background-process state -- see `BackgroundProcessRegistry`.
    /// A subagent gets its own fresh, empty registry (`run_subagent_outcome`),
    /// never the parent's: background tasks don't leak across the same
    /// context-isolation boundary the child's permission engine already
    /// enforces.
    background_processes: BackgroundProcessRegistry,
    config: AgentConfig,
}

impl NativeAgentLoop {
    pub fn new(
        store: Arc<dyn SessionStore>,
        model_provider: Arc<dyn ModelProvider>,
        permission_engine: Arc<dyn PermissionEngine>,
        execution_plane: Arc<dyn ExecutionPlane>,
        pricing: Arc<lh_ledger::PricingTable>,
        prompter: Arc<dyn PermissionPrompter>,
        background_processes: BackgroundProcessRegistry,
        config: AgentConfig,
    ) -> Self {
        Self {
            store,
            model_provider,
            permission_engine,
            execution_plane,
            pricing,
            prompter,
            background_processes,
            config,
        }
    }

    async fn emit(&self, session_id: SessionId, actor: Actor, payload: EventPayload) -> Result<()> {
        self.store
            .append(Event::new(session_id, None, actor, payload))
            .await?;
        Ok(())
    }

    /// Runs one full turn: the user's message, as many model<->tool
    /// round-trips as it takes (bounded by `max_iterations`), to either a
    /// plain end-of-turn response or the iteration cap.
    pub async fn run_turn(&self, session_id: SessionId, user_text: &str) -> Result<TurnOutcome> {
        self.emit(
            session_id,
            Actor::User,
            EventPayload::UserMessage {
                content: vec![ContentBlock::text(user_text)],
            },
        )
        .await?;

        let mut messages = vec![ChatMessage::user_text(user_text)];
        let tools = available_tool_specs(&self.config);
        let mut usage_acc = UsageAccumulator::default();

        for _ in 0..self.config.max_iterations {
            let request = ModelRequest {
                model: self.config.model.clone(),
                system: Some(self.config.system_prompt.clone()),
                messages: messages.clone(),
                tools: tools.clone(),
                max_tokens: self.config.max_tokens,
            };

            let started = Instant::now();
            let response = self.model_provider.complete(request).await?;
            usage_acc.add(response.usage, started.elapsed().as_millis() as u64);

            let text = response.text();
            if !text.is_empty() {
                self.emit(
                    session_id,
                    Actor::Agent,
                    EventPayload::AgentMessageChunk {
                        content: ContentBlock::text(text.clone()),
                    },
                )
                .await?;
            }

            let tool_uses: Vec<(String, String, serde_json::Value)> = response
                .tool_uses()
                .into_iter()
                .map(|(id, name, input)| (id.to_string(), name.to_string(), input.clone()))
                .collect();

            if tool_uses.is_empty() {
                self.emit(
                    session_id,
                    Actor::System,
                    EventPayload::UsageReported {
                        usage: usage_acc.finish(&self.config.provider_name, &self.config.model, &self.pricing),
                    },
                )
                .await?;
                return Ok(TurnOutcome::EndTurn);
            }

            let mut assistant_content = Vec::new();
            if !text.is_empty() {
                assistant_content.push(ChatContent::Text(text));
            }
            for (id, name, input) in &tool_uses {
                assistant_content.push(ChatContent::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                });
            }
            messages.push(ChatMessage {
                role: ChatRole::Assistant,
                content: assistant_content,
            });

            let mut tool_result_content = Vec::new();
            for (call_id, tool_name, input) in tool_uses {
                let (output, is_error) = self
                    .handle_one_tool_call(session_id, &call_id, &tool_name, &input)
                    .await?;
                tool_result_content.push(ChatContent::ToolResult {
                    tool_use_id: call_id,
                    content: output,
                    is_error,
                });
            }
            messages.push(ChatMessage {
                role: ChatRole::User,
                content: tool_result_content,
            });
        }

        self.emit(
            session_id,
            Actor::System,
            EventPayload::Error {
                message: format!(
                    "turn exceeded max_iterations ({}) without reaching an end turn",
                    self.config.max_iterations
                ),
                recoverable: true,
            },
        )
        .await?;
        Ok(TurnOutcome::MaxTurnsExceeded)
    }

    /// Requests permission for and (if allowed) executes one tool call.
    /// Returns `(output_text, is_error)` for feeding back to the model.
    async fn handle_one_tool_call(
        &self,
        session_id: SessionId,
        call_id: &str,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> Result<(String, bool)> {
        let source = ToolSource::Native {
            tool_id: tool_name.to_string(),
        };
        let call = ToolCall {
            call_id: call_id.to_string(),
            tool_name: tool_name.to_string(),
            source: source.clone(),
            args_summary: input.clone(),
            raw_args: input.clone(),
        };
        self.emit(
            session_id,
            Actor::Agent,
            EventPayload::ToolCallRequested { call: call.clone() },
        )
        .await?;

        // bash_output/bash_wait/bash_kill act on a background process that a
        // prior, permission-gated bash_background call already started -- no
        // new risk to ask about, so (mirroring the identical design decision
        // for ACP's terminal/output|wait_for_exit|kill in lh-acp) they skip
        // the live ask entirely, going straight to their own ToolCallUpdated
        // for audit visibility instead of the usual PermissionRequested/
        // PermissionDecided pair.
        if tool_name == "bash_output" || tool_name == "bash_wait" || tool_name == "bash_kill" {
            let (status, output, is_error) = self.query_background_process(tool_name, input).await;
            self.emit(
                session_id,
                Actor::System,
                EventPayload::ToolCallUpdated {
                    call_id: call_id.to_string(),
                    status,
                    output: Some(ContentBlock::text(output.clone())),
                },
            )
            .await?;
            return Ok((output, is_error));
        }

        // plan_update never touches the system (no file, process, or
        // network) -- it's pure bookkeeping, so it also skips the live ask,
        // going straight to a PlanUpdated event plus the usual
        // ToolCallUpdated for audit visibility.
        if tool_name == "plan_update" {
            let (status, output, is_error) = match parse_plan_steps(input) {
                Some(steps) => {
                    let step_count = steps.len();
                    self.emit(session_id, Actor::Agent, EventPayload::PlanUpdated { steps }).await?;
                    (ToolCallStatus::Completed, format!("plan updated ({step_count} step(s))"), false)
                }
                None => (
                    ToolCallStatus::Failed,
                    "plan_update requires 'steps': [{description, status}] with status one of \
                     pending/in_progress/completed"
                        .to_string(),
                    true,
                ),
            };
            self.emit(
                session_id,
                Actor::System,
                EventPayload::ToolCallUpdated {
                    call_id: call_id.to_string(),
                    status,
                    output: Some(ContentBlock::text(output.clone())),
                },
            )
            .await?;
            return Ok((output, is_error));
        }

        let request = PermissionRequest {
            session_id,
            tool_source: source,
            action: tools::permission_action_for(tool_name, input),
            risk_tier: tools::risk_tier_for(tool_name),
        };
        self.emit(
            session_id,
            Actor::System,
            EventPayload::PermissionRequested {
                call_id: call_id.to_string(),
                request: request.clone(),
            },
        )
        .await?;

        let resolution = self.permission_engine.decide(&request).await?;
        let decision = resolution.decision;
        self.emit(
            session_id,
            Actor::System,
            EventPayload::PermissionDecided {
                call_id: call_id.to_string(),
                decision: decision.clone(),
                decided_by: resolution.source,
            },
        )
        .await?;

        let (status, output, is_error) = match &decision {
            PermissionDecision::Allow | PermissionDecision::AllowAlways { .. } => {
                if tool_name == "spawn_subagent" {
                    match task_handoff_from_tool_input(input) {
                        Some(task) => match self.run_subagent(session_id, task).await {
                            Ok(summary) => (ToolCallStatus::Completed, summary, false),
                            Err(e) => (ToolCallStatus::Failed, e.to_string(), true),
                        },
                        None => (
                            ToolCallStatus::Failed,
                            "spawn_subagent requires 'role' and 'instructions'".to_string(),
                            true,
                        ),
                    }
                } else if tool_name == "bash_background" {
                    match self.execution_plane.spawn_background(&call, &decision).await {
                        Ok(process) => {
                            let bash_id = lh_event::EventId::now_v7().to_string();
                            self.background_processes.lock().await.insert(bash_id.clone(), process);
                            (ToolCallStatus::Completed, bash_id, false)
                        }
                        Err(e) => (ToolCallStatus::Failed, e.to_string(), true),
                    }
                } else {
                    match self.execution_plane.execute(&call, &decision).await {
                        Ok(out) => (ToolCallStatus::Completed, out, false),
                        Err(e) => (ToolCallStatus::Failed, e.to_string(), true),
                    }
                }
            }
            PermissionDecision::Deny | PermissionDecision::DenyAlways { .. } => {
                (ToolCallStatus::Cancelled, "denied by user".to_string(), true)
            }
        };

        self.emit(
            session_id,
            Actor::System,
            EventPayload::ToolCallUpdated {
                call_id: call_id.to_string(),
                status,
                output: Some(ContentBlock::text(output.clone())),
            },
        )
        .await?;

        Ok((output, is_error))
    }

    /// `bash_output`/`bash_wait`/`bash_kill`'s actual logic -- looked up by
    /// the id `bash_background` minted and stored in
    /// `self.background_processes`.
    async fn query_background_process(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> (ToolCallStatus, String, bool) {
        let Some(bash_id) = input.get("bash_id").and_then(|v| v.as_str()) else {
            return (ToolCallStatus::Failed, format!("{tool_name} requires 'bash_id'"), true);
        };
        let Some(process) = self.background_processes.lock().await.get(bash_id).cloned() else {
            return (ToolCallStatus::Failed, format!("unknown bash_id: {bash_id}"), true);
        };
        match tool_name {
            "bash_output" => {
                let out = process.output().await;
                let mut text = out.text;
                match out.exit_status {
                    Some(status) => {
                        text.push_str(&format!("\n[exited: code={:?} signal={:?}]", status.code, status.signal));
                    }
                    None => text.push_str("\n[still running]"),
                }
                (ToolCallStatus::Completed, text, false)
            }
            "bash_wait" => {
                let status = process.wait_for_exit().await;
                (
                    ToolCallStatus::Completed,
                    format!("[exited: code={:?} signal={:?}]", status.code, status.signal),
                    false,
                )
            }
            "bash_kill" => {
                process.kill().await;
                (ToolCallStatus::Completed, format!("killed {bash_id}"), false)
            }
            _ => unreachable!("query_background_process is only called for bash_output/bash_wait/bash_kill"),
        }
    }

    /// Thin wrapper around `run_subagent_outcome` (the `ChildRunner::run`
    /// logic) for `handle_one_tool_call`'s spawn_subagent branch, which
    /// needs a single formatted string to feed back to the model as the
    /// tool result -- not a raw `SessionId`/`ChildOutcome` pair.
    async fn run_subagent(&self, parent_session_id: SessionId, task: TaskHandoff) -> Result<String> {
        let (_, outcome) = self.run_subagent_outcome(parent_session_id, task).await?;
        Ok(match outcome {
            ChildOutcome::Success { summary } => summary,
            ChildOutcome::Failed { message } => format!("subagent failed: {message}"),
            ChildOutcome::Cancelled => "subagent was cancelled".to_string(),
        })
    }

    /// Runs one native subagent to completion as a child of `parent_session_id`
    /// (architecture §9) -- the exact same session-tree, permission, and
    /// cost machinery a delegated ACP agent uses (`lh-acp::delegate::
    /// run_delegation`), just driven by another `NativeAgentLoop` in-process
    /// instead of a subprocess round-trip. This is `ChildRunner::run`'s
    /// actual body (see the `impl ChildRunner for NativeAgentLoop` below);
    /// it's a plain method rather than living directly in the trait impl
    /// only so `run_subagent` above can call it without going through a
    /// trait object.
    async fn run_subagent_outcome(
        &self,
        parent_session_id: SessionId,
        task: TaskHandoff,
    ) -> Result<(SessionId, ChildOutcome)> {
        if self.config.subagent_depth >= MAX_SUBAGENT_DEPTH {
            // Defense in depth: `available_tool_specs` already keeps this
            // unreachable by not advertising `spawn_subagent` at max depth,
            // but a caller that bypasses tool-spec filtering must still be
            // refused here, structurally, not just by convention. No child
            // session is ever created on this path, so the returned id is
            // a throwaway -- callers must key off `ChildOutcome::Failed`,
            // not this id, to detect a refusal.
            return Ok((
                SessionId::now_v7(),
                ChildOutcome::Failed {
                    message: format!(
                        "refused: subagent nesting depth {} would exceed the cap ({MAX_SUBAGENT_DEPTH})",
                        self.config.subagent_depth + 1
                    ),
                },
            ));
        }

        let child_session_id = SessionId::now_v7();

        // ChildSessionSpawned lands on the parent *before* the child's own
        // first event -- the daemon's event forwarder discovers session-tree
        // membership live by watching for this event (lh-daemon/src/main.rs),
        // so a client attached to the parent needs to see it first.
        self.emit(
            parent_session_id,
            Actor::System,
            EventPayload::ChildSessionSpawned {
                child: child_session_id,
                kind: ChildKind::NativeSubagent { role: task.role.clone() },
                spec: ChildSpec { task_summary: task.instructions.clone() },
            },
        )
        .await?;
        self.store
            .append(Event::new(
                child_session_id,
                Some(parent_session_id),
                Actor::System,
                EventPayload::SessionDriverSet { driver: SessionDriver::Native },
            ))
            .await?;

        // Session-scoped-only permission engine: a fresh `DefaultPermissionEngine`
        // built directly from the prompter, never handed the project/global
        // `TomlPolicyStore`s the parent's own engine has -- the child
        // structurally cannot read or persist rules at those broader scopes
        // (architecture §9's "permission scope typically stricter than
        // parent," enforced by construction rather than a runtime check).
        let child_permission_engine: Arc<dyn PermissionEngine> =
            Arc::new(DefaultPermissionEngine::new(self.prompter.clone()));

        let child_config = AgentConfig {
            provider_name: self.config.provider_name.clone(),
            model: self.config.model.clone(),
            system_prompt: format!(
                "You are a focused subagent spawned for one task: {}. Do the work, then give a \
                 concise final answer summarizing what you found or did -- your answer is the \
                 only thing the parent agent will see.",
                task.role
            ),
            max_tokens: self.config.max_tokens,
            max_iterations: task.max_turns.unwrap_or(self.config.max_iterations),
            workspace_root: self.config.workspace_root.clone(),
            tool_allowlist: task.tool_allowlist,
            subagent_depth: self.config.subagent_depth + 1,
        };

        let child_loop = NativeAgentLoop::new(
            self.store.clone(),
            self.model_provider.clone(),
            child_permission_engine,
            self.execution_plane.clone(),
            self.pricing.clone(),
            self.prompter.clone(),
            // Fresh, empty registry -- a subagent's background tasks don't
            // leak across the same context-isolation boundary its
            // session-scoped-only permission engine already enforces.
            Arc::new(AsyncMutex::new(HashMap::new())),
            child_config,
        );

        // Boxed: run_turn -> handle_one_tool_call -> run_subagent -> run_turn
        // is a real recursion cycle (a subagent's own turn can itself spawn
        // a subagent, up to MAX_SUBAGENT_DEPTH) -- boxing this one edge
        // gives the compiler a finite state-machine size to work with.
        let turn_result = Box::pin(child_loop.run_turn(child_session_id, &task.instructions)).await;

        // The subagent's answer is whatever text it produced, derived from
        // its own event log -- not threaded out of `run_turn` some other
        // way, matching how the cost ledger is always a derived read-model
        // rather than carried state.
        let summary = self.summarize_child_messages(child_session_id).await?;

        let outcome = match &turn_result {
            Ok(TurnOutcome::EndTurn) => ChildOutcome::Success { summary: summary.clone() },
            Ok(TurnOutcome::MaxTurnsExceeded) => ChildOutcome::Failed {
                message: format!("subagent exceeded its turn limit; partial answer: {summary}"),
            },
            Err(e) => ChildOutcome::Failed { message: e.to_string() },
        };
        self.emit(
            parent_session_id,
            Actor::System,
            EventPayload::ChildSessionEnded { child: child_session_id, outcome: outcome.clone() },
        )
        .await?;

        Ok((child_session_id, outcome))
    }

    async fn summarize_child_messages(&self, child_session_id: SessionId) -> Result<String> {
        let events = self.store.read_from(child_session_id, 0).await?;
        let mut text = String::new();
        for event in events {
            if let EventPayload::AgentMessageChunk { content: ContentBlock::Text { text: chunk } } =
                event.payload
            {
                text.push_str(&chunk);
            }
        }
        Ok(if text.is_empty() { "(subagent produced no text response)".to_string() } else { text })
    }
}

/// Architecture §12.3: the driver-neutral entry point a future
/// `lh-orchestration-mcp` (or any other generic caller holding an
/// `Arc<dyn ChildRunner>`) would use to spawn a native subagent, without
/// needing to know it's talking to a `NativeAgentLoop` specifically.
#[async_trait]
impl ChildRunner for NativeAgentLoop {
    async fn run(
        &self,
        parent_session_id: SessionId,
        task: TaskHandoff,
    ) -> lh_orchestration::Result<(SessionId, ChildOutcome)> {
        self.run_subagent_outcome(parent_session_id, task).await.map_err(Into::into)
    }
}

#[derive(Default)]
struct UsageAccumulator {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    wall_ms: u64,
    any_unknown: bool,
}

impl UsageAccumulator {
    fn add(&mut self, usage: lh_model_provider::ModelUsage, wall_ms: u64) {
        self.wall_ms += wall_ms;
        match (self.input_tokens, usage.input_tokens) {
            (Some(a), Some(b)) => self.input_tokens = Some(a + b),
            (None, Some(b)) => self.input_tokens = Some(b),
            (Some(_), None) => self.any_unknown = true,
            (None, None) => self.any_unknown = true,
        }
        match (self.output_tokens, usage.output_tokens) {
            (Some(a), Some(b)) => self.output_tokens = Some(a + b),
            (None, Some(b)) => self.output_tokens = Some(b),
            (Some(_), None) => self.any_unknown = true,
            (None, None) => self.any_unknown = true,
        }
    }

    fn finish(self, provider_name: &str, model: &str, pricing: &lh_ledger::PricingTable) -> UsageDelta {
        let token_confidence = if self.any_unknown {
            UsageConfidence::Unknown
        } else {
            UsageConfidence::Exact
        };

        // Pricing (architecture §7/§13.3): a known (provider, model) with
        // known token counts gets a real dollar figure; anything else --
        // an unpriced/self-hosted model, or a provider that didn't report
        // usage -- stays an honest `Unknown` rather than a guess.
        let (cost_usd, price_confidence) =
            lh_ledger::price_usage(pricing, provider_name, model, self.input_tokens, self.output_tokens);

        UsageDelta {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cost_usd,
            wall_ms: self.wall_ms,
            confidence: worse(token_confidence, price_confidence),
        }
    }
}

fn worse(a: UsageConfidence, b: UsageConfidence) -> UsageConfidence {
    fn rank(c: UsageConfidence) -> u8 {
        match c {
            UsageConfidence::Exact => 0,
            UsageConfidence::Estimated => 1,
            UsageConfidence::Unknown => 2,
        }
    }
    if rank(a) >= rank(b) {
        a
    } else {
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use lh_model_provider::{ModelProviderCapabilities, ModelResponse, ModelUsage, StopReason};
    use lh_store::SqliteSessionStore;
    use std::collections::VecDeque;
    use tokio::sync::Mutex as AsyncMutex;

    struct ScriptedModelProvider {
        responses: AsyncMutex<VecDeque<ModelResponse>>,
    }

    impl ScriptedModelProvider {
        fn new(responses: Vec<ModelResponse>) -> Self {
            Self {
                responses: AsyncMutex::new(responses.into_iter().collect()),
            }
        }
    }

    #[async_trait]
    impl ModelProvider for ScriptedModelProvider {
        async fn complete(&self, _req: ModelRequest) -> lh_model_provider::Result<ModelResponse> {
            self.responses
                .lock()
                .await
                .pop_front()
                .ok_or_else(|| lh_model_provider::ProviderError::Config("script exhausted".into()))
        }

        fn describe(&self) -> ModelProviderCapabilities {
            ModelProviderCapabilities {
                tool_calling: true,
                streaming: false,
                reports_usage: true,
            }
        }
    }

    struct FixedDecisionEngine(PermissionDecision);

    #[async_trait]
    impl PermissionEngine for FixedDecisionEngine {
        async fn decide(&self, _req: &PermissionRequest) -> lh_permission::Result<lh_permission::PermissionResolution> {
            Ok(lh_permission::PermissionResolution {
                decision: self.0.clone(),
                source: lh_event::DecisionSource::User,
            })
        }
    }

    /// A `NativeAgentLoop` always needs a prompter now (it's what
    /// `run_subagent` uses to build a child's own session-scoped
    /// permission engine) even in tests that never spawn a subagent --
    /// this fixed one mirrors `FixedDecisionEngine`.
    struct FixedPrompter(PermissionDecision);

    #[async_trait]
    impl PermissionPrompter for FixedPrompter {
        async fn ask(&self, _req: &PermissionRequest) -> lh_permission::Result<PermissionDecision> {
            Ok(self.0.clone())
        }
    }

    fn text_response(text: &str) -> ModelResponse {
        ModelResponse {
            content: vec![ChatContent::Text(text.to_string())],
            stop_reason: StopReason::EndTurn,
            usage: ModelUsage {
                input_tokens: Some(10),
                output_tokens: Some(5),
            },
        }
    }

    fn tool_use_response(call_id: &str, tool: &str, input: serde_json::Value) -> ModelResponse {
        ModelResponse {
            content: vec![ChatContent::ToolUse {
                id: call_id.to_string(),
                name: tool.to_string(),
                input,
            }],
            stop_reason: StopReason::ToolUse,
            usage: ModelUsage {
                input_tokens: Some(20),
                output_tokens: Some(8),
            },
        }
    }

    async fn local_plane(workspace_root: &std::path::Path) -> Arc<dyn ExecutionPlane> {
        Arc::new(
            lh_execution::LocalExecutionPlane::new(workspace_root.to_path_buf())
                .await
                .unwrap(),
        )
    }

    #[tokio::test]
    async fn a_plain_text_turn_ends_without_any_tool_calls() {
        let workspace = tempfile::tempdir().unwrap();
        let store = Arc::new(SqliteSessionStore::open_in_memory().unwrap());
        let provider = Arc::new(ScriptedModelProvider::new(vec![text_response("hi there")]));
        let engine = Arc::new(FixedDecisionEngine(PermissionDecision::Allow));
        let prompter = Arc::new(FixedPrompter(PermissionDecision::Allow));
        let session_id = SessionId::now_v7();

        let agent = NativeAgentLoop::new(
            store.clone(),
            provider,
            engine,
            local_plane(workspace.path()).await,
            Arc::new(lh_ledger::PricingTable::new()),
            prompter,
            Arc::new(AsyncMutex::new(HashMap::new())),
            AgentConfig {
                model: "test-model".to_string(),
                workspace_root: workspace.path().to_path_buf(),
                ..Default::default()
            },
        );

        let outcome = agent.run_turn(session_id, "say hi").await.unwrap();
        assert_eq!(outcome, TurnOutcome::EndTurn);

        let events = store.read_from(session_id, 0).await.unwrap();
        let kinds: Vec<&str> = events.iter().map(payload_kind).collect();
        assert_eq!(kinds, vec!["UserMessage", "AgentMessageChunk", "UsageReported"]);
    }

    #[tokio::test]
    async fn an_allowed_tool_call_executes_and_feeds_back_into_the_conversation() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("hello.txt"), "hello from disk").unwrap();

        let store = Arc::new(SqliteSessionStore::open_in_memory().unwrap());
        let provider = Arc::new(ScriptedModelProvider::new(vec![
            tool_use_response("call_1", "read_file", serde_json::json!({"path": "hello.txt"})),
            text_response("the file says hello from disk"),
        ]));
        let engine = Arc::new(FixedDecisionEngine(PermissionDecision::Allow));
        let prompter = Arc::new(FixedPrompter(PermissionDecision::Allow));
        let session_id = SessionId::now_v7();

        let agent = NativeAgentLoop::new(
            store.clone(),
            provider,
            engine,
            local_plane(workspace.path()).await,
            Arc::new(lh_ledger::PricingTable::new()),
            prompter,
            Arc::new(AsyncMutex::new(HashMap::new())),
            AgentConfig {
                model: "test-model".to_string(),
                workspace_root: workspace.path().to_path_buf(),
                ..Default::default()
            },
        );

        let outcome = agent.run_turn(session_id, "what does hello.txt say?").await.unwrap();
        assert_eq!(outcome, TurnOutcome::EndTurn);

        let events = store.read_from(session_id, 0).await.unwrap();
        let kinds: Vec<&str> = events.iter().map(payload_kind).collect();
        assert_eq!(
            kinds,
            vec![
                "UserMessage",
                "ToolCallRequested",
                "PermissionRequested",
                "PermissionDecided",
                "ToolCallUpdated",
                "AgentMessageChunk",
                "UsageReported",
            ]
        );

        let EventPayload::ToolCallUpdated { status, output, .. } = &events[4].payload else {
            panic!("expected ToolCallUpdated");
        };
        assert_eq!(*status, ToolCallStatus::Completed);
        let ContentBlock::Text { text } = output.as_ref().unwrap() else {
            panic!("expected text output");
        };
        assert!(text.contains("hello from disk"));
    }

    #[tokio::test]
    async fn a_denied_tool_call_never_executes() {
        let workspace = tempfile::tempdir().unwrap();
        let target = workspace.path().join("should-not-exist.txt");

        let store = Arc::new(SqliteSessionStore::open_in_memory().unwrap());
        let provider = Arc::new(ScriptedModelProvider::new(vec![
            tool_use_response(
                "call_1",
                "write_file",
                serde_json::json!({"path": "should-not-exist.txt", "content": "nope"}),
            ),
            text_response("okay, I won't write that file"),
        ]));
        let engine = Arc::new(FixedDecisionEngine(PermissionDecision::Deny));
        let prompter = Arc::new(FixedPrompter(PermissionDecision::Deny));
        let session_id = SessionId::now_v7();

        let agent = NativeAgentLoop::new(
            store.clone(),
            provider,
            engine,
            local_plane(workspace.path()).await,
            Arc::new(lh_ledger::PricingTable::new()),
            prompter,
            Arc::new(AsyncMutex::new(HashMap::new())),
            AgentConfig {
                model: "test-model".to_string(),
                workspace_root: workspace.path().to_path_buf(),
                ..Default::default()
            },
        );

        agent.run_turn(session_id, "write a file").await.unwrap();

        assert!(!target.exists());

        let events = store.read_from(session_id, 0).await.unwrap();
        let update = events
            .iter()
            .find(|e| matches!(e.payload, EventPayload::ToolCallUpdated { .. }))
            .unwrap();
        let EventPayload::ToolCallUpdated { status, .. } = &update.payload else {
            unreachable!()
        };
        assert_eq!(*status, ToolCallStatus::Cancelled);
    }

    #[test]
    fn available_tool_specs_respects_the_allowlist() {
        let config = AgentConfig {
            tool_allowlist: Some(vec!["read_file".to_string()]),
            ..Default::default()
        };
        let names: Vec<String> = available_tool_specs(&config).into_iter().map(|t| t.name).collect();
        assert_eq!(names, vec!["read_file".to_string()]);
    }

    #[test]
    fn available_tool_specs_hides_spawn_subagent_past_max_depth() {
        let mut config = AgentConfig { subagent_depth: MAX_SUBAGENT_DEPTH - 1, ..Default::default() };
        let names: Vec<String> = available_tool_specs(&config).into_iter().map(|t| t.name).collect();
        assert!(
            names.iter().any(|n| n == "spawn_subagent"),
            "one level below the cap should still allow spawning"
        );

        config.subagent_depth = MAX_SUBAGENT_DEPTH;
        let names: Vec<String> = available_tool_specs(&config).into_iter().map(|t| t.name).collect();
        assert!(
            !names.iter().any(|n| n == "spawn_subagent"),
            "at the cap, spawn_subagent must not be offered"
        );
    }

    #[tokio::test]
    async fn run_subagent_refuses_at_max_depth_even_if_called_directly() {
        // Defense in depth: available_tool_specs already keeps the model
        // from calling this, but run_subagent itself must refuse too, in
        // case that structural gate is ever bypassed.
        let workspace = tempfile::tempdir().unwrap();
        let store = Arc::new(SqliteSessionStore::open_in_memory().unwrap());
        let provider = Arc::new(ScriptedModelProvider::new(vec![]));
        let engine = Arc::new(FixedDecisionEngine(PermissionDecision::Allow));
        let prompter = Arc::new(FixedPrompter(PermissionDecision::Allow));
        let session_id = SessionId::now_v7();

        let agent = NativeAgentLoop::new(
            store.clone(),
            provider,
            engine,
            local_plane(workspace.path()).await,
            Arc::new(lh_ledger::PricingTable::new()),
            prompter,
            Arc::new(AsyncMutex::new(HashMap::new())),
            AgentConfig {
                model: "test-model".to_string(),
                workspace_root: workspace.path().to_path_buf(),
                subagent_depth: MAX_SUBAGENT_DEPTH,
                ..Default::default()
            },
        );

        let result = agent
            .run_subagent(
                session_id,
                TaskHandoff {
                    role: "nested".to_string(),
                    instructions: "should be refused".to_string(),
                    tool_allowlist: None,
                    max_turns: None,
                },
            )
            .await
            .unwrap();
        assert!(result.contains("refused"), "expected a refusal message, got: {result}");

        // Refused before ever touching the store's session tree for a child.
        let events = store.read_from(session_id, 0).await.unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn child_runner_trait_impl_returns_the_childs_session_id_and_outcome() {
        // Proves the ChildRunner impl is genuinely wired, not just defined:
        // a generic caller holding only `&dyn ChildRunner` (e.g. a future
        // lh-orchestration-mcp) gets the exact same session-tree behavior
        // the spawn_subagent tool's own direct call to `run_subagent` does.
        let workspace = tempfile::tempdir().unwrap();
        let store = Arc::new(SqliteSessionStore::open_in_memory().unwrap());
        let provider = Arc::new(ScriptedModelProvider::new(vec![text_response("done")]));
        let engine = Arc::new(FixedDecisionEngine(PermissionDecision::Allow));
        let prompter = Arc::new(FixedPrompter(PermissionDecision::Allow));
        let parent_session_id = SessionId::now_v7();

        let agent = NativeAgentLoop::new(
            store.clone(),
            provider,
            engine,
            local_plane(workspace.path()).await,
            Arc::new(lh_ledger::PricingTable::new()),
            prompter,
            Arc::new(AsyncMutex::new(HashMap::new())),
            AgentConfig {
                model: "test-model".to_string(),
                workspace_root: workspace.path().to_path_buf(),
                ..Default::default()
            },
        );

        let runner: &dyn ChildRunner = &agent;
        let (child_session_id, outcome) = runner
            .run(
                parent_session_id,
                TaskHandoff {
                    role: "worker".to_string(),
                    instructions: "do the thing".to_string(),
                    tool_allowlist: None,
                    max_turns: None,
                },
            )
            .await
            .unwrap();

        assert!(matches!(outcome, ChildOutcome::Success { summary } if summary == "done"));

        let child_events = store.read_from(child_session_id, 0).await.unwrap();
        assert!(!child_events.is_empty(), "the child session the trait returned must be real, not a throwaway id");
        let parent_events = store.read_from(parent_session_id, 0).await.unwrap();
        let kinds: Vec<&str> = parent_events.iter().map(payload_kind).collect();
        assert_eq!(kinds, vec!["ChildSessionSpawned", "ChildSessionEnded"]);
    }

    #[tokio::test]
    async fn a_subagent_is_spawned_and_its_answer_feeds_back_to_the_parent() {
        let workspace = tempfile::tempdir().unwrap();
        let store = Arc::new(SqliteSessionStore::open_in_memory().unwrap());
        let provider = Arc::new(ScriptedModelProvider::new(vec![
            tool_use_response(
                "call_1",
                "spawn_subagent",
                serde_json::json!({"role": "researcher", "instructions": "find the answer"}),
            ),
            text_response("42"),
            text_response("The answer is 42."),
        ]));
        let engine = Arc::new(FixedDecisionEngine(PermissionDecision::Allow));
        let prompter = Arc::new(FixedPrompter(PermissionDecision::Allow));
        let session_id = SessionId::now_v7();

        let agent = NativeAgentLoop::new(
            store.clone(),
            provider,
            engine,
            local_plane(workspace.path()).await,
            Arc::new(lh_ledger::PricingTable::new()),
            prompter,
            Arc::new(AsyncMutex::new(HashMap::new())),
            AgentConfig {
                model: "test-model".to_string(),
                workspace_root: workspace.path().to_path_buf(),
                ..Default::default()
            },
        );

        let outcome = agent.run_turn(session_id, "what's the answer?").await.unwrap();
        assert_eq!(outcome, TurnOutcome::EndTurn);

        let parent_events = store.read_from(session_id, 0).await.unwrap();
        let parent_kinds: Vec<&str> = parent_events.iter().map(payload_kind).collect();
        assert_eq!(
            parent_kinds,
            vec![
                "UserMessage",
                "ToolCallRequested",
                "PermissionRequested",
                "PermissionDecided",
                "ChildSessionSpawned",
                "ChildSessionEnded",
                "ToolCallUpdated",
                "AgentMessageChunk",
                "UsageReported",
            ]
        );

        let EventPayload::ChildSessionSpawned { child, kind, .. } = &parent_events[4].payload else {
            panic!("expected ChildSessionSpawned");
        };
        assert!(matches!(kind, ChildKind::NativeSubagent { role } if role == "researcher"));
        let child_session_id = *child;

        let EventPayload::ChildSessionEnded { outcome, .. } = &parent_events[5].payload else {
            panic!("expected ChildSessionEnded");
        };
        assert!(matches!(outcome, ChildOutcome::Success { summary } if summary == "42"));

        let EventPayload::ToolCallUpdated { status, output, .. } = &parent_events[6].payload else {
            panic!("expected ToolCallUpdated");
        };
        assert_eq!(*status, ToolCallStatus::Completed);
        let ContentBlock::Text { text } = output.as_ref().unwrap() else {
            panic!("expected text output");
        };
        assert_eq!(text, "42", "the subagent's answer must be what feeds back to the parent's model");

        // The child got its own fresh session -- never the parent's transcript.
        let child_events = store.read_from(child_session_id, 0).await.unwrap();
        let child_kinds: Vec<&str> = child_events.iter().map(payload_kind).collect();
        assert_eq!(child_kinds, vec!["SessionDriverSet", "UserMessage", "AgentMessageChunk", "UsageReported"]);
        let EventPayload::UserMessage { content } = &child_events[1].payload else {
            panic!("expected UserMessage");
        };
        let ContentBlock::Text { text } = &content[0] else { panic!("expected text") };
        assert_eq!(text, "find the answer", "child sees only its own instructions, not the parent's prompt");

        // Visible in the session tree, correctly attributed to the child.
        let tree = store.session_tree(session_id).await.unwrap();
        assert_eq!(tree.children.len(), 1);
        assert_eq!(tree.children[0].session_id, child_session_id);
        assert!(matches!(tree.children[0].driver, Some(SessionDriver::Native)));
    }

    #[tokio::test]
    async fn bash_background_start_poll_and_kill_round_trip() {
        let workspace = tempfile::tempdir().unwrap();
        let store = Arc::new(SqliteSessionStore::open_in_memory().unwrap());
        let provider = Arc::new(ScriptedModelProvider::new(vec![]));
        let engine = Arc::new(FixedDecisionEngine(PermissionDecision::Allow));
        let prompter = Arc::new(FixedPrompter(PermissionDecision::Allow));
        let session_id = SessionId::now_v7();
        let registry: BackgroundProcessRegistry = Arc::new(AsyncMutex::new(HashMap::new()));

        let agent = NativeAgentLoop::new(
            store.clone(),
            provider,
            engine,
            local_plane(workspace.path()).await,
            Arc::new(lh_ledger::PricingTable::new()),
            prompter,
            registry.clone(),
            AgentConfig {
                model: "test-model".to_string(),
                workspace_root: workspace.path().to_path_buf(),
                ..Default::default()
            },
        );

        let (bash_id, is_error) = agent
            .handle_one_tool_call(
                session_id,
                "call_1",
                "bash_background",
                &serde_json::json!({"command": "echo start; sleep 0.3; echo done"}),
            )
            .await
            .unwrap();
        assert!(!is_error);
        assert_eq!(registry.lock().await.len(), 1, "the registry must hold the process the tool call started");

        // Polled immediately -- the command sleeps 0.3s, so this must
        // reflect "still running," not block waiting for it.
        let (early, is_error) = agent
            .handle_one_tool_call(session_id, "call_2", "bash_output", &serde_json::json!({"bash_id": bash_id}))
            .await
            .unwrap();
        assert!(!is_error);
        assert!(early.contains("still running"), "got: {early}");

        let (long_id, _) = agent
            .handle_one_tool_call(session_id, "call_3", "bash_background", &serde_json::json!({"command": "sleep 30"}))
            .await
            .unwrap();
        let (kill_out, is_error) = agent
            .handle_one_tool_call(session_id, "call_4", "bash_kill", &serde_json::json!({"bash_id": long_id}))
            .await
            .unwrap();
        assert!(!is_error);
        assert!(kill_out.contains("killed"), "got: {kill_out}");

        let (err_out, is_error) = agent
            .handle_one_tool_call(session_id, "call_5", "bash_output", &serde_json::json!({"bash_id": "nope"}))
            .await
            .unwrap();
        assert!(is_error);
        assert!(err_out.contains("unknown bash_id"), "got: {err_out}");

        // Only the two bash_background calls should have gone through a
        // fresh permission ask -- bash_output/bash_kill deliberately skip it.
        let events = store.read_from(session_id, 0).await.unwrap();
        let permission_asks =
            events.iter().filter(|e| matches!(e.payload, EventPayload::PermissionRequested { .. })).count();
        assert_eq!(permission_asks, 2);
    }

    #[tokio::test]
    async fn bash_wait_blocks_until_the_process_exits_with_no_fresh_permission_ask() {
        let workspace = tempfile::tempdir().unwrap();
        let store = Arc::new(SqliteSessionStore::open_in_memory().unwrap());
        let provider = Arc::new(ScriptedModelProvider::new(vec![]));
        let engine = Arc::new(FixedDecisionEngine(PermissionDecision::Allow));
        let prompter = Arc::new(FixedPrompter(PermissionDecision::Allow));
        let session_id = SessionId::now_v7();
        let registry: BackgroundProcessRegistry = Arc::new(AsyncMutex::new(HashMap::new()));

        let agent = NativeAgentLoop::new(
            store.clone(),
            provider,
            engine,
            local_plane(workspace.path()).await,
            Arc::new(lh_ledger::PricingTable::new()),
            prompter,
            registry.clone(),
            AgentConfig {
                model: "test-model".to_string(),
                workspace_root: workspace.path().to_path_buf(),
                ..Default::default()
            },
        );

        let (bash_id, _) = agent
            .handle_one_tool_call(
                session_id,
                "call_1",
                "bash_background",
                &serde_json::json!({"command": "sleep 0.2; echo finished"}),
            )
            .await
            .unwrap();

        let (output, is_error) = agent
            .handle_one_tool_call(session_id, "call_2", "bash_wait", &serde_json::json!({"bash_id": bash_id}))
            .await
            .unwrap();
        assert!(!is_error);
        assert!(output.contains("exited"), "bash_wait must report the exit status, got: {output}");

        // The process really did run to completion by the time bash_wait
        // returned, not just "not yet checked" -- bash_output afterwards
        // must see the finished output rather than "still running".
        let (after, is_error) = agent
            .handle_one_tool_call(session_id, "call_3", "bash_output", &serde_json::json!({"bash_id": bash_id}))
            .await
            .unwrap();
        assert!(!is_error);
        assert!(after.contains("finished"), "got: {after}");

        // bash_wait must not have gone through a fresh permission ask --
        // only the one bash_background call should have.
        let events = store.read_from(session_id, 0).await.unwrap();
        let permission_asks =
            events.iter().filter(|e| matches!(e.payload, EventPayload::PermissionRequested { .. })).count();
        assert_eq!(permission_asks, 1);
    }

    #[tokio::test]
    async fn bash_wait_on_an_unknown_id_is_a_clean_error() {
        let workspace = tempfile::tempdir().unwrap();
        let store = Arc::new(SqliteSessionStore::open_in_memory().unwrap());
        let registry: BackgroundProcessRegistry = Arc::new(AsyncMutex::new(HashMap::new()));
        let agent = NativeAgentLoop::new(
            store,
            Arc::new(ScriptedModelProvider::new(vec![])),
            Arc::new(FixedDecisionEngine(PermissionDecision::Allow)),
            local_plane(workspace.path()).await,
            Arc::new(lh_ledger::PricingTable::new()),
            Arc::new(FixedPrompter(PermissionDecision::Allow)),
            registry,
            AgentConfig {
                model: "test-model".to_string(),
                workspace_root: workspace.path().to_path_buf(),
                ..Default::default()
            },
        );

        let (out, is_error) = agent
            .handle_one_tool_call(SessionId::now_v7(), "call_1", "bash_wait", &serde_json::json!({"bash_id": "nope"}))
            .await
            .unwrap();
        assert!(is_error);
        assert!(out.contains("unknown bash_id"), "got: {out}");
    }

    #[tokio::test]
    async fn plan_update_emits_a_plan_updated_event_with_no_permission_ask() {
        let workspace = tempfile::tempdir().unwrap();
        let store = Arc::new(SqliteSessionStore::open_in_memory().unwrap());
        let registry: BackgroundProcessRegistry = Arc::new(AsyncMutex::new(HashMap::new()));
        let session_id = SessionId::now_v7();
        let agent = NativeAgentLoop::new(
            store.clone(),
            Arc::new(ScriptedModelProvider::new(vec![])),
            Arc::new(FixedDecisionEngine(PermissionDecision::Allow)),
            local_plane(workspace.path()).await,
            Arc::new(lh_ledger::PricingTable::new()),
            Arc::new(FixedPrompter(PermissionDecision::Allow)),
            registry,
            AgentConfig {
                model: "test-model".to_string(),
                workspace_root: workspace.path().to_path_buf(),
                ..Default::default()
            },
        );

        let (out, is_error) = agent
            .handle_one_tool_call(
                session_id,
                "call_1",
                "plan_update",
                &serde_json::json!({"steps": [
                    {"description": "find the bug", "status": "in_progress"},
                    {"description": "fix it", "status": "pending"},
                ]}),
            )
            .await
            .unwrap();
        assert!(!is_error, "got: {out}");
        assert!(out.contains("2 step"), "got: {out}");

        let events = store.read_from(session_id, 0).await.unwrap();
        assert_eq!(
            events.iter().filter(|e| matches!(e.payload, EventPayload::PermissionRequested { .. })).count(),
            0,
            "plan_update never touches the system, so it must never ask permission"
        );
        let plan = events
            .iter()
            .find_map(|e| match &e.payload {
                EventPayload::PlanUpdated { steps } => Some(steps.clone()),
                _ => None,
            })
            .expect("a PlanUpdated event must have been appended");
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].description, "find the bug");
        assert_eq!(plan[0].status, lh_event::PlanStepStatus::InProgress);
        assert_eq!(plan[1].status, lh_event::PlanStepStatus::Pending);
    }

    #[tokio::test]
    async fn plan_update_with_a_bad_status_is_a_clean_error_not_a_partial_plan() {
        let workspace = tempfile::tempdir().unwrap();
        let store = Arc::new(SqliteSessionStore::open_in_memory().unwrap());
        let registry: BackgroundProcessRegistry = Arc::new(AsyncMutex::new(HashMap::new()));
        let session_id = SessionId::now_v7();
        let agent = NativeAgentLoop::new(
            store.clone(),
            Arc::new(ScriptedModelProvider::new(vec![])),
            Arc::new(FixedDecisionEngine(PermissionDecision::Allow)),
            local_plane(workspace.path()).await,
            Arc::new(lh_ledger::PricingTable::new()),
            Arc::new(FixedPrompter(PermissionDecision::Allow)),
            registry,
            AgentConfig {
                model: "test-model".to_string(),
                workspace_root: workspace.path().to_path_buf(),
                ..Default::default()
            },
        );

        let (out, is_error) = agent
            .handle_one_tool_call(
                session_id,
                "call_1",
                "plan_update",
                &serde_json::json!({"steps": [{"description": "x", "status": "done"}]}),
            )
            .await
            .unwrap();
        assert!(is_error, "got: {out}");

        let events = store.read_from(session_id, 0).await.unwrap();
        assert!(
            !events.iter().any(|e| matches!(e.payload, EventPayload::PlanUpdated { .. })),
            "a malformed plan_update must not append a partial PlanUpdated event"
        );
    }

    #[tokio::test]
    async fn background_processes_persist_across_separate_native_agent_loop_instances() {
        // Matches how the daemon actually uses this: a fresh NativeAgentLoop
        // is constructed per session/prompt call, but the registry Arc is
        // shared across the whole connection -- a background task started
        // in "turn 1" must still be pollable from "turn 2"'s instance.
        let workspace = tempfile::tempdir().unwrap();
        let store = Arc::new(SqliteSessionStore::open_in_memory().unwrap());
        let session_id = SessionId::now_v7();
        let registry: BackgroundProcessRegistry = Arc::new(AsyncMutex::new(HashMap::new()));
        let config = AgentConfig {
            model: "test-model".to_string(),
            workspace_root: workspace.path().to_path_buf(),
            ..Default::default()
        };

        let turn1 = NativeAgentLoop::new(
            store.clone(),
            Arc::new(ScriptedModelProvider::new(vec![])),
            Arc::new(FixedDecisionEngine(PermissionDecision::Allow)),
            local_plane(workspace.path()).await,
            Arc::new(lh_ledger::PricingTable::new()),
            Arc::new(FixedPrompter(PermissionDecision::Allow)),
            registry.clone(),
            config.clone(),
        );
        let (bash_id, _) = turn1
            .handle_one_tool_call(
                session_id,
                "call_1",
                "bash_background",
                &serde_json::json!({"command": "echo hi; sleep 0.2; echo bye"}),
            )
            .await
            .unwrap();

        let turn2 = NativeAgentLoop::new(
            store.clone(),
            Arc::new(ScriptedModelProvider::new(vec![])),
            Arc::new(FixedDecisionEngine(PermissionDecision::Allow)),
            local_plane(workspace.path()).await,
            Arc::new(lh_ledger::PricingTable::new()),
            Arc::new(FixedPrompter(PermissionDecision::Allow)),
            registry.clone(),
            config,
        );
        let (output, is_error) = turn2
            .handle_one_tool_call(session_id, "call_2", "bash_output", &serde_json::json!({"bash_id": bash_id}))
            .await
            .unwrap();
        assert!(!is_error, "a fresh NativeAgentLoop sharing the registry must still see the process turn1 started");
        assert!(output.contains("hi") || output.contains("still running"), "got: {output}");
    }

    fn payload_kind(event: &Event) -> &'static str {
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
}
