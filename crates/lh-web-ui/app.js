// lite-harness web UI -- the actual Harness Protocol client (architecture
// §11 phase 7). `lh-web-backend` is a byte-transparent WS<->UDS bridge; this
// file is where 100% of the protocol logic lives, mirroring `lh-cli`'s
// Rust main.rs almost line-for-line, in vanilla JS, no build step.

const methods = {
  INITIALIZE: "initialize",
  SESSION_CREATE: "session/create",
  SESSION_PROMPT: "session/prompt",
  SESSION_TREE: "session/tree",
  PERMISSION_ASK: "permission/ask",
  LEDGER_QUERY: "ledger/query",
  SESSION_DELEGATE: "session/delegate",
  AGENTS_LIST: "agents/list",
  EVENT: "event",
};

const PROTOCOL_VERSION = 1;

const statusEl = document.getElementById("status");
const driverSelect = document.getElementById("driver-select");
const agentField = document.getElementById("agent-field");
const agentSelect = document.getElementById("agent-select");
const agentsHint = document.getElementById("agents-hint");
const createSessionBtn = document.getElementById("create-session-btn");
const setupPanel = document.getElementById("setup-panel");
const sessionPanel = document.getElementById("session-panel");
const sessionIdLabel = document.getElementById("session-id-label");
const driverLabel = document.getElementById("driver-label");
const ledgerBtn = document.getElementById("ledger-btn");
const eventLog = document.getElementById("event-log");
const composerForm = document.getElementById("composer-form");
const promptInput = document.getElementById("prompt-input");
const ledgerPanel = document.getElementById("ledger-panel");
const permissionModal = document.getElementById("permission-modal");
const permissionSummary = document.getElementById("permission-summary");
const permissionDetail = document.getElementById("permission-detail");

let ws = null;
let nextId = 1;
const pending = new Map(); // id -> {resolve, reject}
let sessionId = null;
let workspaceCwd = "."; // filled in from /api/cwd before session/create
let mode = "native"; // "native" | "primary" | "delegate"
let agentKind = null; // the AgentKind object for "primary"/"delegate" modes
let promptRequestId = null;
let isDelegateChild = false;

function setStatus(state, label) {
  statusEl.className = `status status-${state}`;
  statusEl.textContent = label;
}

function connect() {
  setStatus("connecting", "connecting...");
  const protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
  ws = new WebSocket(`${protocol}//${window.location.host}/ws`);

  ws.addEventListener("open", async () => {
    setStatus("connected", "connected");
    try {
      const init = await request(methods.INITIALIZE, { protocol_version: PROTOCOL_VERSION });
      appendSystemLine(`connected to daemon (protocol v${init.protocol_version})`);
      try {
        const resp = await fetch("/api/cwd");
        workspaceCwd = await resp.text();
      } catch (e) {
        appendErrorLine(`/api/cwd failed, defaulting to "." : ${e.message}`);
      }
      await loadAgents();
    } catch (e) {
      appendErrorLine(`initialize failed: ${e.message}`);
    }
  });

  ws.addEventListener("close", () => {
    setStatus("disconnected", "disconnected");
    for (const { reject } of pending.values()) {
      reject(new Error("connection closed"));
    }
    pending.clear();
  });

  ws.addEventListener("error", () => {
    setStatus("disconnected", "error");
  });

  ws.addEventListener("message", (ev) => {
    let msg;
    try {
      msg = JSON.parse(ev.data);
    } catch (e) {
      appendErrorLine(`unparseable message from daemon: ${ev.data}`);
      return;
    }
    handleMessage(msg);
  });
}

function handleMessage(msg) {
  // `Message` is untagged on the Rust side and distinguished purely by
  // shape: a Notification has `method` + no `id`; a Request has `method` +
  // `id`; a Response has `id` + (`result` or `error`), no `method`.
  if (typeof msg.method === "string" && msg.id === undefined) {
    handleNotification(msg);
  } else if (typeof msg.method === "string" && msg.id !== undefined) {
    handleIncomingRequest(msg);
  } else if (msg.id !== undefined) {
    handleResponse(msg);
  } else {
    appendErrorLine(`unrecognized message shape: ${JSON.stringify(msg)}`);
  }
}

function handleNotification(msg) {
  if (msg.method === methods.EVENT) {
    renderEvent(msg.params);
  }
}

function handleResponse(msg) {
  const waiter = pending.get(msg.id);
  if (waiter) {
    pending.delete(msg.id);
    if (msg.error) {
      waiter.reject(new Error(`${msg.error.message} (${msg.error.code})`));
    } else {
      waiter.resolve(msg.result);
    }
    return;
  }
  if (msg.id === promptRequestId) {
    handlePromptResponse(msg);
  }
}

async function handleIncomingRequest(msg) {
  if (msg.method === methods.PERMISSION_ASK) {
    const decision = await askForDecision(msg.params.request);
    sendMessage({
      jsonrpc: "2.0",
      id: msg.id,
      result: { decision },
    });
  } else {
    sendMessage({
      jsonrpc: "2.0",
      id: msg.id,
      error: { code: -32601, message: `unhandled method: ${msg.method}` },
    });
  }
}

function sendMessage(obj) {
  ws.send(JSON.stringify(obj));
}

function request(method, params) {
  const id = nextId++;
  return new Promise((resolve, reject) => {
    pending.set(id, { resolve, reject });
    sendMessage({ jsonrpc: "2.0", id, method, params });
  });
}

// --- Setup: agents/list, driver selection ---

async function loadAgents() {
  try {
    const result = await request(methods.AGENTS_LIST, {});
    agentSelect.innerHTML = "";
    for (const info of result.agents) {
      const opt = document.createElement("option");
      opt.value = JSON.stringify(info.kind);
      opt.textContent = agentLabel(info.kind) + (info.can_be_primary ? " (can be primary)" : "");
      opt.dataset.canBePrimary = info.can_be_primary ? "1" : "";
      agentSelect.appendChild(opt);
    }
    if (result.agents.length === 0) {
      agentsHint.textContent = "no delegated agents configured (see LITE_HARNESS_AGENTS_FILE)";
    } else {
      agentsHint.textContent = "";
    }
  } catch (e) {
    agentsHint.textContent = `agents/list failed: ${e.message}`;
  }
  updateAgentFieldVisibility();
}

function agentLabel(kind) {
  if (kind.type === "Custom") return `Custom(${kind.name})`;
  return kind.type;
}

function updateAgentFieldVisibility() {
  const needsAgent = driverSelect.value === "primary" || driverSelect.value === "delegate";
  agentField.hidden = !needsAgent;
}

driverSelect.addEventListener("change", updateAgentFieldVisibility);

// --- session/create ---

createSessionBtn.addEventListener("click", async () => {
  mode = driverSelect.value;
  agentKind = null;
  if (mode === "primary" || mode === "delegate") {
    const selected = agentSelect.selectedOptions[0];
    if (!selected) {
      appendErrorLine("no agent selected");
      return;
    }
    agentKind = JSON.parse(selected.value);
  }

  const primary = mode === "primary" ? { type: "Delegated", agent: agentKind } : { type: "Native" };

  try {
    const result = await request(methods.SESSION_CREATE, {
      cwd: workspaceCwd,
      primary,
    });
    sessionId = result.session_id;
    sessionIdLabel.textContent = `session ${sessionId}`;
    driverLabel.textContent = mode === "primary" ? `driver: ${agentLabel(agentKind)}` : "driver: native";
    setupPanel.hidden = true;
    sessionPanel.hidden = false;
  } catch (e) {
    appendErrorLine(`session/create failed: ${e.message}`);
  }
});

// --- composer: session/prompt or session/delegate ---

composerForm.addEventListener("submit", async (ev) => {
  ev.preventDefault();
  const text = promptInput.value.trim();
  if (!text || promptRequestId !== null) return;

  isDelegateChild = mode === "delegate";
  promptInput.value = "";
  appendLine("user", `you: ${text}`);

  const id = nextId++;
  promptRequestId = id;
  if (isDelegateChild) {
    appendSystemLine(`delegating to ${agentLabel(agentKind)}`);
    sendMessage({
      jsonrpc: "2.0",
      id,
      method: methods.SESSION_DELEGATE,
      params: { agent: agentKind, task_summary: text },
    });
  } else {
    sendMessage({
      jsonrpc: "2.0",
      id,
      method: methods.SESSION_PROMPT,
      params: { text },
    });
  }
});

async function handlePromptResponse(msg) {
  promptRequestId = null;
  if (msg.error) {
    appendErrorLine(`turn failed: ${msg.error.message}`);
  } else if (isDelegateChild) {
    const result = msg.result;
    appendSystemLine(`delegation complete: child session ${result.child_session_id}, outcome: ${describeOutcome(result.outcome)}`);
  } else {
    const result = msg.result;
    appendSystemLine(`turn complete: ${result.stop_reason}`);
  }
  await refreshLedger();
}

function describeOutcome(outcome) {
  if (outcome.type === "Success") return `Success(${outcome.summary})`;
  if (outcome.type === "Failed") return `Failed(${outcome.message})`;
  return "Cancelled";
}

// --- permission/ask ---

function askForDecision(req) {
  permissionSummary.textContent = `${req.risk_tier} / ${toolSourceLabel(req.tool_source)}: ${describeAction(req.action)}`;
  permissionDetail.textContent = JSON.stringify(req, null, 2);
  permissionModal.hidden = false;

  return new Promise((resolve) => {
    const buttons = permissionModal.querySelectorAll("[data-decision]");
    const onClick = (ev) => {
      const kind = ev.currentTarget.dataset.decision;
      let decision;
      if (kind === "allow") decision = { type: "Allow" };
      else if (kind === "allow_always") decision = { type: "AllowAlways", scope: "Project" };
      else if (kind === "deny_always") decision = { type: "DenyAlways", scope: "Project" };
      else decision = { type: "Deny" };

      for (const b of buttons) b.removeEventListener("click", onClick);
      permissionModal.hidden = true;
      resolve(decision);
    };
    for (const b of buttons) b.addEventListener("click", onClick);
  });
}

function toolSourceLabel(source) {
  if (source.type === "Native") return `native:${source.tool_id}`;
  if (source.type === "Mcp") return `mcp:${source.server}/${source.tool}`;
  if (source.type === "Acp") return `acp:${agentLabel(source.agent)}`;
  return source.type;
}

function describeAction(action) {
  switch (action.type) {
    case "FileRead":
      return `read ${action.path}`;
    case "FileWrite":
      return `write ${action.path}`;
    case "Exec":
      return `execute: ${action.command}`;
    case "NetworkFetch":
      return `fetch ${action.url}`;
    case "McpToolCall":
      return `mcp ${action.server}/${action.tool}`;
    case "DelegatedAgentToolCall":
      return `delegated call via ${agentLabel(action.agent)}`;
    case "DelegateAgent":
      return `delegate to ${agentLabel(action.target)}: ${action.task_summary}`;
    case "SpawnSubagent":
      return `spawn subagent (${action.role}): ${action.task_summary}`;
    default:
      return JSON.stringify(action);
  }
}

// --- ledger/query ---

ledgerBtn.addEventListener("click", refreshLedger);

async function refreshLedger() {
  if (!sessionId) return;
  try {
    const result = await request(methods.LEDGER_QUERY, { session_id: sessionId });
    ledgerPanel.textContent = renderLedger(result.rollup, 0);
  } catch (e) {
    ledgerPanel.textContent = `[ledger/query failed] ${e.message}`;
  }
}

function renderLedger(rollup, depth) {
  const indent = "  ".repeat(depth);
  const cost = rollup.cost_usd === null || rollup.cost_usd === undefined ? "$?" : `$${rollup.cost_usd.toFixed(6)}`;
  let line = `${indent}[ledger] session ${rollup.session_id} cost=${cost} (${rollup.confidence}) in=${fmtOpt(rollup.input_tokens)} out=${fmtOpt(rollup.output_tokens)} turns=${rollup.turns}`;
  for (const child of rollup.children) {
    line += "\n" + renderLedger(child, depth + 1);
  }
  return line;
}

function fmtOpt(v) {
  return v === null || v === undefined ? "None" : String(v);
}

// --- event log rendering ---

function renderEvent(event) {
  const payload = event.payload;
  switch (payload.type) {
    case "UserMessage":
      // Already echoed locally when sent; the daemon's own copy is
      // informational only, skip re-printing to avoid duplicate lines.
      break;
    case "AgentMessageChunk":
      appendChunk("agent", renderContent([payload.content]));
      break;
    case "AgentThoughtChunk":
      appendChunk("system", ` (thinking: ${renderContent([payload.content])})`);
      break;
    case "ToolCallRequested":
      appendLine("tool", `-> ${payload.call.tool_name} [${toolSourceLabel(payload.call.source)}]`);
      break;
    case "ToolCallUpdated":
      appendLine("tool", `<- ${payload.call_id} ${payload.status}`);
      break;
    case "PermissionRequested":
      // rendered by the permission modal, which opens right after this event
      break;
    case "PermissionDecided":
      appendLine("system", `[decision: ${describeDecision(payload.decision)}]`);
      break;
    case "UsageReported": {
      const u = payload.usage;
      appendLine(
        "usage",
        `[usage] in=${fmtOpt(u.input_tokens)} out=${fmtOpt(u.output_tokens)} cost=$${fmtOpt(u.cost_usd)} (${u.confidence}, ${u.wall_ms}ms)`,
      );
      break;
    }
    case "SessionDriverSet":
      appendLine("system", `[session driver: ${describeDriver(payload.driver)}]`);
      break;
    case "Error":
      appendLine("error", `[error${payload.recoverable ? " (recoverable)" : ""}] ${payload.message}`);
      break;
    default:
      appendLine("system", `[${event.actor}] ${JSON.stringify(payload)}`);
  }
}

function describeDecision(decision) {
  if (decision.type === "AllowAlways" || decision.type === "DenyAlways") {
    return `${decision.type}(scope: ${decision.scope})`;
  }
  return decision.type;
}

function describeDriver(driver) {
  if (driver.type === "Delegated") return `Delegated(${agentLabel(driver.agent)})`;
  return "Native";
}

function renderContent(blocks) {
  return blocks
    .map((b) => (b.type === "Text" ? b.text : `[${b.kind}]`))
    .join("");
}

let openChunkLine = null;

function appendLine(kind, text) {
  openChunkLine = null;
  const div = document.createElement("div");
  div.className = `line line-${kind}`;
  div.textContent = text;
  eventLog.appendChild(div);
  eventLog.scrollTop = eventLog.scrollHeight;
}

function appendChunk(kind, text) {
  if (!openChunkLine || openChunkLine.dataset.kind !== kind) {
    const div = document.createElement("div");
    div.className = `line line-${kind}`;
    div.dataset.kind = kind;
    eventLog.appendChild(div);
    openChunkLine = div;
  }
  openChunkLine.textContent += text;
  eventLog.scrollTop = eventLog.scrollHeight;
}

function appendSystemLine(text) {
  appendLine("system", text);
}

function appendErrorLine(text) {
  appendLine("error", text);
}

connect();
