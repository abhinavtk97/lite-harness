//! `lite-harness` CLI (architecture §2, §11 phase 2).
//!
//! Connects to the per-workspace daemon, sends one real prompt via
//! `session/prompt`, renders the streamed events, and answers permission
//! requests interactively over stdin -- still a thin client with zero
//! agent logic of its own; every decision it makes is either "print this
//! event" or "forward this human answer back to the daemon."

use std::io::Write;

use anyhow::{anyhow, Result};
use lh_event::{AgentKind, ContentBlock, Event, EventPayload, PermissionDecision};
use lh_protocol::{
    buffered, connect_or_spawn, default_socket_path, methods, read_message, write_message,
    AgentsListParams, AgentsListResult, InitializeParams, InitializeResult, LedgerQueryParams,
    LedgerQueryResult, Message, PermissionAskParams, PermissionAskResult, PrimarySelector,
    Request, RequestId, Response, SessionCreateParams, SessionCreateResult,
    SessionDelegateParams, SessionDelegateResult, SessionPromptParams, SessionPromptResult,
    PROTOCOL_VERSION,
};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite};

/// `--agent <agent>` (architecture §11 phase 4) hands one task from a
/// `Native` root to a child via `session/delegate` -- the root session
/// stays native. `--primary <agent>` (architecture §12, Phase 6) is the
/// orthogonal capability: the *whole* session's root is driven by that
/// agent from `session/create` on, via a normal `session/prompt`. The two
/// are mutually exclusive on one CLI invocation.
#[derive(Debug, Clone)]
enum CliMode {
    Native,
    DelegateChild(AgentKind),
    PrimaryDelegated(AgentKind),
}

fn parse_agent_name(usage: &str, name: &str) -> Result<AgentKind> {
    match name {
        "claude-code" => Ok(AgentKind::ClaudeCode),
        other => anyhow::bail!("{usage}\n\nunknown agent '{other}' (only 'claude-code' is supported today)"),
    }
}

/// `--list-agents` (architecture §12.5) is its own command, not a `CliMode`
/// -- it never creates a session, just queries the registry and exits, so
/// a UI (or a human at the CLI) can see which agents are available and
/// which `can_be_primary` before picking one.
enum Command {
    ListAgents,
    Run(CliMode, String),
}

/// `lite-harness [--agent claude-code | --primary claude-code] <prompt>` --
/// omitting both flags keeps today's default (native loop driving the
/// root, plain `session/prompt`). `lite-harness --list-agents` is the one
/// exception that takes no prompt.
fn parse_args() -> Result<Command> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage = "usage: lite-harness [--agent claude-code | --primary claude-code] <prompt>\n  \
        lite-harness --list-agents\n  \
        e.g. lite-harness \"list the files in this directory\"\n  \
        e.g. lite-harness --agent claude-code \"fix the failing test\"\n  \
        e.g. lite-harness --primary claude-code \"refactor this module\"";

    match args.first().map(String::as_str) {
        Some("--list-agents") => Ok(Command::ListAgents),
        Some("--agent") => {
            let name = args.get(1).ok_or_else(|| anyhow!("{usage}\n\n--agent requires a value"))?;
            let agent = parse_agent_name(usage, name)?;
            let prompt = args.get(2).ok_or_else(|| anyhow!("{usage}"))?.clone();
            Ok(Command::Run(CliMode::DelegateChild(agent), prompt))
        }
        Some("--primary") => {
            let name = args.get(1).ok_or_else(|| anyhow!("{usage}\n\n--primary requires a value"))?;
            let agent = parse_agent_name(usage, name)?;
            let prompt = args.get(2).ok_or_else(|| anyhow!("{usage}"))?.clone();
            Ok(Command::Run(CliMode::PrimaryDelegated(agent), prompt))
        }
        _ => {
            let prompt = args.first().ok_or_else(|| anyhow!("{usage}"))?.clone();
            Ok(Command::Run(CliMode::Native, prompt))
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let command = parse_args()?;

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

    let (mode, prompt) = match command {
        Command::ListAgents => {
            let result: AgentsListResult = request(
                &mut write_half,
                &mut reader,
                &mut next_id,
                methods::AGENTS_LIST,
                serde_json::to_value(AgentsListParams::default())?,
            )
            .await?;
            if result.agents.is_empty() {
                println!("no delegated agents configured (see LITE_HARNESS_AGENTS_FILE)");
            } else {
                for agent in &result.agents {
                    println!(
                        "{:?}{}",
                        agent.kind,
                        if agent.can_be_primary { " (can be primary)" } else { "" }
                    );
                }
            }
            return Ok(());
        }
        Command::Run(mode, prompt) => (mode, prompt),
    };

    let primary = match &mode {
        CliMode::PrimaryDelegated(agent) => PrimarySelector::Delegated { agent: agent.clone() },
        CliMode::Native | CliMode::DelegateChild(_) => PrimarySelector::Native,
    };
    let create_result: SessionCreateResult = request(
        &mut write_half,
        &mut reader,
        &mut next_id,
        methods::SESSION_CREATE,
        serde_json::to_value(SessionCreateParams {
            cwd: cwd.to_string_lossy().to_string(),
            primary,
        })?,
    )
    .await?;
    eprintln!("session {} created\n", create_result.session_id);

    let prompt_id = next_id;
    next_id += 1;
    // `session/delegate` is the only mode that isn't a plain `session/prompt`
    // -- `PrimaryDelegated` already told the daemon which driver to use at
    // `session/create`, so it still speaks `session/prompt` like `Native` does.
    let is_delegate_child = matches!(mode, CliMode::DelegateChild(_));
    match &mode {
        CliMode::Native | CliMode::PrimaryDelegated(_) => {
            if let CliMode::PrimaryDelegated(agent) = &mode {
                eprintln!("root session driven by {agent:?}\n");
            }
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
        CliMode::DelegateChild(agent) => {
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
                } else if is_delegate_child {
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
