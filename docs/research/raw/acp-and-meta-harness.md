# Raw research: ACP and the "meta-harness" pattern

*Verbatim output of a follow-up research pass (two parallel queries), prompted by exploring whether `lite-harness` could act as a meta-harness — spinning up other existing coding-agent harnesses (Claude Code, OpenCode, Codex CLI, Gemini CLI, etc.) as subagents — and whether the Agent Client Protocol (ACP) could help. Conducted via web search, current as of August 2026. See [../agent-harness-landscape.md](../agent-harness-landscape.md) for the synthesized finding (section: "Candidate USP: meta-harness").*

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
