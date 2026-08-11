#!/usr/bin/env python3
"""A minimal fake ACP agent for lh-acp's integration test.

Speaks just enough of the Agent Client Protocol (JSON-RPC 2.0,
newline-delimited, matching agent_client_protocol_schema's v1 wire shapes)
to exercise AcpConnection + HarnessAcpClient end to end: initialize ->
session/new -> on session/prompt, streams an AgentMessageChunk, a ToolCall,
a session/request_permission round trip, a ToolCallUpdate, a UsageUpdate,
then responds to session/prompt itself. No real model, no network --
purely a scripted peer so the harness side can be proven without any
external dependency.

A prompt containing "USE_TERMINAL" instead drives the full terminal/*
lifecycle (create -> output while still running -> wait_for_exit ->
release), exercising the client's terminal capability the same way a real
agent using its own background-bash feature over ACP would.
"""
import json
import sys


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def read():
    line = sys.stdin.readline()
    if not line:
        return None
    return json.loads(line)


def notify(method, params):
    send({"jsonrpc": "2.0", "method": method, "params": params})


def respond(request_id, result):
    send({"jsonrpc": "2.0", "id": request_id, "result": result})


def main():
    session_id = "fake-session-1"

    while True:
        msg = read()
        if msg is None:
            return
        method = msg.get("method")
        if method is None:
            # A response to something we sent (session/request_permission).
            continue

        req_id = msg.get("id")

        if method == "initialize":
            respond(req_id, {"protocolVersion": 1})
        elif method == "session/new":
            respond(req_id, {"sessionId": session_id})
        elif method == "session/prompt":
            prompt_text = ""
            try:
                prompt_text = msg["params"]["prompt"][0]["text"]
            except (KeyError, IndexError, TypeError):
                pass

            if "USE_TERMINAL" in prompt_text:
                send({
                    "jsonrpc": "2.0",
                    "id": "term-create-1",
                    "method": "terminal/create",
                    "params": {
                        "sessionId": session_id,
                        "command": "sh",
                        "args": ["-c", "echo background-start; sleep 0.3; echo background-done"],
                    },
                })
                create_resp = read()
                terminal_id = create_resp["result"]["terminalId"]
                sys.stderr.write(f"[fake-agent] terminal created: {terminal_id}\n")

                send({
                    "jsonrpc": "2.0",
                    "id": "term-output-1",
                    "method": "terminal/output",
                    "params": {"sessionId": session_id, "terminalId": terminal_id},
                })
                early_output = read()
                sys.stderr.write(f"[fake-agent] early output (should still be running): {early_output}\n")

                send({
                    "jsonrpc": "2.0",
                    "id": "term-wait-1",
                    "method": "terminal/wait_for_exit",
                    "params": {"sessionId": session_id, "terminalId": terminal_id},
                })
                wait_resp = read()
                sys.stderr.write(f"[fake-agent] wait_for_exit: {wait_resp}\n")

                send({
                    "jsonrpc": "2.0",
                    "id": "term-release-1",
                    "method": "terminal/release",
                    "params": {"sessionId": session_id, "terminalId": terminal_id},
                })
                release_resp = read()
                sys.stderr.write(f"[fake-agent] release: {release_resp}\n")

                respond(req_id, {"stopReason": "end_turn"})
                continue

            notify("session/update", {
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": "Working on it..."},
                },
            })
            notify("session/update", {
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "tool_call",
                    "toolCallId": "call_exec",
                    "title": "bash: echo hi",
                    "kind": "execute",
                    "status": "pending",
                },
            })

            permission_id = "perm-1"
            send({
                "jsonrpc": "2.0",
                "id": permission_id,
                "method": "session/request_permission",
                "params": {
                    "sessionId": session_id,
                    "toolCall": {
                        "toolCallId": "call_exec",
                        "title": "bash: echo hi",
                        "kind": "execute",
                        "status": "pending",
                    },
                    "options": [
                        {"optionId": "allow_once", "name": "Allow", "kind": "allow_once"},
                        {"optionId": "allow_always", "name": "Always Allow", "kind": "allow_always"},
                        {"optionId": "reject_once", "name": "Reject", "kind": "reject_once"},
                        {"optionId": "reject_always", "name": "Always Reject", "kind": "reject_always"},
                    ],
                },
            })
            perm_response = read()
            sys.stderr.write(f"[fake-agent] permission response: {perm_response}\n")

            notify("session/update", {
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "call_exec",
                    "status": "completed",
                },
            })
            notify("session/update", {
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "usage_update",
                    "used": 100,
                    "size": 10000,
                    "cost": {"amount": 0.01, "currency": "USD"},
                },
            })

            respond(req_id, {"stopReason": "end_turn"})
        else:
            send({
                "jsonrpc": "2.0",
                "id": req_id,
                "error": {"code": -32601, "message": f"unhandled method: {method}"},
            })


if __name__ == "__main__":
    main()
