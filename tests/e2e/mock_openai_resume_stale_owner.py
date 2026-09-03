#!/usr/bin/env python3
"""Deterministic stdlib-only provider for the stale-owner resume E2E."""
import json
import re
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Handler(BaseHTTPRequestHandler):
    server_version = "resume-stale-owner-e2e-mock/1"

    def log_message(self, fmt, *args):
        sys.stderr.write((fmt % args) + "\n")

    def do_GET(self):
        if self.path == "/health":
            self.send_response(200)
            self.send_header("Content-Type", "text/plain")
            self.end_headers()
            self.wfile.write(b"ok\n")
        else:
            self.send_error(404)

    def do_POST(self):
        length = int(self.headers.get("content-length", "0"))
        request = json.loads(self.rfile.read(length))
        messages = request.get("messages", [])
        tools = request.get("tools", [])
        names = {
            tool.get("function", {}).get("name")
            for tool in tools
            if isinstance(tool, dict)
        }
        last_user = next(
            (
                message.get("content", "")
                for message in reversed(messages)
                if message.get("role") == "user"
                and isinstance(message.get("content"), str)
            ),
            "",
        )

        after_last_user = messages[
            max(
                (index for index, message in enumerate(messages)
                 if message.get("role") == "user"),
                default=-1,
            ) + 1 :
        ]
        tool_call_already_returned = any(
            message.get("role") == "assistant" and message.get("tool_calls")
            for message in after_last_user
        )

        create_requested = "Create and finish a child transcript" in last_user
        resume_match = re.search(r"Resume child session ([A-Za-z0-9_-]+)", last_user)
        if "delegate" in names and not tool_call_already_returned:
            if create_requested:
                payload = self.tool_call(
                    "delegate",
                    "resume-stale-owner-create",
                    {
                        "workspace": ".",
                        "label": "stale-owner-child",
                        "task": (
                            "Create a durable child transcript for the stale-owner "
                            "resume E2E, then finish successfully."
                        ),
                    },
                )
            elif resume_match:
                payload = self.tool_call(
                    "delegate",
                    "resume-stale-owner-resume",
                    {
                        "workspace": ".",
                        "label": "stale-owner-child-resume",
                        "resume": resume_match.group(1),
                        "task": "Resume the child transcript and finish successfully.",
                    },
                )
            else:
                payload = self.text("parent turn completed")
        elif tool_call_already_returned and resume_match:
            payload = self.text("parent resume completed successfully")
        else:
            payload = self.text("child transcript established")

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
            "id": "resume-stale-owner-e2e",
            "object": "chat.completion.chunk",
            "created": 1,
            "model": "mock-resume-stale-owner",
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
        raise SystemExit("usage: mock_openai_resume_stale_owner.py PORT RUN_MARKER")
    server = ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler)
    print(server.server_address[1], flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
