# lite-harness

A Rust AI coding-agent harness: a per-workspace daemon (`lite-harnessd`)
that runs a native LLM agent loop, gates every tool call through a
permission engine, executes tools in a sandboxed execution plane, and
tracks cost -- with two thin clients (a terminal CLI and a browser UI)
that speak the same JSON-RPC "Harness Protocol" over a Unix socket. It can
also delegate a task to an external ACP-speaking agent (e.g. Claude Code
via `claude-agent-acp`) through the identical permission/ledger machinery,
or let that external agent drive the whole session.

For the full design (event log, permission model, execution plane,
ACP integration, orchestration), see
[`docs/architecture/architecture.md`](docs/architecture/architecture.md).

## Build

```sh
cargo build --workspace --release
```

This produces three binaries under `target/release/`:

| Binary            | Crate           | What it is                                   |
|--------------------|-----------------|-----------------------------------------------|
| `lite-harnessd`    | `lh-daemon`     | The per-workspace daemon (rarely run by hand) |
| `lite-harness`     | `lh-cli`        | Terminal client                               |
| `lite-harness-web` | `lh-web-backend`| WebSocket<->daemon bridge for the browser UI  |

You never need to start `lite-harnessd` yourself: both clients connect to
its Unix socket and spawn it automatically on first use, looking for the
`lite-harnessd` binary next to their own (override with
`LITE_HARNESS_DAEMON_BIN=/path/to/lite-harnessd` if it's elsewhere).

## Configure a model provider

`session/prompt` needs a model to call. Create
`~/.config/lite-harness/providers.toml`:

```toml
default = "anthropic"

[[provider]]
name = "anthropic"
protocol = "anthropic"                       # or "open-ai-compatible"
base_url = "https://api.anthropic.com"
api_key_env = "ANTHROPIC_API_KEY"             # read from the daemon's own env, never stored here
default_model = "claude-sonnet-5"
```

`protocol = "open-ai-compatible"` covers the Chat Completions shape spoken
by Ollama, vLLM, LM Studio, OpenRouter, Azure OpenAI, LiteLLM, and most
"custom base URL" targets:

```toml
[[provider]]
name = "local-llama"
protocol = "open-ai-compatible"
base_url = "http://localhost:11434/v1"
api_key_env = "OLLAMA_API_KEY"
default_model = "llama3"
```

You can define multiple providers and switch without editing the file via
`LITE_HARNESS_PROVIDER=local-llama`. Override the file location itself
with `LITE_HARNESS_PROVIDERS_FILE=/path/to/providers.toml` (handy for
tests/CI). If no provider config is found, the daemon still starts, but
`session/prompt` fails until one is set.

## Run the CLI

```sh
cd your-project
lite-harness "list the files in this directory"
```

First run in a workspace spawns the daemon for you; subsequent runs from
the same directory reuse it. Streamed events print as they arrive; if a
tool call needs a live permission decision (no policy rule matched it),
you'll be prompted:

```
  [permission] Execute / Native { tool_id: "bash" }: execute: rm
  allow? [y/N/a=always-allow/d=always-deny]:
```

`y`/`N` decide this one call; `a`/`d` also persist a project-scoped rule
(see [Permission policy](#permission-policy) below) so the same action
never asks again.

Other CLI modes:

```sh
lite-harness --list-agents                          # which delegated ACP agents are configured
lite-harness --agent claude-code "fix the failing test"    # native root, hand this one task to Claude Code
lite-harness --primary claude-code "refactor this module"  # the whole session is driven by Claude Code
```

## Run the web UI

```sh
lite-harness-web
```

Then open `http://127.0.0.1:8787` (override the port with
`LITE_HARNESS_WEB_PORT`). Run it from the workspace directory you want to
work in -- same one-daemon-per-workspace rule as the CLI. It serves
`crates/lh-web-ui` (a plain HTML/CSS/JS client, no build step) and proxies
everything else byte-for-byte to the daemon over the same Harness
Protocol the CLI speaks; pick a driver (native / an external agent as
primary / delegate-one-task), create a session, and prompt from the page.

## Delegating to an external ACP agent

`--agent`/`--primary` (CLI) and the "primary"/"delegate" driver options
(web UI) need at least one adapter registered in
`~/.config/lite-harness/agents.toml`:

```toml
[[agent]]
kind = { type = "ClaudeCode" }
can_be_primary = true

[agent.spawn]
command = "npx"
args = ["-y", "@agentclientprotocol/claude-agent-acp"]
api_key_env = "ANTHROPIC_API_KEY"
```

`api_key_env` is read from the daemon's own environment at spawn time and
forwarded to the subprocess -- never written to the file. Override the
registry file location with `LITE_HARNESS_AGENTS_FILE`. An empty/missing
file just means no delegated agents are available (not a startup error);
`--list-agents` / `agents/list` reflect whatever's actually configured.

## Permission policy

Every tool call is checked against, in order: a session-scoped rule (from
an earlier "always" answer this run), a project rule
(`.lite-harness/policy.toml` in the workspace), then a global rule
(`~/.config/lite-harness/policy.toml`). If none match, the client is asked
live. `Destructive`-tier actions always ask, even if a matching "always
allow" rule exists -- policy can't silently green-light destructive calls.
Policy files are written for you when you answer "always"; you generally
don't need to hand-edit them, but the format is:

```toml
[[rule]]
key = "native:bash#exec"
decision = "allow"   # or "deny"
```

## Other config files (`~/.config/lite-harness/`)

| File             | Purpose                                              |
|------------------|-------------------------------------------------------|
| `providers.toml` | Model provider(s) -- see above                        |
| `agents.toml`    | Delegated ACP agent adapters -- see above              |
| `policy.toml`    | Global-scope permission rules                          |
| `pricing.toml`   | Overrides/extends the built-in per-model cost table    |

All of these are optional and load-and-log-don't-crash: a missing or
unparseable file degrades the relevant feature rather than failing daemon
startup.

## Development

```sh
cargo build --workspace       # build everything
cargo test --workspace        # unit + integration tests
cargo clippy --workspace --all-targets
```

Live end-to-end verification (used throughout this project's history) runs
the real daemon against a small local mock server that speaks the
OpenAI-compatible `/chat/completions` shape (point `providers.toml` at it
with `protocol = "open-ai-compatible"`) rather than a live API key, and for
the web UI, drives the real page with Playwright against a headless
Chromium.
