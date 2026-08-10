//! `lite-harness` CLI (architecture §2, §11 phase 2).
//!
//! Connects to the per-workspace daemon, sends one real prompt via
//! `session/prompt`, renders the streamed events, and answers permission
//! requests interactively over stdin -- still a thin client with zero
//! agent logic of its own; every decision it makes is either "print this
//! event" or "forward this human answer back to the daemon."

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use lh_event::{AgentKind, ContentBlock, Event, EventPayload, PermissionDecision};
use lh_protocol::{
    buffered, default_socket_path, methods, read_message, write_message, InitializeParams,
    InitializeResult, LedgerQueryParams, LedgerQueryResult, Message, PermissionAskParams,
    PermissionAskResult, Request, RequestId, Response, SessionCreateParams, SessionCreateResult,
    SessionDelegateParams, SessionDelegateResult, SessionPromptParams, SessionPromptResult,
    PROTOCOL_VERSION,
};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite};
use tokio::net::UnixStream;

/// `lite-harness [--agent claude-code] <prompt>` -- omitting `--agent`
/// keeps today's default (native loop, `session/prompt`); naming an agent
/// sends `session/delegate` instead (architecture §11 phase 4). Just this
/// one flag for v1, matching the single delegated-agent-set decision.
fn parse_args() -> Result<(Option<AgentKind>, String)> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage = "usage: lite-harness [--agent claude-code] <prompt>\n  \
        e.g. lite-harness \"list the files in this directory\"\n  \
        e.g. lite-harness --agent claude-code \"fix the failing test\"";

    if args.first().map(String::as_str) == Some("--agent") {
        let name = args.get(1).ok_or_else(|| anyhow!("{usage}\n\n--agent requires a value"))?;
        let agent = match name.as_str() {
            "claude-code" => AgentKind::ClaudeCode,
            other => anyhow::bail!("{usage}\n\nunknown agent '{other}' (only 'claude-code' is supported today)"),
        };
        let prompt = args.get(2).ok_or_else(|| anyhow!("{usage}"))?.clone();
        Ok((Some(agent), prompt))
    } else {
        let prompt = args.first().ok_or_else(|| anyhow!("{usage}"))?.clone();
        Ok((None, prompt))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let (agent, prompt) = parse_args()?;

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
    eprintln!("connected to daemon (protocol v{})", init_result.protocol_version);

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

    let prompt_id = next_id;
    next_id += 1;
    match &agent {
        None => {
            write_message(
                &mut write_half,
                &Message::Request(Request::new(
                    prompt_id,
                    methods::SESSION_PROMPT,
                    serde_json::to_value(SessionPromptParams { text: prompt })?,
                )),
            )
            .await?;
        }
        Some(agent) => {
            eprintln!("delegating to {agent:?}\n");
            write_message(
                &mut write_half,
                &Message::Request(Request::new(
                    prompt_id,
                    methods::SESSION_DELEGATE,
                    serde_json::to_value(SessionDelegateParams {
                        agent: agent.clone(),
                        task_summary: prompt,
                    })?,
                )),
            )
            .await?;
        }
    }

    let mut stdin = buffered(tokio::io::stdin());

    loop {
        match read_message(&mut reader).await? {
            Some(Message::Notification(n)) if n.method == methods::EVENT => {
                let event: Event = serde_json::from_value(n.params)?;
                print_event(&event);
            }
            Some(Message::Request(req)) if req.method == methods::PERMISSION_ASK => {
                // The daemon only ever sends this when a live human
                // decision is genuinely needed -- a policy-resolved
                // decision never reaches here at all.
                let params: PermissionAskParams = serde_json::from_value(req.params)?;
                let decision = ask_for_decision(&mut stdin, &params.request).await?;
                write_message(
                    &mut write_half,
                    &Message::Response(Response::ok(
                        req.id,
                        serde_json::to_value(PermissionAskResult { decision })?,
                    )),
                )
                .await?;
            }
            Some(Message::Response(resp)) if resp.id == prompt_id => {
                if let Some(err) = resp.error {
                    eprintln!("\n[turn failed] {}", err.message);
                } else if agent.is_some() {
                    let result: SessionDelegateResult =
                        serde_json::from_value(resp.result.unwrap_or_default())?;
                    println!(
                        "\n[delegation complete: child session {}, outcome: {:?}]",
                        result.child_session_id, result.outcome
                    );
                } else {
                    let result: SessionPromptResult =
                        serde_json::from_value(resp.result.unwrap_or_default())?;
                    println!("\n[turn complete: {}]", result.stop_reason);
                }

                match request::<LedgerQueryResult>(
                    &mut write_half,
                    &mut reader,
                    &mut next_id,
                    methods::LEDGER_QUERY,
                    serde_json::to_value(LedgerQueryParams { session_id: create_result.session_id })?,
                )
                .await
                {
                    Ok(ledger) => print_ledger(&ledger.rollup, 0),
                    Err(e) => eprintln!("[ledger/query failed] {e}"),
                }
                break;
            }
            Some(Message::Response(_)) => {
                // Almost certainly the ack for our ledger/query request
                // (its own response is matched directly inside `request()`,
                // not here) -- nothing else on this connection sends the
                // CLI an unsolicited Response.
            }
            Some(other) => {
                eprintln!("[unexpected message] {other:?}");
            }
            None => {
                eprintln!("\n[daemon closed the connection]");
                break;
            }
        }
    }

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

async fn ask_for_decision(
    stdin: &mut (impl AsyncBufRead + Unpin),
    request: &lh_event::PermissionRequest,
) -> Result<PermissionDecision> {
    println!(
        "\n  [permission] {:?} / {:?}: {}",
        request.risk_tier,
        request.tool_source,
        describe_action(&request.action)
    );
    print!("  allow? [y/N/a=always-allow/d=always-deny]: ");
    std::io::stdout().flush().ok();

    let mut line = String::new();
    stdin.read_line(&mut line).await?;
    let answer = line.trim().to_ascii_lowercase();
    Ok(match answer.as_str() {
        "y" | "yes" => PermissionDecision::Allow,
        "a" | "always" => PermissionDecision::AllowAlways {
            scope: lh_event::PolicyScope::Project,
        },
        "d" | "always-deny" => PermissionDecision::DenyAlways {
            scope: lh_event::PolicyScope::Project,
        },
        _ => PermissionDecision::Deny,
    })
}

fn describe_action(action: &lh_event::PermissionAction) -> String {
    match action {
        lh_event::PermissionAction::FileRead { path } => format!("read {}", path.display()),
        lh_event::PermissionAction::FileWrite { path, .. } => format!("write {}", path.display()),
        lh_event::PermissionAction::Exec { command, .. } => format!("execute: {command}"),
        lh_event::PermissionAction::NetworkFetch { url } => format!("fetch {url}"),
        lh_event::PermissionAction::McpToolCall { server, tool, .. } => {
            format!("mcp {server}/{tool}")
        }
        lh_event::PermissionAction::DelegatedAgentToolCall { agent, .. } => {
            format!("delegated call via {agent:?}")
        }
        lh_event::PermissionAction::DelegateAgent { target, task_summary } => {
            format!("delegate to {target:?}: {task_summary}")
        }
        lh_event::PermissionAction::SpawnSubagent { role, task_summary } => {
            format!("spawn subagent ({role}): {task_summary}")
        }
    }
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
        EventPayload::PermissionRequested { .. } => {
            // rendered by ask_for_decision, which runs right after this event
        }
        EventPayload::PermissionDecided { decision, .. } => {
            println!("  [decision: {decision:?}]");
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
        EventPayload::Error { message, recoverable } => {
            eprintln!("\n[error{}] {message}", if *recoverable { " (recoverable)" } else { "" });
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

fn print_ledger(rollup: &lh_ledger::LedgerRollup, depth: usize) {
    let indent = "  ".repeat(depth);
    let cost = match rollup.cost_usd {
        Some(c) => format!("${c:.6}"),
        None => "$?".to_string(),
    };
    println!(
        "{indent}[ledger] session {} cost={cost} ({:?}) in={:?} out={:?} turns={}",
        rollup.session_id, rollup.confidence, rollup.input_tokens, rollup.output_tokens, rollup.turns
    );
    for child in &rollup.children {
        print_ledger(child, depth + 1);
    }
}

fn source_label(source: &lh_event::ToolSource) -> String {
    match source {
        lh_event::ToolSource::Native { tool_id } => format!("native:{tool_id}"),
        lh_event::ToolSource::Mcp { server, tool } => format!("mcp:{server}/{tool}"),
        lh_event::ToolSource::Acp { agent } => format!("acp:{agent:?}"),
    }
}
