//! Pure application state and its transitions. No rendering and no I/O
//! here -- `ui::draw` reads this, `main`'s event loop mutates it. Keeping
//! the split this strict is what makes `App` unit-testable without a
//! terminal at all (see `tests` below) and `ui::draw` testable against
//! `ratatui::backend::TestBackend` with no daemon connection.

use std::collections::HashMap;

use lh_event::{
    ChildKind, ChildOutcome, ContentBlock, Event, EventPayload, PermissionAction, PermissionDecision, PermissionRequest,
    PlanStep, PolicyScope, SessionId, ToolCallStatus,
};
use lh_protocol::{AgentInfo, PrimarySelector, RequestId};

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
    /// The daemon only answers `agents/list` in the window between
    /// `initialize` and `session/create` (its handshake loop stops
    /// checking for it the moment `session/create` arrives) -- so this
    /// step is mandatory in the handshake sequence, not optional, even
    /// though nothing in `App` strictly needs the result until `/agents`
    /// or `/delegate` is actually typed.
    AgentsList,
    SessionCreate,
    Prompt,
    Delegate,
}

/// The full request is carried (not just its id) so the permission modal
/// (`ui::draw_permission_modal`) can render the actual risk tier, tool
/// source, action, and any diff -- `selected` indexes `PermissionChoice::ALL`
/// and starts on `Deny`, the same fail-safe default the pre-modal y/n
/// prompt had (any key other than an explicit "allow" denied).
#[derive(Debug, Clone)]
pub struct PendingPermission {
    pub request_id: RequestId,
    pub request: PermissionRequest,
    pub selected: usize,
}

/// The four ways a `PermissionRequest` can be answered, in the order the
/// modal cycles through and displays them -- `Deny` first/default on
/// purpose (see `PendingPermission`'s doc comment).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionChoice {
    Deny,
    Allow,
    AllowAlways,
    DenyAlways,
}

impl PermissionChoice {
    pub const ALL: [PermissionChoice; 4] = [Self::Deny, Self::Allow, Self::AllowAlways, Self::DenyAlways];

    pub fn label(&self) -> &'static str {
        match self {
            Self::Deny => "Deny",
            Self::Allow => "Allow",
            Self::AllowAlways => "Always Allow",
            Self::DenyAlways => "Always Deny",
        }
    }

    /// "Always" decisions are scoped to the project -- matches `lh-cli`'s
    /// own choice for its equivalent y/N/a/d prompt.
    pub fn decision(&self) -> PermissionDecision {
        match self {
            Self::Deny => PermissionDecision::Deny,
            Self::Allow => PermissionDecision::Allow,
            Self::AllowAlways => PermissionDecision::AllowAlways { scope: PolicyScope::Project },
            Self::DenyAlways => PermissionDecision::DenyAlways { scope: PolicyScope::Project },
        }
    }
}

/// A subagent or ACP-delegated child session, tracked for the sidebar's
/// session tree -- `outcome` is `None` while it's still running.
#[derive(Debug, Clone)]
pub struct ChildSessionInfo {
    pub id: SessionId,
    pub kind: ChildKind,
    pub outcome: Option<ChildOutcome>,
}

/// A `bash_background` process, tracked for the sidebar's live indicator --
/// `running` flips to `false` once a `bash_wait` or `bash_kill` call for
/// this same id completes (`bash_output` just peeks, so it never changes
/// this). There's no equivalent tracker for ACP `terminal/*` sessions: those
/// are raw ACP protocol calls between the daemon and a delegated agent
/// subprocess, and `lh-acp` doesn't surface them as `ToolCallRequested`/
/// `ToolCallUpdated` Harness events the way native tools do -- nothing here
/// could observe them without a `lh-acp` change, which is out of scope for
/// a TUI-only phase (see the plan doc's "write a UI, don't extend the
/// core" decision).
#[derive(Debug, Clone)]
pub struct BackgroundBash {
    pub id: String,
    pub running: bool,
}

/// What `App` needs to remember about an in-flight tool call between its
/// `ToolCallRequested` and matching `ToolCallUpdated` in order to maintain
/// `background_bash` -- keyed by `call_id`, discarded once the matching
/// `ToolCallUpdated` arrives. Not part of any rendered state itself.
#[derive(Debug, Clone)]
enum PendingToolCall {
    /// Its `ToolCallUpdated.output` (once it arrives) *is* the new
    /// background process's id -- see `lh-native-agent`'s `handle_one_tool_call`.
    BashBackground,
    /// `bash_wait`/`bash_kill` both target an already-running process by id
    /// (parsed from the request's own `raw_args`); either one completing
    /// means that process is no longer running.
    BashWaitOrKill { bash_id: String },
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
    /// Which driver owns *this* session's root -- fixed at `session/create`
    /// time (set from `--primary` at startup, see `main.rs`) and never
    /// changed afterward: the daemon's per-connection handshake only
    /// accepts one `session/create`, so switching it mid-REPL would mean
    /// opening a whole new connection, not a slash command. `/primary`
    /// deliberately isn't a command for that reason -- see the phase 8.5
    /// commit message.
    pub primary: PrimarySelector,
    /// Populated once, from the `agents/list` round trip that's part of
    /// the handshake (see `PendingKind::AgentsList`) -- backs `/agents`.
    pub available_agents: Vec<AgentInfo>,
    /// Still-running and finished `bash_background` processes, oldest
    /// first -- rendered as the sidebar's background-process indicator.
    pub background_bash: Vec<BackgroundBash>,
    /// Bookkeeping for `background_bash`; see `PendingToolCall`.
    pending_tool_calls: HashMap<String, PendingToolCall>,
    /// The `session_id` of the most recently applied event, used only to
    /// decide whether a new `AgentMessageChunk`/`AgentThoughtChunk` should
    /// merge into the last transcript bubble -- two chunks from *different*
    /// sessions (e.g. the root and a delegated child interleaving) must
    /// never merge into one bubble, even if both happen to be `Agent`
    /// chunks back to back.
    last_event_session: Option<SessionId>,
    /// Highlighted row in the slash-command autocomplete dropdown -- always
    /// read through `.min(candidate_count - 1)` at the point of use rather
    /// than reset on every keystroke, since `command_candidates` is
    /// recomputed fresh from the input text every time anyway.
    pub autocomplete_selected: usize,
    /// Set by Esc while the dropdown is showing; cleared the next time the
    /// input text actually changes (see `dispatch::handle_key`'s text-editing
    /// arms) -- lets someone dismiss the suggestions without deleting what
    /// they've already typed.
    pub autocomplete_dismissed: bool,
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
            primary: PrimarySelector::Native,
            available_agents: Vec::new(),
            background_bash: Vec::new(),
            pending_tool_calls: HashMap::new(),
            last_event_session: None,
            autocomplete_selected: 0,
            autocomplete_dismissed: false,
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
        // Two different sessions' `Agent` chunks landing back to back (e.g.
        // the root and a delegated child interleaving) must never merge
        // into one bubble -- captured *before* `last_event_session` is
        // overwritten below, so it still reflects the *previous* event.
        let same_session_as_last = self.last_event_session == Some(event.session_id);
        self.last_event_session = Some(event.session_id);
        let tag = self.session_tag(event.session_id);

        match &event.payload {
            // We already pushed our own `User` transcript item locally the
            // moment the prompt was sent -- the daemon's own echo of it
            // back through the event stream would just duplicate it.
            EventPayload::UserMessage { .. } => {}
            EventPayload::AgentMessageChunk { content } => {
                let text = render_content(content);
                match self.transcript.last_mut() {
                    Some(TranscriptItem::Agent(buf)) if same_session_as_last => buf.push_str(&text),
                    _ => self.transcript.push(TranscriptItem::Agent(format!("{tag}{text}"))),
                }
            }
            EventPayload::AgentThoughtChunk { content } => {
                let text = render_content(content);
                match self.transcript.last_mut() {
                    Some(TranscriptItem::Thought(buf)) if same_session_as_last => buf.push_str(&text),
                    _ => self.transcript.push(TranscriptItem::Thought(format!("{tag}{text}"))),
                }
            }
            EventPayload::ToolCallRequested { call } => {
                let pending = match call.tool_name.as_str() {
                    "bash_background" => Some(PendingToolCall::BashBackground),
                    "bash_wait" | "bash_kill" => call
                        .raw_args
                        .get("bash_id")
                        .and_then(|v| v.as_str())
                        .map(|id| PendingToolCall::BashWaitOrKill { bash_id: id.to_string() }),
                    _ => None,
                };
                if let Some(pending) = pending {
                    self.pending_tool_calls.insert(call.call_id.clone(), pending);
                }
                self.transcript.push(TranscriptItem::ToolCall {
                    tool_name: format!("{tag}{}", call.tool_name),
                    source: source_label(&call.source),
                });
            }
            EventPayload::ToolCallUpdated { call_id, status, output } => {
                if let Some(pending) = self.pending_tool_calls.remove(call_id) {
                    match pending {
                        PendingToolCall::BashBackground if *status == ToolCallStatus::Completed => {
                            if let Some(ContentBlock::Text { text }) = output {
                                self.background_bash.push(BackgroundBash { id: text.clone(), running: true });
                            }
                        }
                        PendingToolCall::BashWaitOrKill { bash_id } => {
                            if let Some(entry) = self.background_bash.iter_mut().find(|b| b.id == bash_id) {
                                entry.running = false;
                            }
                        }
                        PendingToolCall::BashBackground => {}
                    }
                }
                self.transcript.push(TranscriptItem::ToolCallUpdate { status: *status });
            }
            EventPayload::PermissionDecided { decision, .. } => {
                self.transcript.push(TranscriptItem::System(format!("{tag}permission: {decision:?}")));
            }
            EventPayload::UsageReported { usage } => {
                let cost = usage.cost_usd.map(|c| format!("${c:.4}")).unwrap_or_else(|| "$?".to_string());
                self.transcript.push(TranscriptItem::System(format!(
                    "{tag}usage: {cost} ({:?}, {}ms)",
                    usage.confidence, usage.wall_ms
                )));
            }
            EventPayload::SessionDriverSet { driver } => {
                self.transcript.push(TranscriptItem::System(format!("{tag}driver: {driver:?}")));
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
                    "{tag}{message}{}",
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

    /// `""` for the root session; a short bracketed label (e.g.
    /// `"[subagent:researcher] "`) for anything else, so a child session's
    /// transcript lines read as visually distinct from the root's own --
    /// falls back to a generic `"[child] "` if the session isn't (yet)
    /// in `child_sessions`, rather than silently rendering it as if it
    /// were the root (`ChildSessionSpawned`, which populates that list, is
    /// always emitted on the parent before the child's own events start
    /// flowing, so this fallback is defensive, not expected in practice).
    fn session_tag(&self, session_id: SessionId) -> String {
        if Some(session_id) == self.session_id {
            return String::new();
        }
        match self.child_sessions.iter().find(|c| c.id == session_id) {
            Some(child) => match &child.kind {
                ChildKind::NativeSubagent { role } => format!("[subagent:{role}] "),
                ChildKind::Delegated { agent } => format!("[delegated:{agent:?}] "),
            },
            None => "[child] ".to_string(),
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

    pub fn decide_pending_permission(&mut self, decision: PermissionDecision) -> Option<(RequestId, PermissionDecision)> {
        let pending = self.pending_permission.take()?;
        Some((pending.request_id, decision))
    }

    /// Moves the modal's highlighted option; a no-op if nothing's pending.
    pub fn cycle_permission_selection(&mut self, forward: bool) {
        if let Some(pending) = self.pending_permission.as_mut() {
            let n = PermissionChoice::ALL.len();
            pending.selected = if forward { (pending.selected + 1) % n } else { (pending.selected + n - 1) % n };
        }
    }

    pub fn selected_permission_choice(&self) -> Option<PermissionChoice> {
        self.pending_permission.as_ref().map(|p| PermissionChoice::ALL[p.selected])
    }

    /// Moves the autocomplete dropdown's highlighted row; a no-op with no
    /// candidates to cycle through. Wraps like `cycle_permission_selection`,
    /// but takes the candidate count as a parameter rather than reading a
    /// stored list, since the candidate set itself is always recomputed
    /// fresh from `command_candidates` -- never stored on `App`.
    pub fn cycle_autocomplete_selection(&mut self, forward: bool, candidate_count: usize) {
        if candidate_count == 0 {
            return;
        }
        self.autocomplete_selected = if forward {
            (self.autocomplete_selected + 1) % candidate_count
        } else {
            (self.autocomplete_selected + candidate_count - 1) % candidate_count
        };
    }
}

/// One entry in the slash-command registry -- the single source of truth for
/// both `/help`'s text (`dispatch::handle_slash_command`) and the
/// autocomplete dropdown's contents (`ui::draw_autocomplete_dropdown`), so
/// the two can never drift the way a hand-written help string could.
#[derive(Debug, PartialEq, Eq)]
pub struct SlashCommand {
    pub name: &'static str,
    pub usage: &'static str,
}

pub const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand { name: "help", usage: "show this command list" },
    SlashCommand { name: "quit", usage: "exit lite-harness-tui" },
    SlashCommand { name: "agents", usage: "list registered delegated agents" },
    SlashCommand { name: "delegate", usage: "<agent> <task summary...> -- hand a task to a delegated agent" },
];

/// Which `SLASH_COMMANDS` entries match `input_text` as typed so far -- only
/// while the command *name* itself is still being typed (no whitespace yet
/// after the leading `/`), so once a delegate's arguments start, the
/// dropdown gets out of the way rather than keep floating over them. Pure
/// and shared: `ui::draw` calls it to render the dropdown, `dispatch::handle_key`
/// calls it (on the same text) to decide whether Up/Down/Tab/Enter/Esc should
/// be intercepted for it -- exactly the `permission_popup_rect`/
/// `permission_modal_rows` split `ui.rs` and `mouse.rs` already established.
pub fn command_candidates(input_text: &str) -> Vec<&'static SlashCommand> {
    let Some(rest) = input_text.strip_prefix('/') else { return Vec::new() };
    if rest.contains(char::is_whitespace) {
        return Vec::new();
    }
    SLASH_COMMANDS.iter().filter(|c| c.name.starts_with(rest)).collect()
}

/// Turns a `PermissionAction` into one readable line -- shared by the
/// transcript's audit-trail summary (`dispatch::handle_client_event`) and
/// the modal's full detail view (`ui::draw_permission_modal`), so the two
/// never drift out of sync with each other.
pub fn describe_permission_action(action: &PermissionAction) -> String {
    match action {
        PermissionAction::FileRead { path } => format!("read {}", path.display()),
        PermissionAction::FileWrite { path, .. } => format!("write {}", path.display()),
        PermissionAction::Exec { command, .. } => format!("execute: {command}"),
        PermissionAction::NetworkFetch { url } => format!("fetch {url}"),
        PermissionAction::McpToolCall { server, tool, .. } => format!("mcp {server}/{tool}"),
        PermissionAction::DelegatedAgentToolCall { agent, .. } => format!("delegated call via {agent:?}"),
        PermissionAction::DelegateAgent { target, task_summary } => format!("delegate to {target:?}: {task_summary}"),
        PermissionAction::SpawnSubagent { role, task_summary } => format!("spawn subagent ({role}): {task_summary}"),
    }
}

/// A minimal but realistic `PermissionRequest` -- shared by this module's
/// own tests and by `dispatch`/`ui`'s test modules (via `crate::app::...`)
/// so every permission-modal test isn't hand-rolling the same struct.
#[cfg(test)]
pub(crate) fn fake_permission_request() -> PermissionRequest {
    PermissionRequest {
        session_id: SessionId::now_v7(),
        tool_source: lh_event::ToolSource::Native { tool_id: "bash".to_string() },
        action: PermissionAction::Exec {
            command: "echo hi".to_string(),
            args: Vec::new(),
            cwd: std::path::PathBuf::from("."),
        },
        risk_tier: lh_event::RiskTier::Execute,
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

pub(crate) fn source_label(source: &lh_event::ToolSource) -> String {
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

    /// A fixed id (not a fresh random one per call) so tests that apply
    /// several events in a row are all describing the *same* session --
    /// matching real usage, where `apply_session_event` only ever runs
    /// once `App::session_id` is known, and needed since `session_tag`
    /// (and the merge-across-sessions guard) now key off whether an
    /// event's `session_id` matches `App::session_id`.
    fn root_session_id() -> SessionId {
        SessionId::nil()
    }

    fn event(payload: EventPayload) -> Event {
        Event::new(root_session_id(), None, Actor::Agent, payload)
    }

    /// A fresh `App` already past the handshake (`session_id` set to
    /// `root_session_id()`), for tests that call `apply_session_event`
    /// directly without going through the real `session/create` round trip.
    fn app_with_root_session() -> App {
        let mut app = App::new();
        app.session_id = Some(root_session_id());
        app
    }

    #[test]
    fn consecutive_agent_message_chunks_accumulate_into_one_bubble() {
        let mut app = app_with_root_session();
        app.apply_session_event(&event(EventPayload::AgentMessageChunk { content: ContentBlock::text("Hel") }));
        app.apply_session_event(&event(EventPayload::AgentMessageChunk { content: ContentBlock::text("lo") }));

        assert_eq!(app.transcript.len(), 1, "expected one bubble, got {:?}", app.transcript);
        let TranscriptItem::Agent(text) = &app.transcript[0] else { panic!("expected Agent") };
        assert_eq!(text, "Hello");
    }

    #[test]
    fn a_tool_call_interrupts_the_accumulating_agent_bubble() {
        let mut app = app_with_root_session();
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
        let mut app = app_with_root_session();
        app.apply_session_event(&event(EventPayload::AgentThoughtChunk { content: ContentBlock::text("thinking a") }));
        app.apply_session_event(&event(EventPayload::AgentThoughtChunk { content: ContentBlock::text("bout it") }));

        assert_eq!(app.transcript.len(), 1);
        let TranscriptItem::Thought(text) = &app.transcript[0] else { panic!("expected Thought") };
        assert_eq!(text, "thinking about it");
    }

    #[test]
    fn an_error_event_renders_recoverable_and_fatal_distinctly() {
        let mut app = app_with_root_session();
        app.apply_session_event(&event(EventPayload::Error { message: "oops".to_string(), recoverable: true }));
        app.apply_session_event(&event(EventPayload::Error { message: "boom".to_string(), recoverable: false }));

        let TranscriptItem::Error(recoverable) = &app.transcript[0] else { panic!("expected Error") };
        assert_eq!(recoverable, "oops (recoverable)");
        let TranscriptItem::Error(fatal) = &app.transcript[1] else { panic!("expected Error") };
        assert_eq!(fatal, "boom");
    }

    #[test]
    fn a_tool_call_from_an_mcp_or_acp_source_labels_it_correctly() {
        let mut app = app_with_root_session();
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
        let mut app = app_with_root_session();
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
        let mut app = app_with_root_session();
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

        app.pending_permission = Some(PendingPermission { request_id: -1, request: fake_permission_request(), selected: 0 });
        assert!(!app.input_enabled());
    }

    #[test]
    fn deciding_a_pending_permission_clears_it_and_reports_the_given_decision() {
        let mut app = App::new();
        app.pending_permission = Some(PendingPermission { request_id: -1, request: fake_permission_request(), selected: 0 });

        let (id, decision) = app.decide_pending_permission(PermissionDecision::Allow).unwrap();
        assert_eq!(id, -1);
        assert!(matches!(decision, PermissionDecision::Allow));
        assert!(app.pending_permission.is_none());

        assert!(
            app.decide_pending_permission(PermissionDecision::Deny).is_none(),
            "nothing pending, should be a no-op"
        );
    }

    #[test]
    fn a_pending_permission_defaults_its_selection_to_deny() {
        let app_selected = PendingPermission { request_id: -1, request: fake_permission_request(), selected: 0 };
        assert_eq!(PermissionChoice::ALL[app_selected.selected], PermissionChoice::Deny);
    }

    #[test]
    fn cycling_the_permission_selection_wraps_in_both_directions() {
        let mut app = App::new();
        app.pending_permission = Some(PendingPermission { request_id: -1, request: fake_permission_request(), selected: 0 });

        assert_eq!(app.selected_permission_choice(), Some(PermissionChoice::Deny));
        app.cycle_permission_selection(true);
        assert_eq!(app.selected_permission_choice(), Some(PermissionChoice::Allow));
        app.cycle_permission_selection(false);
        assert_eq!(app.selected_permission_choice(), Some(PermissionChoice::Deny), "wraps backward past the start");
        app.cycle_permission_selection(false);
        assert_eq!(app.selected_permission_choice(), Some(PermissionChoice::DenyAlways), "wraps backward to the end");
    }

    #[test]
    fn cycling_the_permission_selection_with_nothing_pending_is_a_no_op() {
        let mut app = App::new();
        app.cycle_permission_selection(true);
        assert!(app.selected_permission_choice().is_none());
    }

    #[test]
    fn command_candidates_matches_only_commands_starting_with_the_typed_prefix() {
        assert_eq!(command_candidates("/d").iter().map(|c| c.name).collect::<Vec<_>>(), vec!["delegate"]);
        assert_eq!(command_candidates("/a").iter().map(|c| c.name).collect::<Vec<_>>(), vec!["agents"]);
        assert_eq!(command_candidates("/xyz"), Vec::<&SlashCommand>::new());
    }

    #[test]
    fn command_candidates_lists_everything_for_a_bare_slash() {
        let names: Vec<&str> = command_candidates("/").iter().map(|c| c.name).collect();
        assert_eq!(names.len(), SLASH_COMMANDS.len());
        assert!(names.contains(&"help"));
        assert!(names.contains(&"delegate"));
    }

    #[test]
    fn command_candidates_stops_once_the_command_name_has_a_trailing_space() {
        assert_eq!(command_candidates("/delegate "), Vec::<&SlashCommand>::new());
        assert_eq!(command_candidates("/delegate claude-code do the thing"), Vec::<&SlashCommand>::new());
    }

    #[test]
    fn command_candidates_is_empty_for_text_not_starting_with_a_slash() {
        assert_eq!(command_candidates("hello there"), Vec::<&SlashCommand>::new());
        assert_eq!(command_candidates(""), Vec::<&SlashCommand>::new());
    }

    #[test]
    fn cycling_the_autocomplete_selection_wraps_in_both_directions() {
        let mut app = App::new();
        assert_eq!(app.autocomplete_selected, 0);
        app.cycle_autocomplete_selection(true, 4);
        assert_eq!(app.autocomplete_selected, 1);
        app.cycle_autocomplete_selection(false, 4);
        assert_eq!(app.autocomplete_selected, 0);
        app.cycle_autocomplete_selection(false, 4);
        assert_eq!(app.autocomplete_selected, 3, "wraps backward to the end");
    }

    #[test]
    fn cycling_the_autocomplete_selection_with_no_candidates_is_a_no_op() {
        let mut app = App::new();
        app.cycle_autocomplete_selection(true, 0);
        assert_eq!(app.autocomplete_selected, 0);
    }

    #[test]
    fn every_permission_choice_has_a_distinct_label_and_the_right_decision_shape() {
        assert_eq!(PermissionChoice::Deny.label(), "Deny");
        assert!(matches!(PermissionChoice::Deny.decision(), PermissionDecision::Deny));
        assert_eq!(PermissionChoice::Allow.label(), "Allow");
        assert!(matches!(PermissionChoice::Allow.decision(), PermissionDecision::Allow));
        assert!(matches!(
            PermissionChoice::AllowAlways.decision(),
            PermissionDecision::AllowAlways { scope: PolicyScope::Project }
        ));
        assert!(matches!(
            PermissionChoice::DenyAlways.decision(),
            PermissionDecision::DenyAlways { scope: PolicyScope::Project }
        ));
    }

    #[test]
    fn describe_permission_action_names_every_variant() {
        assert_eq!(describe_permission_action(&PermissionAction::FileRead { path: "a.txt".into() }), "read a.txt");
        assert_eq!(
            describe_permission_action(&PermissionAction::FileWrite { path: "b.txt".into(), diff_summary: None }),
            "write b.txt"
        );
        assert_eq!(
            describe_permission_action(&PermissionAction::Exec {
                command: "ls".to_string(),
                args: vec![],
                cwd: ".".into()
            }),
            "execute: ls"
        );
        assert_eq!(
            describe_permission_action(&PermissionAction::NetworkFetch { url: "https://x".to_string() }),
            "fetch https://x"
        );
        assert_eq!(
            describe_permission_action(&PermissionAction::McpToolCall {
                server: "web".to_string(),
                tool: "search".to_string(),
                args_summary: serde_json::json!({}),
            }),
            "mcp web/search"
        );
        assert!(describe_permission_action(&PermissionAction::DelegatedAgentToolCall {
            agent: lh_event::AgentKind::ClaudeCode,
            acp_tool_call: Box::new(lh_event::ToolCall {
                call_id: "c".to_string(),
                tool_name: "edit".to_string(),
                source: lh_event::ToolSource::Acp { agent: lh_event::AgentKind::ClaudeCode },
                args_summary: serde_json::json!({}),
                raw_args: serde_json::json!({}),
            }),
        })
        .contains("ClaudeCode"));
        assert_eq!(
            describe_permission_action(&PermissionAction::DelegateAgent {
                target: lh_event::AgentKind::ClaudeCode,
                task_summary: "do it".to_string()
            }),
            "delegate to ClaudeCode: do it"
        );
        assert_eq!(
            describe_permission_action(&PermissionAction::SpawnSubagent {
                role: "researcher".to_string(),
                task_summary: "look into it".to_string()
            }),
            "spawn subagent (researcher): look into it"
        );
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

    fn tool_call(call_id: &str, tool_name: &str, raw_args: serde_json::Value) -> Event {
        event(EventPayload::ToolCallRequested {
            call: lh_event::ToolCall {
                call_id: call_id.to_string(),
                tool_name: tool_name.to_string(),
                source: lh_event::ToolSource::Native { tool_id: tool_name.to_string() },
                args_summary: raw_args.clone(),
                raw_args,
            },
        })
    }

    fn tool_call_completed(call_id: &str, output: &str) -> Event {
        event(EventPayload::ToolCallUpdated {
            call_id: call_id.to_string(),
            status: ToolCallStatus::Completed,
            output: Some(ContentBlock::text(output)),
        })
    }

    #[test]
    fn a_bash_background_call_starts_tracked_as_running() {
        let mut app = app_with_root_session();
        app.apply_session_event(&tool_call("call_1", "bash_background", serde_json::json!({ "command": "sleep 5" })));
        app.apply_session_event(&tool_call_completed("call_1", "bash-id-123"));

        assert_eq!(app.background_bash.len(), 1);
        assert_eq!(app.background_bash[0].id, "bash-id-123");
        assert!(app.background_bash[0].running);
    }

    #[test]
    fn a_bash_wait_call_completing_marks_the_matching_process_as_finished() {
        let mut app = app_with_root_session();
        app.apply_session_event(&tool_call("call_1", "bash_background", serde_json::json!({ "command": "sleep 5" })));
        app.apply_session_event(&tool_call_completed("call_1", "bash-id-123"));

        app.apply_session_event(&tool_call("call_2", "bash_wait", serde_json::json!({ "bash_id": "bash-id-123" })));
        app.apply_session_event(&tool_call_completed("call_2", "sleep 5\n"));

        assert_eq!(app.background_bash.len(), 1, "must update in place, not add a second entry");
        assert!(!app.background_bash[0].running);
    }

    #[test]
    fn a_bash_kill_call_completing_also_marks_the_process_as_finished() {
        let mut app = app_with_root_session();
        app.apply_session_event(&tool_call("call_1", "bash_background", serde_json::json!({ "command": "sleep 5" })));
        app.apply_session_event(&tool_call_completed("call_1", "bash-id-123"));

        app.apply_session_event(&tool_call("call_2", "bash_kill", serde_json::json!({ "bash_id": "bash-id-123" })));
        app.apply_session_event(&tool_call_completed("call_2", "killed"));

        assert!(!app.background_bash[0].running);
    }

    #[test]
    fn a_bash_output_call_does_not_change_the_running_state() {
        let mut app = app_with_root_session();
        app.apply_session_event(&tool_call("call_1", "bash_background", serde_json::json!({ "command": "sleep 5" })));
        app.apply_session_event(&tool_call_completed("call_1", "bash-id-123"));

        app.apply_session_event(&tool_call("call_2", "bash_output", serde_json::json!({ "bash_id": "bash-id-123" })));
        app.apply_session_event(&tool_call_completed("call_2", "partial output"));

        assert!(app.background_bash[0].running, "bash_output only peeks, it must not stop the process");
    }

    #[test]
    fn a_malformed_bash_wait_call_with_no_bash_id_is_tracked_as_nothing_in_particular() {
        let mut app = app_with_root_session();
        // Missing `bash_id` entirely -- must not panic, and must not be
        // mistaken for a `bash_background` start either.
        app.apply_session_event(&tool_call("call_1", "bash_wait", serde_json::json!({})));
        app.apply_session_event(&tool_call_completed("call_1", "bash-id-123"));

        assert!(app.background_bash.is_empty());
    }

    #[test]
    fn a_root_session_event_gets_no_tag_prefix() {
        let mut app = app_with_root_session();
        app.apply_session_event(&event(EventPayload::AgentMessageChunk { content: ContentBlock::text("hi") }));
        let TranscriptItem::Agent(text) = &app.transcript[0] else { panic!("expected Agent") };
        assert_eq!(text, "hi", "the root session's own events must not be tagged");
    }

    #[test]
    fn a_native_subagent_childs_events_are_tagged_and_dont_merge_with_the_roots_bubble() {
        let mut app = app_with_root_session();
        let child_id = SessionId::now_v7();
        app.apply_session_event(&event(EventPayload::ChildSessionSpawned {
            child: child_id,
            kind: ChildKind::NativeSubagent { role: "researcher".to_string() },
            spec: lh_event::ChildSpec { task_summary: "look into it".to_string() },
        }));
        app.apply_session_event(&event(EventPayload::AgentMessageChunk { content: ContentBlock::text("root talking") }));

        let child_event = Event::new(child_id, Some(root_session_id()), Actor::Agent, EventPayload::AgentMessageChunk {
            content: ContentBlock::text("child talking"),
        });
        app.apply_session_event(&child_event);

        // ChildSessionSpawned is silent bookkeeping (see its own match arm)
        // -- just the root bubble, then a separate child bubble, not merged.
        assert_eq!(app.transcript.len(), 2, "root bubble, then a separate child bubble, not merged");
        let TranscriptItem::Agent(root_text) = &app.transcript[0] else { panic!("expected Agent") };
        assert_eq!(root_text, "root talking");
        let TranscriptItem::Agent(child_text) = &app.transcript[1] else { panic!("expected Agent") };
        assert_eq!(child_text, "[subagent:researcher] child talking");
    }

    #[test]
    fn a_delegated_childs_events_are_tagged_with_the_agent_kind() {
        let mut app = app_with_root_session();
        let child_id = SessionId::now_v7();
        app.apply_session_event(&event(EventPayload::ChildSessionSpawned {
            child: child_id,
            kind: ChildKind::Delegated { agent: lh_event::AgentKind::ClaudeCode },
            spec: lh_event::ChildSpec { task_summary: "fix it".to_string() },
        }));
        let child_event = Event::new(child_id, Some(root_session_id()), Actor::Agent, EventPayload::AgentMessageChunk {
            content: ContentBlock::text("working on it"),
        });
        app.apply_session_event(&child_event);

        let TranscriptItem::Agent(text) = &app.transcript[0] else { panic!("expected Agent") };
        assert_eq!(text, "[delegated:ClaudeCode] working on it");
    }

    #[test]
    fn an_event_from_a_session_not_yet_known_as_a_child_falls_back_to_a_generic_tag() {
        let mut app = app_with_root_session();
        let unknown_session = SessionId::now_v7();
        let stray_event = Event::new(unknown_session, None, Actor::Agent, EventPayload::AgentMessageChunk {
            content: ContentBlock::text("???"),
        });
        app.apply_session_event(&stray_event);

        let TranscriptItem::Agent(text) = &app.transcript[0] else { panic!("expected Agent") };
        assert_eq!(text, "[child] ???");
    }

    #[test]
    fn consecutive_chunks_from_the_same_child_session_still_accumulate_into_one_bubble() {
        let mut app = app_with_root_session();
        let child_id = SessionId::now_v7();
        app.apply_session_event(&event(EventPayload::ChildSessionSpawned {
            child: child_id,
            kind: ChildKind::NativeSubagent { role: "researcher".to_string() },
            spec: lh_event::ChildSpec { task_summary: "look into it".to_string() },
        }));
        for text in ["Hel", "lo"] {
            let child_event =
                Event::new(child_id, Some(root_session_id()), Actor::Agent, EventPayload::AgentMessageChunk {
                    content: ContentBlock::text(text),
                });
            app.apply_session_event(&child_event);
        }

        let TranscriptItem::Agent(text) = &app.transcript[0] else { panic!("expected Agent") };
        assert_eq!(text, "[subagent:researcher] Hello", "still one bubble across two chunks from the same child");
    }
}
