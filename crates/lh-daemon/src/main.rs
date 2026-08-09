//! `lite-harnessd` — Phase 1 skeleton (architecture §2, §11 phase 1).
//!
//! Listens on a per-workspace Unix domain socket, speaks the Harness
//! Protocol, and for this phase streams a hardcoded canned event sequence
//! instead of running a real agent loop. The point of Phase 1 is proving
//! the daemon+thin-client process model end to end, not agent behavior.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{ensure, Result};
use lh_event::{Actor, ContentBlock, Event, EventPayload, SessionDriver, ToolCall, ToolCallStatus, ToolSource, UsageConfidence, UsageDelta};
use lh_protocol::{
    buffered, default_socket_path, methods, read_message, write_message, InitializeResult,
    Message, Notification, Response, SessionCreateParams, SessionCreateResult, PROTOCOL_VERSION,
};
use lh_store::{SessionStore, SqliteSessionStore};
use tokio::net::{UnixListener, UnixStream};

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

    loop {
        let (stream, _addr) = listener.accept().await?;
        let store = store.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, store).await {
                eprintln!("connection error: {e:#}");
            }
        });
    }
}

async fn handle_connection(stream: UnixStream, store: Arc<dyn SessionStore>) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = buffered(read_half);

    // 1. initialize
    let Some(Message::Request(req)) = read_message(&mut reader).await? else {
        return Ok(());
    };
    ensure!(req.method == methods::INITIALIZE, "expected initialize, got {}", req.method);
    let result = InitializeResult {
        protocol_version: PROTOCOL_VERSION,
    };
    write_message(
        &mut write_half,
        &Message::Response(Response::ok(req.id, serde_json::to_value(result)?)),
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
    let _params: SessionCreateParams = serde_json::from_value(req.params)?;
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

    write_message(
        &mut write_half,
        &Message::Response(Response::ok(
            req.id,
            serde_json::to_value(SessionCreateResult { session_id })?,
        )),
    )
    .await?;

    // 3. stream the canned sequence
    for (actor, payload) in canned_events() {
        let event = Event::new(session_id, None, actor, payload);
        store.append(event.clone()).await?;
        write_message(
            &mut write_half,
            &Message::Notification(Notification::new(
                methods::EVENT,
                serde_json::to_value(&event)?,
            )),
        )
        .await?;
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    Ok(())
}

fn canned_events() -> Vec<(Actor, EventPayload)> {
    vec![
        (
            Actor::Agent,
            EventPayload::AgentMessageChunk {
                content: ContentBlock::text(
                    "Hello! This is lite-harnessd's canned Phase 1 event stream.",
                ),
            },
        ),
        (
            Actor::Agent,
            EventPayload::AgentMessageChunk {
                content: ContentBlock::text(
                    " There's no real LLM or tool execution wired up yet -- this whole \
                     sequence is scripted to prove the daemon-to-CLI streaming path.",
                ),
            },
        ),
        (
            Actor::Agent,
            EventPayload::ToolCallRequested {
                call: ToolCall {
                    call_id: "call_1".to_string(),
                    tool_name: "read_file".to_string(),
                    source: ToolSource::Native {
                        tool_id: "read_file".to_string(),
                    },
                    args_summary: serde_json::json!({"path": "README.md"}),
                    raw_args: serde_json::json!({"path": "README.md"}),
                },
            },
        ),
        (
            Actor::System,
            EventPayload::ToolCallUpdated {
                call_id: "call_1".to_string(),
                status: ToolCallStatus::Completed,
                output: Some(ContentBlock::text("(pretend file contents)")),
            },
        ),
        (
            Actor::Agent,
            EventPayload::AgentMessageChunk {
                content: ContentBlock::text(" Done -- that's the whole scripted Phase 1 proof."),
            },
        ),
        (
            Actor::System,
            EventPayload::UsageReported {
                usage: UsageDelta {
                    input_tokens: Some(120),
                    output_tokens: Some(48),
                    cost_usd: Some(0.0021),
                    wall_ms: 850,
                    confidence: UsageConfidence::Estimated,
                },
            },
        ),
    ]
}

