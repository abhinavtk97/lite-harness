//! Driver-neutral supervisor abstraction (architecture §9, §12.3): the
//! shared shape between "spawn a native subagent" and "delegate to an ACP
//! agent" -- same session-tree, permission, and cost machinery
//! underneath, only the driving mechanism (in-process task vs. an ACP
//! round-trip to a subprocess) differs. `lh-native-agent` and `lh-acp`
//! each implement `ChildRunner` against their own concrete machinery;
//! this crate defines only the shared shape, so it depends on nothing but
//! `lh-event` -- neither of those crates depends on the other, both
//! depend on this.
//!
//! Extracted in Phase 6 once both a native `ChildRunner` impl (Phase 5's
//! `NativeAgentLoop::run_subagent`) and an ACP one (Phase 4's
//! `lh_acp::delegate::run_delegation`) already existed to generalize
//! over -- doing this earlier would have been guessing at the shape.

use async_trait::async_trait;
use lh_event::{ChildOutcome, SessionId};

pub type Result<T> = std::result::Result<T, anyhow::Error>;

/// A self-contained task handed to a child session (native subagent or
/// delegated agent alike) -- deliberately not the parent's transcript,
/// only these explicit instructions, so context isolation is structural
/// rather than a discipline the parent has to maintain (architecture §9).
/// `tool_allowlist`/`max_turns` are native-subagent-specific knobs; an ACP
/// `ChildRunner` simply doesn't use them (its own agent controls both).
#[derive(Debug, Clone)]
pub struct TaskHandoff {
    pub role: String,
    pub instructions: String,
    pub tool_allowlist: Option<Vec<String>>,
    pub max_turns: Option<usize>,
}

/// Runs one child task to completion. Returns the child's own freshly
/// minted `SessionId` alongside the outcome -- a deliberate adaptation of
/// the architecture doc's original `run(parent, task) -> Result<ChildOutcome>`
/// sketch, which gave the caller no way to learn the new session's id at
/// all, even though `session/delegate`'s own response shape
/// (`SessionDelegateResult { child_session_id, outcome }`) has always
/// needed it.
#[async_trait]
pub trait ChildRunner: Send + Sync {
    async fn run(&self, parent_session_id: SessionId, task: TaskHandoff) -> Result<(SessionId, ChildOutcome)>;
}
