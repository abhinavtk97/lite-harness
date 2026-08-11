//! In-process integration tests for `lh_daemon::handle_connection`: drives
//! it directly over an in-memory `UnixStream::pair()` (no real OS
//! subprocess) using the exact same `lh_protocol` wire types `lh-cli`
//! speaks -- exercising `handle_connection`'s real branches, previously
//! only ever verified by hand once per phase, never captured as an
//! automated, CI-enforced test.
//!
//! Deliberately in-process rather than subprocess-based: `cargo
//! llvm-cov`-instrumented coverage counters for code reached only via
//! `tokio::spawn` inside a *subprocess* were confirmed (by direct signal +
//! `.profraw` inspection, not assumed) to never reliably flush, even on a
//! clean process exit. A real end-to-end subprocess smoke test still lives
//! in `tests/binary_smoke.rs`, for genuine "the compiled binary actually
//! works" confidence -- this file carries the scenario/coverage weight.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use lh_acp::registry::AgentsFile;
use lh_daemon::providers::ResolvedProvider;
use lh_event::{AgentKind, EventPayload, PermissionDecision, SessionId};
use lh_execution::{ExecutionPlane, LocalExecutionPlane};
use lh_ledger::{CostLedger, PricingTable, StoreBackedCostLedger};
use lh_permission::TomlPolicyStore;
use lh_protocol::{
    buffered, methods, read_message, write_message, AgentsListParams, AgentsListResult,
    InitializeParams, InitializeResult, LedgerQueryParams, LedgerQueryResult, Message,
    PermissionAskParams, PermissionAskResult, PrimarySelector, Request, RequestId, Response,
    SessionCreateParams, SessionCreateResult, SessionDelegateParams, SessionDelegateResult,
    SessionPromptParams, SessionPromptResult, PROTOCOL_VERSION,
};
use lh_store::{SessionStore, SqliteSessionStore};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use wiremock::{Mock, MockServer, ResponseTemplate};

// --- building the "daemon-side" shared state directly in Rust, matching
// what main.rs's startup wiring assembles from env vars/config files ---

struct DaemonState {
    store: Arc<dyn SessionStore>,
    resolved_provider: Arc<Option<ResolvedProvider>>,
    execution_plane: Arc<dyn ExecutionPlane>,
    project_policy: Option<Arc<TomlPolicyStore>>,
    global_policy: Option<Arc<TomlPolicyStore>>,
    pricing: Arc<PricingTable>,
    cost_ledger: Arc<dyn CostLedger>,
    agents_registry: Arc<AgentsFile>,
}

impl DaemonState {
    async fn new(workspace: &Path) -> Self {
        let store: Arc<dyn SessionStore> = Arc::new(SqliteSessionStore::open_in_memory().unwrap());
        let execution_plane: Arc<dyn ExecutionPlane> =
            Arc::new(LocalExecutionPlane::new(workspace.to_path_buf()).await.unwrap());
        let cost_ledger: Arc<dyn CostLedger> = Arc::new(StoreBackedCostLedger::new(store.clone()));
        Self {
            store,
            resolved_provider: Arc::new(None),
            execution_plane,
            project_policy: None,
            global_policy: None,
            pricing: Arc::new(PricingTable::new()),
            cost_ledger,
            agents_registry: Arc::new(AgentsFile::default()),
        }
    }

    fn with_provider(mut self, provider: ResolvedProvider) -> Self {
        self.resolved_provider = Arc::new(Some(provider));
        self
    }

    fn with_project_policy(mut self, path: PathBuf) -> Self {
        self.project_policy = Some(Arc::new(TomlPolicyStore::load(path).unwrap()));
        self
    }

    fn with_agents(mut self, agents: Vec<lh_acp::registry::DelegatedAgentAdapter>) -> Self {
        self.agents_registry = Arc::new(AgentsFile { agents });
        self
    }

    /// Spawns `lh_daemon::handle_connection` in-process against one half of
    /// an in-memory `UnixStream::pair()`, returning a `TestClient` wrapping
    /// the other half -- the in-process equivalent of connecting to a real
    /// socket.
    fn connect(self) -> TestClient {
        let (server_half, client_half) = UnixStream::pair().unwrap();
        tokio::spawn(lh_daemon::handle_connection(
            server_half,
            self.store,
            self.resolved_provider,
            self.execution_plane,
            self.project_policy,
            self.global_policy,
            self.pricing,
            self.cost_ledger,
            self.agents_registry,
        ));
        TestClient::new(client_half)
    }
}

fn mock_provider(base_url: &str) -> ResolvedProvider {
    let provider = lh_model_provider::OpenAiCompatibleProvider::new(
        format!("{base_url}/v1"),
        "test-key",
        "mock-model",
        Default::default(),
    );
    ResolvedProvider {
        provider: Arc::new(provider),
        name: "mock".to_string(),
        model: "mock-model".to_string(),
    }
}

fn write_file(dir: &Path, rel: &str, contents: &str) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

fn fake_agent_adapter(can_be_primary: bool, api_key_env: &str) -> lh_acp::registry::DelegatedAgentAdapter {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("lh-acp/tests/fixtures/fake_acp_agent.py");
    std::env::set_var(api_key_env, "unused-by-the-fake-agent");
    lh_acp::registry::DelegatedAgentAdapter {
        kind: AgentKind::ClaudeCode,
        spawn: lh_acp::registry::SpawnSpec {
            command: "python3".to_string(),
            args: vec![fixture.to_string_lossy().to_string()],
            api_key_env: api_key_env.to_string(),
        },
        can_be_primary,
    }
}

// --- a real Unix-socket protocol client, mirroring lh-cli's own request() ---

struct TestClient {
    write_half: OwnedWriteHalf,
    reader: tokio::io::BufReader<OwnedReadHalf>,
    next_id: RequestId,
}

impl TestClient {
    fn new(stream: UnixStream) -> Self {
        let (r, w) = stream.into_split();
        Self { write_half: w, reader: buffered(r), next_id: 1 }
    }

    async fn send_request(&mut self, method: &str, params: serde_json::Value) -> RequestId {
        let id = self.next_id;
        self.next_id += 1;
        write_message(&mut self.write_half, &Message::Request(Request::new(id, method, params))).await.unwrap();
        id
    }

    async fn respond_ok<T: serde::Serialize>(&mut self, id: RequestId, result: T) {
        write_message(
            &mut self.write_half,
            &Message::Response(Response::ok(id, serde_json::to_value(result).unwrap())),
        )
        .await
        .unwrap();
    }

    async fn recv(&mut self) -> Option<Message> {
        read_message(&mut self.reader).await.unwrap()
    }

    /// Sends a request and reads messages until the matching `Response`
    /// arrives, answering any `permission/ask` `Request`s along the way
    /// with a fixed decision, and collecting every streamed `event`
    /// notification. This is the shape almost every scenario below needs.
    async fn call_collecting_events(
        &mut self,
        method: &str,
        params: serde_json::Value,
        permission_decision: PermissionDecision,
    ) -> (Result<serde_json::Value, String>, Vec<lh_event::Event>) {
        let id = self.send_request(method, params).await;
        let mut events = Vec::new();
        loop {
            match self.recv().await {
                Some(Message::Notification(n)) if n.method == methods::EVENT => {
                    events.push(serde_json::from_value(n.params).unwrap());
                }
                Some(Message::Request(req)) if req.method == methods::PERMISSION_ASK => {
                    let _params: PermissionAskParams = serde_json::from_value(req.params).unwrap();
                    self.respond_ok(req.id, PermissionAskResult { decision: permission_decision.clone() }).await;
                }
                Some(Message::Response(resp)) if resp.id == id => {
                    return (
                        match resp.error {
                            Some(e) => Err(e.message),
                            None => Ok(resp.result.unwrap_or_default()),
                        },
                        events,
                    );
                }
                Some(other) => panic!("unexpected message while waiting for {method}: {other:?}"),
                None => panic!("connection closed while waiting for {method}"),
            }
        }
    }

    async fn initialize(&mut self) {
        let id = self.send_request(methods::INITIALIZE, serde_json::to_value(InitializeParams { protocol_version: PROTOCOL_VERSION }).unwrap()).await;
        match self.recv().await.unwrap() {
            Message::Response(resp) => {
                let result: InitializeResult = serde_json::from_value(resp.result.unwrap()).unwrap();
                assert_eq!(result.protocol_version, PROTOCOL_VERSION);
                assert_eq!(resp.id, id);
            }
            other => panic!("expected InitializeResult, got {other:?}"),
        }
    }

    async fn agents_list(&mut self) -> AgentsListResult {
        let id = self.send_request(methods::AGENTS_LIST, serde_json::to_value(AgentsListParams::default()).unwrap()).await;
        match self.recv().await.unwrap() {
            Message::Response(resp) => {
                assert_eq!(resp.id, id);
                serde_json::from_value(resp.result.unwrap()).unwrap()
            }
            other => panic!("expected AgentsListResult, got {other:?}"),
        }
    }

    async fn create_session(&mut self, cwd: &Path, primary: PrimarySelector) -> Result<SessionCreateResult, String> {
        let id = self.send_request(
            methods::SESSION_CREATE,
            serde_json::to_value(SessionCreateParams { cwd: cwd.to_string_lossy().to_string(), primary }).unwrap(),
        )
        .await;
        match self.recv().await.unwrap() {
            Message::Response(resp) => {
                assert_eq!(resp.id, id);
                match resp.error {
                    Some(e) => Err(e.message),
                    None => Ok(serde_json::from_value(resp.result.unwrap()).unwrap()),
                }
            }
            other => panic!("expected SessionCreateResult, got {other:?}"),
        }
    }

    async fn ledger_query(&mut self, session_id: SessionId) -> LedgerQueryResult {
        let id = self.send_request(methods::LEDGER_QUERY, serde_json::to_value(LedgerQueryParams { session_id }).unwrap()).await;
        match self.recv().await.unwrap() {
            Message::Response(resp) => {
                assert_eq!(resp.id, id);
                serde_json::from_value(resp.result.unwrap()).unwrap()
            }
            other => panic!("expected LedgerQueryResult, got {other:?}"),
        }
    }
}

fn payload_kind(event: &lh_event::Event) -> &'static str {
    match &event.payload {
        EventPayload::UserMessage { .. } => "UserMessage",
        EventPayload::AgentMessageChunk { .. } => "AgentMessageChunk",
        EventPayload::AgentThoughtChunk { .. } => "AgentThoughtChunk",
        EventPayload::ToolCallRequested { .. } => "ToolCallRequested",
        EventPayload::ToolCallUpdated { .. } => "ToolCallUpdated",
        EventPayload::PermissionRequested { .. } => "PermissionRequested",
        EventPayload::PermissionDecided { .. } => "PermissionDecided",
        EventPayload::UsageReported { .. } => "UsageReported",
        EventPayload::ChildSessionSpawned { .. } => "ChildSessionSpawned",
        EventPayload::ChildSessionEnded { .. } => "ChildSessionEnded",
        EventPayload::SessionForked { .. } => "SessionForked",
        EventPayload::SessionResumed { .. } => "SessionResumed",
        EventPayload::SessionDriverSet { .. } => "SessionDriverSet",
        EventPayload::PlanUpdated { .. } => "PlanUpdated",
        EventPayload::Error { .. } => "Error",
    }
}

/// A stateful OpenAI-compatible mock: pops one scripted JSON body per
/// request, falling back to a plain "done" text reply once the script is
/// exhausted -- lets a test drive a multi-turn tool-call conversation.
struct ScriptedResponder {
    bodies: Mutex<VecDeque<serde_json::Value>>,
}

impl wiremock::Respond for ScriptedResponder {
    fn respond(&self, _req: &wiremock::Request) -> ResponseTemplate {
        let mut q = self.bodies.lock().unwrap();
        let body = q.pop_front().unwrap_or_else(|| {
            serde_json::json!({
                "choices": [{"message": {"role": "assistant", "content": "done"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1}
            })
        });
        ResponseTemplate::new(200).set_body_json(body)
    }
}

fn text_response(text: &str) -> serde_json::Value {
    serde_json::json!({
        "choices": [{"message": {"role": "assistant", "content": text}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5}
    })
}

fn bash_tool_call_response(call_id: &str, command: &str) -> serde_json::Value {
    serde_json::json!({
        "choices": [{
            "message": {
                "role": "assistant", "content": null,
                "tool_calls": [{
                    "id": call_id, "type": "function",
                    "function": {"name": "bash", "arguments": serde_json::to_string(&serde_json::json!({"command": command})).unwrap()},
                }],
            },
            "finish_reason": "tool_calls",
        }],
        "usage": {"prompt_tokens": 15, "completion_tokens": 8},
    })
}

async fn scripted_mock_server(bodies: Vec<serde_json::Value>) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/chat/completions"))
        .respond_with(ScriptedResponder { bodies: Mutex::new(bodies.into()) })
        .mount(&server)
        .await;
    server
}

// --- tests ---

#[tokio::test]
async fn happy_path_native_prompt_and_ledger_round_trip() {
    let server = scripted_mock_server(vec![text_response("hello from the mock model")]).await;
    let workspace = tempfile::tempdir().unwrap();
    let state = DaemonState::new(workspace.path()).await.with_provider(mock_provider(&server.uri()));
    let mut client = state.connect();

    client.initialize().await;
    let agents = client.agents_list().await;
    assert!(agents.agents.is_empty());

    let create = client.create_session(workspace.path(), PrimarySelector::Native).await.unwrap();

    let (result, events) = client
        .call_collecting_events(
            methods::SESSION_PROMPT,
            serde_json::to_value(SessionPromptParams { text: "hi".to_string() }).unwrap(),
            PermissionDecision::Allow,
        )
        .await;
    let prompt_result: SessionPromptResult = serde_json::from_value(result.unwrap()).unwrap();
    assert_eq!(prompt_result.stop_reason, "EndTurn");

    let kinds: Vec<&str> = events.iter().map(payload_kind).collect();
    assert_eq!(kinds, vec!["SessionDriverSet", "UserMessage", "AgentMessageChunk", "UsageReported"]);

    let ledger = client.ledger_query(create.session_id).await;
    assert_eq!(ledger.rollup.turns, 1);
    // "mock-model" isn't in the built-in pricing table -- an honest
    // Unknown, not a fabricated Exact, matching the project's "never guess
    // a cost" principle (lh-ledger/src/pricing.rs).
    assert_eq!(ledger.rollup.confidence, lh_event::UsageConfidence::Unknown);
}

#[tokio::test]
async fn agents_list_reflects_a_configured_delegated_agent() {
    let workspace = tempfile::tempdir().unwrap();
    let state = DaemonState::new(workspace.path())
        .await
        .with_agents(vec![fake_agent_adapter(true, "LH_ACP_DAEMON_TEST_KEY_LIST")]);
    let mut client = state.connect();
    client.initialize().await;

    let agents = client.agents_list().await;
    assert_eq!(agents.agents.len(), 1);
    assert!(matches!(agents.agents[0].kind, AgentKind::ClaudeCode));
    assert!(agents.agents[0].can_be_primary);

    // Queryable more than once, before session/create -- not a one-shot.
    let agents2 = client.agents_list().await;
    assert_eq!(agents2.agents.len(), 1);
}

#[tokio::test]
async fn permission_resolves_via_project_policy_with_no_live_ask() {
    let server = scripted_mock_server(vec![bash_tool_call_response("call_1", "echo hi"), text_response("done")]).await;
    let workspace = tempfile::tempdir().unwrap();
    write_file(workspace.path(), "policy.toml", "[[rule]]\nkey = \"native:bash#exec\"\ndecision = \"allow\"\n");
    let state = DaemonState::new(workspace.path())
        .await
        .with_provider(mock_provider(&server.uri()))
        .with_project_policy(workspace.path().join("policy.toml"));
    let mut client = state.connect();

    client.initialize().await;
    client.create_session(workspace.path(), PrimarySelector::Native).await.unwrap();

    // Any permission/ask arriving here would make call_collecting_events
    // answer it -- assert none did, by checking decided_by on the resulting
    // PermissionDecided event instead of relying on absence alone.
    let (result, events) = client
        .call_collecting_events(
            methods::SESSION_PROMPT,
            serde_json::to_value(SessionPromptParams { text: "run echo".to_string() }).unwrap(),
            PermissionDecision::Deny, // if this got used, the test below would fail loudly
        )
        .await;
    result.unwrap();

    let decided = events.iter().find_map(|e| match &e.payload {
        EventPayload::PermissionDecided { decided_by, .. } => Some(*decided_by),
        _ => None,
    });
    assert_eq!(decided, Some(lh_event::DecisionSource::PolicyRule));
}

#[tokio::test]
async fn permission_live_ask_allow_actually_runs_the_tool() {
    let server = scripted_mock_server(vec![
        bash_tool_call_response("call_1", "echo allowed"),
        text_response("first done"),
    ])
    .await;
    let workspace = tempfile::tempdir().unwrap();
    let state = DaemonState::new(workspace.path()).await.with_provider(mock_provider(&server.uri()));
    let mut client = state.connect();
    client.initialize().await;
    client.create_session(workspace.path(), PrimarySelector::Native).await.unwrap();

    let (result, events) = client
        .call_collecting_events(
            methods::SESSION_PROMPT,
            serde_json::to_value(SessionPromptParams { text: "run echo".to_string() }).unwrap(),
            PermissionDecision::Allow,
        )
        .await;
    result.unwrap();
    let tool_status = events.iter().find_map(|e| match &e.payload {
        EventPayload::ToolCallUpdated { status, .. } => Some(*status),
        _ => None,
    });
    assert_eq!(tool_status, Some(lh_event::ToolCallStatus::Completed));
}

#[tokio::test]
async fn session_prompt_with_no_provider_configured_errors() {
    let workspace = tempfile::tempdir().unwrap();
    let state = DaemonState::new(workspace.path()).await; // no with_provider()
    let mut client = state.connect();
    client.initialize().await;
    client.create_session(workspace.path(), PrimarySelector::Native).await.unwrap();

    let (result, _events) = client
        .call_collecting_events(
            methods::SESSION_PROMPT,
            serde_json::to_value(SessionPromptParams { text: "hi".to_string() }).unwrap(),
            PermissionDecision::Allow,
        )
        .await;
    let err = result.unwrap_err();
    assert!(err.contains("no model provider configured"), "got: {err}");
}

#[tokio::test]
async fn session_delegate_to_a_fake_acp_agent_and_unknown_agent_errors() {
    let workspace = tempfile::tempdir().unwrap();
    let state = DaemonState::new(workspace.path())
        .await
        .with_agents(vec![fake_agent_adapter(false, "LH_ACP_DAEMON_TEST_KEY_DELEGATE")]);
    let mut client = state.connect();
    client.initialize().await;
    client.create_session(workspace.path(), PrimarySelector::Native).await.unwrap();

    let (result, events) = client
        .call_collecting_events(
            methods::SESSION_DELEGATE,
            serde_json::to_value(SessionDelegateParams { agent: AgentKind::ClaudeCode, task_summary: "diagnose".to_string() })
                .unwrap(),
            PermissionDecision::Allow,
        )
        .await;
    let delegate_result: SessionDelegateResult = serde_json::from_value(result.unwrap()).unwrap();
    assert!(matches!(delegate_result.outcome, lh_event::ChildOutcome::Success { .. }));
    assert!(events.iter().any(|e| matches!(e.payload, EventPayload::ChildSessionSpawned { .. })));
    assert!(events.iter().any(|e| matches!(e.payload, EventPayload::ChildSessionEnded { .. })));

    // A fresh connection with no agents configured must reject the exact
    // same delegate call cleanly, not hang or panic.
    let state2 = DaemonState::new(workspace.path()).await;
    let mut client2 = state2.connect();
    client2.initialize().await;
    client2.create_session(workspace.path(), PrimarySelector::Native).await.unwrap();
    let (result2, _events2) = client2
        .call_collecting_events(
            methods::SESSION_DELEGATE,
            serde_json::to_value(SessionDelegateParams { agent: AgentKind::ClaudeCode, task_summary: "x".to_string() })
                .unwrap(),
            PermissionDecision::Allow,
        )
        .await;
    let err = result2.unwrap_err();
    assert!(err.contains("no delegated agent adapter configured"), "got: {err}");
}

#[tokio::test]
async fn primary_selector_delegated_round_trips_and_rejects_bad_configs() {
    let workspace = tempfile::tempdir().unwrap();

    // 1. can_be_primary = true -> a full delegated-root prompt round trip.
    let state = DaemonState::new(workspace.path())
        .await
        .with_agents(vec![fake_agent_adapter(true, "LH_ACP_DAEMON_TEST_KEY_PRIMARY")]);
    let mut client = state.connect();
    client.initialize().await;
    let created = client
        .create_session(workspace.path(), PrimarySelector::Delegated { agent: AgentKind::ClaudeCode })
        .await
        .unwrap();
    let (result, events) = client
        .call_collecting_events(
            methods::SESSION_PROMPT,
            serde_json::to_value(SessionPromptParams { text: "please run the diagnostic".to_string() }).unwrap(),
            PermissionDecision::Allow,
        )
        .await;
    let prompt_result: SessionPromptResult = serde_json::from_value(result.unwrap()).unwrap();
    assert_eq!(prompt_result.stop_reason, "EndTurn");
    assert!(events.iter().any(|e| matches!(e.payload, EventPayload::UsageReported { .. })));
    let ledger = client.ledger_query(created.session_id).await;
    assert_eq!(ledger.rollup.confidence, lh_event::UsageConfidence::Estimated);

    // 2. can_be_primary = false -> session/create itself must reject it.
    let state2 = DaemonState::new(workspace.path())
        .await
        .with_agents(vec![fake_agent_adapter(false, "LH_ACP_DAEMON_TEST_KEY_PRIMARY")]);
    let mut client2 = state2.connect();
    client2.initialize().await;
    let err = client2
        .create_session(workspace.path(), PrimarySelector::Delegated { agent: AgentKind::ClaudeCode })
        .await
        .unwrap_err();
    assert!(err.contains("not marked can_be_primary"), "got: {err}");

    // 3. No agents configured at all -> session/create rejects unknown agent.
    let state3 = DaemonState::new(workspace.path()).await;
    let mut client3 = state3.connect();
    client3.initialize().await;
    let err3 = client3
        .create_session(workspace.path(), PrimarySelector::Delegated { agent: AgentKind::ClaudeCode })
        .await
        .unwrap_err();
    assert!(err3.contains("no delegated agent adapter configured"), "got: {err3}");
}

#[tokio::test]
async fn dropping_the_connection_before_initialize_or_before_session_create_is_handled_gracefully() {
    let workspace = tempfile::tempdir().unwrap();

    // Connect and disconnect before sending anything.
    {
        let state = DaemonState::new(workspace.path()).await;
        let _client = state.connect();
    }

    // Connect, initialize, then disconnect before agents/list or
    // session/create -- covers the agents/list loop's own EOF branch.
    {
        let state = DaemonState::new(workspace.path()).await;
        let mut client = state.connect();
        client.initialize().await;
    }

    // A fresh connection afterwards works fine -- neither of the above
    // left anything in a bad state (each is its own handle_connection
    // task, so this is really just confirming no panic/hang happened).
    let state = DaemonState::new(workspace.path()).await;
    let mut client = state.connect();
    client.initialize().await;
}

#[tokio::test]
async fn wrong_method_or_malformed_params_closes_the_connection_instead_of_hanging() {
    let workspace = tempfile::tempdir().unwrap();

    // Wrong method where initialize was expected.
    {
        let state = DaemonState::new(workspace.path()).await;
        let mut client = state.connect();
        client.send_request(methods::SESSION_CREATE, serde_json::json!({})).await;
        // The daemon's ensure! fails and the task ends without a response
        // -- the client just observes the socket close.
        assert!(client.recv().await.is_none());
    }

    // Wrong method where session/create was expected (after a valid
    // initialize + agents/list loop).
    {
        let state = DaemonState::new(workspace.path()).await;
        let mut client = state.connect();
        client.initialize().await;
        client.send_request(methods::LEDGER_QUERY, serde_json::json!({})).await;
        assert!(client.recv().await.is_none());
    }

    // Malformed session/create params (missing required `cwd`).
    {
        let state = DaemonState::new(workspace.path()).await;
        let mut client = state.connect();
        client.initialize().await;
        client.send_request(methods::SESSION_CREATE, serde_json::json!({"primary": {"type": "Native"}})).await;
        assert!(client.recv().await.is_none());
    }
}
