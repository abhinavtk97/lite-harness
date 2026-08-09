//! Bridges the transport-agnostic `PermissionPrompter` trait to this
//! connection's socket via a genuine bidirectional JSON-RPC round trip:
//! `ask()` sends a `permission/ask` *request* to the client (never a
//! notification) and awaits the matching *response* -- so this only ever
//! fires when a live human decision is actually needed (the permission
//! engine found no matching policy rule), not for every
//! `PermissionRequested` audit-log event the way the earlier
//! event-triggered design did.
//!
//! Phase 2/3 simplification retained: at most one permission ask is ever
//! outstanding per connection (the native agent loop handles tool calls
//! sequentially), so a single pending slot is sufficient -- an
//! id-keyed map (for genuinely concurrent asks, e.g. from parallel
//! subagents or a delegated ACP agent running alongside the native loop)
//! is a mechanical upgrade for whenever that concurrency actually shows up.

use std::sync::Arc;

use async_trait::async_trait;
use lh_event::{PermissionDecision, PermissionRequest};
use lh_permission::{PermissionError, PermissionPrompter};
use lh_protocol::{methods, write_message, Message, PermissionAskParams, Request as ProtoRequest, RequestId};
use tokio::sync::{oneshot, Mutex};

use crate::SharedWriter;

#[derive(Clone)]
pub struct SocketPrompter {
    write_half: SharedWriter,
    /// Daemon-originated request ids: start at -1 and decrement. The
    /// client's own outgoing ids (see `lh-cli`) start at 1 and only
    /// increment, so the two id spaces can never collide on the same
    /// connection -- structurally, not just by convention.
    next_id: Arc<Mutex<RequestId>>,
    pending: Arc<Mutex<Option<(RequestId, oneshot::Sender<PermissionDecision>)>>>,
}

impl SocketPrompter {
    pub fn new(write_half: SharedWriter) -> Self {
        Self {
            write_half,
            next_id: Arc::new(Mutex::new(-1)),
            pending: Arc::new(Mutex::new(None)),
        }
    }

    async fn next_request_id(&self) -> RequestId {
        let mut guard = self.next_id.lock().await;
        let id = *guard;
        *guard -= 1;
        id
    }

    /// Called by the connection's read loop when an incoming
    /// `Message::Response` arrives from the client. `id` must match the
    /// id `ask()` most recently allocated; a mismatch leaves the pending
    /// ask untouched (it's either stray/duplicate, or belongs to
    /// something else) rather than being consumed. Returns `true` iff a
    /// pending ask was actually resolved.
    pub async fn resolve(&self, id: RequestId, decision: PermissionDecision) -> bool {
        let mut guard = self.pending.lock().await;
        match guard.take() {
            Some((pending_id, tx)) if pending_id == id => {
                let _ = tx.send(decision);
                true
            }
            Some(other) => {
                *guard = Some(other);
                false
            }
            None => false,
        }
    }
}

#[async_trait]
impl PermissionPrompter for SocketPrompter {
    async fn ask(&self, request: &PermissionRequest) -> lh_permission::Result<PermissionDecision> {
        let (tx, rx) = oneshot::channel();
        let id = self.next_request_id().await;
        *self.pending.lock().await = Some((id, tx));

        let params = serde_json::to_value(PermissionAskParams {
            request: request.clone(),
        })
        .map_err(|e| PermissionError::PromptFailed(format!("serializing permission/ask params: {e}")))?;
        let msg = Message::Request(ProtoRequest::new(id, methods::PERMISSION_ASK, params));
        {
            let mut w = self.write_half.lock().await;
            write_message(&mut *w, &msg)
                .await
                .map_err(|e| PermissionError::PromptFailed(format!("writing permission/ask: {e}")))?;
        }

        rx.await.map_err(|_| {
            PermissionError::PromptFailed(
                "connection closed while waiting for a permission decision".to_string(),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lh_event::{PermissionAction, RiskTier, SessionId, ToolSource};
    use lh_protocol::{buffered, read_message};
    use std::path::PathBuf;
    use tokio::sync::Mutex as AsyncMutex;

    fn sample_request() -> PermissionRequest {
        PermissionRequest {
            session_id: SessionId::now_v7(),
            tool_source: ToolSource::Native {
                tool_id: "bash".to_string(),
            },
            action: PermissionAction::Exec {
                command: "ls".to_string(),
                args: vec![],
                cwd: PathBuf::from("."),
            },
            risk_tier: RiskTier::Execute,
        }
    }

    #[tokio::test]
    async fn ask_writes_one_permission_ask_request_with_a_negative_id() {
        let (client_side, server_side) = tokio::net::UnixStream::pair().unwrap();
        let (_server_read, server_write) = server_side.into_split();
        let (client_read, _client_write) = client_side.into_split();
        let write_half: SharedWriter = Arc::new(AsyncMutex::new(server_write));
        let prompter = SocketPrompter::new(write_half);

        let ask_task = tokio::spawn({
            let prompter = prompter.clone();
            async move { prompter.ask(&sample_request()).await }
        });

        let mut reader = buffered(client_read);
        let got = read_message(&mut reader).await.unwrap().unwrap();
        let Message::Request(req) = got else {
            panic!("expected Request, got {got:?}");
        };
        assert_eq!(req.method, methods::PERMISSION_ASK);
        assert!(req.id < 0);

        // Resolve it so the spawned ask() task can finish cleanly.
        prompter
            .resolve(req.id, PermissionDecision::Allow)
            .await;
        let decision = ask_task.await.unwrap().unwrap();
        assert!(matches!(decision, PermissionDecision::Allow));
    }

    #[tokio::test]
    async fn resolve_with_the_wrong_id_leaves_the_ask_pending() {
        let (client_side, server_side) = tokio::net::UnixStream::pair().unwrap();
        let (_server_read, server_write) = server_side.into_split();
        let (client_read, _client_write) = client_side.into_split();
        let write_half: SharedWriter = Arc::new(AsyncMutex::new(server_write));
        let prompter = SocketPrompter::new(write_half);

        let ask_task = tokio::spawn({
            let prompter = prompter.clone();
            async move { prompter.ask(&sample_request()).await }
        });

        let mut reader = buffered(client_read);
        let got = read_message(&mut reader).await.unwrap().unwrap();
        let Message::Request(req) = got else {
            panic!("expected Request, got {got:?}");
        };

        let resolved = prompter.resolve(req.id - 100, PermissionDecision::Deny).await;
        assert!(!resolved);

        let resolved = prompter.resolve(req.id, PermissionDecision::Allow).await;
        assert!(resolved);
        let decision = ask_task.await.unwrap().unwrap();
        assert!(matches!(decision, PermissionDecision::Allow));
    }
}
