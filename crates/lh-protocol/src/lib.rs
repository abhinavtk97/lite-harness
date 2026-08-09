//! The Harness Protocol: JSON-RPC 2.0 over newline-delimited frames
//! (architecture §4). This is what `lite-harnessd` speaks to its own
//! clients (CLI, web backend, headless callers) — distinct from ACP, which
//! is what the daemon speaks *outbound* to delegated agents.

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

pub type RequestId = i64;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Message {
    Request(Request),
    Response(Response),
    Notification(Notification),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: JsonRpcVersion,
    pub id: RequestId,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

impl Request {
    pub fn new(id: RequestId, method: impl Into<String>, params: serde_json::Value) -> Self {
        Self {
            jsonrpc: JsonRpcVersion,
            id,
            method: method.into(),
            params,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: JsonRpcVersion,
    pub id: RequestId,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<RpcError>,
}

impl Response {
    pub fn ok(id: RequestId, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: JsonRpcVersion,
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: RequestId, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: JsonRpcVersion,
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub jsonrpc: JsonRpcVersion,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

impl Notification {
    pub fn new(method: impl Into<String>, params: serde_json::Value) -> Self {
        Self {
            jsonrpc: JsonRpcVersion,
            method: method.into(),
            params,
        }
    }
}

/// Always serializes/deserializes as the literal string `"2.0"`.
#[derive(Debug, Clone, Copy, Default)]
pub struct JsonRpcVersion;

impl Serialize for JsonRpcVersion {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("2.0")
    }
}

impl<'de> Deserialize<'de> for JsonRpcVersion {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s != "2.0" {
            return Err(serde::de::Error::custom("unsupported jsonrpc version"));
        }
        Ok(JsonRpcVersion)
    }
}

// --- Harness Protocol method names, params, and results (architecture §4) ---

pub mod methods {
    pub const INITIALIZE: &str = "initialize";
    pub const SESSION_CREATE: &str = "session/create";
    pub const SESSION_TREE: &str = "session/tree";
    /// Streamed daemon -> client notification carrying an `lh_event::Event`.
    pub const EVENT: &str = "event";
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeParams {
    pub protocol_version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeResult {
    pub protocol_version: u32,
}

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCreateParams {
    pub cwd: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCreateResult {
    pub session_id: lh_event::SessionId,
}

// --- Newline-delimited JSON framing ---

pub async fn write_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg: &Message,
) -> std::io::Result<()> {
    let mut line = serde_json::to_string(msg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await
}

/// Reads one newline-delimited JSON message. Returns `Ok(None)` on clean EOF.
pub async fn read_message<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<Message>> {
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        return Ok(None);
    }
    let trimmed = line.trim_end();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let msg = serde_json::from_str(trimmed)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(Some(msg))
}

/// Convenience: wrap a raw async reader in a line-buffered reader.
pub fn buffered<R: tokio::io::AsyncRead>(reader: R) -> BufReader<R> {
    BufReader::new(reader)
}

/// Where the daemon listens and clients connect, for a given workspace
/// root. One daemon per workspace (architecture §2), not global — the path
/// is derived from a hash of the canonicalized cwd so repeated invocations
/// from the same workspace land on the same socket.
pub fn default_socket_path(cwd: &std::path::Path) -> std::path::PathBuf {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    cwd.hash(&mut hasher);
    let hash = hasher.finish();
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    runtime_dir
        .join("lite-harness")
        .join(format!("{hash:016x}.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn request_round_trips_over_a_pipe() {
        let (mut client, server) = tokio::io::duplex(4096);
        let mut server = buffered(server);

        let req = Message::Request(Request::new(
            1,
            methods::INITIALIZE,
            serde_json::to_value(InitializeParams {
                protocol_version: PROTOCOL_VERSION,
            })
            .unwrap(),
        ));
        write_message(&mut client, &req).await.unwrap();

        let got = read_message(&mut server).await.unwrap().unwrap();
        match got {
            Message::Request(r) => {
                assert_eq!(r.method, methods::INITIALIZE);
                assert_eq!(r.id, 1);
            }
            other => panic!("expected Request, got {other:?}"),
        }
    }

    #[test]
    fn message_distinguishes_variants_by_shape() {
        let notif = Notification::new(methods::EVENT, serde_json::json!({"hello": "world"}));
        let json = serde_json::to_string(&Message::Notification(notif)).unwrap();
        let parsed: Message = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Message::Notification(_)));

        let resp = Response::ok(1, serde_json::json!({"ok": true}));
        let json = serde_json::to_string(&Message::Response(resp)).unwrap();
        let parsed: Message = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Message::Response(_)));
    }
}
