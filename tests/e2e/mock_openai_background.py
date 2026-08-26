#!/usr/bin/env python3
"""Deterministic, stdlib-only provider for background restart recovery E2E."""
import json
import os
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


TASK_MARKER = os.environ["E2E_TASK_MARKER"]
COMMAND = "exec -a %s sleep 300" % TASK_MARKER


def sse(payload):
    return "data: " + json.dumps(payload, separators=(",", ":")) + "\n\n"


class Handler(BaseHTTPRequestHandler):
    calls = 0

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        self.rfile.read(length)
        Handler.calls += 1
        if Handler.calls == 1:
            arguments = json.dumps(
                {"command": COMMAND, "background": True}, separators=(",", ":")
            )
            chunks = [
                {
                    "id": "e2e-bg-call",
                    "object": "chat.completion.chunk",
                    "model": "mock-background",
                    "choices": [
                        {
                            "index": 0,
                            "delta": {
                                "role": "assistant",
                                "tool_calls": [
                                    {
                                        "index": 0,
                                        "id": "call-background-restart",
                                        "type": "function",
                                        "function": {
                                            "name": "bash",
                                            "arguments": arguments,
                                        },
                                    }
                                ],
                            },
                            "finish_reason": None,
                        }
                    ],
                },
                {
                    "id": "e2e-bg-call",
                    "object": "chat.completion.chunk",
                    "model": "mock-background",
                    "choices": [
                        {"index": 0, "delta": {}, "finish_reason": "tool_calls"}
                    ],
                },
            ]
        else:
            chunks = [
                {
                    "id": "e2e-bg-final",
                    "object": "chat.completion.chunk",
                    "model": "mock-background",
                    "choices": [
                        {
                            "index": 0,
                            "delta": {
                                "role": "assistant",
                                "content": "background task started",
                            },
                            "finish_reason": None,
                        }
                    ],
                },
                {
                    "id": "e2e-bg-final",
                    "object": "chat.completion.chunk",
                    "model": "mock-background",
                    "choices": [
                        {"index": 0, "delta": {}, "finish_reason": "stop"}
                    ],
                },
            ]
        body = "".join(sse(chunk) for chunk in chunks) + "data: [DONE]\n\n"
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Content-Length", str(len(body.encode())))
        self.end_headers()
        self.wfile.write(body.encode())
        self.wfile.flush()

    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(b'{"ok":true}')

    def log_message(self, fmt, *args):
        return


if __name__ == "__main__":
    port = int(os.environ["E2E_MOCK_PORT"])
    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
