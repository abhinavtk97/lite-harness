//! `lite-harnessd` (architecture §2, §11 phases 2-3).
//!
//! Listens on a per-workspace Unix domain socket, speaks the Harness
//! Protocol, and runs a real native agent loop on `session/prompt`: calls a
//! configured `ModelProvider`, gates every tool call through a
//! `PermissionEngine` that round-trips permission asks back to whichever
//! client is attached, executes allowed tool calls through a sandboxed
//! `ExecutionPlane`, and streams every step out as `Event`s. No ACP/
//! delegation yet (Phase 4+).

mod permission;
mod providers;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{ensure, Result};
use lh_event::{Actor, Event, EventPayload, SessionDriver};
use lh_execution::{ExecutionPlane, LocalExecutionPlane};
use lh_ledger::{CostLedger, PricingTable, StoreBackedCostLedger};
use lh_native_agent::{AgentConfig, NativeAgentLoop};
use lh_permission::{DefaultPermissionEngine, PermissionEngine, SessionPolicyStore, TomlPolicyStore};
use lh_protocol::{
    buffered, default_socket_path, methods, read_message, write_message, InitializeResult,
    LedgerQueryParams, LedgerQueryResult, Message, Notification, PermissionRespondParams,
    Request as ProtoRequest, RequestId, Response, SessionCreateParams, SessionCreateResult,
    SessionPromptParams, SessionPromptResult, PROTOCOL_VERSION,
};
use lh_store::{SessionStore, SqliteSessionStore};
use permission::SocketPrompter;
use providers::ResolvedProvider;
use serde::Serialize;
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex as AsyncMutex;

type SharedWriter = Arc<AsyncMutex<OwnedWriteHalf>>;

#[tokio::main]
async fn main() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let sock_path = default_socket_path(&cwd);

    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if sock_path.exists() {
        std::fs::remove_file(&sock_path)?;
    }

    let listener = UnixListener::bind(&sock_path)?;
    eprintln!("lite-harnessd listening on {}", sock_path.display());

    let db_path = sock_path.with_extension("db");
    eprintln!("session store: {}", db_path.display());
    let store: Arc<dyn SessionStore> = Arc::new(SqliteSessionStore::open(&db_path)?);

    let resolved_provider = match providers::resolve_default_provider() {
        Ok(Some(p)) => {
            eprintln!("model provider ready (model={})", p.model);
            Some(p)
        }
        Ok(None) => {
            eprintln!(
                "no model provider configured -- session/prompt will fail until one is set \
                 (see LITE_HARNESS_PROVIDERS_FILE)"
            );
            None
        }
        Err(e) => {
            eprintln!("failed to load model provider config: {e:#}");
            None
        }
    };
    let resolved_provider = Arc::new(resolved_provider);

    let execution_plane: Arc<dyn ExecutionPlane> = Arc::new(LocalExecutionPlane::new(cwd.clone()).await?);
    let caps = execution_plane.describe();
    eprintln!(
        "execution plane ready (sandboxed={}, mechanism={}, network_restricted={})",
        caps.sandboxed, caps.mechanism, caps.network_restricted
    );

    // Policy layering (architecture §6): project-scoped rules live next to
    // the workspace, global-scoped rules follow the same
    // `~/.config/lite-harness/` convention as provider config (§13.2).
    // Loaded once here and shared (via Arc) across every connection so
    // concurrent sessions see a consistent in-memory view, not just an
    // eventually-consistent one via separate reloads of the same file.
    let project_policy_path = cwd.join(".lite-harness/policy.toml");
    let project_policy = match TomlPolicyStore::load(project_policy_path.clone()) {
        Ok(store) => Some(Arc::new(store)),
        Err(e) => {
            eprintln!("failed to load project policy store at {}: {e:#}", project_policy_path.display());
            None
        }
    };
    let global_policy = match std::env::var_os("HOME") {
        Some(home) => {
            let path = PathBuf::from(home).join(".config/lite-harness/policy.toml");
            match TomlPolicyStore::load(path.clone()) {
                Ok(store) => Some(Arc::new(store)),
                Err(e) => {
                    eprintln!("failed to load global policy store at {}: {e:#}", path.display());
                    None
                }
            }
        }
        None => None,
    };

    // Pricing (architecture §7/§13.3): built-in defaults for a few
    // well-known hosted models, overridable/extensible via the same
    // `~/.config/lite-harness/` convention -- an unpriced (e.g.
    // self-hosted) model just stays an honest `Unknown`, never a guess.
    let mut pricing = PricingTable::with_builtin_defaults();
    if let Some(home) = std::env::var_os("HOME") {
        let path = PathBuf::from(home).join(".config/lite-harness/pricing.toml");
        if let Err(e) = pricing.merge_overrides_from_file(&path) {
            eprintln!("failed to load pricing overrides from {}: {e:#}", path.display());
        }
    }
    let pricing = Arc::new(pricing);

    let cost_ledger: Arc<dyn CostLedger> = Arc::new(StoreBackedCostLedger::new(store.clone()));

    loop {
        let (stream, _addr) = listener.accept().await?;
        let store = store.clone();
        let resolved_provider = resolved_provider.clone();
        let execution_plane = execution_plane.clone();
        let project_policy = project_policy.clone();
        let global_policy = global_policy.clone();
        let pricing = pricing.clone();
        let cost_ledger = cost_ledger.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(
                stream,
                store,
                resolved_provider,
                execution_plane,
                project_policy,
                global_policy,
                pricing,
                cost_ledger,
            )
            .await
            {
                eprintln!("connection error: {e:#}");
            }
        });
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_connection(
    stream: UnixStream,
    store: Arc<dyn SessionStore>,
    resolved_provider: Arc<Option<ResolvedProvider>>,
    execution_plane: Arc<dyn ExecutionPlane>,
    project_policy: Option<Arc<TomlPolicyStore>>,
    global_policy: Option<Arc<TomlPolicyStore>>,
    pricing: Arc<PricingTable>,
    cost_ledger: Arc<dyn CostLedger>,
) -> Result<()> {
    let (read_half, write_half) = stream.into_split();
    let mut reader = buffered(read_half);
    let write_half: SharedWriter = Arc::new(AsyncMutex::new(write_half));

    // 1. initialize
    let Some(Message::Request(req)) = read_message(&mut reader).await? else {
        return Ok(());
    };
    ensure!(req.method == methods::INITIALIZE, "expected initialize, got {}", req.method);
    respond(
        &write_half,
        req.id,
        InitializeResult {
            protocol_version: PROTOCOL_VERSION,
        },
    )
    .await?;

    // 2. session/create
    let Some(Message::Request(req)) = read_message(&mut reader).await? else {
        return Ok(());
    };
    ensure!(
        req.method == methods::SESSION_CREATE,
        "expected session/create, got {}",
        req.method
    );
    let params: SessionCreateParams = serde_json::from_value(req.params)?;
    let session_id = lh_event::SessionId::now_v7();

    store
        .append(Event::new(
            session_id,
            None,
            Actor::System,
            EventPayload::SessionDriverSet {
                driver: SessionDriver::Native,
            },
        ))
        .await?;
    respond(&write_half, req.id, SessionCreateResult { session_id }).await?;

    // Every event appended for this session (by the agent loop, or by
    // anything else in the future) gets forwarded to this connection --
    // the store's broadcast channel is the one and only fan-out path.
    // `forwarded_seq` tracks how far the forwarder has actually *written*
    // to the socket, so the session/prompt response (below) can wait for
    // it rather than racing it for the writer lock.
    let forwarded_seq = spawn_event_forwarder(store.clone(), session_id, write_half.clone());

    let prompter = SocketPrompter::new();
    let permission_engine: Arc<dyn PermissionEngine> = Arc::new(DefaultPermissionEngine::with_policy_stores(
        Arc::new(prompter.clone()),
        Arc::new(SessionPolicyStore::new()),
        project_policy,
        global_policy,
    ));
    let workspace_root = PathBuf::from(&params.cwd);

    // 3. steady state: session/prompt and permission/respond, interleaved.
    loop {
        match read_message(&mut reader).await? {
            Some(Message::Request(req)) if req.method == methods::SESSION_PROMPT => {
                handle_session_prompt(
                    req,
                    session_id,
                    &workspace_root,
                    &store,
                    &resolved_provider,
                    &permission_engine,
                    &execution_plane,
                    &pricing,
                    &write_half,
                    forwarded_seq.clone(),
                )
                .await?;
            }
            Some(Message::Request(req)) if req.method == methods::PERMISSION_RESPOND => {
                let params: PermissionRespondParams = serde_json::from_value(req.params)?;
                prompter.resolve(params.decision).await;
                respond(&write_half, req.id, serde_json::json!({})).await?;
            }
            Some(Message::Request(req)) if req.method == methods::LEDGER_QUERY => {
                let params: LedgerQueryParams = serde_json::from_value(req.params)?;
                match cost_ledger.rollup(params.session_id).await {
                    Ok(rollup) => respond(&write_half, req.id, LedgerQueryResult { rollup }).await?,
                    Err(e) => respond_err(&write_half, req.id, e.to_string()).await?,
                }
            }
            Some(other) => {
                eprintln!("[unexpected message] {other:?}");
            }
            None => break,
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_session_prompt(
    req: ProtoRequest,
    session_id: lh_event::SessionId,
    workspace_root: &std::path::Path,
    store: &Arc<dyn SessionStore>,
    resolved_provider: &Arc<Option<ResolvedProvider>>,
    permission_engine: &Arc<dyn PermissionEngine>,
    execution_plane: &Arc<dyn ExecutionPlane>,
    pricing: &Arc<PricingTable>,
    write_half: &SharedWriter,
    forwarded_seq: tokio::sync::watch::Receiver<Option<u64>>,
) -> Result<()> {
    let params: SessionPromptParams = serde_json::from_value(req.params)?;

    let Some(ResolvedProvider { provider, name, model }) = resolved_provider.as_ref() else {
        respond_err(write_half, req.id, "no model provider configured on the daemon").await?;
        return Ok(());
    };

    let agent = Arc::new(NativeAgentLoop::new(
        store.clone(),
        provider.clone(),
        permission_engine.clone(),
        execution_plane.clone(),
        pricing.clone(),
        AgentConfig {
            provider_name: name.clone(),
            model: model.clone(),
            workspace_root: workspace_root.to_path_buf(),
            ..Default::default()
        },
    ));

    // Run the turn in its own task so this connection's read loop stays
    // free to receive permission/respond requests while the turn is in
    // flight -- the two are genuinely concurrent from here on.
    let write_half = write_half.clone();
    let store = store.clone();
    let req_id = req.id;
    let mut forwarded_seq = forwarded_seq;
    tokio::spawn(async move {
        let outcome = agent.run_turn(session_id, &params.text).await;

        // The turn's own events were appended to the store (and thus
        // broadcast) synchronously as part of run_turn, but the forwarder
        // task writes them to the socket independently -- wait for it to
        // actually catch up before sending the final response, or a
        // client could see "turn complete" before the events that
        // happened during the turn (architecture §4: streamed events and
        // this response share one connection, and must arrive in a sane
        // order even though they're produced by different tasks).
        if let Ok(target_seq) = store.latest_seq(session_id).await {
            while forwarded_seq.borrow().unwrap_or(0) < target_seq {
                if forwarded_seq.changed().await.is_err() {
                    break; // forwarder task ended (e.g. socket closed)
                }
            }
        }

        let msg = match outcome {
            Ok(stop) => Message::Response(Response::ok(
                req_id,
                serde_json::to_value(SessionPromptResult {
                    stop_reason: format!("{stop:?}"),
                })
                .expect("SessionPromptResult always serializes"),
            )),
            Err(e) => Message::Response(Response::err(req_id, 1, e.to_string())),
        };
        let mut w = write_half.lock().await;
        let _ = write_message(&mut *w, &msg).await;
    });

    Ok(())
}

/// Forwards every event appended for `session_id` to this connection's
/// socket, in the order the store assigned them. Returns a watch channel
/// reporting the highest `seq` actually written so far, so other tasks on
/// this connection (namely the session/prompt response) can wait for the
/// forwarder to catch up instead of racing it for the writer lock.
fn spawn_event_forwarder(
    store: Arc<dyn SessionStore>,
    session_id: lh_event::SessionId,
    write_half: SharedWriter,
) -> tokio::sync::watch::Receiver<Option<u64>> {
    let (forwarded_tx, forwarded_rx) = tokio::sync::watch::channel(None);
    let mut rx = store.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) if event.session_id == session_id => {
                    let seq = event.seq;
                    let notif = Message::Notification(Notification::new(
                        methods::EVENT,
                        serde_json::to_value(&event).expect("Event always serializes"),
                    ));
                    let mut w = write_half.lock().await;
                    if write_message(&mut *w, &notif).await.is_err() {
                        break;
                    }
                    drop(w);
                    let _ = forwarded_tx.send(Some(seq));
                }
                Ok(_other_session) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    forwarded_rx
}

async fn respond<T: Serialize>(write_half: &SharedWriter, id: RequestId, result: T) -> Result<()> {
    let mut w = write_half.lock().await;
    write_message(
        &mut *w,
        &Message::Response(Response::ok(id, serde_json::to_value(result)?)),
    )
    .await?;
    Ok(())
}

async fn respond_err(write_half: &SharedWriter, id: RequestId, message: impl Into<String>) -> Result<()> {
    let mut w = write_half.lock().await;
    write_message(&mut *w, &Message::Response(Response::err(id, 1, message))).await?;
    Ok(())
}
