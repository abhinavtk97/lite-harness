//! Bridges the transport-agnostic `PermissionPrompter` trait to this
//! connection's socket. Phase 2 simplification: at most one permission ask
//! is ever outstanding per connection (the native agent loop handles tool
//! calls sequentially), so a single pending slot is sufficient -- a
//! call-id-keyed map (for genuinely concurrent asks, e.g. from parallel
//! subagents) is a mechanical upgrade for whenever that concurrency
//! actually shows up.

use std::sync::Arc;

use async_trait::async_trait;
use lh_event::{PermissionDecision, PermissionRequest};
use lh_permission::{PermissionError, PermissionPrompter};
use tokio::sync::{oneshot, Mutex};

#[derive(Clone)]
pub struct SocketPrompter {
    pending: Arc<Mutex<Option<oneshot::Sender<PermissionDecision>>>>,
}

impl SocketPrompter {
    pub fn new() -> Self {
        Self {
            pending: Arc::new(Mutex::new(None)),
        }
    }

    /// Called by the connection's read loop when a `permission/respond`
    /// request arrives. Returns `true` if there was a pending ask to
    /// resolve.
    pub async fn resolve(&self, decision: PermissionDecision) -> bool {
        if let Some(tx) = self.pending.lock().await.take() {
            let _ = tx.send(decision);
            true
        } else {
            false
        }
    }
}

impl Default for SocketPrompter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PermissionPrompter for SocketPrompter {
    async fn ask(&self, _request: &PermissionRequest) -> lh_permission::Result<PermissionDecision> {
        let (tx, rx) = oneshot::channel();
        *self.pending.lock().await = Some(tx);
        rx.await.map_err(|_| {
            PermissionError::PromptFailed(
                "connection closed while waiting for a permission decision".to_string(),
            )
        })
    }
}
