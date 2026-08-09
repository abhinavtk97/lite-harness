//! `lite-harness` CLI — Phase 1 skeleton (architecture §2, §11 phase 1).
//!
//! Connects to the per-workspace daemon over its Unix domain socket,
//! auto-spawning it if it isn't already running, then drives one session
//! and prints the streamed events. Proves the "CLI is a thin client, never
//! load-bearing for the agent loop" property end to end -- there is no
//! agent logic in this binary at all.

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use lh_event::{ContentBlock, Event, EventPayload};
use lh_protocol::{
    buffered, default_socket_path, methods, read_message, write_message, InitializeParams,
    InitializeResult, Message, Request, RequestId, SessionCreateParams, SessionCreateResult,
    PROTOCOL_VERSION,
};
use tokio::io::{AsyncBufRead, AsyncWrite};
use tokio::net::UnixStream;

#[tokio::main]
async fn main() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let sock_path = default_socket_path(&cwd);

    let stream = connect_or_spawn(&sock_path).await?;
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = buffered(read_half);
    let mut next_id: RequestId = 1;

    let init_result: InitializeResult = request(
        &mut write_half,
        &mut reader,
        &mut next_id,
        methods::INITIALIZE,
        serde_json::to_value(InitializeParams {
            protocol_version: PROTOCOL_VERSION,
        })?,
    )
    .await?;
    eprintln!(
        "connected to daemon (protocol v{})",
        init_result.protocol_version
    );

    let create_result: SessionCreateResult = request(
        &mut write_half,
        &mut reader,
        &mut next_id,
        methods::SESSION_CREATE,
        serde_json::to_value(SessionCreateParams {
            cwd: cwd.to_string_lossy().to_string(),
        })?,
    )
    .await?;
    eprintln!("session {} created\n", create_result.session_id);

    loop {
        match read_message(&mut reader).await? {
            Some(Message::Notification(n)) if n.method == methods::EVENT => {
                let event: Event = serde_json::from_value(n.params)?;
                print_event(&event);
            }
            Some(other) => {
                eprintln!("[unexpected message] {other:?}");
            }
            None => break, // daemon closed the connection: canned sequence is done
        }
    }

    println!();
    Ok(())
}

async fn request<T: serde::de::DeserializeOwned>(
    writer: &mut (impl AsyncWrite + Unpin),
    reader: &mut (impl AsyncBufRead + Unpin),
    next_id: &mut RequestId,
    method: &str,
    params: serde_json::Value,
) -> Result<T> {
    let id = *next_id;
    *next_id += 1;
    write_message(writer, &Message::Request(Request::new(id, method, params))).await?;

    loop {
        match read_message(reader).await?.ok_or_else(|| {
            anyhow!("daemon closed the connection while waiting for a response to {method}")
        })? {
            Message::Response(resp) if resp.id == id => {
                if let Some(err) = resp.error {
                    return Err(anyhow!("{method} failed: {} ({})", err.message, err.code));
                }
                let result = resp
                    .result
                    .ok_or_else(|| anyhow!("{method} response had neither result nor error"))?;
                return Ok(serde_json::from_value(result)?);
            }
            // Ignore anything else while waiting for this specific response
            // (there is nothing else in Phase 1, but real sessions may
            // interleave permission requests etc. here later).
            _ => continue,
        }
    }
}

async fn connect_or_spawn(sock_path: &std::path::Path) -> Result<UnixStream> {
    if let Ok(stream) = UnixStream::connect(sock_path).await {
        return Ok(stream);
    }

    let daemon_bin = daemon_binary_path()?;
    eprintln!(
        "no daemon running at {}, starting {}",
        sock_path.display(),
        daemon_bin.display()
    );
    std::process::Command::new(&daemon_bin)
        .spawn()
        .with_context(|| format!("failed to spawn daemon at {}", daemon_bin.display()))?;

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(stream) = UnixStream::connect(sock_path).await {
            return Ok(stream);
        }
        if Instant::now() > deadline {
            anyhow::bail!("timed out waiting for lite-harnessd to start listening");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn daemon_binary_path() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("LITE_HARNESS_DAEMON_BIN") {
        return Ok(PathBuf::from(p));
    }
    let exe = std::env::current_exe()?;
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow!("current executable has no parent directory"))?;
    let name = if cfg!(windows) {
        "lite-harnessd.exe"
    } else {
        "lite-harnessd"
    };
    Ok(dir.join(name))
}

fn print_event(event: &Event) {
    match &event.payload {
        EventPayload::UserMessage { content } => {
            println!("you: {}", render_content(content));
        }
        EventPayload::AgentMessageChunk { content } => {
            print!("{}", render_content(std::slice::from_ref(content)));
            let _ = std::io::stdout().flush();
        }
        EventPayload::AgentThoughtChunk { content } => {
            print!(" (thinking: {})", render_content(std::slice::from_ref(content)));
            let _ = std::io::stdout().flush();
        }
        EventPayload::ToolCallRequested { call } => {
            println!("\n  -> {} [{}]", call.tool_name, source_label(&call.source));
        }
        EventPayload::ToolCallUpdated { call_id, status, .. } => {
            println!("  <- {call_id} {status:?}");
        }
        EventPayload::UsageReported { usage } => {
            println!(
                "\n  [usage] in={:?} out={:?} cost=${:?} ({:?}, {}ms)",
                usage.input_tokens, usage.output_tokens, usage.cost_usd, usage.confidence, usage.wall_ms
            );
        }
        EventPayload::SessionDriverSet { driver } => {
            eprintln!("[session driver: {driver:?}]");
        }
        other => {
            println!("[{:?}] {other:?}", event.actor);
        }
    }
}

fn render_content(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .map(|b| match b {
            ContentBlock::Text { text } => text.clone(),
            ContentBlock::Other { kind, .. } => format!("[{kind}]"),
        })
        .collect::<Vec<_>>()
        .join("")
}

fn source_label(source: &lh_event::ToolSource) -> String {
    match source {
        lh_event::ToolSource::Native { tool_id } => format!("native:{tool_id}"),
        lh_event::ToolSource::Mcp { server, tool } => format!("mcp:{server}/{tool}"),
        lh_event::ToolSource::Acp { agent } => format!("acp:{agent:?}"),
    }
}
