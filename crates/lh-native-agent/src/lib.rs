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
//! Phase 3 scope beyond that: still no policy persistence (every tool call
//! is asked about, via `PermissionEngine`), no subagents, no ACP.

mod tools;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use lh_event::{
    Actor, ContentBlock, Event, EventPayload, PermissionDecision, PermissionRequest, SessionId,
    ToolCall, ToolCallStatus, ToolSource, UsageConfidence, UsageDelta,
};
use lh_execution::ExecutionPlane;
use lh_model_provider::{ChatContent, ChatMessage, ChatRole, ModelProvider, ModelRequest};
use lh_permission::PermissionEngine;
use lh_store::SessionStore;

pub use tools::builtin_tool_specs;

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
        }
    }
}

pub struct NativeAgentLoop {
    store: Arc<dyn SessionStore>,
    model_provider: Arc<dyn ModelProvider>,
    permission_engine: Arc<dyn PermissionEngine>,
    execution_plane: Arc<dyn ExecutionPlane>,
    pricing: Arc<lh_ledger::PricingTable>,
    config: AgentConfig,
}

impl NativeAgentLoop {
    pub fn new(
        store: Arc<dyn SessionStore>,
        model_provider: Arc<dyn ModelProvider>,
        permission_engine: Arc<dyn PermissionEngine>,
        execution_plane: Arc<dyn ExecutionPlane>,
        pricing: Arc<lh_ledger::PricingTable>,
        config: AgentConfig,
    ) -> Self {
        Self {
            store,
            model_provider,
            permission_engine,
            execution_plane,
            pricing,
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
        let tools = builtin_tool_specs();
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
                match self.execution_plane.execute(&call, &decision).await {
                    Ok(out) => (ToolCallStatus::Completed, out, false),
                    Err(e) => (ToolCallStatus::Failed, e.to_string(), true),
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
        let session_id = SessionId::now_v7();

        let agent = NativeAgentLoop::new(
            store.clone(),
            provider,
            engine,
            local_plane(workspace.path()).await,
            Arc::new(lh_ledger::PricingTable::new()),
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
        let session_id = SessionId::now_v7();

        let agent = NativeAgentLoop::new(
            store.clone(),
            provider,
            engine,
            local_plane(workspace.path()).await,
            Arc::new(lh_ledger::PricingTable::new()),
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
        let session_id = SessionId::now_v7();

        let agent = NativeAgentLoop::new(
            store.clone(),
            provider,
            engine,
            local_plane(workspace.path()).await,
            Arc::new(lh_ledger::PricingTable::new()),
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
