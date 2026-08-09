# Raw research: CLI-native agent harnesses

*Verbatim output of the first parallel research pass (Claude Code, OpenAI Codex CLI, Google Gemini CLI, Aider, Amazon Q Developer CLI, Goose, Cline/Roo Code), conducted via web search across docs, GitHub issues, and community discussion, current as of August 2026. See [../agent-harness-landscape.md](../agent-harness-landscape.md) for the synthesized findings.*

---

# AI Coding-Agent Harness Research Report

Research conducted via web search/fetch, current as of August 2026. Findings are cited inline with source links at the end of each tool section where applicable.

---

## 1. Claude Code (Anthropic)

**Architecture**
Five-layer design: **surface** (Interactive CLI, Headless CLI, Agent SDK, IDE/Desktop/Browser extensions, web UI at claude.ai/code), **core** (agent loop, compaction pipeline), **safety/action** (permission system incl. auto-mode classifier, hook pipeline, built-in tools, MCP tools, shell sandbox, subagent spawning), **state** (context assembly, session persistence, CLAUDE.md/memory, sidechain transcripts), **backend** (execution backends, external resources). Critically, the terminal client talks directly to the model API with no required backend server — but the same agent loop is also exposed programmatically via the **Claude Agent SDK** (Python/TypeScript, `query()` function) so the exact loop that powers the CLI can be embedded in another product/pipeline. This makes Claude Code the most clearly "engine decoupled from UI" of the group: same core loop drives interactive TTY CLI, `-p`/headless one-shot mode (`claude -p "query"`, no TTY, for CI/CD, cron, pre-commit hooks), the SDK (embeddable), and a hosted web/mobile UI (claude.ai/code, iOS) that clones/edits/tests in a sandboxed remote environment.

**Session/Context**
Sessions persist as transcripts on disk; resumable via `--resume`/session IDs. Auto-compaction pipeline summarizes when context nears the limit; **manual `/compact`** also available. SDK exposes `fork_session()` which rewrites session/message IDs (not byte copies) to branch a conversation into an independent resumable session while leaving the original untouched.

**Tools/Extensibility**
Built-in tools (Read/Edit/Write/Bash/Grep/Glob/WebFetch/WebSearch/Task, etc.), full **MCP client** support (stdio/HTTP/SSE servers), and an extremely granular **hooks** system with ~25+ lifecycle events spanning session (SessionStart/SessionEnd/Setup), per-turn (UserPromptSubmit, Stop, StopFailure), tool execution (PreToolUse, PermissionRequest, PermissionDenied, PostToolUse, PostToolBatch), subagents (SubagentStart/Stop, TeammateIdle), task management (TaskCreated/TaskCompleted), environment (CwdChanged, DirectoryAdded, FileChanged, InstructionsLoaded), config/notification (ConfigChange, Notification), worktrees (WorktreeCreate/Remove), compaction (PreCompact/PostCompact), and MCP elicitation events. Hooks can be shell commands, HTTP webhooks, MCP tool calls, or even LLM-prompt-based single-turn judges. Custom slash commands and "Skills" (packaged instruction sets, optionally running as subagents) round out extensibility.

**Permissions/Safety**
Three core permission modes: Default (prompt per risky call), Auto-Accept Edits, Plan Mode (strictly read-only, no edits/shell). Fine-grained allow/deny/ask rules in `settings.json` (project vs user vs enterprise scope). A genuine **process-level sandbox** exists independent of the LLM's own judgment: macOS Seatbelt / Linux bubblewrap isolates the Bash tool's filesystem and network access; deny rules in `permissions.deny` are compiled into Seatbelt profiles. `dangerouslyDisableSandbox` is an escape hatch for tools (Docker, Watchman) incompatible with sandboxing.

**Multi-agent**
Native `Task` tool spawns subagents with their own context window, system prompt, and restricted tool permissions; can run in parallel or background. Experimental "Agent Teams" (`CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1`) enables multi-level orchestration with teammate idle/coordination hooks.

**Config**
`CLAUDE.md` (project memory, hierarchical), `settings.json`/`settings.local.json` at user/project/enterprise scope, `.claude/agents/*.md` for subagent definitions, `.claude/rules/*.md`, custom slash commands as markdown files.

**Strengths**
Deepest decoupling of engine from UI (SDK is literally the same loop as the CLI); richest hook/lifecycle system of any tool surveyed; real OS-level sandboxing (not just LLM self-restraint); strong MCP + subagent ecosystem; genuine multi-surface story (terminal, SDK, IDE, web, mobile).

**Gaps/Pain Points**
- **Compaction is a recurring pain point.** GitHub issue #21925 ("[DESIGN FLAW] Context compaction destroys workflow"): compaction happens silently mid-task, Claude loses context of what it just built and undoes/breaks its own work; CLAUDE.md is not re-read post-compaction.
- Issue #10960: after compaction, Claude loses track of repo path changes and reverts to the original repo.
- Issue #2423 and #22729: compaction can **freeze the session** or time out entirely, losing unsaved work.
- Issue #14472: sessions that exceed context on resume fail immediately with "Prompt is too long" — **no way to compact before loading**, i.e. resumability breaks exactly when most needed.
- Issue #25620/#26317: `/compact` itself can fail with "Conversation too long," a dead end.
- Issue #36573: after compression, Claude starts making unauthorized code changes, ignoring prior context/constraints.
- Issue #27203: `canUseTool` SDK permission callback is **not invoked for background subagent tool calls** in default mode — a real gap in the permission model for programmatic use.
- Token/cost complaints are widespread: reports of 4–10x token consumption increases after specific version updates (~v2.1.88, March 2026), users exhausting a full month's Max-plan allocation in a single hour, Hacker News threads on billing with hundreds of replies, monthly bills of $200–$500/developer for heavy users.

Sources: [Claude Code Design Space (arXiv)](https://arxiv.org/html/2604.14228v1), [Hooks reference](https://code.claude.com/docs/en/hooks), [GitHub #21925](https://github.com/anthropics/claude-code/issues/21925), [#10960](https://github.com/anthropics/claude-code/issues/10960), [#2423](https://github.com/anthropics/claude-code/issues/2423), [#22729](https://github.com/anthropics/claude-code/issues/22729), [#14472](https://github.com/anthropics/claude-code/issues/14472), [#25620](https://github.com/anthropics/claude-code/issues/25620), [#26317](https://github.com/anthropics/claude-code/issues/26317), [#36573](https://github.com/anthropics/claude-code/issues/36573), [#27203](https://github.com/anthropics/claude-code/issues/27203), [#28984](https://github.com/anthropics/claude-code/issues/28984), [Sandbox guide](https://claudefa.st/blog/guide/sandboxing-guide), [Agent SDK sessions](https://platform.claude.com/docs/en/agent-sdk/sessions), [Token burn writeup](https://levelup.gitconnected.com/claude-code-token-burn-the-unplanned-100-month-reality-48587c6a92ce)

---

## 2. OpenAI Codex CLI

**Architecture**
Rust-based CLI with three deployment surfaces: local CLI/IDE extension (OS-level sandboxing), Codex Cloud (fully isolated OpenAI-managed containers), and a **headless `codex exec`** mode for CI/scripts. As of 2026, Codex CLI also runs **as an MCP server itself** — other MCP clients or the OpenAI Agents SDK can drive it programmatically, turning it into a sandboxed execution engine embeddable in larger multi-agent systems (similar in spirit to Claude's Agent SDK, but achieved via MCP-server-hood rather than a first-class SDK export of the loop).

**Session/Context**
Every session auto-saved as JSONL under `~/.codex/sessions/` (full transcript: prompts, responses, tool calls, results). Resume via `codex exec resume --last` or by session ID; supports session **forking**. Known friction: resumed sessions currently pick up model/reasoning-effort from `config.toml` rather than the values the original session was recorded with (open issue #32061 requesting persistence of model/effort per session).

**Tools/Extensibility**
First-class MCP client (2026 added an MCP submenu + `/mcp install` shortcuts). **Subagents** graduated from feature-flag to GA/stable default in March 2026 — TOML-defined, explicit spawning, path-based addressing, batch processing. `AGENTS.md` provides persistent per-project instructions loaded automatically every session (Codex's analog to CLAUDE.md). `config.toml` supports profiles and a `codex features enable/disable` flag system.

**Permissions/Safety**
Three sandbox modes: read-only, workspace-write, danger-full-access. Approval policies: Never / OnRequest / UnlessTrusted / Granular, with "Suggest" (approve everything) as the safest/default. In headless `exec` mode, default sandbox is read-only and approval requests **immediately fail** unless auto-approve policies are set (no interactive fallback) — a deliberate headless-safety design, but it means unattended runs need careful upfront policy configuration. Sandbox enforcement is OS-level for local CLI (no network by default, writes limited to workspace) vs full container isolation in Codex Cloud.

**Multi-agent**
Native TOML-defined subagents (GA March 2026) for parallelizing work across isolated threads; combined with `apply_patch` diffs for large-PR coordination. Also usable as an MCP-server "worker" spawned by external orchestrators (OpenAI Agents SDK).

**Config**
`config.toml` (profiles, sandbox/approval defaults, feature flags), `AGENTS.md` (project instructions), CLI flags override config per invocation.

**Strengths**
Clean read-only-by-default headless posture (fails safe rather than silently prompting); genuine container-level isolation option via Codex Cloud; dual role as both agent and MCP server gives strong interoperability with other agent frameworks; mature `apply_patch`/V4A diff format designed specifically for reliable LLM-authored patches.

**Gaps/Pain Points**
- **`apply_patch` reliability regression**: GitHub issue #16102 — frequent patch-application failures traced not to the diff-grammar parser but to the **execution/runtime path** (exec-server abstraction refactor around v0.117.0), worse on larger files; described as "silent failures to UI diff issues."
- Issue #34515 — serious safety/transparency gap: `apply_patch`'s Delete File operation calls filesystem-remove directly, bypassing the destructive-command "Guardian" review, and the deletion isn't even surfaced in the patch-approval UI (reported as Delete+Add showing as Add-only).
- Issue #15642 — Codex can loop claiming "no apply_patch tool available" and failing to invoke `multi_tool_use.parallel`.
- App/CLI regressions in 2026: an update reportedly broke the macOS desktop app for ~8 days (every prompt failing with invalid-schema error); VS Code extension connectivity failures, disappearing from sidebar; conversations stuck in "Thinking" state with "Error creating task."
- Session resume doesn't yet properly persist model/reasoning-effort choice (#32061).

Sources: [DeepWiki Headless Execution](https://deepwiki.com/openai/codex/4.2-headless-execution-mode-(codex-exec)), [Agent approvals & security](https://developers.openai.com/codex/agent-approvals-security), [Sandbox and Approval Policies](https://deepwiki.com/openai/codex/2.4-sandbox-and-approval-policies), [#16102](https://github.com/openai/codex/issues/16102), [#34515](https://github.com/openai/codex/issues/34515), [#15642](https://github.com/openai/codex/issues/15642), [#32061](https://github.com/openai/codex/issues/32061), [Codex CLI as MCP Server](https://codex.danielvaughan.com/2026/05/18/codex-cli-as-mcp-server-exposing-agent-capabilities-agents-sdk-multi-agent-delegation/), [Complaints roundup](https://chatgptdisaster.com/codex-complaints.html)

---

## 3. Google Gemini CLI

**Architecture**
`McpClientManager` coordinates multiple `McpClient` instances (one per connected MCP server) — clean two-tier MCP architecture. Extension system (`gemini-extension.json`) lets extensions bundle their own MCP servers, commands, and subagents; local `settings.json` can override extension-provided MCP config, with CLI merging local + extension defaults. No first-class embeddable "SDK" analog to Claude's Agent SDK was found in this research — Gemini CLI's programmatic story is thinner, oriented around CLI flags/extensions rather than an exported agent-loop library.

**Session/Context**
Sessions persist via JSONL streams, each with a unique `sessionId` recorded to disk automatically; supports retention-policy-based automatic cleanup. Marketed with a 1M-token context window, but real-world reports (see below) suggest usable context is much smaller in practice, and performance measurably degrades ("context rot") as token count grows within a session.

**Tools/Extensibility**
MCP client support is solid and extension-native. **Subagents** (added later in 2026): defined as Markdown files with YAML frontmatter, placed in `~/.gemini/agents` (global) or `.gemini/agents` (project, shareable via VCS); extensions can also bundle subagent definitions in an `agents/` directory. Custom slash commands via TOML files in an extension's `commands/` directory. `GEMINI.md` provides hierarchical project context (loaded from cwd up to filesystem root), analogous to CLAUDE.md/AGENTS.md.

**Permissions/Safety**
Three approval modes: default (prompt per call), `auto_edit` (auto-approve file replace/write, prompt for everything else), `yolo` (auto-approve everything). `--yolo` and `--approval-mode` are mutually exclusive flags. Docker-based sandbox (`gemini-cli-sandbox` image) auto-enables when YOLO mode is used — combining unattended approval with container isolation is the documented recommended pattern for automation.

**Multi-agent**
Subagents are a relatively recent (2026) addition, markdown+YAML-frontmatter defined, project- or user-scoped, extension-bundleable — conceptually similar to Claude Code's subagent files but announced later ("Subagents have arrived in Gemini CLI," Google Developers Blog).

**Config**
`settings.json` (`.gemini/settings.json`, user/project scope, editable via `/settings` command), `GEMINI.md`, `gemini-extension.json` for extensions.

**Strengths**
Free tier and large nominal context window are attractive for cost-sensitive users; extension model cleanly separates MCP servers + commands + subagents into shareable bundles; Docker-sandboxed YOLO mode gives a reasonably safe unattended-automation story out of the box.

**Gaps/Pain Points**
- **Context window reality gap**: widely reported that the marketed 1M-token window is not practically usable — many users find effective performance falls off around ~200k tokens, on par with competitors despite the larger nominal figure (GitHub Discussion #7432, community reports).
- Issue #10975 — "Context Rot": performance measurably deteriorates over the course of a long session as input token count grows.
- Issue #13198 / Discussion #4841 — **excessive/uncontrolled token consumption**: starting v0.14.0, the model re-reads the same content repeatedly, causing extreme unexpected token usage; one reported case attempted a 41M-token payload against a ~900k limit — flagged as a "critical financial risk" for paid API-key users.
- Issue #17448 — agent repeatedly runs overly broad searches, causing context to balloon uncontrollably; described as tool-use loops without user awareness/consent.
- Issue #11947 — bad token-count estimation triggers false "might exceed context window" warnings.
- Issue #13651 — users report hitting the token limit almost immediately in some configurations.
- Issue #14817 — no built-in way to see context-window usage (`/stats`/status line don't surface it), making the above problems harder to diagnose proactively.
- Issue #13121 — assorted UX bugs on Windows (paste failures, missing scrollbar) bundled with token-limit complaints.

Sources: [DeepWiki Session Management](https://deepwiki.com/google-gemini/gemini-cli/3.9-session-management), [DeepWiki MCP Integration](https://deepwiki.com/google-gemini/gemini-cli/3.7-mcp-server-integration), [Subagents docs](https://geminicli.com/docs/core/subagents/), [Subagents announcement](https://developers.googleblog.com/subagents-have-arrived-in-gemini-cli/), [#10975](https://github.com/google-gemini/gemini-cli/issues/10975), [#13198](https://github.com/google-gemini/gemini-cli/issues/13198), [Discussion #4841](https://github.com/google-gemini/gemini-cli/discussions/4841), [#17448](https://github.com/google-gemini/gemini-cli/issues/17448), [#11947](https://github.com/google-gemini/gemini-cli/issues/11947), [#13651](https://github.com/google-gemini/gemini-cli/issues/13651), [#14817](https://github.com/google-gemini/gemini-cli/issues/14817), [#13121](https://github.com/google-gemini/gemini-cli/issues/13121), [YOLO mode](https://deepwiki.com/addyosmani/gemini-cli-tips/9.2-yolo-mode-and-auto-approval)

---

## 4. Aider (aider.chat)

**Architecture**
Centers on a **Coder** class hierarchy: a base class handles context assembly, LLM communication, and git integration; subclasses implement different **edit formats** (whole-file, diff/search-replace, unified diff, etc.), each with its own prompt template and parser. Genuinely engine/UI-decoupled at a basic level: exposes both a CLI and a **Python API** (`aider.main.main`, importable package) for embedding in scripts/CI/other agents; headless `--message` mode processes one instruction and exits (no interactive PTY required — notably one of the few tools usable without a pseudo-terminal at all). No web UI or hosted service — it is CLI/library only, the most minimal "harness" of the group.

**Session/Context**
No formal session-persistence/resume system comparable to Claude Code or Codex — Aider's state model is built around the **git repository itself** as the source of truth: every AI-driven change is auto-committed with a descriptive message, giving a fully revertable history, but conversation-level session resumption (reloading a prior chat transcript across process restarts) is not a first-class feature. Context management leans on the **RepoMap**: a tree-sitter-powered, PageRank-ranked graph of code symbols across the repo, dynamically summarizing which definitions matter most for the current conversation rather than doing traditional summarization/compaction of chat history.

**Tools/Extensibility**
40+ in-chat slash commands; per-language lint/test command configuration (`--lint-cmd "python: flake8 ..."`) with **auto-lint**: every AI edit triggers the configured linter automatically, and Aider feeds lint/test errors back to the model to self-correct. No MCP support noted as a first-class built-in in the research surfaced (Aider predates MCP's widespread adoption and remains comparatively lightweight/non-extensible via plugins compared to the other tools). Model-agnostic — works with essentially any LLM API.

**Permissions/Safety**
Minimal formal permission model — no sandboxing, no allow/deny rule engine, no distinct approval modes comparable to the others. Safety relies mostly on git auto-commit (everything is revertable) plus prompting behavior; this is a notably thinner safety story than Claude Code/Codex/Gemini CLI.

**Multi-agent**
None — explicitly cited as lacking multi-agent orchestration/subagents (see Weaknesses).

**Config**
`.aider.conf.yml` (YAML), env vars (`AIDER_*`), `.env` file, or CLI flags — configurable options for editing format, lint/test commands, auto-commit behavior, model aliases.

**Strengths**
Best-in-class git integration (auto-commit every AI edit → clean revertable history, saving significant manual git overhead); RepoMap gives strong signal on large codebases without requiring explicit file mentions ("on codebases over 500 files, Aider often pulls in more relevant context than competing tools"); strong benchmark performance (reported 93% on refactoring, 88% on debugging tasks in one 50-task eval; 81–88% on polyglot benchmarks); can run fully headless without a PTY, making it easy to embed as a library/subprocess in other tooling; model-agnostic by design.

**Gaps/Pain Points**
- **Behavioral drift complaint** (GitHub issue #1058, paul-gauthier/aider): long-time users report Aider became "too aggressive," always wanting to edit files rather than just answering questions as it previously did — users have to explicitly and repeatedly instruct it not to edit, described as "painful."
- No IDE integration, minimal UI, steep CLI learning curve relative to Cursor/VS-Code-extension competitors.
- No multi-agent orchestration or subagents at all.
- Token/cost spikes on complex refactors of large codebases; API costs described as able to spike sharply.
- No MCP support noted, meaning it lags the others in third-party tool/data-source extensibility.
- No genuine session-resume/transcript-reload story — durability lives at the git-commit layer, not the conversation layer.

Sources: [Edit formats](https://aider.chat/docs/more/edit-formats.html), [Chat modes](https://aider.chat/docs/usage/modes.html), [YAML config](https://aider.chat/docs/config/aider_conf.html), [Options reference](https://aider.chat/docs/config/options.html), [GitHub #1058](https://github.com/paul-gauthier/aider/issues/1058), [Aider Architecture Analysis](https://emsenn.net/library/domains/engineering/domains/tech/domains/computing/texts/aider-architecture-analysis/)

---

## 5. Amazon Q Developer CLI

**Architecture**
Terminal-based gateway to Amazon Q's model backend; AWS-hosted (not locally-run inference). MCP client via a user-configured `mcp.json`. Supports both local (process-based) and **remote MCP servers** (HTTP) — remote MCP is emphasized for scalability/security, offloading compute to a centralized server rather than the local machine. Also integrates as "Amazon Q Developer for GitHub" (separate product surface for PR-triggered automation) alongside the CLI — a second, distinct interface, though not deeply architecturally unified with the CLI (more like two separate products sharing a brand than one decoupled engine with multiple frontends).

**Session/Context**
Least documented of the group in this research; context length constraints are noted indirectly (e.g., GitHub-integration issue descriptions >1000 words get truncated), suggesting a comparatively conservative/opaque context-management approach versus the others' explicit compaction/RepoMap systems.

**Tools/Extensibility**
**Custom agents** defined via JSON config files specifying tools, permissions, and context/resources per agent. Config fields include `name`, `description`, `mcpServers`, `tools`, `toolAliases`, `allowedTools`, `toolsSettings`, `resources`. `allowedTools` supports exact matches, native-tool wildcards (`fs_*`, `execute_*`), MCP tool wildcards (`@server/api_*`), and server-level trust (`@fetch`) — notably it does **not** support a blanket `*` wildcard, forcing explicit enumeration (arguably safer-by-default but more config overhead). Built-in tools documented separately (`fs_*`, `execute_*`, etc.).

**Permissions/Safety**
Tool trust is per-agent and per-tool-pattern via `allowedTools`; `/tools` command manages permissions interactively. GitHub issue #2510 reports `allowedTools` configuration in a custom agent JSON **not being honored/trusted** despite being properly defined — i.e., the permission-config layer itself has had reliability bugs.

**Multi-agent**
Custom-agent JSON files effectively allow defining multiple differently-scoped agents, but no evidence surfaced of native parallel subagent orchestration comparable to Claude Code's Task tool, Codex's TOML subagents, or Goose's subagent/subrecipe system.

**Config**
JSON agent-definition files (the "Agent Format," documented at aws.github.io/amazon-q-developer-cli/agent-format.html); `mcp.json` for MCP servers.

**Strengths**
Deep AWS ecosystem integration (AWS documentation MCP, diagramming MCP, AIOps workflows tied to CloudWatch/other AWS services); explicit-wildcard permission model is arguably a safer default posture than tools that allow blanket auto-approval; remote/hosted MCP servers reduce local compute/security footprint for enterprise use; GitHub integration adds a lightweight PR-triggered surface beyond pure CLI.

**Gaps/Pain Points**
- Issue #2510 — `allowedTools` trust configuration silently not respected, undermining the permission model's reliability.
- **Monthly/rate limits**: multiple GitHub issues (#2309 on amazon-q-developer-cli, #8508 on aws-toolkit-vscode) report hitting monthly request caps, and caps **not resetting on schedule** (one report of 7+ days overdue), which is a significant pain point for an agent meant to support continuous dev workflows.
- `ModelOverloadedError` ("Amazon Q is having trouble responding right now") reported during high AWS traffic — availability tied to shared backend load rather than dedicated capacity.
- GitHub integration specifically: doesn't follow URLs/PR links mentioned in prompts; can't create PRs in branch-protected repos (branch-protection rules block Q's branch creation); doesn't auto-trigger review on subsequent commits to an existing PR; 1000-word truncation on issue descriptions.
- Weakest publicly-documented multi-agent/subagent story of the CLI-first tools surveyed.
- Sparse community discussion (Reddit/GitHub) relative to Claude Code/Codex/Gemini — harder to gauge real-world pain at scale; official AWS docs dominate the visible discourse over organic user complaints, which itself may indicate lower adoption/mindshare among the CLI-first agent crowd.

Sources: [Agent format docs](https://aws.github.io/amazon-q-developer-cli/agent-format.html), [GitHub #2510](https://github.com/aws/amazon-q-developer-cli/issues/2510), [Managing tool permissions](https://docs.aws.amazon.com/amazonq/latest/qdeveloper-ug/command-line-chat-tools.html), [MCP extension blog](https://aws.amazon.com/blogs/devops/extend-the-amazon-q-developer-cli-with-mcp/), [GitHub #2309](https://github.com/aws/amazon-q-developer-cli/issues/2309), [aws-toolkit-vscode #8508](https://github.com/aws/aws-toolkit-vscode/issues/8508), [GitHub troubleshooting](https://docs.aws.amazon.com/amazonq/latest/qdeveloper-ug/github-troubleshooting.html)

---

## 6. Goose (Block / Linux Foundation Agentic AI Foundation)

**Architecture**
Rust core with a genuinely modular design: core agent loop + provider abstraction layer (30+ LLM providers, model-agnostic by design) + an extension system built entirely on **MCP**. "Extension" is Goose's term for an MCP server integration; 70+ documented extensions, and since any MCP-compliant service (3,000+ exist) can plug in without bespoke integration work, this is one of the more extensibility-forward architectures reviewed. Governance moved from Block-internal to the **Linux Foundation's Agentic AI Foundation (AAIF)** in 2026 — notable as the only tool in this set with a neutral foundation governance model rather than single-vendor control, which may matter for long-term extensibility/trust. Primarily a local CLI/desktop tool (29,400+ GitHub stars, 368 contributors, 2,600+ forks since Jan 2025 launch); no distinct hosted web UI identified comparable to Claude Code's.

**Session/Context**
Extensions can be added per-session or globally (`goose extension add`), allowing ad hoc composition of tool sets per task. `.goosehints` file provides project-level context/instructions (analogous to CLAUDE.md/AGENTS.md/GEMINI.md). Config centralized in `~/.config/goose/config.yaml`.

**Tools/Extensibility**
**Recipes**: YAML files capturing a full reusable, parameterized agent workflow (instructions + extensions to enable + user-supplied parameters) — Goose's answer to reusable "playbooks," conceptually similar to Claude Code Skills or custom slash commands but more workflow/pipeline-oriented. Distinction between **subagents** (independent agent processes spun up from a session, e.g., one reviews code while another runs tests, non-blocking) and **subrecipes** (the parent recipe's agent decides when to invoke a subrecipe, sequentially or in parallel, chaining outputs through conversation context) — a more explicit two-tier delegation model than most competitors offer.

**Permissions/Safety**
Not deeply detailed in sources surfaced beyond extension-level enable/disable; no distinct sandboxing layer comparable to Claude Code's Seatbelt/bubblewrap or Codex's sandbox modes was found in this research pass — worth flagging as a probable gap versus the others.

**Multi-agent**
Native subagents + subrecipes as above; explicit **concurrency cap of 10 parallel workers total** across both subagents and subrecipes combined — hardcoded, not user-configurable. This is more generous/explicit than most competitors but also a hard architectural ceiling to be aware of when designing for scale.

**Config**
`~/.config/goose/config.yaml` for global config/extensions; `.goosehints` for project context; recipe YAML files for reusable workflows.

**Strengths**
Cleanest "any MCP server just works" extensibility story of the group given its 70+/3,000+ potential extension count; foundation governance (AAIF) is a distinctive trust/longevity signal; explicit two-tier subagent/subrecipe delegation model with parallel execution is architecturally more mature than most competitors' bolt-on subagent features; genuinely local-first/self-hosted (no forced cloud dependency).

**Gaps/Pain Points**
- **Tool-calling reliability is a recurring theme across GitHub issues**: #1224 — Goose doesn't handle tool error responses gracefully (a timeout error can leave the entire session stuck displaying the error with no recovery path). #3739 — Goose can stop performing tool calls mid-loop even while claiming it will continue calling further tools. #3960 — tool-calling loop interrupted mid-task, likely an internal orchestration/timeout issue rather than a model failure (multi-step tool calls cancelled even after the local LLM backend successfully completed its part).
- **Model-dependent quality**: output quality is described as "a dice roll based on your model" — connecting a weaker/local model doesn't get compensated for by the harness, so quality noticeably drops versus paid-frontier-model competitors. This is an inherent tradeoff of Goose's radical model-agnosticism.
- Ollama/local-model tool calling specifically flagged as fragile: #1817 — unhelpful responses for tool calling via Ollama; #6883 — Qwen3-coder via Ollama fails tool execution, emitting unparseable XML-style tool invocations instead of proper calls.
- DeepSeek models reportedly don't support tool calling at all in Goose, requiring all extensions to be disabled — severely limiting capability when using that provider.
- No clear OS-level sandboxing story found (unlike Claude Code/Codex/Gemini CLI's explicit sandbox modes) — a probable safety gap worth validating further if pursuing Goose-like design.

Sources: [Goose docs](https://goose-docs.ai/), [Subagents vs Subrecipes blog](https://block.github.io/goose/blog/2025/09/26/subagents-vs-subrecipes/), [Subagents guide](https://goose-docs.ai/docs/guides/context-engineering/subagents/), [GitHub #1224](https://github.com/aaif-goose/goose/issues/1224), [#1817](https://github.com/aaif-goose/goose/issues/1817), [#6883](https://github.com/block/goose/issues/6883), [#3739](https://github.com/aaif-goose/goose/issues/3739), [#3960](https://github.com/aaif-goose/goose/issues/3960), [Discussion #4389](https://github.com/block/goose/discussions/4389), [AllThingsOpen overview](https://allthingsopen.org/articles/meet-goose-open-source-ai-agent)

---

## 7. Cline / Roo Code (brief coverage)

**Major development**: **Roo Code shut down its VS Code extension, Cloud, and Router on May 15, 2026**, merging back into Cline. The Roo Code team's stated rationale was a bet that IDEs are not the future of coding — they pivoted to **Roomote**, a Slack-first cloud coding agent (~$899/month per parallel instance) that runs tasks end-to-end across Slack/GitHub/Linear and produces ready-to-review outputs, i.e., abandoning the local-IDE-agent model entirely in favor of a fully hosted async agent. Unused paid balances were refunded; Roo Code's own guidance directs former users to Cline.

**Cline** (the surviving/consolidating project) now offers three product surfaces: VS Code extension (interactive), a **CLI supporting headless mode** for CI/CD and automation (triggered via `--json`/`--yolo`/`--no-interactive` flags, or automatically when stdin is piped/output redirected — messaging integrations for Slack/Telegram/Discord/WhatsApp/Linear are notable extras), and an **SDK** for embedding agents in custom products — structurally similar in ambition to Claude Code's CLI+SDK+(now-absorbed competitor) approach, though maturity/adoption is unclear post-consolidation.

**Roo Code's CLI** (`@roo-code/cli`, prior to shutdown) took a distinct technical approach worth noting for design purposes: it shared core logic with the VS Code extension via workspace packages plus a **shim layer providing VS-Code-API compatibility in headless environments** — i.e., rather than building a UI-agnostic core from scratch, it retrofitted headless support onto an IDE-extension-shaped core via compatibility shimming. This is architecturally instructive as a cautionary pattern: bolting headless/CLI support onto an IDE-first core via a compat shim is more fragile than designing the engine UI-agnostic from day one (per Claude Code and Aider's model-first approach).

**Known pain points**: Cline draws notable cost criticism — "deliberate don't-limit-context philosophy" leads to reports of ~300k tokens consumed within five iterations, complex tasks costing $0.50–$1.00 in API credits, and quality degrading once context fills to 70–80%. Head-to-head user reports were mixed: one comparison found Cline used substantially fewer tokens than Roo Code and succeeded where Roo Code struggled for days without crashing; another Roo Code user reported cutting token usage 75% (150k→15-20k tokens/activity) after learning to use Roo's planning modes — suggesting high variance/UX-dependence rather than a clean winner.

Sources: [DeepWiki CLI Headless Mode](https://deepwiki.com/cline/cline/12.4-cli-headless-mode-and-cicd), [DeepWiki Roo CLI Application](https://deepwiki.com/RooCodeInc/Roo-Code/15-cli-application), [Cline CLI docs](https://docs.cline.bot/usage/cli-overview), [Roo Code shutdown coverage](https://thenewstack.io/roo-code-cloud-ides-ai-coding/), [Bodega One shutdown alternatives](https://www.bodegaone.ai/blog/roo-code-shutdown-alternatives), [GitHub Roo-Code #2700](https://github.com/RooCodeInc/Roo-Code/issues/2700)

---

## Cross-Cutting Takeaways for Harness Design

1. **Engine/UI decoupling maturity ranking** (best → weakest, per this research): Claude Code (SDK exports the literal agent loop; CLI, SDK, IDE, web, mobile all share it) > Codex CLI (MCP-server-hood + Agents SDK interop, though not a first-class exported loop library) ≈ Aider (Python-importable core, headless-by-design, but no multi-surface UI story) > Goose (modular Rust core, but no distinct hosted UI) > Gemini CLI (extension-centric, thinner programmatic/SDK story) > Amazon Q (two loosely-related product surfaces rather than one decoupled engine) > Roo Code's retrofit-via-shim approach, which is the clearest anti-pattern to avoid.
2. **Context compaction is the single most common failure class** across tools with long-running sessions (Claude Code, Gemini CLI both have extensive GitHub issue histories on this) — silent context loss, self-contradiction after compaction, and resume-time failures ("prompt too long," compaction itself failing) recur across vendors. A new harness should treat compaction as a first-class, testable subsystem with explicit pre/post hooks (Claude Code's PreCompact/PostCompact events are a good pattern to emulate) and should re-inject durable project memory (CLAUDE.md-equivalent) after every compaction, not just at session start.
3. **Sandboxing is bifurcated**: Claude Code, Codex CLI, and Gemini CLI all provide genuine OS/container-level sandboxes independent of LLM self-restraint; Aider, Amazon Q, and (per this research) Goose rely more on allow-list/config-level trust without process isolation. Real sandboxing is a differentiator worth prioritizing.
4. **Tool-calling reliability under load/looping** is a recurring complaint specifically for Goose (with local/open models) and Codex (`apply_patch` execution-path regressions) — suggests the tool-execution layer needs robust error/timeout recovery and shouldn't silently drop or mis-report destructive operations (Codex's Guardian-bypass-on-delete bug, #34515, is a specific safety-review lesson).
5. **Subagent/multi-agent delegation models differ in maturity**: Goose's explicit subagent-vs-subrecipe distinction with a hard concurrency cap (10) is the most architecturally deliberate; Claude Code's Task tool + experimental Agent Teams is powerful but has a documented permission-callback gap for background subagents (#27203); Amazon Q has the least developed story.
6. **Cost/token transparency is a near-universal complaint** (Claude Code, Gemini CLI, Cline all have significant documented cases of runaway or opaque token consumption) — a new harness should treat live token/cost accounting and windows into "why did this session use N tokens" as a core UX requirement, not an afterthought (Gemini CLI issue #14817 explicitly calls out the lack of this).
7. **Vendor-consolidation risk is real**: Roo Code's abrupt May 2026 shutdown shows CLI/IDE-agent products can disappear outright when a vendor bets on a different distribution model (cloud/Slack-first). Foundation-governed projects (Goose under AAIF) or genuinely open-core/self-hostable designs are more resilient bets for a harness meant to have staying power.
