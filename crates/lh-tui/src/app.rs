//! Pure application state and its transitions. No rendering and no I/O
//! here -- `ui::draw` reads this, `main`'s event loop mutates it. Keeping
//! the split this strict is what makes `App` unit-testable without a
//! terminal at all (see `tests` below) and `ui::draw` testable against
//! `ratatui::backend::TestBackend` with no daemon connection.

use lh_event::{ChildKind, ChildOutcome, ContentBlock, Event, EventPayload, PermissionDecision, PlanStep, SessionId, ToolCallStatus};
use lh_protocol::RequestId;

use crate::input::InputBox;

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

/// A subagent or ACP-delegated child session, tracked for the sidebar's
/// session tree -- `outcome` is `None` while it's still running.
#[derive(Debug, Clone)]
pub struct ChildSessionInfo {
    pub id: SessionId,
    pub kind: ChildKind,
    pub outcome: Option<ChildOutcome>,
}

pub struct App {
    pub phase: ConnPhase,
    pub transcript: Vec<TranscriptItem>,
    pub input: InputBox,
    /// Lines scrolled *up* from the bottom (0 = pinned to the newest
    /// content). A raw line count rather than a fraction/percentage so
    /// `ui::draw` can clamp it against whatever the real viewport height
    /// happens to be this frame, without `App` needing to know terminal
    /// dimensions at all.
    pub transcript_scroll: u16,
    pub session_id: Option<lh_event::SessionId>,
    pub pending: Option<(RequestId, PendingKind)>,
    pub pending_permission: Option<PendingPermission>,
    pub should_quit: bool,
    pub status: String,
    /// The most recent `PlanUpdated` snapshot -- replaces wholesale each
    /// time rather than merging, matching how the event itself is defined
    /// (a full `Vec<PlanStep>`, not a delta).
    pub plan_steps: Vec<PlanStep>,
    /// Subagents and ACP-delegated children spawned from this session,
    /// oldest first -- rendered as the sidebar's session tree.
    pub child_sessions: Vec<ChildSessionInfo>,
    /// The id of an in-flight `ledger/query`, if any -- tracked separately
    /// from `pending` because a ledger refresh must never gate input the
    /// way `pending` does (see `dispatch::handle_client_event`).
    pub pending_ledger_query: Option<RequestId>,
    pub last_ledger: Option<lh_ledger::LedgerRollup>,
}

impl App {
    pub fn new() -> Self {
        Self {
            phase: ConnPhase::Connecting,
            transcript: Vec::new(),
            input: InputBox::new(),
            transcript_scroll: 0,
            session_id: None,
            pending: None,
            pending_permission: None,
            should_quit: false,
            status: "connecting...".to_string(),
            plan_steps: Vec::new(),
            child_sessions: Vec::new(),
            pending_ledger_query: None,
            last_ledger: None,
        }
    }

    /// Turn text is only editable when not waiting on a prompt response and
    /// not in the middle of answering a permission ask.
    pub fn input_enabled(&self) -> bool {
        self.phase == ConnPhase::Ready && self.pending.is_none() && self.pending_permission.is_none()
    }

    pub fn scroll_up(&mut self, lines: u16) {
        self.transcript_scroll = self.transcript_scroll.saturating_add(lines);
    }

    pub fn scroll_down(&mut self, lines: u16) {
        self.transcript_scroll = self.transcript_scroll.saturating_sub(lines);
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
            EventPayload::PlanUpdated { steps } => {
                self.plan_steps = steps.clone();
            }
            EventPayload::ChildSessionSpawned { child, kind, .. } => {
                self.child_sessions.push(ChildSessionInfo { id: *child, kind: kind.clone(), outcome: None });
            }
            EventPayload::ChildSessionEnded { child, outcome } => {
                if let Some(info) = self.child_sessions.iter_mut().find(|c| c.id == *child) {
                    info.outcome = Some(outcome.clone());
                }
            }
            EventPayload::Error { message, recoverable } => {
                self.transcript.push(TranscriptItem::Error(format!(
                    "{message}{}",
                    if *recoverable { " (recoverable)" } else { "" }
                )));
            }
            // PermissionRequested is rendered implicitly by pending_permission
            // going Some (see main's PermissionAsk handling); SessionForked/
            // SessionResumed have no v1 UI use yet (no resume flow exists --
            // see the plan doc's Phase 8 stretch goal).
            _ => {}
        }
    }

    pub fn push_user_message(&mut self, text: String) {
        self.transcript.push(TranscriptItem::User(text));
        // Sending a message is a clear signal the user wants to see what
        // happens next -- jump back to the newest content even if they'd
        // scrolled up to reread something earlier. Streamed content
        // arriving via `apply_session_event` deliberately does *not* do
        // this, so scrolling up mid-response to reread it doesn't get
        // yanked back down involuntarily.
        self.transcript_scroll = 0;
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

    #[test]
    fn scroll_up_and_down_are_saturating_in_both_directions() {
        let mut app = App::new();
        app.scroll_down(5); // already at 0, must not wrap
        assert_eq!(app.transcript_scroll, 0);

        app.scroll_up(3);
        assert_eq!(app.transcript_scroll, 3);
        app.scroll_up(u16::MAX);
        assert_eq!(app.transcript_scroll, u16::MAX, "must not overflow");

        app.scroll_down(u16::MAX);
        assert_eq!(app.transcript_scroll, 0);
    }

    #[test]
    fn sending_a_message_jumps_back_to_the_newest_content() {
        let mut app = App::new();
        app.scroll_up(10);
        app.push_user_message("hi".to_string());
        assert_eq!(app.transcript_scroll, 0);
    }

    #[test]
    fn streamed_content_does_not_disturb_a_manually_scrolled_position() {
        let mut app = App::new();
        app.scroll_up(10);
        app.apply_session_event(&event(EventPayload::AgentMessageChunk { content: ContentBlock::text("more") }));
        assert_eq!(app.transcript_scroll, 10, "reading old context shouldn't get yanked away by new streaming output");
    }

    #[test]
    fn plan_updated_replaces_the_whole_step_list_each_time() {
        let mut app = App::new();
        app.apply_session_event(&event(EventPayload::PlanUpdated {
            steps: vec![lh_event::PlanStep { description: "step one".to_string(), status: lh_event::PlanStepStatus::Pending }],
        }));
        assert_eq!(app.plan_steps.len(), 1);

        app.apply_session_event(&event(EventPayload::PlanUpdated {
            steps: vec![
                lh_event::PlanStep { description: "step one".to_string(), status: lh_event::PlanStepStatus::Completed },
                lh_event::PlanStep { description: "step two".to_string(), status: lh_event::PlanStepStatus::InProgress },
            ],
        }));
        assert_eq!(app.plan_steps.len(), 2, "a newer PlanUpdated replaces, not appends");
        assert!(matches!(app.plan_steps[0].status, lh_event::PlanStepStatus::Completed));
    }

    #[test]
    fn a_spawned_child_session_appears_running_then_gets_its_outcome_on_ended() {
        let mut app = App::new();
        let child_id = SessionId::now_v7();
        app.apply_session_event(&event(EventPayload::ChildSessionSpawned {
            child: child_id,
            kind: ChildKind::NativeSubagent { role: "researcher".to_string() },
            spec: lh_event::ChildSpec { task_summary: "look into it".to_string() },
        }));
        assert_eq!(app.child_sessions.len(), 1);
        assert!(app.child_sessions[0].outcome.is_none(), "should start out running");

        app.apply_session_event(&event(EventPayload::ChildSessionEnded {
            child: child_id,
            outcome: ChildOutcome::Success { summary: "done".to_string() },
        }));
        assert_eq!(app.child_sessions.len(), 1, "must update in place, not append a second entry");
        assert!(matches!(app.child_sessions[0].outcome, Some(ChildOutcome::Success { .. })));
    }

    #[test]
    fn ending_an_unknown_child_session_is_a_no_op() {
        let mut app = App::new();
        app.apply_session_event(&event(EventPayload::ChildSessionEnded {
            child: SessionId::now_v7(),
            outcome: ChildOutcome::Cancelled,
        }));
        assert!(app.child_sessions.is_empty());
    }
}
