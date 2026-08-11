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
    pub const SESSION_PROMPT: &str = "session/prompt";
    pub const SESSION_TREE: &str = "session/tree";
    /// Daemon -> client request, sent only when a live human decision is
    /// actually needed (i.e. the permission engine found no matching
    /// policy rule) -- never for a policy-short-circuited decision. See
    /// `PermissionAskParams`/`PermissionAskResult`.
    pub const PERMISSION_ASK: &str = "permission/ask";
    pub const LEDGER_QUERY: &str = "ledger/query";
    /// Delegates one task to an external ACP-speaking agent as a child
    /// session of the caller's session (architecture §5, §11 phase 4).
    /// Root-session substitution (an ACP agent driving the *root*, not a
    /// child) is `PrimarySelector` at `session/create`, Phase 6/§12 -- not
    /// this method.
    pub const SESSION_DELEGATE: &str = "session/delegate";
    /// Queryable any number of times *before* `session/create` (architecture
    /// §12.5) -- a UI needs to know which delegated agents are registered,
    /// and which of those `can_be_primary`, before it can build a "which
    /// agent should drive this session?" picker that only offers valid
    /// `PrimarySelector` choices. Not restricted to before `session/create`
    /// structurally, just conventionally placed there since that's the only
    /// point a client actually needs it.
    pub const AGENTS_LIST: &str = "agents/list";
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
    /// Which driver owns the *root* session (architecture §12) -- defaults
    /// to `Native` so every pre-Phase-6 caller that never set this field
    /// keeps today's behavior unchanged. `session/delegate` (unaffected by
    /// this field) remains the way a `Native` root hands off one task to a
    /// child; this is the orthogonal "the whole session is driven by an
    /// external agent from the start" capability.
    #[serde(default)]
    pub primary: PrimarySelector,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum PrimarySelector {
    #[default]
    Native,
    Delegated { agent: lh_event::AgentKind },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCreateResult {
    pub session_id: lh_event::SessionId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionPromptParams {
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionPromptResult {
    pub stop_reason: String,
}

/// Params for a daemon-initiated `permission/ask` request. No `call_id`
/// field -- `PermissionPrompter::ask()` never receives one (the agent loop
/// tracks call ids separately for its own `PermissionRequested`/
/// `PermissionDecided` events); correlation with the reply happens purely
/// via the JSON-RPC request id, and the CLI's prompt only ever needed the
/// `PermissionRequest` itself to render.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionAskParams {
    pub request: lh_event::PermissionRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionAskResult {
    pub decision: lh_event::PermissionDecision,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerQueryParams {
    pub session_id: lh_event::SessionId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerQueryResult {
    pub rollup: lh_ledger::LedgerRollup,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDelegateParams {
    pub agent: lh_event::AgentKind,
    pub task_summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDelegateResult {
    pub child_session_id: lh_event::SessionId,
    pub outcome: lh_event::ChildOutcome,
}

/// No fields today -- a struct (not `()`) so a filter (e.g. "only agents
/// that can be primary") has somewhere to go later without a breaking
/// wire-shape change.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentsListParams {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    pub kind: lh_event::AgentKind,
    /// §12.5 -- whether `PrimarySelector::Delegated { agent: kind }` at
    /// `session/create` would be accepted for this adapter, mirroring
    /// `DelegatedAgentAdapter.can_be_primary` exactly (that field is the
    /// registry's own; this is its public, protocol-level reflection).
    pub can_be_primary: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentsListResult {
    pub agents: Vec<AgentInfo>,
}

// --- Newline-delimited JSON framing ---
//
// Generic over the serialized type so a second, ACP-specific JSON-RPC
// envelope (`lh-acp`, architecture §5 -- ACP's `id` is untagged
// null/number/string, unlike this protocol's `RequestId = i64`) can reuse
// the exact same byte-level framing without duplicating it.

pub async fn write_json_line<W: AsyncWrite + Unpin, T: Serialize>(
    writer: &mut W,
    msg: &T,
) -> std::io::Result<()> {
    let mut line = serde_json::to_string(msg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await
}

/// Reads one newline-delimited JSON value. Returns `Ok(None)` on clean EOF
/// or a blank line.
pub async fn read_json_line<R: AsyncBufReadExt + Unpin, T: serde::de::DeserializeOwned>(
    reader: &mut R,
) -> std::io::Result<Option<T>> {
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

pub async fn write_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg: &Message,
) -> std::io::Result<()> {
    write_json_line(writer, msg).await
}

/// Reads one newline-delimited JSON message. Returns `Ok(None)` on clean EOF.
pub async fn read_message<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<Message>> {
    read_json_line(reader).await
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

// --- Daemon connection bootstrap ---
//
// Shared by every Harness Protocol client that needs to reach a
// per-workspace daemon and is willing to spawn one if it isn't already
// running -- `lh-cli` and `lh-web-backend` both do exactly this, so it
// lives here once rather than twice (the same "no parallel mechanism"
// discipline applied everywhere else in this project).

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("io error resolving current executable: {0}")]
    CurrentExe(#[source] std::io::Error),
    #[error("current executable has no parent directory")]
    NoParentDir,
    #[error("failed to spawn daemon at {path}: {source}")]
    Spawn {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("timed out waiting for lite-harnessd to start listening at {0}")]
    Timeout(std::path::PathBuf),
}

/// Where to find `lite-harnessd`: `LITE_HARNESS_DAEMON_BIN` if set
/// (e2e/dev override), otherwise the sibling binary next to whichever
/// executable is calling this.
pub fn daemon_binary_path() -> Result<std::path::PathBuf, ConnectError> {
    if let Ok(p) = std::env::var("LITE_HARNESS_DAEMON_BIN") {
        return Ok(std::path::PathBuf::from(p));
    }
    let exe = std::env::current_exe().map_err(ConnectError::CurrentExe)?;
    let dir = exe.parent().ok_or(ConnectError::NoParentDir)?;
    let name = if cfg!(windows) { "lite-harnessd.exe" } else { "lite-harnessd" };
    Ok(dir.join(name))
}

/// Connects to the daemon at `sock_path`, spawning it first if nothing is
/// listening yet -- the exact bootstrap every thin client (CLI, web
/// backend) wants: "just work" on first run, without a separate `daemon
/// start` step.
pub async fn connect_or_spawn(sock_path: &std::path::Path) -> Result<tokio::net::UnixStream, ConnectError> {
    if let Ok(stream) = tokio::net::UnixStream::connect(sock_path).await {
        return Ok(stream);
    }

    let daemon_bin = daemon_binary_path()?;
    eprintln!("no daemon running at {}, starting {}", sock_path.display(), daemon_bin.display());
    std::process::Command::new(&daemon_bin)
        .spawn()
        .map_err(|source| ConnectError::Spawn { path: daemon_bin.clone(), source })?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Ok(stream) = tokio::net::UnixStream::connect(sock_path).await {
            return Ok(stream);
        }
        if std::time::Instant::now() > deadline {
            return Err(ConnectError::Timeout(sock_path.to_path_buf()));
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
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

    #[tokio::test]
    async fn permission_ask_round_trips_with_a_negative_daemon_originated_id() {
        // Daemon-initiated request ids are negative (client ids start at 1
        // and only increment) so the two id spaces can never collide on
        // the same connection -- RequestId = i64 needs no special-casing
        // for that, which this test pins down.
        let (mut client, server) = tokio::io::duplex(4096);
        let mut server = buffered(server);

        let request = lh_event::PermissionRequest {
            session_id: lh_event::SessionId::now_v7(),
            tool_source: lh_event::ToolSource::Native {
                tool_id: "bash".to_string(),
            },
            action: lh_event::PermissionAction::Exec {
                command: "ls".to_string(),
                args: vec![],
                cwd: std::path::PathBuf::from("."),
            },
            risk_tier: lh_event::RiskTier::Execute,
        };
        let ask = Message::Request(Request::new(
            -1,
            methods::PERMISSION_ASK,
            serde_json::to_value(PermissionAskParams { request }).unwrap(),
        ));
        write_message(&mut client, &ask).await.unwrap();

        let got = read_message(&mut server).await.unwrap().unwrap();
        let Message::Request(r) = got else {
            panic!("expected Request, got {got:?}");
        };
        assert_eq!(r.id, -1);
        assert_eq!(r.method, methods::PERMISSION_ASK);
        let params: PermissionAskParams = serde_json::from_value(r.params).unwrap();
        assert_eq!(params.request.risk_tier, lh_event::RiskTier::Execute);

        let reply = Message::Response(Response::ok(
            r.id,
            serde_json::to_value(PermissionAskResult {
                decision: lh_event::PermissionDecision::Allow,
            })
            .unwrap(),
        ));
        write_message(&mut server, &reply).await.unwrap();

        let mut client = buffered(client);
        let got = read_message(&mut client).await.unwrap().unwrap();
        let Message::Response(resp) = got else {
            panic!("expected Response, got {got:?}");
        };
        assert_eq!(resp.id, -1);
        let result: PermissionAskResult = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert!(matches!(result.decision, lh_event::PermissionDecision::Allow));
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

    // `LITE_HARNESS_DAEMON_BIN` is process-global state, so these tests
    // serialize on it -- otherwise a parallel test run could read another
    // test's override.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn real_daemon_bin() -> std::path::PathBuf {
        // Sibling workspace binary, not a dependency of this crate --
        // lh-protocol is a pure library, so there's no `CARGO_BIN_EXE_*`
        // available to lean on the way a crate with its own [[bin]] target
        // could (see lh-cli/lh-web-backend's own test helpers). Requires
        // `cargo test --workspace` to have built it.
        //
        // Which subdirectory it landed in depends on the build tool: plain
        // `cargo test` and older `cargo-llvm-cov` both build into
        // `target/debug`, but newer `cargo-llvm-cov` (confirmed: the
        // version `taiki-e/install-action` pulls in CI, unlike the older
        // one installed locally in this session) segregates instrumented
        // builds into `target/llvm-cov-target/debug` instead, to avoid
        // invalidating the regular build cache. Check both rather than
        // guessing, so this doesn't silently regress on the next tool
        // version bump either way.
        let workspace_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let name = if cfg!(windows) { "lite-harnessd.exe" } else { "lite-harnessd" };
        for target_subdir in ["target/llvm-cov-target/debug", "target/debug"] {
            let candidate = workspace_root.join(target_subdir).join(name);
            if candidate.exists() {
                return candidate;
            }
        }
        workspace_root.join("target/debug").join(name)
    }

    #[test]
    fn daemon_binary_path_respects_the_env_override() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("LITE_HARNESS_DAEMON_BIN", "/some/override/path");
        let got = daemon_binary_path().unwrap();
        std::env::remove_var("LITE_HARNESS_DAEMON_BIN");
        assert_eq!(got, std::path::PathBuf::from("/some/override/path"));
    }

    #[test]
    fn daemon_binary_path_falls_back_to_the_sibling_of_the_current_exe() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("LITE_HARNESS_DAEMON_BIN");
        let got = daemon_binary_path().unwrap();
        let name = if cfg!(windows) { "lite-harnessd.exe" } else { "lite-harnessd" };
        assert_eq!(got.file_name().unwrap().to_str().unwrap(), name);
    }

    #[tokio::test]
    async fn connect_or_spawn_connects_directly_if_something_is_already_listening() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("already-listening.sock");
        let listener = tokio::net::UnixListener::bind(&sock_path).unwrap();
        let accept_task = tokio::spawn(async move { listener.accept().await.unwrap() });

        // No daemon binary override needed at all -- this path never looks
        // at `daemon_binary_path()` because it never has to spawn anything.
        let stream = connect_or_spawn(&sock_path).await.unwrap();
        drop(stream);
        accept_task.await.unwrap();
    }

    /// Scans `/proc` for a `lite-harnessd` process whose cwd is exactly
    /// `workspace` and sends it SIGTERM. Precise cwd matching (rather than
    /// killing every process named `lite-harnessd`) matters because other
    /// test binaries in this workspace spawn real daemons of their own,
    /// potentially concurrently -- an unscoped kill would make those tests
    /// flaky.
    fn kill_daemon_with_cwd(workspace: &std::path::Path) {
        let Ok(entries) = std::fs::read_dir("/proc") else { return };
        for entry in entries.flatten() {
            let pid = entry.file_name();
            let pid = pid.to_string_lossy();
            if !pid.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let Ok(comm) = std::fs::read_to_string(entry.path().join("comm")) else { continue };
            if comm.trim() != "lite-harnessd" {
                continue;
            }
            if std::fs::read_link(entry.path().join("cwd")).ok().as_deref() != Some(workspace) {
                continue;
            }
            let _ = std::process::Command::new("kill").args(["-TERM", &pid]).status();
        }
    }

    #[tokio::test]
    async fn connect_or_spawn_spawns_a_real_daemon_when_nothing_is_listening() {
        let _guard = ENV_LOCK.lock().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let canonical_workspace = workspace.path().canonicalize().unwrap();
        let sock_path = default_socket_path(&canonical_workspace);

        // connect_or_spawn spawns the daemon inheriting *this test
        // process's* current_dir (it takes no cwd argument, matching
        // lite-harnessd's own real startup: both derive the socket path
        // from `std::env::current_dir()`). Save/restore around the chdir
        // since this mutates process-global state; safe under ENV_LOCK
        // because no other test in this file depends on cwd.
        let original_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(&workspace).unwrap();
        std::env::set_var("LITE_HARNESS_DAEMON_BIN", real_daemon_bin());
        std::env::set_var("HOME", home.path());
        std::env::remove_var("LITE_HARNESS_PROVIDERS_FILE");
        std::env::remove_var("LITE_HARNESS_AGENTS_FILE");

        let result = connect_or_spawn(&sock_path).await;

        std::env::set_current_dir(&original_cwd).unwrap();
        std::env::remove_var("LITE_HARNESS_DAEMON_BIN");
        kill_daemon_with_cwd(&canonical_workspace);

        result.unwrap();
    }
}
