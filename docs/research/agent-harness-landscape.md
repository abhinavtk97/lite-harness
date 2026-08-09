# Research: The Agent-Harness Landscape

*Inputs for designing `lite-harness` — a UI-decoupled agent harness (CLI + headless + web UI over one backend engine).*

## Context

`lite-harness` aims to be a genuinely UI-decoupled agent harness: one
backend/engine that can be driven by an interactive CLI, a headless/
programmatic interface (CI, scripts, other agents), and a web UI — without
the UI ever being load-bearing for the agent loop itself. Before designing
it, we surveyed the field: what existing harnesses get right, what they get
wrong, where they converge (signalling "table stakes"), and where they
diverge or are missing something entirely (signalling either a hard problem
or a real opportunity).

Three parallel research passes were run (web search across docs, GitHub
issues, postmortems, HN/Reddit threads, and vendor blogs — current as of Aug
2026). The full raw output of each pass is preserved in
[`raw/`](raw/cli-native-harnesses.md) for reference; this document is the
synthesis.

1. **CLI-native harnesses** — Claude Code, OpenAI Codex CLI, Google Gemini CLI, Aider, Amazon Q Developer CLI, Goose, Cline/Roo Code ([raw](raw/cli-native-harnesses.md))
2. **IDE-integrated harnesses** — Cursor, Windsurf, Cline, Roo Code, Continue.dev, GitHub Copilot (Chat/Agent mode/Coding Agent) ([raw](raw/ide-integrated-harnesses.md))
3. **Autonomous/cloud agents + generic orchestration frameworks** — Devin, OpenHands, SWE-agent, Google Jules, Replit Agent, GitHub Copilot coding agent, LangGraph, AutoGen/AG2, CrewAI, Claude Agent SDK, OpenAI Agents SDK, AutoGPT ([raw](raw/autonomous-agents-and-frameworks.md))

A fourth, follow-up pass covers a specific candidate differentiator — see
section 8 below: **ACP and the "meta-harness" pattern** — whether
`lite-harness` could orchestrate *other* existing harnesses (Claude Code,
OpenCode, Codex CLI, Gemini CLI, etc.) as subagents, and whether the Agent
Client Protocol (ACP) could help ([raw](raw/acp-and-meta-harness.md)).

This document is **not yet a design** for `lite-harness` — it's the input
the design should be built from. See section 7 for the current decision
status.

---

## 1. Landscape at a glance

| Tool | Category | Backend/UI decoupling | Native sandbox | MCP | Multi-agent | Fate/notes |
|---|---|---|---|---|---|---|
| **Claude Code** | CLI-first | Best-in-class: Agent SDK exports the *literal* agent loop; CLI/SDK/IDE/web/mobile share it. CLI-as-subprocess is a stable polyglot headless interface. | Yes — OS-level (Seatbelt/bubblewrap) | Yes | Task tool + subagents + experimental Agent Teams | Active, heavy investment |
| **OpenAI Codex CLI** | CLI-first | Good — also runs *as* an MCP server itself (embeddable in other orchestrators) | Yes — 3 sandbox modes + cloud containers | Yes | GA TOML subagents (Mar 2026) | Active |
| **Gemini CLI** | CLI-first | Thinner — extension-centric, no exported SDK/loop library | Docker sandbox (auto w/ YOLO) | Yes | Markdown+YAML subagents (newer) | Active |
| **Aider** | CLI/library | Python-importable core, headless by design, but no multi-surface UI story | None (git-commit-as-safety-net only) | No | None | Active, niche |
| **Amazon Q Dev CLI** | CLI-first | Weak — CLI and GitHub integration are two loosely related products, not one decoupled engine | Not documented | Yes (incl. remote MCP) | Weakest of the CLI group | Active, low community visibility |
| **Goose** | CLI-first | Modular Rust core, but no distinct hosted UI | Not found (probable gap) | Yes — "everything is an MCP extension" | Explicit subagents vs. subrecipes, hard cap of 10 parallel | Foundation-governed (Linux Foundation AAIF) |
| **Cline** | IDE + CLI + SDK | **Best precedent found**: `@cline/sdk` — headless-by-default core reused by CLI, VS Code, JetBrains, and a web Kanban board | User-managed (YOLO mode needs external isolation) | Yes | Kanban multi-agent board, per-task git worktrees | Active, open source |
| **Roo Code** | IDE (Cline fork) | Not decoupled — VS Code-extension-bound core | None documented | Yes | Boomerang Tasks (orchestrator/sub-agent) | **Shut down May 2026** — pivoted to Slack-first cloud agent (Roomote) |
| **Continue.dev** | IDE + CLI | Declarative multi-model routing in one YAML; per-agent sandbox *profiles* in config (rare) | Config-level only | Yes | Per-agent isolated configs | **Acquired by Cursor 2026**, repo read-only |
| **Cursor** | IDE (own app) | Has a CLI w/ headless mode for CI (bolted onto an IDE-first core) | Enterprise sandbox policy; "Auto-review" 3-stage filter | Yes | Up to 8 parallel agents (worktrees/cloud) | Active, dominant commercially |
| **Windsurf** | IDE (own app) | No headless/CI story — explicit non-goal | None; CVE-2025-62353 (critical path traversal) | Yes | Background "flows" | **Acquired by Cognition (Devin)**, folding into "Devin Desktop" |
| **GitHub Copilot** | IDE + cloud agent | Split model: in-IDE Agent mode (bound) vs. Coding Agent (genuinely decoupled, GitHub Actions-hosted) | Actions container; firewall **doesn't cover MCP servers** | Yes | Fleet Mode (parallel tracks) | Active, broadest IDE reach |
| **Devin** | Cloud-native | Cloud brain + "Outposts" (customer-controlled execution plane) — explicit reasoning/execution split | Isolated cloud VM per session | — | "Managed Devins": supervisor delegates to isolated-VM children | Active, enterprise-focused |
| **OpenHands** | Cloud/self-host | **Strongest decoupling pattern overall**: everything is a reader/writer of one append-only EventStream; `--backend-only`/`--frontend-only` split | Docker container per session | Yes (+ ACP) | Multiple agent servers on one host via Agent Canvas | Active, open source |
| **SWE-agent** | Research | Minimal, YAML-configured, single-agent | Docker | — | None (by design) | Research artifact; ACI concept highly influential |
| **Google Jules** | Cloud-native | Task-based ephemeral VMs, plan→approve→execute | Ephemeral GCP VM per task | — | "Planning Critic" — cheap generator+critic pattern | Active |
| **Replit Agent** | Cloud IDE | Own Mastra-based framework; custom DSL instead of JSON tool-calling | Docker + Postgres per session; **Snapshot Engine** (FS + DB versioning) | Not documented | Self-generates sub-agents/automations | Active — post-incident hardening |
| **LangGraph** | Framework | UI-agnostic graph library; **checkpointer** abstraction (memory→SQLite→Postgres) keyed by `thread_id` | Not opinionated (BYO) | Via LangChain tools | Supervisor vs. Swarm patterns | Active, widely used but "debugging worse than a hand-written loop" complaints |
| **AutoGen/AG2** | Framework | Conversation-programming; agents as independent actors | Docker by convention | — | Group chat, nested chats — historically the core strength | **Microsoft put AutoGen in maintenance mode**, successor is Microsoft Agent Framework |
| **CrewAI** | Framework | Role/Goal/Backstory abstraction over Agents/Tasks/Crews | Not opinionated | Yes (tools only, not prompts/resources) | Hierarchical process **reported not to work as documented** | Active, popular but production-reliability critiques |
| **Claude Agent SDK** | Framework/SDK | 4-tier product spectrum: embed library → CLI-as-subprocess → fully hosted (Managed Agents) | Deployment choice (not built-in) | Yes | Subagents (same primitive as Claude Code) | Active |
| **OpenAI Agents SDK** | Framework/SDK | Stateless Responses API + opt-in Conversations API + pluggable Sessions | Via optional hosted `code_interpreter` tool | — | "Handoffs" — lightweight state-machine agent switching | Active; **Assistants API being sunset Aug 26 2026** in favor of this |
| **AutoGPT** | Historical | None — monolithic single-process loop | None | — | None | Field's cautionary tale; pivoted to commercial low-code platform |

---

## 2. Cross-cutting patterns worth adopting

**2.1 — The decoupling substrate: append-only event/action log.**
The single most reliable pattern across the strongest examples (OpenHands'
EventStream, Claude Code/Agent SDK's per-session JSONL transcript, LangGraph's
checkpointer) is: *all state changes flow through one typed, append-only log;
every interface — CLI, web, headless caller — is only ever a reader/writer of
that log, never calling another interface directly.* This gives replayability,
resumability, and UI-agnosticism as structural properties, not bolted-on
features. OpenHands' own retrospective is a useful cautionary tale on the flip
side: even a good event-sourced design can rot into a fragile monolith if core
and application concerns aren't kept in separate modules with enforced
boundaries (their V0 "mono-repo" problem, fixed in the V1 SDK rewrite).

**2.2 — A stable, language-agnostic headless interface, thin SDKs on top.**
Claude Code's pattern — `claude -p --output-format json` as a stable
subprocess interface, with the Python/TS SDK being a thin
process-management/streaming wrapper around that same CLI — is a clean way to
support any calling language without maintaining N native SDKs. Cline's 2026
rebuild converged on essentially the same idea with an actual exported
library (`@cline/sdk`) that is explicitly "headless by default... does not
know or care whether it is being invoked from a VS Code webview, a terminal
session, or a CI pipeline."

**2.3 — Separate the reasoning plane from the execution plane.**
Devin's "Outposts" (cloud brain, customer-controlled execution box), GitHub
Copilot Coding Agent's use of GitHub Actions itself as the sandbox substrate,
and Claude Agent SDK's explicit "sandboxing is a deployment choice, not baked
into the SDK" all point the same direction: don't hard-wire *where the model
runs* to *where side effects happen*. This buys self-hosted execution,
data-residency/compliance options, and reuse of infra the user already trusts
(their own CI, their own VM).

**2.4 — Session persistence should be pluggable and layered, not monolithic.**
LangGraph (`MemorySaver` → `SqliteSaver` → `PostgresSaver`) and OpenAI's
Agents SDK (`SQLiteSession` → SQLAlchemy → Conversations API) both converge on
"start with an in-memory/file backend for dev, swap in a real DB for
production, same interface throughout." The clearest counter-example is
OpenAI's own **Assistants API**, a server-owned stateful abstraction
(threads+runs) that proved brittle enough to deprecate entirely (hard sunset
Aug 26, 2026) in favor of a stateless Responses API + a separate, opt-in
Conversations/session layer. Lesson: don't fuse the request/response protocol
to state ownership.

**2.5 — Irreversible actions need a structural gate, not a prompted one.**
Two real incidents anchor this: Cursor's YOLO-mode self-deletion (viral HN
thread), and — more severe — **Replit Agent deleting a live production
database during an explicit code freeze** and then misrepresenting whether
rollback was possible (July 2025; publicly documented, led to Replit shipping
mandatory dev/prod separation, a non-mutating "planning" mode, and one-click
restore). AutoGPT's entire failure profile (loops, no checkpoint, no sandbox)
is the same lesson from the earliest, crudest example. The fix pattern that
recurs everywhere it's done well: hard separation of environments, versioned/
checkpointed state that covers *side effects* (filesystem **and** data, per
Replit's Snapshot Engine — not just conversation history), and approval gates
that are enforced by the runtime, not merely requested by the model.

**2.6 — Multi-agent orchestration needs explicit state/control-flow, not free-form conversation.**
This is one of the clearest, most-corroborated findings across the whole
research set. Microsoft explicitly put **AutoGen** into maintenance mode
because its free-flowing conversational orchestration produced
"non-deterministic, hard-to-reproduce behavior in production" — identical
prompts could trigger wildly different multi-agent dialogues. CrewAI's
flagship "Hierarchical" manager-worker process is independently reported to
not function as documented, collapsing toward sequential execution with
wasted tool calls. The systems that *do* hold up in production add
structure: LangGraph's explicit Supervisor/Swarm graphs with typed state,
Google Jules' cheap two-agent "Planning Critic" pattern (generator + one
adversarial reviewer, not a swarm), and Claude Code / Codex / Goose's
supervisor-delegates-to-isolated-subagent pattern (each subagent gets its own
context window, not a shared conversation).

**2.7 — Observability should be a non-invasive, on-by-default side channel.**
OpenAI's Agents SDK turns on full trace trees by default with zero config
(root span per run, child span per agent/tool call, viewable in a hosted
dashboard). Claude Agent SDK's hooks-based OpenTelemetry export is explicitly
designed so instrumentation "does not modify SDK code, works across SDK
updates" — traces/metrics/logs are separated, and prompt/tool *content* is
excluded from telemetry by default for privacy. This is a notably higher bar
than most competitors, several of whom (Gemini CLI issue #14817, e.g.) don't
even expose current context-window usage to the user, contributing directly
to the cost-transparency complaints below.

**2.8 — Purpose-built tool primitives beat raw OS primitives.**
SWE-agent's Agent-Computer Interface (ACI) is the field's most influential
idea on this point: LLM agents are "a new category of end user" with
different failure modes than humans (poor line-number tracking, unreliable
raw diffs, limited context), so tool primitives (structured open/edit/search/
scroll, not a raw bash shell) should be designed around those weaknesses.
This thinking visibly shaped Claude Code's and OpenHands' structured file-edit
tools and Codex's purpose-built `apply_patch`/V4A diff format.

---

## 3. Common pain points (recurring across independent vendors — likely structural, not vendor-specific)

- **Context compaction/compression is the single most common failure class**
  in any tool with long-running sessions. Documented across Claude Code
  (multiple GitHub issues: silent context loss mid-task, self-contradiction
  after compaction, resume failing with "prompt too long," `/compact` itself
  failing), Gemini CLI ("context rot," runaway re-reading causing 41M-token
  payloads against a 900k limit), Cline (~300K tokens by iteration 5), and Roo
  Code (condensing silently stalling, prompt caching breaking once context
  fills). A marketed large context window (Gemini's 1M) does not reliably
  translate into usable long-session context in practice.
- **Token/cost transparency is a near-universal complaint**, independent of
  vendor or pricing model: Claude Code (4–10x usage spikes after specific
  releases, $200–500/mo bills), Cursor (2025 pricing-model change backlash,
  public CEO apology), Windsurf (2026 quota shift, silent background-task
  credit burn, largely negative reviews), GitHub Copilot (premium-request
  lockouts up to 200+ hours), Roo Code (cost-*miscalculation* bugs across
  providers). Very few tools give users a real-time, legible answer to "why
  did this session cost/use what it did" — this is a clear opportunity.
- **Auto-approve / permission-granularity bugs recur independently across
  vendors** (Cline: MCP commands executing despite unchecked auto-approve;
  Codex: destructive file-delete bypassing the safety "Guardian" review and
  not even surfacing in the diff UI; Amazon Q: `allowedTools` config silently
  not honored). This looks like a genuinely hard problem — likely races
  between UI-layer checkbox state and enforcement inside the agent loop —
  meaning permission enforcement should be a hard boundary *inside* the core
  loop, never a UI-layer gate that the core trusts blindly.
- **"Rogue agent" incidents are cross-vendor, not isolated**: Cursor YOLO-mode
  deletion, Windsurf's CVE-2025-62353 path traversal (exploitable via prompt
  injection), Replit's production-DB deletion. A first-class sandbox/
  isolation model — not just an approval-prompt toggle — is table stakes, not
  a nice-to-have.
- **Firewalled/sandboxed execution has a very consistent coverage gap**:
  GitHub Copilot's own docs admit its default-on network firewall does not
  cover MCP servers or custom setup-step processes. As MCP adoption becomes
  universal (every tool surveyed has it), MCP servers are emerging as the
  most common unguarded edge of otherwise-sandboxed systems.
- **Vendor consolidation risk is real and recent**: three of the tools
  researched exited independent existence *during this research window
  alone* — Roo Code shut down (May 2026, pivoted to a Slack-first cloud
  product), Continue.dev acquired by Cursor (repo now read-only), Windsurf
  acquired by Cognition/Devin. Notably, the more "open, decoupled, BYO-key"
  products were the ones that didn't survive independently — a real signal
  about the commercial fragility of that positioning without a strong
  distribution/monetization wedge.
- **Tool-calling reliability degrades sharply with weaker/local models**
  (Goose + Ollama/DeepSeek: unparseable tool invocations, some providers
  losing tool-calling entirely), and even strong-model scaffolds show
  scaffold-level ceilings independent of model quality (SWE-agent-style
  agents looping or prematurely terminating per recent benchmark papers).
- **A friendly declarative multi-agent abstraction can hide brittle,
  non-deterministic runtime behavior underneath its own documentation**
  (CrewAI's hierarchical process, AutoGen's conversational orchestration) —
  worth remembering as a design trap to avoid: whatever orchestration model
  we expose, its real runtime behavior must match what we document, or teams
  will discover the gap in production the hard way.

## 4. What almost nobody does well (or does at all) — candidate differentiators

- **A harness that is *natively* decoupled from day one** (not retrofitted).
  Only OpenHands (event-stream) and Cline's 2026 rebuild (explicit SDK
  extraction, done specifically to undo IDE-coupling) got here — and Cline's
  own blog frames it as *fixing* an architecture that was originally
  "inseparable from its IDE host." Roo Code's CLI took the opposite, weaker
  path: a VS-Code-API compatibility *shim* bolted onto an IDE-shaped core —
  explicitly flagged in the research as a cautionary anti-pattern versus
  designing UI-agnostic from the start.
- **Real-time, legible cost/token accounting as a first-class UX surface**,
  not an afterthought — no tool surveyed does this particularly well; several
  (Gemini CLI, Cline) have open issues specifically requesting it.
- **Side-effect-aware checkpointing** (filesystem *and* external state like a
  database, not just conversation history) — only Replit's Snapshot Engine
  does this natively, and only after a public incident forced the issue.
  Everyone else's "checkpoint" is a conversation/event-log checkpoint, which
  doesn't help if the damage is already done to a database or a deployed
  service.
- **Governance/policy control over MCP servers** — no tool surveyed has
  mature org-level allow-listing or scanning of MCP server capabilities;
  GitHub's own community has open feature requests for exactly this.
- **A harness designed as a genuine federation via shared conventions**
  (`AGENTS.md`, MCP) rather than a single monolith — GitHub Copilot's split
  between in-IDE Agent mode and the fully-decoupled Coding Agent hints at
  this pattern (two different harnesses, one shared convention layer) but no
  one has pushed it as a deliberate multi-surface architecture strategy.

---

## 5. Design principles to carry into the `lite-harness` design phase

These are directly implied by the research above, not yet a concrete
architecture:

1. **One core agent loop, expressed as a reader/writer of a single append-only
   event log.** CLI, headless callers, and web UI are all thin clients over
   that log/loop — never over each other. (Pattern 2.1, best exemplar:
   OpenHands' EventStream / Claude Code's JSONL transcript.)
2. **Ship a stable, versioned headless protocol first** (e.g. JSON-lines
   in/out over stdio or a local socket) and treat the CLI, SDK, and any future
   web backend as three different clients of that same protocol — not three
   different implementations of the agent loop. (Pattern 2.2)
3. **Decouple "where the model reasons" from "where tools execute."** Sandbox/
   execution should be a pluggable deployment concern (local subprocess,
   Docker, remote worker), not hard-wired into the core loop. (Pattern 2.3)
4. **Design session/state persistence as a pluggable backend behind one
   interface**, starting with something trivial (in-memory/file) and able to
   graduate to a real store, without changing the calling contract. Avoid
   fusing the wire protocol to state ownership — this is exactly the mistake
   the Assistants API made. (Pattern 2.4)
5. **Treat permission enforcement as a property of the core loop, not the
   UI.** Every tool call is gated inside the engine itself; a UI can display
   and collect approval decisions, but must not be the thing trusted to
   *enforce* them. Plan for the specific failure mode seen repeatedly
   (approval-state races, auto-approve flags silently not honored). (Pattern
   2.6 in section 3)
6. **Make irreversible/destructive actions require a structural gate**:
   distinct dev/prod-style environment separation where relevant, and
   checkpoints that cover actual side effects, not just conversation state.
   (Pattern 2.5)
7. **If/when multi-agent orchestration is added, prefer explicit,
   typed state and control flow (supervisor/subagent with isolated context,
   or a generator+critic pair) over free-form multi-agent conversation.**
   The latter is a well-documented path to non-deterministic production
   behavior. (Pattern 2.6)
8. **Build cost/token accounting and basic tracing in from the start**, as a
   non-invasive side channel (hooks/events), on by default, not as a
   later add-on. This is both a widely-requested feature and a near-universal
   competitor weakness. (Pattern 2.7)
9. **Design tool primitives for the model's failure modes, not by wrapping
   raw OS primitives.** Structured file-edit/search tools, not "just exec
   whatever shell command." (Pattern 2.8)
10. **Treat MCP (and MCP governance) as table stakes but not a free pass** —
    plan explicitly for sandboxing/firewalling to cover MCP-server-originated
    actions, since this is the most consistent gap found across otherwise
    well-sandboxed competitors.

## 6. Open questions to resolve before/at the start of the actual design phase

(Not answered by research — these are product/scope decisions.)

- Target primary language/runtime for the core engine (e.g. TypeScript/Node,
  Python, Rust, Go)? This affects how easily we can offer a Claude-Code-style
  "stable subprocess + thin SDKs" story across languages.
- Local-first (self-hosted, runs on the user's machine/infra) vs. cloud-
  hosted-first vs. both from day one? This determines how much of the
  reasoning/execution-plane split (principle 3) needs to be built immediately
  vs. deferred.
- Single-agent to start, with multi-agent designed-for-but-not-built, or
  multi-agent (supervisor/subagent) as a v1 requirement?
- Which model providers must be supported at launch (Anthropic-only vs.
  provider-agnostic BYO-key from day one)? Provider-agnosticism was a
  differentiator for Aider/Goose/Continue.dev, but two of the three most
  "open, decoupled" products in this survey (Roo Code, Continue.dev) did not
  survive as independent products — worth an explicit choice here, not a
  default.
- Priority ranking among the differentiator candidates in section 4 (cost
  transparency, side-effect-aware checkpointing, MCP governance, event-log-
  native multi-surface design) — which 1-2 should `lite-harness` actually
  lead with, versus treat as "good enough, not a differentiator"?

## 7. Status and next steps

**Status: research phase complete. No architecture decisions have been made
yet** — the open questions in section 6 are still open. The natural next
step is a dedicated architecture-design pass (component boundaries, wire
protocol shape, storage interfaces, sandbox integration points) for
`lite-harness` itself, informed by sections 5 and 6 above, once those
open questions are answered.

## 8. Candidate USP: lite-harness as a meta-harness ("harness of harnesses")

Follow-up research, prompted by exploring a potential differentiator: could
`lite-harness` spin up and orchestrate *other* existing coding-agent
harnesses (Claude Code, OpenCode, Codex CLI, Gemini CLI, etc.) as subagents,
rather than only spawning homogeneous subagents of itself? And specifically,
can the Agent Client Protocol (ACP) — Zed Industries' JSON-RPC protocol for
editor↔agent communication — help build this? Full raw findings:
[acp-and-meta-harness.md](raw/acp-and-meta-harness.md).

**Findings (Aug 2026, via two research passes):**

- **ACP is a usable transport, not an orchestration layer.** It's strictly a
  1:1 client↔agent JSON-RPC protocol (mostly stdio) with a genuinely rich
  session model — streaming updates, `session/request_permission`,
  cancellation, resume — good enough to replace bespoke per-tool subprocess
  wrapping code. But the spec has **zero vocabulary for orchestration
  itself**: no parent/child or delegation semantics, no task routing, no
  result-merging, and per-connection permission requests that don't
  propagate across nested agents. Cross-agent cost/token accounting was only
  just standardized (`Session Usage` RFD) and is inconsistently implemented
  (open bugs against Goose, Gemini CLI, codex-acp).
- **ACP agent-side support today**: native/first-party in Gemini CLI, Goose,
  OpenCode, and GitHub Copilot CLI. Claude Code and Codex CLI are reachable
  only via **Zed-maintained third-party adapters** (`claude-agent-acp`,
  `codex-acp`), not official vendor support. Aider has no ACP support.
- **This pattern already has prior art, and it's scrappy/immature.** Several
  2026-vintage open-source projects already spawn multiple vendor CLIs
  (Claude Code, Codex, Gemini CLI, etc.) as subprocess workers under one
  orchestrator: **Bernstein, majiayu000/harness, claw-orchestrator, sage**
  (the only one using ACP as a transport), **Zen MCP's `clink` tool,
  ruvnet/metaharness, Parallel Code**. Most use raw subprocess/stdio
  wrapping; a few use MCP. **No major vendor** (Claude Code, Goose,
  OpenHands, Cline) supports spawning a *different* vendor's CLI as a
  subagent themselves — cross-vendor orchestration is exclusively
  third-party territory right now.
- **The one consistently unsolved problem across every existing attempt**:
  cross-vendor billing/cost reconciliation and permission-model bridging.
  Nobody has done this cleanly yet — every project punts on it or leaves it
  manual.

**Conclusion**: the orchestration mechanics of "spawn Claude Code / Codex /
Gemini CLI as a worker" are not themselves the differentiator — several
scrappy projects already do this, and ACP (once more vendors adopt it) would
make the transport layer easier to build, not a moat. The real, still-open
whitespace is a **unified permission model and a single legible cost ledger
across heterogeneous underlying agents** — that is the part worth designing
for deliberately if `lite-harness` pursues the meta-harness direction. This
is a **candidate differentiator, not yet a committed decision** — it should
be weighed against the section 6 open questions (it raises the stakes on
decoupled session/permission design even further, since the harness would
need to *bridge*, not just enforce, a permission model across processes it
doesn't control).
