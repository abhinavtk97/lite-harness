# Raw research: IDE-integrated agent harnesses

*Verbatim output of the second parallel research pass (Cursor, Windsurf, Cline, Roo Code, Continue.dev, GitHub Copilot), conducted via web search across docs, GitHub issues, CVE databases, and community discussion, current as of August 2026. See [../agent-harness-landscape.md](../agent-harness-landscape.md) for the synthesized findings.*

---

# Research Report: IDE-Integrated Coding Agent Harnesses

Scope note: all six tools are editor-native (or editor-first) harnesses, which is the opposite design point from a UI-decoupled backend. The research below surfaces exactly where each one *does* or *doesn't* have a decoupled core, since that's the load-bearing question for your design.

---

## 1. Cursor (Composer / Agent)

**Architecture**
Cursor started as a VS Code fork (2023) and remains a full standalone editor, not an extension — meaning its agent loop is compiled into the app itself, not a pluggable layer. By Cursor 3.0 (early 2026) it ships its own frontier model (**Composer**, RL-trained specifically for its own agent loop), a **Multi-Agent Interface** for up to 8 parallel agents on git worktrees (local) or remote cloud machines, and — notably — a **Cursor CLI** with both an interactive Shell Mode and a **headless mode for CI**. This is Cursor's answer to the "no automation story" gap: the CLI invokes the same agent loop outside the GUI. Model access is via Cursor's own backend (proxied/metered), not raw BYO-key by default, though enterprise/BYO-key options exist. Also ships a JetBrains plugin, iOS/Android apps, and a standalone PR-reviewing bot (**BugBot**).

**Session/Context**
Codebase indexing: files are **chunked locally**, chunks sent to Cursor's servers, embeddings computed (OpenAI or a custom embedding model), stored in a remote vector DB (**Turbopuffer**); actual file content stays local and is fetched locally at query time, only relevant chunks are shipped to the LLM. Cursor combines this semantic search with lexical/grep search in a hybrid pipeline (reported +23.5% eval lift over grep alone). Context window handling scales with the chosen model; bigger-context models let more repo content in before truncation. No first-class long-term "session persistence across restarts" comparable to Windsurf's Memories — community workaround is a manual "memory bank" of markdown files.

**Tools/Extensibility**
Built-in file edit, terminal exec, web/browser research, and (as of 2.5) a **plugin system**. MCP is supported. Rules live in `.cursor/rules/*.mdc` (superseding the legacy single `.cursorrules`), with four activation modes: **Always Apply**, **Auto Attached** (glob-triggered), **Agent Requested** (model decides), **Manual** (`@ruleName`). Also natively discovers `AGENTS.md` (the emerging cross-tool standard also read by Codex CLI, Gemini CLI, OpenCode).

**Permissions/Safety**
"YOLO mode" exists and is notoriously dangerous — see Gaps below. Cursor 3.6 (May 29, 2026) added **Cursor Auto-review**: a three-stage filter (allowlist → sandbox isolation → classifier subagent judgment) claimed to cut approval prompts ~84%. `permissions.json` allowlists exist; Enterprise can enforce sandboxed-terminal policy (sandbox availability, git/network access) at the team level. Diff review panel shows changes before apply; inline diffs are toggleable.

**Multi-agent**
Native to the product: parallel agents on local git worktrees or **Cloud/Background Agents** (renamed from "Background Agent") that read GitHub issues, open branches, commit, and draft PRs asynchronously while off-machine, with `/multitask` for fan-out subagents and Slack integration to trigger agents from a channel.

**Config**
`.cursor/rules/*.mdc`, `AGENTS.md`, `permissions.json`, per-project sandbox overrides for common ops (git clone, npm/pip install work out of the box in sandbox).

**Strengths**
Best-in-class codebase indexing/retrieval pipeline; tightest coupling of a custom agent-tuned model (Composer) to its own harness; genuinely has a CLI/headless story unlike most IDE-first competitors; enterprise-grade sandbox policy controls; fast iteration cadence.

**Gaps/Pain Points**
- **"Goes rogue" deletion incident**: widely-cited HN thread — "Cursor goes rogue in YOLO mode, deletes itself and everything else" (news.ycombinator.com/item?id=44262383); a user: "Deleting everything on my computer is absolutely insane. Felt like Ultron took over."
- **AI support hallucination scandal**: Cursor's AI-powered support bot ("Sam") fabricated a fake login policy to explain an outage, users canceled subscriptions in protest (covered by Fortune, aol.com/finance).
- **Pricing backlash (mid-2025)**: silently replaced fixed "500 fast requests/month" with usage-based credits pegged to live API cost; confusing "rate limit" language masked what was really a billing change; CEO issued public apology + refunds July 4, 2025 for June16–July4 overcharges.
- **Context loss on session reset**: Reddit threads document the context window resetting between sessions, prompting DIY "memory bank" workarounds.
- **Linux packaging complaints**: ships as AppImage, poor integration/compatibility.
- **UI lag/crashes** reported on files >500 lines with heavy agent use (Cursor forum).
- Value skepticism: "VS Code + Copilot is cheaper, more stable, doesn't lock you into a single IDE."

Sources: [Cursor 2026 guide](https://www.deployhq.com/guides/cursor), [How Cursor Indexes Codebases](https://read.engineerscodex.com/p/how-cursor-indexes-codebases-fast), [Cursor codebase indexing docs](https://cursor.com/help/customization/indexing), [HN: Cursor goes rogue](https://news.ycombinator.com/item?id=44262383), [Cursor rules guide](https://www.vibecodingacademy.ai/blog/cursor-rules-complete-guide), [Cursor changelog 2.5](https://cursor.com/changelog/2-5), [Cursor pricing backlash timeline](https://www.wearefounders.uk/cursors-pricing-disaster-the-full-timeline-of-how-an-ai-coding-darling-burned-its-most-loyal-users/), [Cursor agent sandboxing blog](https://cursor.com/blog/agent-sandboxing).

---

## 2. Windsurf (Cascade)

**Architecture**
Windsurf (formerly Codeium's IDE) was **acquired by Cognition (Devin's maker) in July 2025** for ~$250M after a collapsed $3B OpenAI deal and a $2.4B Google DeepMind reverse-acqui-hire that took CEO Varun Mohan, co-founder Douglas Chen, and key researchers (no equity). Windsurf is now being folded into Cognition's **Devin Desktop** product line ("Devin Local"). Cascade's agent loop is a **12-tool ecosystem** (file ops, semantic+pattern search, terminal exec) tightly bound to the IDE surface — **no first-class headless API for CI/CD or scripted automation** is offered; this is an explicit architectural non-goal, not a bug. Model access is proxied through Windsurf/Cognition's backend and metered by credits, not raw BYO-key.

**Session/Context**
Cascade tracks actions, terminal output, and codebase context live. Persistent context across sessions is handled by **Memories**: correcting Cascade or stating a preference gets auto-saved and referenced in future sessions (auto-generated memories don't consume credits). "Flows" are Cascade's multi-step reasoning chains — a task is broken into steps, shown as a plan, then executed with **checkpoints** at each step for rollback.

**Tools/Extensibility**
File edit, terminal, semantic + pattern search, web research built in. MCP supported, with recent updates adding more granular server-level tool permissions. Rules: legacy single `.windsurfrules` at project root (12,000-char cap) plus newer `.windsurf/rules/*.md` directory (frontmatter `trigger` field: **Always On, Manual, Model Decision, Glob**); global rules at `~/.codeium/windsurf/memories/global_rules.md` (6,000-char cap) apply across all workspaces.

**Permissions/Safety**
A documented **CVE-2025-62353** (CVSS 9.8, Critical): path traversal flaw across all Windsurf IDE versions allowing arbitrary local file read/write via direct exploit or indirect prompt injection. MCP servers run with user-level permissions — a misconfigured MCP server means full filesystem/network access; default codebase indexing includes `.env`/secrets/config files unless explicitly excluded. Early-2026 npm typosquatting campaign ("SANDWORM_MODE") specifically targeted Windsurf/Cursor/Claude Code via rogue MCP servers. No documented offline/full on-prem mode for individuals (Enterprise has hybrid deployment only) — a blocker for air-gapped/regulated environments.

**Multi-agent**
Background "flows"/tasks that run and consume credits silently (see pain points); being absorbed into Cognition's broader Devin async-agent fleet model post-acquisition, but as a standalone Cascade feature it's less parallel-agent-native than Cursor or Cline.

**Config**
`.windsurfrules` / `.windsurf/rules/*.md`, `~/.codeium/windsurf/memories/global_rules.md`, per-server MCP permission settings.

**Strengths**
Memories system is one of the more genuinely persistent cross-session context mechanisms among IDE tools (not just a summarization hack); Flows give a visible plan-then-execute UX; strong "AI-native, not bolted on" positioning pre-acquisition.

**Gaps/Pain Points**
- **CVE-2025-62353** critical path-traversal vulnerability (witness.ai security writeup).
- **Credit system backlash**: March 2026 price increase + shift to daily/weekly usage quotas; users "felt betrayed"; failed requests still charge credits; users report spending 2–3x expected.
- **Instability**: Trustpilot reviews skew heavily 1-star, citing wasted credits, login issues, inconsistent output; "Cascade times out or crashes more often than Cursor, costing a session every two weeks on average" (roborhythms.com review).
- **No headless/CI story** — explicit architectural gap vs. Cursor CLI and Cline SDK.
- **Acquisition uncertainty**: product identity in flux as it's rebranded into Devin Desktop, raising continuity/roadmap concerns for existing users.
- **No enterprise offline/air-gap mode** for individual devs.

Sources: [Windsurf security risks](https://witness.ai/blog/windsurf-security/), [Windsurf review after six months](https://www.roborhythms.com/windsurf-review/), [Cognition acquisition of Windsurf](https://cognition.com/blog/windsurf), [Devin: The Next Chapter](https://devin.ai/blog/windsurfs-next-chapter), [Windsurf rules guide](https://thepromptshelf.dev/blog/windsurfrules-complete-guide-2026/), [Windsurf Cascade docs](https://docs.windsurf.com/windsurf/cascade), [DeployHQ Windsurf guide](https://www.deployhq.com/guides/windsurf).

---

## 3. Cline (formerly Claude Dev)

**Architecture — the most relevant example for your "decoupled backend" goal**
Cline underwent a deliberate architectural rebuild in 2026, described in its own blog as moving from an architecture "inseparable from its IDE host" to a **standalone, portable SDK**: `@cline/sdk`, an open-source agent runtime that now powers Cline's **CLI**, **Kanban** (web-based multi-agent board), **VS Code extension**, and **JetBrains plugin** — all as separate front-ends over one core. Layered stack: `@cline/agents` (stateless agent loop: iteration, tool orchestration, event emission) sits under `@cline/core` (stateful orchestration: session lifecycle, persistence, config discovery). Per Cline's own description, "the agent loop is now headless by default. It does not know or care whether it is being invoked from a VS Code webview, a terminal session, or a CI pipeline." This is architecturally the closest match among the six to what you're designing. Model access: fully **BYO-key**, model-agnostic — calls providers (Anthropic, OpenAI, local models, OpenRouter, etc.) directly with no Cline-operated proxy markup.

**Session/Context**
Uses a **shadow Git repository** (separate from the project's real git history) that auto-commits file state after every tool use — this underlies checkpoints. "Focus Chain" (default on since v3.25) periodically re-anchors the agent on the original task (default cadence: every 6 messages) to fight drift, part of what Cline calls "context engineering." Known weakness: token/context growth is aggressive — GitHub Discussion #2979 reports ~300K tokens reached after just 5 iterations because Cline sends the whole code + user context + past revisions with each request; even the Memory Bank feature (meant to compress this) still produces large payloads after a few exchanges.

**Tools/Extensibility**
File edit, terminal exec (with live output streaming), browser automation, MCP client support (auto-detects tools/resources on configured servers). `.clinerules` files (version-controlled, project-specific, and the agent can edit them on request) hold standards/conventions/procedures, read automatically by CLI, VS Code, and JetBrains.

**Permissions/Safety**
**Plan/Act two-phase model**: Plan mode reads/reasons only; Act mode executes with **per-step approval** by default (every file edit/terminal command needs explicit approval). **YOLO Mode** exists and disables all safety checks (file deletion, system modification, network requests) — Cline's own docs recommend restricting it to an isolated container/VM/throwaway branch. Two complementary undo systems: Cline checkpoints ("step back two prompts") + git ("throw the whole branch out"). Known bug: **GitHub issue #9357** — "Cline executes MCP server commands without asking for approval" even when the specific tool's auto-approve checkbox is unchecked in the UI; **issue #7899** — auto-approve broken / MCP config buggy.

**Multi-agent**
Actively building toward this: **Cline Kanban** lets you run many agents in parallel from a web-based task board, each card getting its own git worktree, auto-commit, and dependency chains; **Cline CLI 2.0** turns the terminal into an "agent control plane." Publicly stated direction ("moving towards a multi-agent future with tasks running in parallel") with acknowledged open questions about visibility/control at scale.

**Config**
`.clinerules`, provider/model config (BYO API key per-provider), MCP server config, Kanban board config for parallel task orchestration.

**Strengths**
Genuinely decoupled, headless-by-default core (SDK) reused across CLI/IDE/Kanban — the strongest UI-independence story of the six tools researched; fully open source; no proxy markup on model calls (pure pass-through billing); conservative default approval model (Plan/Act) appeals to regulated/careful teams; git-based checkpoint system is transparent and inspectable (it's just another git repo).

**Gaps/Pain Points**
- **Runaway token/cost growth**: Discussion #2979/#1539 — sends full code + history each turn; ~300K tokens by iteration 5; users report hitting input limits before finishing tasks.
- **MCP auto-approve bugs**: issue #9357 (approval bypassed), issue #7899 (config buggy) — undermines trust in the granular permission model.
- **YOLO mode risk** explicitly flagged by Cline's own docs as needing container/VM isolation — same rogue-agent risk class as Cursor.
- Being a fast-moving open-source rebuild (SDK migration in 2026), some IDE-extension behavior/UX is mid-migration and less polished than commercial competitors.

Sources: [Cline SDK announcement](https://cline.bot/blog/introducing-cline-sdk-the-upgraded-agent-runtime), [MarkTechPost on Cline SDK](https://www.marktechpost.com/2026/05/14/cline-releases-cline-sdk-an-open-source-agent-runtime-now-powering-its-cli-and-kanban-with-ide-extensions-being-migrated/), [Cline Rebuilt Harness deep dive](https://essamamdani.com/blog/cline-rebuilt-harness-why-it-matters), [cline/cline GitHub](https://github.com/cline/cline), [GitHub issue #9357](https://github.com/cline/cline/issues/9357), [GitHub issue #7899](https://github.com/cline/cline/issues/7899), [Cline token discussion #2979](https://github.com/cline/cline/discussions/2979), [Cline Plan & Act docs](https://docs.cline.bot/core-workflows/plan-and-act), [Shadow Git explainer](https://medium.com/codex/clines-backroom-git-the-secret-history-of-view-changes-8523c7c6437f), [Cline CLI 2.0](https://cline.bot/blog/introducing-cline-cli-2-0), [Cline Kanban](https://azukiazusa.dev/en/blog/cline-kanban/).

---

## 4. Roo Code (Cline fork) — **now discontinued, important finding**

**Status as of this research (Aug 2026)**: Roo Code **shut down entirely on May 15, 2026** (announced April 21, 2026). All products — the VS Code extension, Roo Code Cloud, and Roo Code Router — were shut down; unused balances refunded; the GitHub repo (~24,200 stars, ~3,300 forks at archive time) is now **archived/read-only**. The team's stated reasoning: they "no longer believe IDEs are the future of coding" and pivoted to **Roomote** (roomote.dev), a Slack-first cloud agent product. Roo Code's own migration recommendation for extension users is **Cline** ("have incorporated much of what we built"); a community fork called **Zoo Code** (zoocode.dev) picked up the archived extension.

Given the shutdown, this is a useful **cautionary case study** for harness design (single-editor lock-in + no independent distribution channel = existential risk when the team's strategy pivots) rather than a live competitor to benchmark against. Findings below reflect its architecture while it was active:

**Architecture**: A Cline fork; same core VS Code-extension-bound design (not decoupled) — no independent SDK/CLI comparable to Cline's rebuild.

**Session/Context**: Context condensing (auto-summarization) was buggy — GitHub issue #10781: silently fails/stalls in "Condensing Chat" when the target model's context window is smaller than current content; issue #9831: condensing logic doesn't respect Claude Sonnet's 1M-token option on Bedrock. Issue #1989: **API costs explode** once context fills, because prompt caching stops working at that point.

**Tools/Extensibility**: **Custom Modes** (Code, Architect, Ask, Debug, + user-defined) let teams build specialized agent personas with tailored tools/instructions. A **Marketplace** (remote-loaded, 5-min cache, real-time search) distributed both MCP servers and Modes as installable components.

**Permissions/Safety**: Fine-grained **auto-approve** system — independent boolean per category (file reads, file writes, command execution, MCP calls, mode switches, subtask creation, follow-ups), a master `autoApprovalEnabled` toggle, plus numeric guardrails `allowedMaxRequests`/`allowedMaxCost` that halt autonomy after a threshold regardless of flags. Command allow/deny lists with longest-prefix-wins conflict resolution — a more granular permission model on paper than most competitors, though issue #11095 requested exact-command (not just prefix) matching because prefix matching was "inherently unsafe."

**Multi-agent**: **Boomerang Tasks** — a parent/orchestrator agent spawns specialized sub-agents for parallel execution of complex multi-step work (via Orchestrator Mode), an early version of what became a broader industry pattern (SPARC methodology community docs built on top of it).

**Config**: `.roo/` rules/modes directories, marketplace-installed Modes/MCP configs.

**Known pain points (pre-shutdown)**: pricing/cost-calculation bugs across multiple model providers (issue #8982, #8650 — cost miscalculation, especially tiered pricing above 200K tokens for Gemini/Sonnet); incorrect context-window metadata for some models (issue #9344 — Mistral Codestral shown as 256K vs actual 128K); "Critical User Experience Issues with Roo" (issue #7438) citing broken/missing context compression causing inability to maintain basic code context.

Sources: [Roo Code shutdown coverage](https://nerova.ai/news/roo-code-shutting-down-may-15-2026-what-users-should-do-next), [Bodega One shutdown/alternatives](https://www.bodegaone.ai/blog/roo-code-shutdown-alternatives), [Roo Code GitHub issues #10781](https://github.com/RooCodeInc/Roo-Code/issues/10781), [#1989](https://github.com/RooCodeInc/Roo-Code/issues/1989), [#7438](https://github.com/RooCodeInc/Roo-Code/issues/7438), [Boomerang Tasks docs](https://docs.roocode.com/features/boomerang-tasks), [Auto-Approving Actions docs](https://docs.roocode.com/features/auto-approving-actions), [Marketplace docs](https://docs.roocode.com/features/marketplace).

---

## 5. Continue.dev

**Architecture**
Open source (Apache 2.0), IDE extension (VS Code, JetBrains) **plus a CLI**. Notably, **Continue was acquired by Cursor in 2026**; the `continuedev/continue` repo is now **read-only**, v2.0.0 is the final release from the original team, and there's no more official roadmap — it's effectively a community-maintained legacy of Cursor's acquisition strategy (Cursor absorbing the leading open BYO-key alternative). This is a significant strategic signal: the two major "open, decoupled, BYO-key" tools in this space (Continue.dev, Roo Code) have both exited independent existence in 2026 — one via acquisition, one via shutdown. Model access: fully config-driven BYO-key/BYO-endpoint, provider-agnostic by design ("every provider is a plug-in").

**Session/Context**
`@Codebase` context provider (embeddings-based retrieval) is now **deprecated** in favor of built-in tools, `.continue/rules` files, and MCP servers. Indexing (when used) computes embeddings **locally** via transformers.js, stored at `~/.continue/index`, combined with keyword search. Multiple built-in context providers: `@code`, `@codebase`, `@docs`, `@file`, `@folder`, `@terminal`, `@problems`, `@diff`, `@url`, `@open`, `@repo-map`. Custom context providers are TypeScript modules implementing a small interface.

**Tools/Extensibility**
Single YAML config file (`~/.continue/config.yaml` typically) declares everything: models (with per-model `roles` array — chat/edit/apply/autocomplete/embed/rerank/summarize — enabling model routing, e.g. frontier model for Agent mode + fast local model for autocomplete, no glue code needed), context providers, rules, prompts/slash-commands, docs sources, and `mcpServers`. MCP servers plug directly into the context surface, letting teams expose Jira/GitHub/internal search without writing a custom provider.

**Permissions/Safety**
Mode-gated tool availability: **Chat mode** = no tools; **Plan mode** = read-only tools only; **Agent mode** = all tools. Default: asks permission per tool call (Continue/Cancel); **tool policies** (`tools.allow` / `tools.deny`) can set per-tool automatic approval, skipping the prompt.

**Multi-agent**
Supports multi-agent setups where **each agent has its own sandbox configuration and tool restrictions** — e.g., a fully-trusted personal-assistant agent vs. restricted family/work agents vs. sandboxed public-facing agents, all defined declaratively in config.

**Config**
Single YAML file, `.continue/rules`, `mcpServers` key, per-agent `tools.allow`/`tools.deny` and sandbox profiles.

**Strengths**
Cleanest declarative multi-model-routing config of the group (one YAML, model roles, zero glue code); genuinely provider-agnostic and always was (no proxy lock-in); the only tool researched with an explicit **per-agent sandbox profile** concept baked into config rather than bolted on.

**Gaps/Pain Points**
- **`@Codebase` context provider reliability**: GitHub issue #2695 ("Codebase context: several issues"), #7072 ("`@Codebase` doesn't include relevant files"), #4578 (folder/codebase providers **hang/freeze** even on small file sets, Windows-specific indexing bugs) — reliability problems significant enough that the team deprecated the feature entirely in favor of MCP/rules-based context.
- **Acquisition uncertainty**: repo now read-only under Cursor ownership; users who chose Continue specifically for its independence/BYO-key philosophy face the same lock-in risk they were trying to avoid, now one step removed.
- Comparatively less name-recognition/momentum than Cursor/Cline/Copilot in 2026 discourse — search results skew toward "what happened to it" rather than active usage complaints, suggesting a shrinking active-user base post-acquisition.

Sources: [Continue.dev architecture (Better Stack)](https://betterstack.com/community/guides/ai/continue-dev-ai/), [DeepWiki continuedev/continue](https://deepwiki.com/continuedev/continue), [Continue acquired by Cursor (HN)](https://news.ycombinator.com/item?id=48548758), [Cursor acquires Continue.dev writeup](https://www.bodegaone.ai/blog/cursor-acquires-continue-dev), [GitHub issue #2695](https://github.com/continuedev/continue/issues/2695), [GitHub issue #7072](https://github.com/continuedev/continue/issues/7072), [GitHub issue #4578](https://github.com/continuedev/continue/issues/4578), [Continue Agent mode docs](https://docs.continue.dev/ide-extensions/agent/how-it-works).

---

## 6. GitHub Copilot (Chat / Agent mode / Coding Agent / ex-Workspace)

**Architecture — three distinct surfaces, worth separating clearly**
1. **Copilot Chat / Agent mode (in-IDE, VS Code)**: an autonomous "peer programmer" inside the editor — analyzes codebase, proposes edits, runs terminal commands/tests, multi-step. Tightly bound to the IDE (VS Code, JetBrains, Neovim, Vim, Visual Studio, github.com web UI) — broadest **IDE surface coverage** of any tool researched, but each surface is a separate integration, not one decoupled core exposed via CLI/headless/web uniformly.
2. **Copilot coding agent** (the autonomous background/PR agent): genuinely decoupled from any editor — it's a **cloud-based, GitHub Actions-powered background task**. You assign it a GitHub Issue; it spins up in an isolated Actions container, explores the codebase, writes code, runs builds/linters/tests/security scans, and opens a draft PR — all without a human present during execution. This is architecturally the closest Copilot component to a UI-independent backend, though it's fully GitHub-hosted (not self-hostable/portable).
3. **Copilot Workspace**: technical preview **sunset May 30, 2025**; its spec→plan→code workflow was absorbed into Copilot Coding Agent (task execution) + **Copilot Spaces** (context grounding) — i.e., Workspace as a standalone product no longer exists.
There's also a standalone **Copilot CLI** and a public **Copilot SDK** (`github/copilot-sdk`) with session persistence and "infinite sessions" (see below) — this is Copilot's most SDK-like, embeddable surface.

**Session/Context**
Copilot SDK/CLI: sessions **persist to disk** (`~/.copilot/session-state/`) and can be disconnected/resumed later, even from a different process — described as "infinite sessions" via automatic **compaction**: when nearing the model's context window limit, history is replaced with an intelligent summary rather than discarded, and every compaction creates a numbered/titled **checkpoint** file enabling rewind/recovery. Separately, **codebase indexing** ("workspace index") has a **remote, per-repository, shared index** (built server-side, shared by everyone with repo access) plus a **local index** that works without GitHub sync but caps at 2,500 files and has lower accuracy than the remote semantic index.

**Tools/Extensibility**
MCP supported in Agent mode (VS Code Stable) — tools like shell commands, `@workspace` search, and custom MCP tools are all callable. Custom instructions via `.github/copilot-instructions.md` (also supports `SKILL.md`-style project skills). **Custom agents**: lightweight agent definitions (own system prompt, tool restrictions, optional MCP servers) discovered from `.github/agents/`, an org-level `{org}/.github` repo, or `~/.copilot/agents`. `AGENTS.md` is natively read.

**Permissions/Safety**
Coding agent runs in a **firewalled, sandboxed GitHub Actions container** with **read-only repo access by default**, can only push to branches prefixed `copilot/`, and all existing branch protections/required checks still apply. Documented limitation: the **firewall only covers processes started via the agent's own Bash tool** — it does **not** apply to MCP servers or to processes launched in custom setup steps, and doesn't apply to anything outside the Actions appliance — an explicit, acknowledged gap ("sophisticated attacks may bypass the firewall... allowing unauthorized network access and data exfiltration," per Microsoft Learn's own security module). Firewall config is **repo-level only** — no org/enterprise policy to stop a repo admin from disabling it, a governance gap GitHub's own community discussions flag (#171470). Known functional bug: agent **cannot access git submodules** even within the same org (community discussion #180953).

**Multi-agent**
**Fleet Mode** (`/fleet` in Copilot CLI): an orchestrator layer decomposes an objective into independent work "tracks" and dispatches multiple sub-agents in parallel. Also "Mission Control" for orchestrating agents, and the CLI can delegate specific sub-tasks to the cloud Coding Agent.

**Config**
`.github/copilot-instructions.md`, `.github/agents/*.md` (with frontmatter), `AGENTS.md`, firewall allowlist config (repo-level YAML/settings), Copilot Spaces for context grounding.

**Strengths**
Broadest IDE reach of any tool in this survey (VS Code, JetBrains, Neovim, Vim, Visual Studio, github.com); the Coding Agent is the most "enterprise CI-native" async background-agent design (runs inside your own Actions infra, respects existing branch protections rather than needing its own trust model); deep native GitHub platform integration (issues → agent → PR → checks, all first-party); strong enterprise governance (SSO, audit logs, IP indemnification, SOC 2, policy controls); GitHub's Q4 2025 research claims 35% faster task completion for Enterprise teams (self-reported).

**Gaps/Pain Points**
- **Premium-request rate-limit backlash**: The Register — "Customers revolt as GitHub Copilot 'fixes' rate limits" (April 2026); GitHub community discussion #192485 reports **hours-to-200+ hours of lockout** after hitting `user_weekly_rate_limited`; discussion #164026 ("Extremely Disappointed with GitHub Copilot Premium Request Limits") and #197702 (limits "consumed way too fast" after a recent update) show sustained, recurring complaint volume ("about three dozen [complaints] in the past two days" per one report).
- **Firewall coverage gap**: explicitly does not cover MCP servers or custom setup-step processes — a real prompt-injection/exfiltration attack surface acknowledged in GitHub's own docs.
- **No org-level firewall enforcement**: repo admins can unilaterally disable agent network restrictions with no central policy override.
- **Git submodule support broken**: agent can't read/commit across submodule boundaries even in-org.
- **Cost model shift to credits**: complex Agent-mode tasks on frontier models (e.g., Opus-class) can burn **$0.50–$2.00 of credits per task** depending on context length/tool calls — same "agent mode is expensive" complaint pattern seen with Windsurf/Cursor.
- **Workspace's discontinuation** left some users confused about where the spec→plan workflow moved (community discussion #195273, "What happened to @workspace").

Sources: [GitHub Docs: About coding agent](https://docs.github.com/copilot/concepts/agents/coding-agent/about-coding-agent), [Security risks/limitations (MS Learn)](https://learn.microsoft.com/en-us/training/modules/github-copilot-code-agent/2-security-risks-limitations-copilot-code-agent), [Customize agent firewall docs](https://docs.github.com/en/copilot/how-tos/use-copilot-agents/coding-agent/customize-the-agent-firewall), [Community discussion #171470 (firewall)](https://github.com/orgs/community/discussions/171470), [Community discussion #180953 (submodules)](https://github.com/orgs/community/discussions/180953), [The Register: rate-limit revolt](https://www.theregister.com/software/2026/04/15/customers-revolt-as-github-copilot-fixes-rate-limits/5225088), [Community discussion #192485](https://github.com/orgs/community/discussions/192485), [Copilot SDK session persistence docs](https://docs.github.com/en/copilot/how-tos/copilot-sdk/use-copilot-sdk/session-persistence), [Copilot CLI context management](https://docs.github.com/en/copilot/concepts/agents/copilot-cli/context-management), [Fleet mode](https://github.blog/ai-and-ml/github-copilot/run-multiple-agents-at-once-with-fleet-in-copilot-cli/), [Custom agents docs](https://docs.github.com/en/copilot/how-tos/copilot-sdk/features/custom-agents), [Copilot Workspace sunset](https://github.com/orgs/community/discussions/195273).

---

## Cross-Cutting Observations for Your Design

1. **Decoupled-core precedent**: Of the six, **Cline's `@cline/sdk` rebuild** (2026) is the clearest existing precedent for exactly what you're building — a headless-by-default agent runtime with CLI/IDE/web-board front-ends over one core. Continue.dev's per-agent sandbox-profile config is a useful reference for multi-agent permission design. GitHub's split between in-IDE Agent mode (bound) and Coding Agent (genuinely decoupled, Actions-hosted) shows a viable pattern of *two different harnesses sharing conventions* (`AGENTS.md`, MCP) rather than one universal core — worth deciding early whether you want one core or a shared-convention federation.
2. **"Rogue agent" incidents are a recurring, cross-vendor pattern**, not a Cursor-only problem: Cursor's YOLO-mode deletion, Cline's own docs warning YOLO mode needs container isolation, Windsurf's CVE-2025-62353 (prompt-injection-triggered arbitrary file read/write). A first-class sandbox/isolation model, not just an approval-prompt toggle, is table stakes.
3. **Credit/token billing opacity is the single most common user complaint** across every commercial tool surveyed (Cursor's June 2025 pricing fumble, Windsurf's March 2026 quota shift + silent background-task credit burn, Copilot's premium-request lockouts, Roo Code's cost-miscalculation bugs). A transparent, predictable cost-accounting model in the harness is a real differentiator opportunity.
4. **Two of six tools exited independent existence during this research window** (Roo Code shut down May 2026; Continue.dev acquired by Cursor, repo now read-only) — both were the more "open/decoupled/BYO-key" options in the set. That's a cautionary signal about the commercial viability of pure open decoupled harnesses without a strong distribution/monetization wedge — worth factoring into your business-model thinking, not just the technical architecture.
5. **MCP is now universal table stakes** across all six (even the discontinued Roo Code had it), as are rules/instructions files, converging on `AGENTS.md` as the emerging cross-tool convention alongside vendor-specific formats (`.cursor/rules/*.mdc`, `.clinerules`, `.windsurfrules`, `.github/copilot-instructions.md`).
6. **Auto-approve/permission granularity bugs recur** independently across vendors (Cline #9357/#7899, implicitly similar risk in Cursor's allowlist-vs-sandbox interaction) — suggesting this is a genuinely hard problem (race conditions between UI checkbox state and agent-loop enforcement), not vendor incompetence; worth designing your permission enforcement as a hard boundary in the core loop rather than a UI-layer gate.
