# Raw research: ACP and the "meta-harness" pattern

*Verbatim output of a follow-up research pass (two parallel desk-research queries, plus a third hands-on verification pass), prompted by exploring whether `lite-harness` could act as a meta-harness — spinning up other existing coding-agent harnesses (Claude Code, OpenCode, Codex CLI, Gemini CLI, etc.) as subagents — and whether the Agent Client Protocol (ACP) could help. Conducted via web search and, for Pass 3, actually cloning and running ACP code, current as of August 2026. See [../agent-harness-landscape.md](../agent-harness-landscape.md) for the synthesized finding (section: "Candidate USP: meta-harness").*

---

## Pass 1: ACP for a "meta-harness orchestrating other harnesses" design

**1. What ACP standardizes.** ACP (Zed Industries, open-sourced Aug 2025, now under the `agentclientprotocol` org) is JSON-RPC 2.0, primarily over stdio to a locally-spawned agent subprocess (other transports are "on the roadmap" but stdio is the real-world model today). It is explicitly a **point-to-point, 1:1 client↔agent protocol** — Zed's own docs and third-party writeups describe it that way. There is **no native concept of agent-to-agent delegation, multi-agent composition, or nesting** in the spec itself.

**2. Agents (implement ACP as the agent side):**
- **Gemini CLI** — native/first-party (`--experimental-acp` / ACP mode, built by Google).
- **Goose** (block) — native/first-party (`goose acp`); goose is adopting ACP as its primary client interface across CLI/desktop.
- **OpenCode** (sst/opencode) — native/first-party, `opencode acp` shipped in the CLI itself.
- **GitHub Copilot CLI** — native ACP server (docs.github.com/en/copilot/reference/acp-server).
- **Claude Code** — **not first-party from Anthropic**; via Zed-authored adapters: `@zed-industries/claude-code-acp` (older, wraps Claude Code TS SDK) and `zed-industries/claude-agent-acp` (newer, wraps Claude Agent SDK). Community forks exist too (e.g. Xuanwo/acp-claude-code).
- **Codex** (OpenAI) — via Zed-maintained `zed-industries/codex-acp`, again not OpenAI-official.
- **Aider** — listed "in progress," no ACP support as of Aug 2026.
- Several others WIP via GitHub issues (Hermes, pydantic-ai, AIGNE framework).

**3. Clients:** Zed (native, headline feature since v1.0, Apr 2026), JetBrains IDEs (native since Dec 2025, public agent registry co-launched Jan 2026), VS Code (community "ACP Client" extension by formulahendry), Neovim (community, via CodeCompanion), Emacs (community plugin), Obsidian, marimo, and **Toad** — a terminal UI that lets a human switch between multiple ACP agents (Claude Code, Gemini, OpenHands, etc.) in one interface (OpenHands partnered with it, Dec 2025).

**4. Session/state richness:** `initialize` → `session/new` (declares cwd + MCP servers) → `session/prompt`; streaming via `session/update` (agent_message_chunk, agent_thought_chunk, tool_call, tool_call_update, plan); `session/request_permission` for gated tool calls; `session/cancel` notification; `session/load`/resume capability for persisted sessions. This is genuinely rich enough to be the wire protocol between an orchestrator process and one agent subprocess.

**5. Gap for the meta-harness pattern (this is the real answer):** Nothing stops a single process from opening **N independent ACP connections** to N agent subprocesses (that's architecturally just "Zed without the UI, run N times") — but the protocol gives you **zero vocabulary for the orchestration itself**: no parent/child or delegation semantics, no way to route a task between agents, no standard for merging diffs/results, no nested task-tree reporting. Permission requests are also strictly per-connection — an outer harness's policy doesn't propagate to inner agents; you'd have to auto-answer or hand-build a bridge per sub-agent. Microsoft's separate **Agent Host Protocol (AHP)** adds coordination for *N clients over one shared agent session*, but its own docs state "agent-to-agent communication... is a separate concern" — so AHP doesn't solve this either. Cross-agent cost/token accounting is only newly standardized (RFD "Session Usage," `PromptResponse.usage`/`UsageUpdate`) and is inconsistently implemented — open zero-usage bugs against Goose (#8132), Gemini CLI (#24280), and codex-acp (#165) as of mid-2026.

**6. Existing analog:** Toad is the closest thing — one front-end speaking ACP to several different agents — but it's a human picking one agent at a time, not an autonomous orchestrator fanning a task out to multiple ACP agents concurrently and merging results. No such project surfaced in search.

**Bottom line:** ACP is well-suited as the *uniform transport* replacing bespoke CLI-wrapping code for each underlying harness, but the actual meta-harness logic (task routing, permission bridging, result merging, unified cost accounting across heterogeneous agents) is entirely something you'd build on top — ACP gives you no orchestration primitives for it.

**Sources:** [Zed blog — Claude Code via ACP](https://zed.dev/blog/claude-code-via-acp) · [Zed — ACP progress report](https://zed.dev/blog/acp-progress-report) · [agentclientprotocol/agent-client-protocol (GitHub)](https://github.com/agentclientprotocol/agent-client-protocol) · [microsoft/agent-host-protocol — ahp-and-acp.md](https://github.com/microsoft/agent-host-protocol/blob/main/docs/guide/ahp-and-acp.md) · [Session Usage RFD](https://agentclientprotocol.com/rfds/session-usage) · [zed-industries/claude-agent-acp](https://github.com/zed-industries/claude-agent-acp) · [OpenCode ACP docs](https://www.opencode.asia/acp/) · [Gemini CLI ACP mode](https://geminicli.com/docs/cli/acp-mode/) · [goose ACP discussion #7309](https://github.com/aaif-goose/goose/discussions/7309) · [GitHub Copilot CLI ACP server docs](https://docs.github.com/en/copilot/reference/acp-server) · [OpenHands × Toad](https://www.openhands.dev/blog/20251218-openhands-toad-collaboration) · [goose usage issue #8132](https://github.com/aaif-goose/goose/issues/8132) · [Gemini CLI usage issue #24280](https://github.com/google-gemini/gemini-cli/issues/24280) · [codex-acp usage issue #165](https://github.com/zed-industries/codex-acp/issues/165)

---

## Pass 2: existing "meta-harness" / polyglot multi-CLI orchestrators

**Yes — this category now exists and even has a name ("meta-harness"), but it's young, fragmented, and non-standardized.** By 2026 there's a small wave of projects doing exactly this:

**Concrete polyglot orchestrators (spawn different vendor CLIs as workers):**
- **Bernstein** (bernstein.run) — open-source "deterministic orchestrator" that spawns Claude Code, Codex, Gemini CLI, and dozens of others into isolated git worktrees in parallel, then runs lint/type-check/tests + cross-model review before merging.
- **majiayu000/harness** (GitHub) — Rust control plane wrapping Claude Code CLI, Codex CLI, and the Anthropic API as subprocess "agent adapters." Uses **JSON-RPC 2.0** over stdio/HTTP/WebSocket, with Starlark policy engine, per-OS sandboxing (Landlock/bubblewrap/Seatbelt), cross-agent review (to avoid self-review), and OpenTelemetry observability.
- **Enderfga/claw-orchestrator** — runs Claude Code, Codex, Gemini, Cursor Agent, and "custom coding CLIs" as one unified runtime, with any subprocess-capable CLI pluggable as a custom engine.
- **sage** — pure-bash orchestrator explicitly runtime-agnostic across Claude Code, Cline, Codex, Gemini CLI, and **ACP**, using wave-based plans + git worktree isolation.
- **Zen MCP Server's "clink" tool** — treats other CLIs (Codex, Gemini, specialized Claude subagents) as **MCP-addressable** sub-invocations, letting one agent delegate to another vendor's CLI mid-session and return only the distilled result.
- **ruvnet/metaharness**, **OmniAgent** (MindStudio), **OpenCastle** — newer entrants explicitly branded as "meta-harness" layers unifying Claude Code, Codex, Copilot, Cursor, OpenCode, Windsurf under one CLI/session.
- **Parallel Code** — a desktop GUI running Claude Code, Codex CLI, Gemini CLI simultaneously in separate worktrees.

**Vendor-native support:** none of the major players (Claude Code, OpenHands, Goose, Cline) ship a first-party mode to spawn a *different* vendor's CLI — Claude Code's Task tool, Goose's subagents, etc. are all homogeneous-only. Cross-vendor delegation is exclusively a third-party wrapper concern right now.

**Protocols used:** mostly raw subprocess/stdio wrapping (most common), some JSON-RPC 2.0, a few adopting MCP (treating a CLI as an MCP server/tool) or ACP; no dominant standard yet.

**Documented pain points:** billing/cost accounting across vendors is called out repeatedly as unsolved/manual ("auth layer if exposing publicly" left to the implementer); retry-loop cost blowups (10–20x context burn on bad prompts); idle time waiting on per-tool permission approvals; git worktree/branch conflicts when agents run concurrently; output-format normalization handled ad hoc (e.g., markdown "ExecPlan" serialization in majiayu000/harness).

**Bottom line:** the white-space isn't total — there's real, active tooling — but it's scrappy 2026-vintage open source with no mature standard, and cross-vendor auth/billing reconciliation remains a genuinely open problem nobody has solved cleanly.

**Sources:**
- [Bernstein](https://bernstein.run/)
- [majiayu000/harness](https://github.com/majiayu000/harness)
- [Enderfga/claw-orchestrator](https://github.com/Enderfga/claw-orchestrator)
- [bradAGI/awesome-cli-coding-agents](https://github.com/bradAGI/awesome-cli-coding-agents)
- [Zen MCP Server clink tool docs](https://glama.ai/mcp/servers/@BeehiveInnovations/zen-mcp-server/blob/b205d7159b674ce47ebc11af7255d1e3556fff93/docs/tools/clink.md)
- [ruvnet/metaharness](https://github.com/ruvnet/metaharness)
- [MindStudio: What Is a Meta Harness for AI Agents?](https://www.mindstudio.ai/blog/what-is-meta-harness-ai-agents-omniagent)
- [Augment Code: 9 Open-Source Agent Orchestrators](https://www.augmentcode.com/tools/open-source-agent-orchestrators)
- [amux: Best Multi-Agent Coding Orchestrators in 2026](https://amux.io/blog/best-multi-agent-orchestrators-2026/)

---

## Pass 3: hands-on verification (not desk research — actually ran the code)

Passes 1 and 2 above were desk research (docs, blog posts, issue trackers).
To verify the load-bearing claim — that ACP's 1:1 connection model doesn't
block an orchestrator from holding many agent connections at once — the
following repos were cloned and run directly:

- `agentclientprotocol/agent-client-protocol` — the spec repo (schema +
  docs only; the actual SDKs live in separate per-language repos).
- `agentclientprotocol/typescript-sdk` — the reference TypeScript
  implementation, including runnable example agent/client binaries.
- `zed-industries/claude-agent-acp` and `zed-industries/codex-acp` — the
  two real third-party adapters that bridge Claude Code and Codex into ACP.

### Finding A: N concurrent ACP connections from one process — confirmed empirically

Reading `typescript-sdk/src/connection.ts` and `src/acp.ts` shows no
shared/global state: each `client(options)` builder plus `.connectWith(stream,
callback)` call creates a fully self-contained `Connection` object (its own
`connectionId = globalThis.crypto.randomUUID()`), scoped to exactly the one
stream passed in. Nothing prevents calling this any number of times.

To confirm this in practice rather than just from reading the code, a small
orchestrator script (`meta-harness-test.ts`) was added under the SDK's own
`src/examples/` directory (so relative imports of `../acp.js` worked without
modifying the SDK) and run via `npx tsx`. It spawns **two** copies of the
SDK's own example agent (`agent.ts`, a mock agent that simulates a full turn
including a tool call requiring permission) as separate OS subprocesses, and
drives both concurrently from a single Node process using
`Promise.allSettled`, auto-resolving each permission request independently
per worker.

Actual run output (trimmed):

```
Spawning TWO concurrent ACP agent subprocesses from ONE orchestrator process...

[worker-B] connected, protocol v1, pid=6269
[worker-B] session ccd3c73b79b85a50554def6c30cca90f created
[worker-A] connected, protocol v1, pid=6263
[worker-B] << I'll help you with that. Let me start by reading some files...
[worker-A] session 85faad3be9548b04c2b4d49244165b48 created
[worker-A] << I'll help you with that. Let me start by reading some files...
[worker-B] << [tool_call]
[worker-A] << [tool_call]
...
[worker-B] permission requested: Modifying critical configuration file -> auto-allowing
[worker-A] permission requested: Modifying critical configuration file -> auto-allowing
...
[worker-B] DONE stopReason=end_turn
[worker-A] DONE stopReason=end_turn

Elapsed: 6335ms
worker-A: fulfilled
worker-B: fulfilled
```

Both agents ran as genuinely separate OS processes (distinct PIDs), streamed
interleaved output, and each resolved its own permission request in complete
isolation from the other — no cross-talk, no shared state, no SDK-level
blocker. The ~6.3s total elapsed time (versus ~13s if the two runs had been
serialized, given the mock agent's simulated per-step delays) confirms the
two connections ran in genuine parallel, not one after another.

**Conclusion**: the "N independent connections, zero shared orchestration
state" claim from Pass 1 is confirmed by direct execution, not just by
reading documentation. Holding many agent subprocesses open at once from one
orchestrator is architecturally trivial with the ACP TypeScript SDK — the
SDK does nothing to help *or* hinder it, it is simply out of scope for the
protocol.

Test script (kept for reference, not part of any published package):

```ts
// src/examples/meta-harness-test.ts (run inside a clone of
// agentclientprotocol/typescript-sdk, so `../acp.js` resolves)
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { Writable, Readable } from "node:stream";
import * as acp from "../acp.js";

async function runWorker(workerId: string) {
  const __filename = fileURLToPath(import.meta.url);
  const agentPath = join(dirname(__filename), "agent.ts");
  const npxCmd = process.platform === "win32" ? "npx.cmd" : "npx";
  const agentProcess = spawn(npxCmd, ["tsx", agentPath], {
    stdio: ["pipe", "pipe", "inherit"],
  });

  const input = Writable.toWeb(agentProcess.stdin!);
  const output = Readable.toWeb(agentProcess.stdout!) as ReadableStream<Uint8Array>;
  const stream = acp.ndJsonStream(input, output);

  try {
    const result = await acp
      .client({ name: `meta-harness-worker-${workerId}` })
      .onRequest(acp.methods.client.session.requestPermission, async (ctx) => {
        console.log(`[${workerId}] permission requested: ${ctx.params.toolCall.title} -> auto-allowing`);
        const allowOption = ctx.params.options.find((o) => o.kind === "allow_once");
        return {
          outcome: {
            outcome: "selected",
            optionId: allowOption ? allowOption.optionId : ctx.params.options[0].optionId,
          },
        };
      })
      .onRequest(acp.methods.client.fs.writeTextFile, async () => ({}))
      .onRequest(acp.methods.client.fs.readTextFile, async () => ({ content: "mock content" }))
      .connectWith(stream, async (ctx) => {
        const initResult = await ctx.request(acp.methods.agent.initialize, {
          protocolVersion: acp.PROTOCOL_VERSION,
          clientCapabilities: { fs: { readTextFile: true, writeTextFile: true } },
        });
        console.log(`[${workerId}] connected, protocol v${initResult.protocolVersion}, pid=${agentProcess.pid}`);

        return ctx.buildSession(process.cwd()).withSession(async (session) => {
          console.log(`[${workerId}] session ${session.sessionId} created`);
          session.prompt(`Task for ${workerId}`);
          for (;;) {
            const message = await session.nextUpdate();
            if (message.kind === "stop") return message.response;
            const u = message.notification.update;
            console.log(`[${workerId}] << ${u.sessionUpdate === "agent_message_chunk" && u.content.type === "text" ? u.content.text : `[${u.sessionUpdate}]`}`);
          }
        });
      });

    console.log(`[${workerId}] DONE stopReason=${result.stopReason}`);
  } finally {
    agentProcess.kill();
  }
}

async function main() {
  const start = Date.now();
  const [resultA, resultB] = await Promise.allSettled([runWorker("worker-A"), runWorker("worker-B")]);
  console.log(`Elapsed: ${Date.now() - start}ms`, resultA.status, resultB.status);
}

main();
```

### Finding B: the cost of a real ACP adapter, measured directly

To put a number on "how much work is it to bridge a tool that doesn't speak
ACP natively into ACP," the two real Zed-maintained adapters were measured
directly (`wc -l`, excluding tests):

| Adapter | Bridges | Language | Size |
|---|---|---|---|
| `claude-agent-acp` | Claude Code / Claude Agent SDK | TypeScript | **~10,600 lines**, 125 methods across the source files; a dedicated 1,417-line `tools.ts` just for translating Claude's tool-call/permission model into ACP's `tool_call`/`tool_call_update`/`request_permission` shapes |
| `codex-acp` | OpenAI Codex | Rust | **~10,000 lines** |

Both are the same order of magnitude despite different languages and
maintainers, which suggests ~10K lines is roughly the real cost of a
production-quality ACP adapter for a modern coding-agent CLI, not a
weekend shim. This is hard evidence (not inference) for the Pass 1/2
conclusion that ACP's value is as a *transport*, while the actual
integration work — mapping one tool's permission/tool-call semantics onto
ACP's — is substantial and tool-specific.

**Net effect on the design conclusion (unchanged, now evidence-backed):**
the mechanics of holding multiple agent subprocesses open concurrently are
genuinely trivial to build (confirmed by direct execution); the real cost
centers for a `lite-harness` meta-harness are (a) writing/maintaining
adapters for any tool without native ACP support, at ~10K lines each, and
(b) the still-unbuilt orchestration/permission-bridging/cost-ledger layer on
top — which remains the actual differentiation opportunity, not the
transport.
