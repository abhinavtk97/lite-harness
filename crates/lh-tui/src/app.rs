//! Pure application state and its transitions. No rendering and no I/O
//! here -- `ui::draw` reads this, `main`'s event loop mutates it. Keeping
//! the split this strict is what makes `App` unit-testable without a
//! terminal at all (see `tests` below) and `ui::draw` testable against
//! `ratatui::backend::TestBackend` with no daemon connection.

use lh_event::{ContentBlock, Event, EventPayload, PermissionDecision, ToolCallStatus};
use lh_protocol::RequestId;

#[derive(Debug, Clone)]
pub enum TranscriptItem {
    User(String),
    /// Accumulates consecutive `AgentMessageChunk`s into one growing bubble
    /// rather than one line per chunk -- matches how a streaming chat UI
    /// actually reads.
    Agent(String),
    Thought(String),
    ToolCall { tool_name: String, source: String },
    ToolCallUpdate { status: ToolCallStatus },
    System(String),
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnPhase {
    Connecting,
    Ready,
}

/// What an in-flight request (by id) will do with its `Response` once it
/// arrives off the event channel -- `App::handle_client_event`'s dispatch
/// table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingKind {
    Initialize,
    SessionCreate,
    Prompt,
}

/// The transcript already gets a system line describing the request the
/// moment it arrives (see `main::handle_client_event`) -- all this needs to
/// carry forward is the id to answer. A richer field re-appears here once
/// the permission-ask modal (a later phase) actually renders the request
/// itself rather than relying on that transcript line.
#[derive(Debug, Clone, Copy)]
pub struct PendingPermission {
    pub request_id: RequestId,
}

pub struct App {
    pub phase: ConnPhase,
    pub transcript: Vec<TranscriptItem>,
    pub input: String,
    pub session_id: Option<lh_event::SessionId>,
    pub pending: Option<(RequestId, PendingKind)>,
    pub pending_permission: Option<PendingPermission>,
    pub should_quit: bool,
    pub status: String,
}

impl App {
    pub fn new() -> Self {
        Self {
            phase: ConnPhase::Connecting,
            transcript: Vec::new(),
            input: String::new(),
            session_id: None,
            pending: None,
            pending_permission: None,
            should_quit: false,
            status: "connecting...".to_string(),
        }
    }

    /// Turn text is only editable when not waiting on a prompt response and
    /// not in the middle of answering a permission ask.
    pub fn input_enabled(&self) -> bool {
        self.phase == ConnPhase::Ready && self.pending.is_none() && self.pending_permission.is_none()
    }

    pub fn apply_session_event(&mut self, event: &Event) {
        match &event.payload {
            // We already pushed our own `User` transcript item locally the
            // moment the prompt was sent -- the daemon's own echo of it
            // back through the event stream would just duplicate it.
            EventPayload::UserMessage { .. } => {}
            EventPayload::AgentMessageChunk { content } => {
                let text = render_content(content);
                match self.transcript.last_mut() {
                    Some(TranscriptItem::Agent(buf)) => buf.push_str(&text),
                    _ => self.transcript.push(TranscriptItem::Agent(text)),
                }
            }
            EventPayload::AgentThoughtChunk { content } => {
                let text = render_content(content);
                match self.transcript.last_mut() {
                    Some(TranscriptItem::Thought(buf)) => buf.push_str(&text),
                    _ => self.transcript.push(TranscriptItem::Thought(text)),
                }
            }
            EventPayload::ToolCallRequested { call } => {
                self.transcript.push(TranscriptItem::ToolCall {
                    tool_name: call.tool_name.clone(),
                    source: source_label(&call.source),
                });
            }
            EventPayload::ToolCallUpdated { status, .. } => {
                self.transcript.push(TranscriptItem::ToolCallUpdate { status: *status });
            }
            EventPayload::PermissionDecided { decision, .. } => {
                self.transcript.push(TranscriptItem::System(format!("permission: {decision:?}")));
            }
            EventPayload::UsageReported { usage } => {
                let cost = usage.cost_usd.map(|c| format!("${c:.4}")).unwrap_or_else(|| "$?".to_string());
                self.transcript.push(TranscriptItem::System(format!(
                    "usage: {cost} ({:?}, {}ms)",
                    usage.confidence, usage.wall_ms
                )));
            }
            EventPayload::SessionDriverSet { driver } => {
                self.transcript.push(TranscriptItem::System(format!("driver: {driver:?}")));
            }
            EventPayload::Error { message, recoverable } => {
                self.transcript.push(TranscriptItem::Error(format!(
                    "{message}{}",
                    if *recoverable { " (recoverable)" } else { "" }
                )));
            }
            // PermissionRequested is rendered implicitly by pending_permission
            // going Some (see main's PermissionAsk handling), and the
            // remaining variants (ChildSession*/SessionForked/SessionResumed/
            // PlanUpdated) are deliberately not rendered yet -- sidebar/plan
            // panel work lands in a later phase (see the plan doc).
            _ => {}
        }
    }

    pub fn push_user_message(&mut self, text: String) {
        self.transcript.push(TranscriptItem::User(text));
    }

    pub fn push_system(&mut self, text: impl Into<String>) {
        self.transcript.push(TranscriptItem::System(text.into()));
    }

    pub fn push_error(&mut self, text: impl Into<String>) {
        self.transcript.push(TranscriptItem::Error(text.into()));
    }

    /// `y`/`n` only for now (Allow/Deny) -- the always-allow/always-deny
    /// variants and a real modal overlay land with the permission-ask UI
    /// phase; this keeps the connection from hanging in the meantime.
    pub fn decide_pending_permission(&mut self, allow: bool) -> Option<(RequestId, PermissionDecision)> {
        let pending = self.pending_permission.take()?;
        let decision = if allow { PermissionDecision::Allow } else { PermissionDecision::Deny };
        Some((pending.request_id, decision))
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

fn render_content(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text { text } => text.clone(),
        ContentBlock::Other { kind, .. } => format!("[{kind}]"),
    }
}

fn source_label(source: &lh_event::ToolSource) -> String {
    match source {
        lh_event::ToolSource::Native { tool_id } => format!("native:{tool_id}"),
        lh_event::ToolSource::Mcp { server, tool } => format!("mcp:{server}/{tool}"),
        lh_event::ToolSource::Acp { agent } => format!("acp:{agent:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lh_event::{Actor, SessionId, UsageConfidence, UsageDelta};

    fn event(payload: EventPayload) -> Event {
        Event::new(SessionId::now_v7(), None, Actor::Agent, payload)
    }

    #[test]
    fn consecutive_agent_message_chunks_accumulate_into_one_bubble() {
        let mut app = App::new();
        app.apply_session_event(&event(EventPayload::AgentMessageChunk { content: ContentBlock::text("Hel") }));
        app.apply_session_event(&event(EventPayload::AgentMessageChunk { content: ContentBlock::text("lo") }));

        assert_eq!(app.transcript.len(), 1, "expected one bubble, got {:?}", app.transcript);
        let TranscriptItem::Agent(text) = &app.transcript[0] else { panic!("expected Agent") };
        assert_eq!(text, "Hello");
    }

    #[test]
    fn a_tool_call_interrupts_the_accumulating_agent_bubble() {
        let mut app = App::new();
        app.apply_session_event(&event(EventPayload::AgentMessageChunk { content: ContentBlock::text("before") }));
        app.apply_session_event(&event(EventPayload::ToolCallRequested {
            call: lh_event::ToolCall {
                call_id: "call_1".to_string(),
                tool_name: "bash".to_string(),
                source: lh_event::ToolSource::Native { tool_id: "bash".to_string() },
                args_summary: serde_json::json!({}),
                raw_args: serde_json::json!({}),
            },
        }));
        app.apply_session_event(&event(EventPayload::AgentMessageChunk { content: ContentBlock::text("after") }));

        assert_eq!(app.transcript.len(), 3);
        assert!(matches!(app.transcript[0], TranscriptItem::Agent(ref t) if t == "before"));
        assert!(matches!(app.transcript[1], TranscriptItem::ToolCall { .. }));
        assert!(matches!(app.transcript[2], TranscriptItem::Agent(ref t) if t == "after"));
    }

    #[test]
    fn consecutive_agent_thought_chunks_accumulate_into_one_bubble() {
        let mut app = App::new();
        app.apply_session_event(&event(EventPayload::AgentThoughtChunk { content: ContentBlock::text("thinking a") }));
        app.apply_session_event(&event(EventPayload::AgentThoughtChunk { content: ContentBlock::text("bout it") }));

        assert_eq!(app.transcript.len(), 1);
        let TranscriptItem::Thought(text) = &app.transcript[0] else { panic!("expected Thought") };
        assert_eq!(text, "thinking about it");
    }

    #[test]
    fn an_error_event_renders_recoverable_and_fatal_distinctly() {
        let mut app = App::new();
        app.apply_session_event(&event(EventPayload::Error { message: "oops".to_string(), recoverable: true }));
        app.apply_session_event(&event(EventPayload::Error { message: "boom".to_string(), recoverable: false }));

        let TranscriptItem::Error(recoverable) = &app.transcript[0] else { panic!("expected Error") };
        assert_eq!(recoverable, "oops (recoverable)");
        let TranscriptItem::Error(fatal) = &app.transcript[1] else { panic!("expected Error") };
        assert_eq!(fatal, "boom");
    }

    #[test]
    fn a_tool_call_from_an_mcp_or_acp_source_labels_it_correctly() {
        let mut app = App::new();
        app.apply_session_event(&event(EventPayload::ToolCallRequested {
            call: lh_event::ToolCall {
                call_id: "call_mcp".to_string(),
                tool_name: "search".to_string(),
                source: lh_event::ToolSource::Mcp { server: "web".to_string(), tool: "search".to_string() },
                args_summary: serde_json::json!({}),
                raw_args: serde_json::json!({}),
            },
        }));
        app.apply_session_event(&event(EventPayload::ToolCallRequested {
            call: lh_event::ToolCall {
                call_id: "call_acp".to_string(),
                tool_name: "edit".to_string(),
                source: lh_event::ToolSource::Acp { agent: lh_event::AgentKind::ClaudeCode },
                args_summary: serde_json::json!({}),
                raw_args: serde_json::json!({}),
            },
        }));

        let TranscriptItem::ToolCall { source, .. } = &app.transcript[0] else { panic!("expected ToolCall") };
        assert_eq!(source, "mcp:web/search");
        let TranscriptItem::ToolCall { source, .. } = &app.transcript[1] else { panic!("expected ToolCall") };
        assert_eq!(source, "acp:ClaudeCode");
    }

    #[test]
    fn an_other_content_block_renders_by_its_kind_tag() {
        let mut app = App::new();
        app.apply_session_event(&event(EventPayload::AgentMessageChunk {
            content: ContentBlock::Other { kind: "image".to_string(), value: serde_json::json!({}) },
        }));
        let TranscriptItem::Agent(text) = &app.transcript[0] else { panic!("expected Agent") };
        assert_eq!(text, "[image]");
    }

    #[test]
    fn default_app_matches_new() {
        let app = App::default();
        assert_eq!(app.phase, ConnPhase::Connecting);
        assert!(app.transcript.is_empty());
    }

    #[test]
    fn usage_reported_renders_a_readable_summary_line() {
        let mut app = App::new();
        app.apply_session_event(&event(EventPayload::UsageReported {
            usage: UsageDelta {
                input_tokens: Some(10),
                output_tokens: Some(5),
                cost_usd: Some(0.0025),
                wall_ms: 120,
                confidence: UsageConfidence::Exact,
            },
        }));
        let TranscriptItem::System(text) = &app.transcript[0] else { panic!("expected System") };
        assert!(text.contains("$0.0025"), "got: {text}");
        assert!(text.contains("Exact"), "got: {text}");
    }

    #[test]
    fn input_is_disabled_while_connecting_or_awaiting_a_response_or_a_permission_decision() {
        let mut app = App::new();
        assert!(!app.input_enabled(), "still Connecting");

        app.phase = ConnPhase::Ready;
        assert!(app.input_enabled());

        app.pending = Some((1, PendingKind::Prompt));
        assert!(!app.input_enabled());
        app.pending = None;

        app.pending_permission = Some(PendingPermission { request_id: -1 });
        assert!(!app.input_enabled());
    }

    #[test]
    fn deciding_a_pending_permission_clears_it_and_reports_the_right_decision() {
        let mut app = App::new();
        app.pending_permission = Some(PendingPermission { request_id: -1 });

        let (id, decision) = app.decide_pending_permission(true).unwrap();
        assert_eq!(id, -1);
        assert!(matches!(decision, PermissionDecision::Allow));
        assert!(app.pending_permission.is_none());

        assert!(app.decide_pending_permission(false).is_none(), "nothing pending, should be a no-op");
    }
}
