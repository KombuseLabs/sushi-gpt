#!/usr/bin/env python3
"""Deterministic local wire peer. No model, login, network, or tool execution."""

import json
import os
import sys

if "--version" in sys.argv:
    print("2.1.263 (Claude Code)")
    sys.exit(0)

MODE = os.path.basename(sys.argv[0]).split(".")[0]
with open(sys.argv[0] + ".pid", "w") as marker:
    marker.write(str(os.getpid()))


def emit(value):
    print(json.dumps(value), flush=True)


def receive():
    line = sys.stdin.readline()
    if not line:
        sys.exit(0)
    return json.loads(line)


def mcp(request_id, method, params=None):
    emit(
        {
            "type": "control_request",
            "request_id": request_id,
            "request": {
                "subtype": "mcp_message",
                "server_name": "native",
                "message": {
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "method": method,
                    "params": params or {},
                },
            },
        }
    )


def response(response_id, content, reason, mcp_calls=()):
    def event(value):
        emit({"type": "stream_event", "event": value, "parent_tool_use_id": None})

    # Claude Code 2.1.x precedes every real turn with these informational frames.
    emit({"type": "command_lifecycle", "command_uuid": "cmd", "state": "queued"})
    emit({"type": "system", "subtype": "status", "status": "requesting"})
    emit(
        {
            "type": "rate_limit_event",
            "rate_limit_info": {"status": "allowed", "rateLimitType": "five_hour"},
        }
    )
    event(
        {
            "type": "message_start",
            "message": {
                "id": response_id,
                "model": "observed-claude",
                "role": "assistant",
                "content": [],
                "usage": {"input_tokens": 5, "output_tokens": 0},
            },
        }
    )
    for index, block in enumerate(content):
        event({"type": "content_block_start", "index": index, "content_block": block})
        event({"type": "content_block_stop", "index": index})
    # Claude Code 2.1.x emits the assistant summary and the MCP tools/call requests
    # before message_delta/message_stop, without waiting for the tool results.
    emit({"type": "assistant", "message": {"id": response_id, "content": content}})
    for request_id, name, arguments, tool_use_id in mcp_calls:
        params = {"name": name, "arguments": arguments}
        if tool_use_id:
            params["_meta"] = {"claudecode/toolUseId": tool_use_id, "progressToken": 2}
        mcp(request_id, "tools/call", params)
    event(
        {
            "type": "message_delta",
            "delta": {"stop_reason": reason},
            "usage": {"output_tokens": 2},
        }
    )
    event({"type": "message_stop"})


initialize = receive()
assert initialize["request"]["subtype"] == "initialize"
mcp("list", "tools/list")
tools = receive()["response"]["response"]["mcp_response"]["result"]["tools"]
tool = next(
    tool for tool in tools if tool["description"] == "Synthetic document callback"
)
emit(
    {
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": initialize["request_id"],
            "response": {"account": {"sentinel": "MUST_NOT_BE_RECORDED"}},
        },
    }
)
turns = 0
while True:
    user = receive()
    if user.get("type") == "control_request":
        sys.exit(0)
    assert user["type"] == "user"
    turns += 1
    if turns == 1:
        count = 2 if MODE == "sequential" else 1
        blocks = [
            {
                "type": "tool_use",
                "id": f"call-{i}",
                "name": "mcp__native__" + tool["name"],
                "input": {"title": f"fixture-{i}"},
            }
            for i in range(count)
        ]
        # The first call is issued before message_stop, as the real CLI does, and carries
        # the CLI's tool_use id. A sequential CLI issues the next call only after the
        # previous result; that one relies on name/args correlation.
        response(
            "model-tools",
            blocks,
            "tool_use",
            mcp_calls=[("tool-0", tool["name"], {"title": "fixture-0"}, "call-0")],
        )
        for i in range(count):
            if i > 0:
                mcp(
                    f"tool-{i}",
                    "tools/call",
                    {"name": tool["name"], "arguments": {"title": f"fixture-{i}"}},
                )
            answer = receive()
            if answer.get("type") == "control_request":
                sys.exit(0)
            assert "document-fixture-created" in json.dumps(answer)
        if MODE == "early_exit":
            sys.exit(0)
    response(
        f"model-final-{turns}",
        [{"type": "text", "text": "document completed"}],
        "end_turn",
    )
    emit(
        {
            "type": "result",
            "subtype": "success",
            "is_error": MODE == "error",
            "queued_turn_count": 0,
        }
    )
