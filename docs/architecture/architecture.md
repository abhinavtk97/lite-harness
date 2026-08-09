# `lite-harness` — Detailed System Architecture

*A rendered, web-friendly version of this document is available at [docs/architecture/index.html](index.html) (published on GitHub Pages). This file is the raw source.*

## Context

Prior research (see [../research/agent-harness-landscape.md](../research/agent-harness-landscape.md)) surveyed 20+ existing AI coding-agent harnesses and concluded: the real differentiator for `lite-harness` isn't the mechanics of spawning other agents as subprocesses — several scrappy 2026 open-source projects already do that. The differentiator is **a genuinely unified permission model and a single legible cost ledger across heterogeneous underlying agents**, and the Agent Client Protocol (ACP) — verified hands-on in a follow-up spike (real code cloned and run, not just docs; see [../research/raw/acp-and-meta-harness.md](../research/raw/acp-and-meta-harness.md)) — is a good *transport* for reaching those agents but gives zero orchestration primitives of its own.

The project has now moved from research to architecture, with one crucial scope simplification: **no new ACP adapters need to be built** — `lite-harness` only needs to be a correct ACP *client* against agents that already speak ACP (natively, or via existing, mature third-party adapters like `claude-agent-acp` and `codex-acp`). Four foundational decisions were made before this design was drafted:

- **Core identity**: `lite-harness` is BOTH its own native coding agent (own LLM loop, own tools, own sandboxing) AND a meta-harness that can delegate subtasks to other agents via ACP. Not a pure orchestrator.
- **Language/runtime**: Rust for the core engine.
- **Deployment model for v1**: local-first — runs entirely on the user's machine; a remote/cloud execution plane is a future pluggable concern, not built now.
- **v1 delegated-agent set**: just ONE external agent to start (Claude Code via `claude-agent-acp`), to prove the architecture end-to-end before generalizing to Codex/Gemini CLI/Goose/OpenCode.

This document is the detailed architecture that follows from those decisions and from the ten design principles established in the prior research phase (event-log-as-substrate, UI-agnostic core, decoupled reasoning/execution planes, pluggable session storage, permission enforcement inside the core not the UI, structural gates on destructive actions, typed multi-agent control flow, built-in cost/observability, ACI-style tool design, and MCP governed through the same boundary as everything else).

---

## 1. High-level component diagram

```
                              ┌───────────────────────────────────────────────────┐
                              │                lite-harness-core (daemon)          │
                              │                                                     │
                              │   ┌─────────────┐     ┌────────────────────────┐   │
  ┌──────────────┐  Harness   │   │  Native      │     │  ACP Orchestration     │   │
  │   CLI (tty)  │◄──Protocol─┼──►│  Agent Loop  │     │  Module                │   │
  └──────────────┘  (local    │   │  (LLM +      │     │  (spawns/owns N        │   │
                     UDS/     │   │  tool exec)  │     │  claude-agent-acp /    │   │
  ┌──────────────┐  named     │   └──────┬───────┘     │  codex-acp / … procs)  │   │
  │  Web UI       │  pipe)    │          │             └──────────┬─────────────┘   │
  │ (browser) ───►│───────────┼──►┌──────▼─────────────────────────▼─────────────┐  │
  └──────────────┘  (via      │   │         Permission Engine (ONE gate)         │  │
                     local     │   │  intercepts: native tool_call requests  AND │  │
  ┌──────────────┐  web        │   │  ACP session/request_permission calls       │  │
  │ Headless /    │  backend   │   └──────┬───────────────────────────────────┬──┘  │
  │ SDK / CI      │  process)  │          │                                   │     │
  └──────────────┘◄───────────┼──►┌───────▼────────┐                 ┌────────▼───┐ │
                              │   │  Sandbox /      │                 │  Cost       │ │
                              │   │  ExecutionPlane │                 │  Ledger     │ │
                              │   │  (Landlock/     │                 │  (usage     │ │
                              │   │  Seatbelt/local │                 │  events →   │ │
                              │   │  proc exec)     │                 │  rollups)   │ │
                              │   └────────┬────────┘                 └──────┬─────┘ │
                              │            │                                 │       │
                              │   ┌────────▼─────────────────────────────────▼────┐  │
                              │   │        Event Log / Session Store               │  │
                              │   │  append-only log per session, SQLite behind    │  │
                              │   │  a `SessionStore` trait                        │  │
                              │   └─────────────────────────────────────────────────┘ │
                              └───────────────────────────────────────────────────────┘
                                              │                        │
                                     spawns, stdio JSON-RPC   spawns, stdio JSON-RPC
                                              │                        │
                                   ┌──────────▼─────────┐   ┌──────────▼─────────┐
                                   │ claude-agent-acp    │   │ codex-acp (later)  │
                                   │ (wraps Claude Code) │   │ (wraps Codex CLI)  │
                                   └─────────────────────┘   └────────────────────┘
```

**Key invariant**: CLI, Web UI, and headless clients never talk to the native agent loop, ACP module, permission engine, sandbox, or cost ledger directly — only to the daemon over the Harness Protocol (§4). The web UI's backend is itself just another Harness Protocol client with zero agent-loop logic, so it can be killed/restarted without affecting in-flight sessions.

---

## 2. Process model: a local daemon, not an embedded library

**Decision: a long-running local daemon (`lite-harnessd`)**, with the CLI and the web backend as thin IPC clients that spawn-or-attach to it.

Why not an embedded library linked into each UI: closing the CLI terminal would then kill in-flight native tool execution, kill ACP subprocess connections (their stdio pipes die with the parent), and drop pending permission prompts — exactly the "UI is load-bearing" failure this project exists to avoid. A daemon also lets ACP connections (expensive to re-establish: subprocess spawn + `initialize` + `session/new` handshake) stay warm across multiple CLI invocations and a web UI reconnect, and gives one single writer for the event log instead of needing cross-process lock/merge semantics.

Concrete shape:
- Single Tokio async process. First CLI/web invocation auto-spawns the daemon if none is running for the current workspace (same self-bootstrap pattern as `rust-analyzer`/`sccache`).
- Transport: **Unix domain socket** at `$XDG_RUNTIME_DIR/lite-harness/<workspace-hash>.sock`. One daemon **per workspace root** (detected via `.git` or `--workspace`), not global — this keeps sandbox policy and permission scoping project-local, matching the layered policy model in §6.
- The daemon has **no direct terminal/UI I/O** — only structured diagnostic logs to a file. This physically enforces "core has no UI dependency" rather than relying on convention.
- Idle daemons self-terminate after a configurable timeout (default 30 min) with zero active sessions/clients, so they don't linger as orphans but stay warm across a rapid sequence of CLI calls.
- Crash recovery: the event log is durable (fsynced), so a daemon restart reconstructs session state by replay; only in-flight ACP subprocess connections and native tool executions are lost and get marked `interrupted` on next open.

---

## 3. Unified event/session data model

Everything that happens — a native LLM turn, a native tool call, a delegated ACP agent's message chunk, a permission decision, a cost report, a subagent spawning — is one `Event` appended to a per-session log.

```rust
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Event {
    pub seq: u64,                          // monotonic within a session
    pub event_id: EventId,                 // ULID/UUIDv7
    pub session_id: SessionId,
    pub parent_session_id: Option<SessionId>,  // None for root; Some for any subagent/delegated child
    pub ts: DateTime<Utc>,
    pub actor: Actor,
    pub payload: EventPayload,
}

pub enum Actor {
    User,
    NativeAgent,
    NativeSubagent { child_session: SessionId },
    DelegatedAgent { child_session: SessionId, agent_kind: AgentKind },
    System,
}

pub enum EventPayload {
    UserMessage { content: Vec<ContentBlock> },
    AgentMessageChunk { content: ContentBlock },
    AgentThoughtChunk { content: ContentBlock },
    ToolCallRequested { call: ToolCall },
    ToolCallUpdated { call_id: ToolCallId, status: ToolCallStatus, output: Option<ContentBlock> },
    PermissionRequested { call_id: ToolCallId, request: PermissionRequest },
    PermissionDecided { call_id: ToolCallId, decision: PermissionDecision, decided_by: DecisionSource },
    UsageReported { usage: UsageDelta },
    ChildSessionSpawned { child: SessionId, kind: ChildKind, spec: ChildSpec },
    ChildSessionEnded { child: SessionId, outcome: ChildOutcome },
    SessionForked { from_seq: u64, new_session: SessionId },
    SessionResumed { at_seq: u64 },
    PlanUpdated { steps: Vec<PlanStep> },
    Error { message: String, recoverable: bool },
}

pub struct ToolCall {
    pub call_id: ToolCallId,
    pub tool_name: String,
    pub source: ToolSource,   // Native(BuiltinToolId) | Mcp(server, tool) | Acp(agent_kind)
    pub args_summary: serde_json::Value,
    pub raw_args: serde_json::Value,
}
```

**Design intent**: `ToolCall.source: ToolSource` is what unifies native, MCP-originated, and ACP-delegated tool calls under one shape — the permission engine and every UI key off `ToolSource`, never off "which subsystem produced this." ACP's `session/update` variants (`agent_message_chunk`, `tool_call`, `tool_call_update`, `plan`, `agent_thought_chunk`) map close to 1:1 onto `EventPayload` variants by design: the ACP orchestration module is a *translator*, not a special case. Adding delegated-agent #2/#3 should never require a new `EventPayload` variant.

### Session tree

A **root session** is created per top-level conversation. A **native subagent** creates a child session (`parent_session_id` set), its own event sub-stream, and a permission scope that can only be a *restriction* of the parent's. A **delegated ACP agent** creates a child session identical in shape, differing only in `ChildKind::Delegated(AgentKind::ClaudeCode)` vs. `ChildKind::NativeSubagent`. This gives one recursive tree; a delegated ACP agent is a leaf from our side (ACP is 1:1), while a native subagent can itself spawn further children.

### Storage

Interface-first, per design principle 4 — never a concrete DB call sprinkled through the codebase:

```rust
#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn append(&self, event: Event) -> Result<()>;
    async fn read_from(&self, session_id: SessionId, from_seq: u64) -> Result<EventStream>;
    async fn subscribe(&self, session_id: SessionId) -> Result<BoxStream<'static, Event>>;
    async fn session_tree(&self, root: SessionId) -> Result<SessionTree>;
    async fn latest_seq(&self, session_id: SessionId) -> Result<u64>;
}
```

v1 implementation: SQLite (`rusqlite`, WAL mode) — one row per event, `session_id`+`seq` indexed, JSON payload column. Crash-safe appends without hand-rolling an append-only file format. Behind the trait, a future `PostgresSessionStore` can be added for a hosted deployment without touching the agent loop, ACP module, or protocol layer — this is exactly the layering the Assistants API got wrong (it fused protocol to state ownership) and that this design avoids.

**Resume**: reopen by `session_id`, `read_from(last_acked_seq)` to catch a client up. For a resumed delegated child, the ACP module attempts ACP's `session/load` if the adapter declares that capability; otherwise starts fresh but stays attached to the same tree node, recording the degraded resume explicitly.

**Fork**: copy-on-branch — a new `session_id` with a `forked_from: Option<(SessionId, u64)>` pointer; `read_from` transparently stitches the parent prefix + child suffix rather than physically copying events. Forking a session with an active delegated-agent child does **not** attempt to fork the underlying ACP agent's own state (no such primitive in most adapters); it starts a fresh ACP session for the forked branch and records the discontinuity in the log.

---

## 4. The Harness Protocol

A stable, versioned, local-first RPC protocol between `lite-harnessd` and its own clients (CLI, web backend, headless/SDK) — **distinct from ACP**, which is what the daemon speaks *outbound* to delegated agents.

- **Transport**: JSON-RPC 2.0 over the same UDS the daemon listens on, newline-delimited framing. Chosen deliberately to match ACP's own message shape, so the team needs only one JSON-RPC plumbing implementation shared by both the inbound protocol server and the outbound ACP client.
- **Versioning**: an `initialize` handshake (mirroring ACP's own pattern) negotiates protocol version + capability flags; method names stay stable across minor versions, major version bumps only for breaking changes.
- **Requests** (client→daemon): `session/create`, `session/prompt`, `session/fork`, `session/resume`, `session/cancel`, `permission/respond`, `ledger/query`, `session/tree`, `agents/list`.
- **Notifications** (daemon→client, streamed): `event` — literally the same `Event` struct from §3. This is the single most important protocol decision: **no parallel "UI event" schema that could drift from ground truth.** CLI renders events as terminal output, web renders them as chat bubbles/tool cards, headless callers get structured JSON.
- Even UI-driven approval flows go through the append-only path: `permission/respond` is a request whose effect is to append a `PermissionDecided` event — never a side channel that could bypass it.
- **Headless/CI**: `lite-harness run --json <prompt>` does `session/create` + `session/prompt` + drains `event` notifications to stdout as NDJSON and exits with a status code. This *is* the headless interface — same protocol path as the CLI, not a separate implementation. A future SDK is a typed wrapper over the same RPC calls.
- **Web UI**: `lh-web-backend` holds one Harness Protocol UDS connection to the daemon and re-exposes it as a websocket to the browser, translating framing only, never interpreting event semantics.

---

## 5. ACP orchestration module

### Spawning and holding a connection

Uses the official `agent-client-protocol` Rust crate (the same crate that powers the Zed editor). `lite-harness` implements the crate's `Client` trait; `claude-agent-acp` implements `Agent`.

```rust
struct AcpConnection {
    child: tokio::process::Child,
    conn: acp::ClientSideConnection,
    agent_kind: AgentKind,
    acp_session_id: acp::SessionId,      // ACP's own session id
    our_session_id: SessionId,           // our tree's child SessionId
}

#[async_trait]
impl acp::Client for HarnessAcpClient {
    async fn session_update(&self, params: acp::SessionUpdateParams) -> Result<()> {
        let event = translate_acp_update(self.our_session_id, params);  // pure mapping, §3
        self.store.append(event).await
    }

    async fn request_permission(
        &self,
        params: acp::RequestPermissionParams,
    ) -> Result<acp::RequestPermissionResponse> {
        // Same PermissionEngine that gates native tool calls — not a parallel path.
        let req = translate_acp_permission_request(self.our_session_id, &params);
        self.store.append(Event::permission_requested(self.our_session_id, req.clone())).await?;
        let decision = self.permission_engine.decide(&req).await?;
        self.store.append(Event::permission_decided(self.our_session_id, decision.clone())).await?;
        Ok(translate_decision_to_acp_response(decision, &params))
    }

    async fn read_text_file(&self, params: acp::ReadTextFileParams) -> Result<...> { /* via sandboxed FS, §8 */ }
    async fn write_text_file(&self, params: acp::WriteTextFileParams) -> Result<...> { /* gated the same way */ }
}
```

Flow for delegating one task to Claude Code: look up `AgentKind::ClaudeCode` in the agent registry → spawn `claude-agent-acp` with piped stdio → ACP `initialize` → `session/new` → append `ChildSessionSpawned`, mint our own `SessionId`, record the `our_session_id ↔ acp_session_id` mapping → `session/prompt` → all further traffic is inbound `session/update` / `request_permission` handled by `HarnessAcpClient` as above → on completion append `ChildSessionEnded` and, if reported, a final `UsageReported`. `session/cancel` on the Harness Protocol cascades into an ACP `session/cancel` before subprocess teardown.

### Adding delegated agent #2, #3, ... without touching the core

The core orchestration module never changes when adding Codex/Gemini/Goose/OpenCode. What's added is one small, declarative adapter value:

```rust
pub struct DelegatedAgentAdapter {
    pub kind: AgentKind,
    pub spawn: SpawnSpec,                 // binary/path, args, env, cwd rule
    pub capabilities: AcpCapabilities,     // session/load? plan updates? usage reporting?
    pub usage_mapping: UsageMappingHint,   // how (if at all) this agent reports usage
    pub health_check: HealthCheck,
}
```

Registered in a config file the daemon loads at startup (not a recompile), and exposed read-only over the Harness Protocol (`agents/list`) so UIs can present a "delegate to: [...]" picker without hardcoding the list. v1 assumes `claude-agent-acp` is pre-installed/discoverable on `PATH` or via a configured path — auto-install is out of scope for the first vertical slice. **Needs a quick check against the adapter's README before implementation**: how it sources credentials (its own OAuth vs. passing through `ANTHROPIC_API_KEY` vs. shelling out to an already-logged-in `claude` CLI).

---

## 6. Permission engine — the core differentiator

### Policy model

```rust
pub struct PermissionRequest {
    pub session_id: SessionId,
    pub tool_source: ToolSource,           // Native | Mcp | Acp(agent_kind)
    pub action: PermissionAction,          // structured, not a free string
    pub risk_tier: RiskTier,               // Read | Write | Execute | Network | Destructive
}

pub enum PermissionAction {
    FileRead { path: PathBuf },
    FileWrite { path: PathBuf, diff_summary: Option<String> },
    Exec { command: String, args: Vec<String>, cwd: PathBuf },
    NetworkFetch { url: String },
    McpToolCall { server: String, tool: String, args_summary: serde_json::Value },
    DelegatedAgentToolCall { agent: AgentKind, acp_tool_call: ToolCall },
}

pub enum PermissionDecision { Allow, AllowAlways(PolicyScope), Deny, DenyAlways(PolicyScope) }
pub enum PolicyScope { Session, Project, Global }
```

Rule resolution: session-scoped > project-scoped (`.lite-harness/policy.toml`) > global-scoped (`~/.config/lite-harness/policy.toml`) > built-in default (read auto-allowed; write/execute/network/destructive default to `Ask`).

### One interception point — structurally, not by convention

```rust
#[async_trait]
pub trait PermissionEngine: Send + Sync {
    /// Called by (a) the native tool executor before ANY tool, built-in or
    /// MCP-sourced, and (b) HarnessAcpClient::request_permission for ANY
    /// delegated agent's tool call. No other path exists.
    async fn decide(&self, req: &PermissionRequest) -> Result<PermissionDecision>;
}
```

The bypass class that recurs across every competitor studied (Cline #9357, Codex #34515, Amazon Q #2510 — UI checkbox state silently not enforced by the actual execution path) is closed at the type level, not by discipline:

```rust
pub trait ExecutionPlane {
    // The mandatory `proof` argument means there is no code path from a
    // ToolCall to actual execution that skips a PermissionDecision — it's
    // a compile error, not a runtime risk.
    async fn execute(&self, call: &ToolCall, proof: &PermissionDecision) -> Result<ToolCallOutput>;
}
```

`AllowAlways`/`DenyAlways` persist into the policy store at the chosen scope. MCP-originated calls get identical treatment: `PermissionAction::McpToolCall` flows through the same `decide()` path — no "MCP is trusted" shortcut anywhere, directly addressing the MCP-governance gap found in every competitor surveyed.

### Structural gate on destructive actions

Two mechanisms, not just a scarier prompt: (1) `RiskTier::Destructive` requests can **never** be satisfied by a blanket `AllowAlways` rule — the engine hard-codes a check that forces interactive `Ask` for that specific call regardless of session-level "allow everything" settings; (2) the `ExecutionPlane`'s OS-level sandbox (§8) is a backstop in case the heuristic risk classifier misses something — the structural gate is sandboxing, not just a bigger warning dialog. This is the direct answer to the Replit/Cursor-style incidents in the prior research.

---

## 7. Cost ledger

Captured as `UsageReported` events, never a separately-mutated counter — the ledger is a derived read-model over the event log (principle 8).

```rust
pub struct UsageDelta {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cost_usd: Option<Decimal>,
    pub wall_ms: u64,
    pub confidence: UsageConfidence,   // Exact | Estimated | Unknown
}
```

Native LLM calls: computed from the provider's usage block + a pluggable `ModelPricing` table. Delegated ACP agents: mapped via each adapter's declared `UsageMappingHint` (`ReportsTokens` / `ReportsCostOnly` / `NoUsageReporting`); when an agent reports nothing, the ledger still records that a turn happened (count, duration, which agent) rather than silently omitting the line — an honest gap, not a fabricated number.

Aggregation: `CostLedger::rollup(session_id)` folds `UsageReported` events recursively across the whole session tree (native subagents and delegated agents alike), with `None`-propagation so a subtree containing any `Unknown` turns is flagged partial. A materialized `cost_rollups` table keeps `ledger/query` O(1); it can always be rebuilt by replaying events. Exposed as a literal tree ("root: $0.42 = $0.31 native + $0.11 delegated(Claude Code), 1 turn cost-unknown"), not just a flat number — this is the "single legible cost ledger across heterogeneous agents" differentiator made concrete.

---

## 8. Sandbox / execution plane

```rust
#[async_trait]
pub trait ExecutionPlane: Send + Sync {
    async fn execute(&self, call: &ToolCall, proof: &PermissionDecision) -> Result<ToolCallOutput>;
    async fn read_file(&self, path: &Path, proof: &PermissionDecision) -> Result<Vec<u8>>;
    async fn write_file(&self, path: &Path, contents: &[u8], proof: &PermissionDecision) -> Result<()>;
    fn describe(&self) -> ExecutionPlaneCapabilities;
}
```

v1 `LocalExecutionPlane`:
- **Linux**: `landlock` crate (kernel 5.13+) restricts filesystem access to the workspace root + scratch dir at the kernel level. Network-restriction Landlock rules need kernel 6.7+; probe at startup and fall back to a namespace/`bubblewrap`-based approach on older kernels rather than silently having no network sandboxing.
- **macOS**: shell out to `/usr/bin/sandbox-exec` with a generated `.sb` profile (no well-maintained Rust crate for Seatbelt exists), restricting file/network access.
- Process execution via `tokio::process::Command`, `cwd` pinned to the workspace root, environment scrubbed to an explicit allowlist, wall-clock timeout and output byte cap enforced regardless of OS sandbox support.
- **Windows**: out of scope for v1; runs without a hard OS sandbox and forces `Ask` on every write/execute request rather than pretending to be sandboxed.

This trait is why a future remote/container execution plane can slot in without touching the permission engine, event schema, or ACP module: every caller depends only on `ExecutionPlane`, injected once at daemon startup from config (`execution_plane = "local"` today, `"docker"`/`"remote-vm"` later).

---

## 9. Native subagents — same machinery as delegated agents

The native agent loop's own subagent spawning reuses the exact same session-tree, permission, and cost machinery as a delegated ACP agent — only `ChildKind` and the driving mechanism (in-process task vs. ACP round-trip to a subprocess) differ.

```rust
pub struct NativeSubagentSpec {
    pub role: String,
    pub system_prompt_override: Option<String>,
    pub tool_allowlist: Vec<ToolId>,      // must be a subset of the parent's
    pub permission_scope: PolicyScope,     // typically session-scoped, stricter than parent
    pub max_turns: Option<u32>,
}
```

Isolation is enforced at two levels: conversation/context (each child gets an explicit `TaskHandoff { instructions, relevant_context_refs }`, never the parent's full transcript) and policy (`tool_allowlist`/`permission_scope` can only narrow relative to the parent — the policy store refuses to register a broader child-scoped rule).

The supervisor decides "native subagent or delegated ACP agent?" through one shared trait, giving typed control flow instead of free-form multi-agent conversation (the exact failure mode that pushed Microsoft to retire AutoGen's conversational orchestration):

```rust
#[async_trait]
pub trait ChildRunner: Send + Sync {
    async fn run(&self, parent: SessionId, task: TaskHandoff) -> Result<ChildOutcome>;
}
// impls: NativeSubagentRunner, AcpDelegatedRunner
```

---

## 10. Cargo workspace layout

```
lite-harness/
├── Cargo.toml
├── crates/
│   ├── lh-event            # Event/EventPayload/ToolCall/session-tree types — near-zero deps
│   ├── lh-store             # SessionStore trait + SqliteSessionStore impl
│   ├── lh-permission         # PermissionEngine trait, DefaultPermissionEngine, policy model
│   ├── lh-execution           # ExecutionPlane trait, LocalExecutionPlane (Landlock/Seatbelt)
│   ├── lh-ledger               # CostLedger rollup logic, UsageDelta, ModelPricing
│   ├── lh-native-agent           # NativeAgentLoop, ChildRunner/NativeSubagentRunner, built-in tools
│   ├── lh-mcp                     # MCP client integration for native tools — routes via lh-permission
│   ├── lh-acp                      # HarnessAcpClient, agent registry, DelegatedAgentAdapter
│   ├── lh-protocol                  # Harness Protocol wire types + JSON-RPC framing
│   ├── lh-daemon                     # lite-harnessd binary — wires everything, owns UDS listener
│   ├── lh-cli                         # lite-harness binary — thin Harness Protocol client
│   ├── lh-web-backend                  # local web server — Harness Protocol client, websocket bridge
│   └── lh-web-ui                        # presentation only, over lh-web-backend's API
└── xtask/                                # dev tooling, integration harness with a fake ACP agent
```

Dependency direction: `lh-event` underlies everything; `lh-store`, `lh-permission`, `lh-execution`, `lh-ledger` are independent siblings depending only on `lh-event`; `lh-native-agent`/`lh-mcp`/`lh-acp` depend on those plus `lh-event`; `lh-daemon` is the only crate that constructs concrete implementations and wires trait objects together; `lh-cli`/`lh-web-backend` depend **only** on `lh-protocol` — a compile-time enforcement of "the UI is never load-bearing."

---

## 11. Build order / phasing

1. **Skeleton & protocol plumbing** — `lh-event` types, `SqliteSessionStore`, `lh-protocol` framing, a daemon that echoes a hardcoded event stream, a CLI that connects and prints events. Proves the daemon+thin-client process model with zero agent logic.
2. **Native agent loop, one real task, no sandbox yet** — single LLM provider, minimal built-in tools (read/write/edit/bash, initially unsandboxed and explicitly flagged as dev-only), `DefaultPermissionEngine` in `Ask`-only mode round-tripping through the daemon to the CLI.
3. **Real sandboxing** — `LocalExecutionPlane` with Landlock (Linux first), policy persistence (`.lite-harness/policy.toml`), native cost ledger.
4. **The ACP vertical slice — the architecture's actual proof point** — `lh-acp`, `HarnessAcpClient`, the Claude Code adapter, one delegated task end-to-end. The test that matters: **one permission-prompt path** handles both a native tool call and a Claude-Code-originated one, and the cost ledger shows one aggregated total across both. Everything after this phase is expansion, not new architecture.
5. **Native subagents** — `ChildRunner`/`NativeSubagentRunner`, session-tree fork/resume. Should require touching `lh-permission`/`lh-ledger` not at all — itself a validation that those abstractions were shaped correctly.
6. **Web UI** — `lh-web-backend` as a pure protocol client, `lh-web-ui` as presentation only. Should be "just write a UI," not "extend the core."
7. **Expand delegated-agent set** (Codex via `codex-acp`, then Gemini CLI/Goose/OpenCode natively), MCP integration, remote/container execution plane, richer policy, Windows support — additive against the traits established in phases 1-4.

**Open judgment call, flagged for review**: phase 4 (ACP) is placed before full native-agent polish and before native subagents, on the theory that proving cross-agent-type permission/cost unification early — while the surface area is small — reduces the riskiest unknown (the exact shape of `claude-agent-acp`'s permission-request payloads and usage-reporting fields won't be fully known until integrated against) sooner. If the team judges native-agent maturity as the more urgent near-term risk, phases 3 and 4 can be swapped with no architectural change.

---

## Critical files to create first, in build order

- `crates/lh-event/src/lib.rs` — `Event`/`EventPayload`/`ToolCall`/session-tree types (§3); everything else depends on this being right.
- `crates/lh-protocol/src/lib.rs` — Harness Protocol wire types + JSON-RPC framing (§4); the daemon/client contract, defined before any UI exists.
- `crates/lh-permission/src/lib.rs` — `PermissionEngine` trait, `PermissionRequest`/`PermissionDecision`, policy resolution (§6) — the load-bearing abstraction for the project's core differentiator.
- `crates/lh-acp/src/client.rs` — `HarnessAcpClient`'s `impl acp::Client`, ACP-to-`Event` translation, `DelegatedAgentAdapter`/agent registry (§5) — the other half of the differentiator, and the piece with the most external-crate integration risk.
- `crates/lh-daemon/src/main.rs` — wires concrete `SessionStore`/`PermissionEngine`/`ExecutionPlane`/`ChildRunner` implementations together (§2, §10) — proof the whole architecture compiles as one system.

---

## Verification

This phase produces a design document, not running code, so "verification" means: the plan is internally consistent and testable against the stated requirements before any Rust is written.

- Cross-check §1-11 against the ten design principles from the research phase — confirm each principle maps to a concrete mechanism above, not just a restated goal. Spot checks: principle 5 → §6's `proof: &PermissionDecision` argument; principle 4 → §3's `SessionStore` trait; principle 1 → §3's `Event` as the sole state-changing primitive.
- Once Phases 1-4 above are implemented, the concrete acceptance test for "the architecture works" is: from the CLI, run one task that triggers both a native tool call and a delegated Claude Code tool call in the same session, see both permission prompts rendered identically (differing only in the displayed `ToolSource`), and see one `ledger/query` response with a combined cost total covering both.
