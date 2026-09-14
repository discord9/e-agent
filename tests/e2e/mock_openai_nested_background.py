#!/usr/bin/env python3
"""Stdlib-only deterministic OpenAI-compatible SSE provider for the nested E2E.

The first request with ``delegate`` asks for a long-lived child subagent. The
first request without ``delegate`` but with ``bash`` asks that child to start a
long-lived background command. Tool-result requests receive a normal final
answer, which leaves the background task itself alive in the child registry.

``GET /requests`` exposes a sequenced snapshot of provider requests. Recovery
requests are classified from their killed-task Notice, rather than from a
prompt, so the restart E2E can assert ownership and exact wake-up counts.
"""
import json
import os
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Handler(BaseHTTPRequestHandler):
    server_version = "nested-e2e-mock/1"

    def log_message(self, fmt, *args):
        sys.stderr.write((fmt % args) + "\n")

    def do_GET(self):
        if self.path != "/requests":
            self.send_error(404)
            return
        with self.server.requests_lock:
            body = json.dumps({"requests": self.server.requests}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        length = int(self.headers.get("content-length", "0"))
        request = json.loads(self.rfile.read(length))
        messages = request.get("messages", [])
        tools = request.get("tools", [])
        names = {tool.get("function", {}).get("name") for tool in tools}
        has_tool_result = any(message.get("role") == "tool" for message in messages)
        has_recovery_notice = any(
            message.get("role") == "user"
            and isinstance(message.get("content"), str)
            and message["content"].startswith("[e-agent exited with ")
            for message in messages
        )
        if has_recovery_notice:
            # A fresh persisted killed-task Notice gets one regular reaction,
            # never another tool call. Historical replay has no wake-up.
            payload = self.text("nested E2E mock acknowledged recovery Notice")
        elif os.environ.get("EAGENT_OWNER_CLEANUP_E2E") != "1":
            if "delegate" in names and not has_tool_result:
                arguments = {
                    "workspace": ".",
                    "label": "parent-delegate-live",
                    "task": (
                        "Start one long-lived background bash task with label "
                        "child-own-long-background, then finish."
                    ),
                }
                payload = self.tool_call("delegate", "call-delegate-nested", arguments)
            elif "bash" in names and not has_tool_result:
                payload = self.tool_call(
                    "bash",
                    "call-bash-nested",
                    {"command": f"exec -a {self.server.run_marker}-child-own-background sleep 600", "background": True},
                )
            else:
                payload = self.text("nested E2E mock completed the tool call")
        else:
            prompt = " ".join(
                message.get("content", "")
                for message in messages
                if message.get("role") == "user" and isinstance(message.get("content"), str)
            )
            case = "cancel" if "cancel" in prompt else "normal"
            label = f"owner-cleanup-{case}"
            if "delegate" in names and not has_tool_result:
                payload = self.tool_call(
                    "delegate", "call-delegate-nested", {
                        "workspace": ".", "label": label,
                        "task": f"Start one {case} child-owned background bash task, then finish.",
                    }
                )
            elif "bash" in names and not has_tool_result:
                seconds = "30" if case == "cancel" else "3"
                payload = self.tool_call(
                    "bash", "call-bash-nested", {
                        "command": f"exec -a {self.server.run_marker}-child-own-background sleep {seconds}",
                        "background": True,
                    }
                )
            else:
                payload = self.text("nested E2E mock completed the tool call")
        emitted_tool_calls = [
            call["function"]["name"]
            for call in payload[0]["choices"][0]["delta"].get("tool_calls", [])
        ]
        self.record_request(messages, names, has_tool_result, emitted_tool_calls)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()
        for chunk in payload:
            self.wfile.write(("data: " + json.dumps(chunk) + "\n\n").encode())
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()

    def record_request(self, messages, names, has_tool_result, emitted_tool_calls):
        notices = [
            message["content"]
            for message in messages
            if message.get("role") == "user"
            and isinstance(message.get("content"), str)
            and message["content"].startswith("[e-agent exited with ")
        ]
        record = {
            "tool_names": sorted(name for name in names if name),
            "emitted_tool_calls": emitted_tool_calls,
            "tool_result_count":  sum(message.get("role") == "tool" for message in messages),
            "has_tool_result": has_tool_result,
            "recovery_notices": notices,
            "recovery_notice_count": len(notices),
            "has_parent_recovery": any("parent-delegate-live" in notice for notice in notices),
            "has_child_recovery": any(
                f"{self.server.run_marker}-child-own-background" in notice
                for notice in notices
            ),
        }
        with self.server.requests_lock:
            record["sequence"] = len(self.server.requests) + 1
            self.server.requests.append(record)
            if self.server.request_log:
                with open(self.server.request_log, "a", encoding="utf-8") as log:
                    log.write(json.dumps(record, separators=(",", ":")) + "\n")

    @staticmethod
    def base(delta, finish=None):
        return {
            "id": "nested-e2e",
            "object": "chat.completion.chunk",
            "created": 1,
            "model": "mock-nested",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        }

    @classmethod
    def tool_call(cls, name, call_id, arguments):
        return [
            cls.base(
                {
                    "role": "assistant",
                    "tool_calls": [
                        {
                            "index": 0,
                            "id": call_id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": json.dumps(arguments, separators=(",", ":")),
                            },
                        }
                    ],
                }
            ),
            cls.base({}, "tool_calls"),
        ]

    @classmethod
    def text(cls, text):
        return [cls.base({"role": "assistant", "content": text}), cls.base({}, "stop")]


if __name__ == "__main__":
    if len(sys.argv) not in (3, 4):
        raise SystemExit("usage: mock_openai_nested_background.py PORT RUN_MARKER [REQUEST_LOG]")
    server = ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler)
    server.run_marker = sys.argv[2]
    server.request_log = sys.argv[3] if len(sys.argv) == 4 else None
    server.requests = []
    server.requests_lock = threading.Lock()
    print(server.server_address[1], flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
