//! A bidirectional JSON-RPC connection to an ACP-speaking agent subprocess
//! (architecture §5) -- the ACP-side generalization of the exact
//! request/response/notification pattern the daemon<->CLI connection uses
//! for `permission/ask` (see `lh-daemon/src/permission.rs`): we send
//! Requests and await Responses, we receive Notifications, and we receive
//! Requests we must reply to, all on one duplex stream.
//!
//! The wire envelope and every message type is `agent_client_protocol_schema`
//! (the *schema* crate, not the higher-level `agent-client-protocol` crate
//! with its `Role`/`Builder`/`Dispatch` transport machinery -- see the plan
//! doc for why). That crate already provides the generic JSON-RPC envelope
//! (`v1::{Request, Response, Notification, RequestId}`) and pre-built
//! routing enums for exactly the two directions this connection needs:
//! `v1::{ClientRequest, AgentResponse, ClientNotification}` for what *we*
//! send, and `v1::{AgentRequest, ClientResponse, AgentNotification}` for
//! what we receive -- so no method-string matching is needed for dispatch,
//! only untagged-by-shape deserialization the crate already implements.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use agent_client_protocol_schema::v1::{
    self as acp, Error as AcpError, JsonRpcMessage, Notification as AcpNotification,
    Request as AcpRequest, RequestId as AcpRequestId, Response as AcpResponse,
};
use tokio::io::BufReader;
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::{oneshot, Mutex};

use crate::client::HarnessAcpClient;
use crate::registry::DelegatedAgentAdapter;

pub type Result<T> = std::result::Result<T, AcpTransportError>;

#[derive(Debug, thiserror::Error)]
pub enum AcpTransportError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to spawn agent subprocess: {0}")]
    Spawn(String),
    #[error("agent returned an error: {0} ({1:?})")]
    Agent(String, agent_client_protocol_schema::v1::ErrorCode),
    #[error("connection closed before a response arrived")]
    ConnectionClosed,
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

/// The three JSON-RPC message shapes, with raw (unparsed) params/results --
/// deliberately not tied to any one method's concrete types, since which
/// type applies depends on `method`/direction, decided after this envelope
/// is parsed. Mirrors `lh_protocol::Message`'s own untagged-by-shape design
/// (proven across four phases), reusing `lh_protocol`'s generic
/// `write_json_line`/`read_json_line` for the actual byte framing.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
enum AcpEnvelope {
    Request(AcpRequest<serde_json::Value>),
    Response(AcpResponse<serde_json::Value>),
    Notification(AcpNotification<serde_json::Value>),
}

async fn write_envelope(writer: &mut ChildStdin, envelope: AcpEnvelope) -> Result<()> {
    let wrapped = JsonRpcMessage::wrap(envelope);
    lh_protocol::write_json_line(writer, &wrapped).await?;
    Ok(())
}

/// A live connection to one spawned ACP agent subprocess, for one
/// delegated (child) session.
pub struct AcpConnection {
    child: tokio::process::Child,
    writer: Arc<Mutex<ChildStdin>>,
    next_id: Arc<Mutex<i64>>,
    pending: Arc<Mutex<HashMap<AcpRequestId, oneshot::Sender<std::result::Result<serde_json::Value, AcpError>>>>>,
}

impl AcpConnection {
    /// Spawns the adapter's subprocess and starts the background read loop
    /// that drives `client`'s callbacks for the lifetime of the connection.
    /// Does *not* perform the ACP `initialize`/`session/new` handshake --
    /// that's `delegate::run_delegation`'s job, using `call()` below.
    pub async fn spawn(
        adapter: &DelegatedAgentAdapter,
        workspace_root: &Path,
        client: Arc<HarnessAcpClient>,
    ) -> Result<Self> {
        let env = adapter
            .spawn
            .resolved_env()
            .map_err(|e| AcpTransportError::Spawn(e.to_string()))?;

        let mut command = tokio::process::Command::new(&adapter.spawn.command);
        command
            .args(&adapter.spawn.args)
            .current_dir(workspace_root)
            .envs(env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true);

        let mut child = command
            .spawn()
            .map_err(|e| AcpTransportError::Spawn(e.to_string()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AcpTransportError::Spawn("child had no stdin".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AcpTransportError::Spawn("child had no stdout".to_string()))?;

        let writer = Arc::new(Mutex::new(stdin));
        let next_id = Arc::new(Mutex::new(1i64));
        let pending: Arc<Mutex<HashMap<AcpRequestId, oneshot::Sender<std::result::Result<serde_json::Value, AcpError>>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        spawn_read_loop(stdout, pending.clone(), writer.clone(), client);

        Ok(Self {
            child,
            writer,
            next_id,
            pending,
        })
    }

    /// Sends a Request and awaits its Response's `result`, deserialized as
    /// `T`. `params` should already be the serialized concrete request
    /// struct (e.g. `serde_json::to_value(InitializeRequest::new(..))`).
    pub async fn call<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<T> {
        let id = {
            let mut guard = self.next_id.lock().await;
            let id = *guard;
            *guard += 1;
            AcpRequestId::Number(id)
        };

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), tx);

        let request = AcpRequest {
            id: id.clone(),
            method: method.into(),
            params: Some(params),
        };
        {
            let mut w = self.writer.lock().await;
            write_envelope(&mut w, AcpEnvelope::Request(request)).await?;
        }

        match rx.await {
            Ok(Ok(value)) => Ok(serde_json::from_value(value)?),
            Ok(Err(e)) => Err(AcpTransportError::Agent(e.message.clone(), e.code)),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(AcpTransportError::ConnectionClosed)
            }
        }
    }

    pub async fn kill(&mut self) -> Result<()> {
        self.child.kill().await?;
        Ok(())
    }
}

/// Drives the read loop for the lifetime of the child process: routes
/// Responses back to whichever `call()` is waiting on that id, dispatches
/// Notifications (`session/update`) to `client`, and answers Requests the
/// agent sends us (`session/request_permission`, `fs/read_text_file`,
/// `fs/write_text_file`, `terminal/*`) by calling into `client` and
/// writing a Response back -- the bidirectional trifecta this connection
/// exists for.
fn spawn_read_loop(
    stdout: ChildStdout,
    pending: Arc<Mutex<HashMap<AcpRequestId, oneshot::Sender<std::result::Result<serde_json::Value, AcpError>>>>>,
    writer: Arc<Mutex<ChildStdin>>,
    client: Arc<HarnessAcpClient>,
) {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        loop {
            let wrapped: Option<JsonRpcMessage<AcpEnvelope>> =
                match lh_protocol::read_json_line(&mut reader).await {
                    Ok(v) => v,
                    Err(_) => break,
                };
            let Some(wrapped) = wrapped else { break };
            let envelope = wrapped.inner().clone();

            match envelope {
                AcpEnvelope::Response(resp) => {
                    let (id, outcome) = match resp {
                        AcpResponse::Result { id, result } => (id, Ok(result)),
                        AcpResponse::Error { id, error } => (id, Err(error)),
                    };
                    if let Some(tx) = pending.lock().await.remove(&id) {
                        let _ = tx.send(outcome);
                    }
                }
                AcpEnvelope::Notification(notif) => {
                    if notif.method.as_ref() == "session/update" {
                        if let Some(params) = notif.params {
                            match serde_json::from_value::<acp::SessionNotification>(params) {
                                Ok(session_notif) => {
                                    if let Err(e) = client.handle_session_update(session_notif).await {
                                        eprintln!("[lh-acp] session/update handling failed: {e:#}");
                                    }
                                }
                                Err(e) => eprintln!("[lh-acp] failed to parse session/update: {e}"),
                            }
                        }
                    }
                }
                AcpEnvelope::Request(req) => {
                    let params = req.params.unwrap_or(serde_json::Value::Null);
                    let outcome = handle_incoming_request(&req.method, params, &client).await;
                    let response = match outcome {
                        Ok(result) => AcpResponse::Result { id: req.id.clone(), result },
                        Err(e) => AcpResponse::Error { id: req.id.clone(), error: e },
                    };
                    let mut w = writer.lock().await;
                    let _ = write_envelope(&mut w, AcpEnvelope::Response(response)).await;
                }
            }
        }
    });
}

async fn handle_incoming_request(
    method: &str,
    params: serde_json::Value,
    client: &Arc<HarnessAcpClient>,
) -> std::result::Result<serde_json::Value, AcpError> {
    match method {
        "session/request_permission" => {
            let req: acp::RequestPermissionRequest =
                serde_json::from_value(params).map_err(|e| AcpError::invalid_params().data(e.to_string()))?;
            let resp = client
                .handle_request_permission(req)
                .await
                .map_err(|e| AcpError::internal_error().data(e.to_string()))?;
            serde_json::to_value(resp).map_err(|e| AcpError::internal_error().data(e.to_string()))
        }
        "fs/read_text_file" => {
            let req: acp::ReadTextFileRequest =
                serde_json::from_value(params).map_err(|e| AcpError::invalid_params().data(e.to_string()))?;
            let resp = client
                .handle_read_text_file(req)
                .await
                .map_err(|e| AcpError::internal_error().data(e.to_string()))?;
            serde_json::to_value(resp).map_err(|e| AcpError::internal_error().data(e.to_string()))
        }
        "fs/write_text_file" => {
            let req: acp::WriteTextFileRequest =
                serde_json::from_value(params).map_err(|e| AcpError::invalid_params().data(e.to_string()))?;
            let resp = client
                .handle_write_text_file(req)
                .await
                .map_err(|e| AcpError::internal_error().data(e.to_string()))?;
            serde_json::to_value(resp).map_err(|e| AcpError::internal_error().data(e.to_string()))
        }
        "terminal/create" => {
            let req: acp::CreateTerminalRequest =
                serde_json::from_value(params).map_err(|e| AcpError::invalid_params().data(e.to_string()))?;
            let resp = client
                .handle_terminal_create(req)
                .await
                .map_err(|e| AcpError::internal_error().data(e.to_string()))?;
            serde_json::to_value(resp).map_err(|e| AcpError::internal_error().data(e.to_string()))
        }
        "terminal/output" => {
            let req: acp::TerminalOutputRequest =
                serde_json::from_value(params).map_err(|e| AcpError::invalid_params().data(e.to_string()))?;
            let resp = client
                .handle_terminal_output(req)
                .await
                .map_err(|e| AcpError::internal_error().data(e.to_string()))?;
            serde_json::to_value(resp).map_err(|e| AcpError::internal_error().data(e.to_string()))
        }
        "terminal/wait_for_exit" => {
            let req: acp::WaitForTerminalExitRequest =
                serde_json::from_value(params).map_err(|e| AcpError::invalid_params().data(e.to_string()))?;
            let resp = client
                .handle_wait_for_terminal_exit(req)
                .await
                .map_err(|e| AcpError::internal_error().data(e.to_string()))?;
            serde_json::to_value(resp).map_err(|e| AcpError::internal_error().data(e.to_string()))
        }
        "terminal/kill" => {
            let req: acp::KillTerminalRequest =
                serde_json::from_value(params).map_err(|e| AcpError::invalid_params().data(e.to_string()))?;
            let resp = client
                .handle_kill_terminal(req)
                .await
                .map_err(|e| AcpError::internal_error().data(e.to_string()))?;
            serde_json::to_value(resp).map_err(|e| AcpError::internal_error().data(e.to_string()))
        }
        "terminal/release" => {
            let req: acp::ReleaseTerminalRequest =
                serde_json::from_value(params).map_err(|e| AcpError::invalid_params().data(e.to_string()))?;
            let resp = client
                .handle_terminal_release(req)
                .await
                .map_err(|e| AcpError::internal_error().data(e.to_string()))?;
            serde_json::to_value(resp).map_err(|e| AcpError::internal_error().data(e.to_string()))
        }
        other => {
            eprintln!("[lh-acp] no handler for incoming method: {other}");
            Err(AcpError::method_not_found())
        }
    }
}
