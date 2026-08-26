#!/usr/bin/env python3
"""Stdlib-only deterministic OpenAI-compatible SSE provider for the nested E2E.

The first request with ``delegate`` asks for a long-lived child subagent.  The
first request without ``delegate`` but with ``bash`` asks that child to start a
long-lived background command.  Tool-result requests receive a normal final
answer, which leaves the background task itself alive in the child registry.
"""
import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Handler(BaseHTTPRequestHandler):
    server_version = "nested-e2e-mock/1"

    def log_message(self, fmt, *args):
        sys.stderr.write((fmt % args) + "\n")

    def do_POST(self):
        length = int(self.headers.get("content-length", "0"))
        request = json.loads(self.rfile.read(length))
        messages = request.get("messages", [])
        tools = request.get("tools", [])
        names = {tool.get("function", {}).get("name") for tool in tools}
        has_tool_result = any(message.get("role") == "tool" for message in messages)

        if "delegate" in names and not has_tool_result:
            arguments = {
                "workspace": ".",
                "label": "parent-delegate-live",
                "background": True,
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
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()
        for chunk in payload:
            self.wfile.write(("data: " + json.dumps(chunk) + "\n\n").encode())
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()

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
    if len(sys.argv) != 3:
        raise SystemExit("usage: mock_openai_nested_background.py PORT RUN_MARKER")
    server = ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler)
    server.run_marker = sys.argv[2]
    print(server.server_address[1], flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
