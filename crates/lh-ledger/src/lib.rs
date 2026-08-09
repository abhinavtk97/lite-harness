//! The cost ledger (architecture §7): a derived read-model over the event
//! log, never a separately-mutated counter. `CostLedger::rollup` folds
//! every `UsageReported` event across a session tree into one number,
//! propagating `Unknown` confidence upward rather than silently treating
//! a missing figure as zero -- a subtree with any unpriced turn in it is
//! flagged partial, not quietly wrong.
//!
//! Pricing itself (turning token counts into a dollar figure) lives in
//! [`pricing`] -- `lh-native-agent` calls it directly when it reports each
//! turn's usage, so by the time an event lands in the log its `cost_usd`
//! is already resolved (or honestly `None`). This crate's own job is pure
//! aggregation of numbers that already exist.

pub mod pricing;

pub use pricing::{price_usage, ModelPrice, PricingTable};

use async_trait::async_trait;
use futures::future::BoxFuture;
use lh_event::{EventPayload, SessionId, UsageConfidence, UsageDelta};
use lh_store::{SessionStore, SessionTreeNode};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub type Result<T> = std::result::Result<T, LedgerError>;

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("store error: {0}")]
    Store(#[from] lh_store::StoreError),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LedgerRollup {
    pub session_id: SessionId,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
    /// Number of `UsageReported` events in this session alone (not
    /// counting children).
    pub turns: usize,
    /// Worst confidence seen anywhere in this subtree (this session and
    /// all its children) -- `Unknown` if any turn anywhere couldn't be
    /// priced or was missing token counts.
    pub confidence: UsageConfidence,
    pub children: Vec<LedgerRollup>,
}

#[async_trait]
pub trait CostLedger: Send + Sync {
    /// Rolls up cost/usage for `session_id` and every descendant in its
    /// session tree (native subagents and delegated agents alike, once
    /// those exist -- architecture §9/§12).
    async fn rollup(&self, session_id: SessionId) -> Result<LedgerRollup>;
}

pub struct StoreBackedCostLedger {
    store: Arc<dyn SessionStore>,
}

impl StoreBackedCostLedger {
    pub fn new(store: Arc<dyn SessionStore>) -> Self {
        Self { store }
    }

    fn rollup_node<'a>(&'a self, node: &'a SessionTreeNode) -> BoxFuture<'a, Result<LedgerRollup>> {
        Box::pin(async move {
            let events = self.store.read_from(node.session_id, 0).await?;

            let mut acc = NodeAccumulator::default();
            for event in &events {
                if let EventPayload::UsageReported { usage } = &event.payload {
                    acc.add(usage);
                }
            }

            let mut rollup = acc.into_rollup(node.session_id);
            for child in &node.children {
                let child_rollup = self.rollup_node(child).await?;
                rollup = combine(rollup, child_rollup);
            }
            Ok(rollup)
        })
    }
}

#[async_trait]
impl CostLedger for StoreBackedCostLedger {
    async fn rollup(&self, session_id: SessionId) -> Result<LedgerRollup> {
        let tree = self.store.session_tree(session_id).await?;
        self.rollup_node(&tree).await
    }
}

#[derive(Default)]
struct NodeAccumulator {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cost_usd: Option<f64>,
    turns: usize,
    confidence: Option<UsageConfidence>,
}

impl NodeAccumulator {
    fn add(&mut self, usage: &UsageDelta) {
        self.turns += 1;
        self.input_tokens = add_opt_u64(self.input_tokens, usage.input_tokens);
        self.output_tokens = add_opt_u64(self.output_tokens, usage.output_tokens);
        self.cost_usd = add_opt_f64(self.cost_usd, usage.cost_usd);
        self.confidence = Some(match self.confidence {
            Some(existing) => worse(existing, usage.confidence),
            None => usage.confidence,
        });
    }

    fn into_rollup(self, session_id: SessionId) -> LedgerRollup {
        LedgerRollup {
            session_id,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cost_usd: self.cost_usd,
            turns: self.turns,
            // A session with zero turns of its own (e.g. a parent that
            // only ever delegated) starts at the best confidence -- it
            // has nothing of its own to be unsure about; combining with
            // children below still poisons this if they warrant it.
            confidence: self.confidence.unwrap_or(UsageConfidence::Exact),
            children: Vec::new(),
        }
    }
}

fn add_opt_u64(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x + y),
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (None, None) => None,
    }
}

fn add_opt_f64(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x + y),
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (None, None) => None,
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

fn combine(mut parent: LedgerRollup, child: LedgerRollup) -> LedgerRollup {
    parent.input_tokens = add_opt_u64(parent.input_tokens, child.input_tokens);
    parent.output_tokens = add_opt_u64(parent.output_tokens, child.output_tokens);
    parent.cost_usd = add_opt_f64(parent.cost_usd, child.cost_usd);
    parent.confidence = worse(parent.confidence, child.confidence);
    parent.children.push(child);
    parent
}

#[cfg(test)]
mod tests {
    use super::*;
    use lh_event::{Actor, ContentBlock, Event, EventPayload, SessionDriver};
    use lh_store::SqliteSessionStore;

    fn usage_event(session_id: SessionId, usage: UsageDelta) -> Event {
        Event::new(session_id, None, Actor::System, EventPayload::UsageReported { usage })
    }

    fn driver_event(session_id: SessionId, parent: Option<SessionId>) -> Event {
        Event::new(
            session_id,
            parent,
            Actor::System,
            EventPayload::SessionDriverSet { driver: SessionDriver::Native },
        )
    }

    fn exact(input: u64, output: u64, cost: f64) -> UsageDelta {
        UsageDelta {
            input_tokens: Some(input),
            output_tokens: Some(output),
            cost_usd: Some(cost),
            wall_ms: 10,
            confidence: UsageConfidence::Exact,
        }
    }

    fn unknown() -> UsageDelta {
        UsageDelta {
            input_tokens: None,
            output_tokens: None,
            cost_usd: None,
            wall_ms: 10,
            confidence: UsageConfidence::Unknown,
        }
    }

    #[tokio::test]
    async fn rolls_up_multiple_turns_in_one_session() {
        let store = Arc::new(SqliteSessionStore::open_in_memory().unwrap());
        let session_id = SessionId::now_v7();
        store.append(driver_event(session_id, None)).await.unwrap();
        store.append(usage_event(session_id, exact(100, 50, 0.01))).await.unwrap();
        store.append(usage_event(session_id, exact(200, 100, 0.02))).await.unwrap();

        let ledger = StoreBackedCostLedger::new(store);
        let rollup = ledger.rollup(session_id).await.unwrap();

        assert_eq!(rollup.turns, 2);
        assert_eq!(rollup.input_tokens, Some(300));
        assert_eq!(rollup.output_tokens, Some(150));
        assert!((rollup.cost_usd.unwrap() - 0.03).abs() < 1e-9);
        assert_eq!(rollup.confidence, UsageConfidence::Exact);
    }

    #[tokio::test]
    async fn an_unknown_turn_poisons_confidence_but_the_known_total_survives() {
        let store = Arc::new(SqliteSessionStore::open_in_memory().unwrap());
        let session_id = SessionId::now_v7();
        store.append(driver_event(session_id, None)).await.unwrap();
        store.append(usage_event(session_id, exact(100, 50, 0.01))).await.unwrap();
        store.append(usage_event(session_id, unknown())).await.unwrap();

        let ledger = StoreBackedCostLedger::new(store);
        let rollup = ledger.rollup(session_id).await.unwrap();

        assert_eq!(rollup.turns, 2);
        // The known turn's cost is not thrown away just because a sibling
        // turn was unpriced -- it's an honest partial total.
        assert_eq!(rollup.cost_usd, Some(0.01));
        assert_eq!(rollup.confidence, UsageConfidence::Unknown);
    }

    #[tokio::test]
    async fn a_child_sessions_cost_rolls_into_the_parent_and_poisons_confidence() {
        let store = Arc::new(SqliteSessionStore::open_in_memory().unwrap());
        let root = SessionId::now_v7();
        let child = SessionId::now_v7();

        store.append(driver_event(root, None)).await.unwrap();
        store.append(usage_event(root, exact(100, 50, 0.01))).await.unwrap();

        store.append(driver_event(child, Some(root))).await.unwrap();
        store.append(usage_event(child, unknown())).await.unwrap();

        let ledger = StoreBackedCostLedger::new(store);
        let rollup = ledger.rollup(root).await.unwrap();

        assert_eq!(rollup.children.len(), 1);
        assert_eq!(rollup.children[0].session_id, child);
        // Root's own turn was priced fine...
        assert_eq!(rollup.cost_usd, Some(0.01));
        // ...but the tree-wide confidence reflects the unpriced child.
        assert_eq!(rollup.confidence, UsageConfidence::Unknown);
    }

    #[tokio::test]
    async fn a_session_with_no_usage_events_rolls_up_to_zero_turns_not_an_error() {
        let store = Arc::new(SqliteSessionStore::open_in_memory().unwrap());
        let session_id = SessionId::now_v7();
        store.append(driver_event(session_id, None)).await.unwrap();
        store
            .append(Event::new(
                session_id,
                None,
                Actor::User,
                EventPayload::UserMessage { content: vec![ContentBlock::text("hi")] },
            ))
            .await
            .unwrap();

        let ledger = StoreBackedCostLedger::new(store);
        let rollup = ledger.rollup(session_id).await.unwrap();

        assert_eq!(rollup.turns, 0);
        assert_eq!(rollup.cost_usd, None);
        assert_eq!(rollup.confidence, UsageConfidence::Exact);
    }
}
