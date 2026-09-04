#!/usr/bin/env python3
"""Deterministic stdlib OpenAI SSE provider for the subagent stall E2E."""
import json
import os
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Handler(BaseHTTPRequestHandler):
    server_version = "subagent-stall-e2e-mock/1"

    def log_message(self, fmt, *args):
        sys.stderr.write((fmt % args) + "\n")

    def do_POST(self):
        length = int(self.headers.get("content-length", "0"))
        request = json.loads(self.rfile.read(length))
        messages = request.get("messages", [])
        tools = request.get("tools", [])
        names = {tool.get("function", {}).get("name") for tool in tools}
        has_tool_result = any(message.get("role") == "tool" for message in messages)

        with self.server.call_lock:
            self.server.call_count += 1
            number = self.server.call_count
            with open(self.server.calls_file, "a", encoding="utf-8") as calls:
                calls.write(json.dumps({"number": number, "path": self.path}) + "\n")

        user_text = " ".join(
            message.get("content", "")
            for message in messages
            if message.get("role") == "user" and isinstance(message.get("content"), str)
        )
        # The parent asks for a delegate; the child receives the delegated
        # task text.  Both tool lists contain delegate+bash, so the task text
        # is the deterministic boundary between the two real calls.
        child_task = "requested silent child-owned" in user_text
        if not child_task and "delegate" in names and not has_tool_result:
            arguments = {
                "workspace": ".",
                "task": "Start the requested silent child-owned background command, then finish.",
                "label": "subagent-stall-child",
            }
            if set(arguments) != {"workspace", "task", "label"}:
                raise AssertionError("delegate payload contains an obsolete argument")
            payload = self.tool_call("delegate", "stall-delegate", arguments)
        elif child_task and "bash" in names and not has_tool_result:
            command = (
                f"printf '%s\\n' {self.server.run_marker}-child-output; "
                f"exec -a {self.server.run_marker}-child-silent sleep 330 "
                f"# {self.server.run_marker}-full-command-marker-{self.server.run_marker}"
            )
            arguments = {"command": command, "background": True}
            if set(arguments) != {"command", "background"} or "detached" in arguments:
                raise AssertionError("bash payload contains an obsolete argument")
            payload = self.tool_call("bash", "stall-bash", arguments)
        else:
            # The completion-driven follow-up is intentionally delayed a little.
            # This leaves an observable interval in which the child's durable
            # completion exists while its parent delegate row still exists.
            if any(
                isinstance(message.get("content"), str)
                and "[background task" in message["content"]
                for message in messages
            ):
                time.sleep(2)
            payload = self.text("subagent stall E2E turn complete")

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
            "id": "subagent-stall-e2e",
            "object": "chat.completion.chunk",
            "created": 1,
            "model": "mock-subagent-stall",
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
        raise SystemExit("usage: mock_openai_subagent_stall.py PORT RUN_MARKER")
    server = ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler)
    server.run_marker = sys.argv[2]
    server.calls_file = os.environ["MOCK_CALLS_FILE"]
    server.call_count = 0
    server.call_lock = threading.Lock()
    print(server.server_address[1], flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
