#!/usr/bin/env python3
"""Isolated stdlib acceptance test for optional background bash titles.

Runs a local OpenAI-compatible SSE mock and a real ``e-agent --serve``.  It
checks the public HTTP representation and persisted session history rather
than implementation details.
"""
import json
import os
import shutil
import socket
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
ARTIFACT_ROOT = ROOT / ".e-agent" / "title-acceptance"
DEFAULT_BINARY = Path("/mnt/nvme_rust/rust-targets-2/e-agent-background-title/debug/e-agent")
EXACT_TITLE = "acceptance title: λ"
LONG_UNICODE_TITLE = "标题🚀" * 180
CASES = {
    "exact-live": {"command": "sleep 30", "background": True, "title": EXACT_TITLE},
    "omitted": {"command": "sleep 30", "background": True},
    "blank": {"command": "sleep 30", "background": True, "title": " \n\t "},
    "foreground": {"command": "printf foreground-title-ignored", "title": EXACT_TITLE},
    "long-unicode": {"command": "sleep 30", "background": True, "title": LONG_UNICODE_TITLE},
    "completion": {"command": "printf completion-title-ok", "background": True, "title": EXACT_TITLE},
}


def fail(message):
    raise AssertionError(message)


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def sse(chunk):
    return "data: " + json.dumps(chunk, separators=(",", ":")) + "\n\n"


class MockHandler(BaseHTTPRequestHandler):
    def log_message(self, _fmt, *_args):
        return

    def do_GET(self):
        if self.path == "/health":
            self.send_response(200)
            self.end_headers()
            self.wfile.write(b"ok")
        else:
            self.send_error(404)

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        request = json.loads(raw)
        messages = request.get("messages", [])
        prompt = next(
            (m["content"] for m in reversed(messages)
             if m.get("role") == "user" and isinstance(m.get("content"), str)
             and m["content"].startswith("TITLE-ACCEPTANCE:")),
            None,
        )
        has_tool_result = any(m.get("role") == "tool" for m in messages)
        case_name = prompt.split(":", 1)[1] if prompt else ""
        if case_name in CASES and not has_tool_result:
            arguments = json.dumps(CASES[case_name], separators=(",", ":"), ensure_ascii=False)
            chunks = [
                {"id": "title-call", "object": "chat.completion.chunk", "model": "title-mock",
                 "choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [{
                     "index": 0, "id": "call-" + case_name, "type": "function",
                     "function": {"name": "bash", "arguments": arguments},
                 }]}, "finish_reason": None}]},
                {"id": "title-call", "object": "chat.completion.chunk", "model": "title-mock",
                 "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]},
            ]
        else:
            chunks = [
                {"id": "title-final", "object": "chat.completion.chunk", "model": "title-mock",
                 "choices": [{"index": 0, "delta": {"role": "assistant", "content": "mock finished"},
                              "finish_reason": None}]},
                {"id": "title-final", "object": "chat.completion.chunk", "model": "title-mock",
                 "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
            ]
        body = "".join(sse(chunk) for chunk in chunks) + "data: [DONE]\n\n"
        encoded = body.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)


def request(base, token, method, path, body=None):
    data = None if body is None else json.dumps(body).encode()
    headers = {"Authorization": "Bearer " + token}
    if data is not None:
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(base + path, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=5) as response:
            text = response.read().decode()
            return response.status, json.loads(text) if text else None
    except urllib.error.HTTPError as error:
        text = error.read().decode()
        return error.code, json.loads(text) if text else None


def wait_until(description, predicate, timeout=20):
    end = time.monotonic() + timeout
    last = None
    while time.monotonic() < end:
        last = predicate()
        if last:
            return last
        time.sleep(0.1)
    fail(f"timed out waiting for {description}; last={last!r}")


def history_entries(base, token, session_id):
    status, payload = request(base, token, "GET", f"/api/sessions/{session_id}/history")
    if status != 200:
        fail(f"history for {session_id}: HTTP {status}, {payload!r}")
    return payload.get("entries", [])


def task_for_session(base, token, session_id):
    status, tasks = request(base, token, "GET", "/api/tasks")
    if status != 200:
        fail(f"task list: HTTP {status}, {tasks!r}")
    return [task for task in tasks if task.get("session_id") == session_id]


def start_case(base, token, name):
    session_id = "title-" + name + "-" + uuid.uuid4().hex[:10]
    status, _ = request(base, token, "POST", "/api/sessions", {"id": session_id})
    if status != 201:
        fail(f"create {name}: HTTP {status}")
    status, _ = request(base, token, "POST", f"/api/sessions/{session_id}/prompt",
                        {"text": "TITLE-ACCEPTANCE:" + name})
    if status != 202:
        fail(f"prompt {name}: HTTP {status}")
    return session_id


def tool_result_values(entries):
    values = []
    for entry in entries:
        message = entry.get("message", {})
        if entry.get("type") == "message" and message.get("Tool"):
            values.append(message["Tool"].get("content", ""))
    return values


def assert_started(entries, task_id, label):
    expected = f"started background task {task_id}: {label}"
    values = tool_result_values(entries)
    if not any(expected in value for value in values):
        fail(f"start label missing exact text {expected!r} in tool results {values!r}")


def assert_live_task(base, token, session_id, label, command):
    def lookup():
        tasks = task_for_session(base, token, session_id)
        return tasks[0] if len(tasks) == 1 else None
    task = wait_until(f"one live task for {session_id}", lookup)
    if task.get("label") != label:
        fail(f"task label={task.get('label')!r}, expected {label!r}")
    if task.get("full_command") != command:
        fail(f"full_command={task.get('full_command')!r}, expected {command!r}")
    if task.get("owner_session") != session_id:
        fail(f"owner_session={task.get('owner_session')!r}, expected unchanged {session_id!r}")
    expected = f"started background task {task['id']}: {label}"
    entries = wait_until(
        "persisted start tool result",
        lambda: (lambda entries: entries if any(expected in value for value in tool_result_values(entries)) else None)(
            history_entries(base, token, session_id)
        ),
    )
    assert_started(entries, task["id"], label)
    return task


def assert_completion(base, token, session_id, label, output_part):
    def completed():
        entries = history_entries(base, token, session_id)
        matches = [entry for entry in entries if entry.get("type") == "background_completion"
                   and entry.get("label") == label]
        return matches[-1] if matches else None
    entry = wait_until(f"completion labeled {label!r}", completed)
    if output_part not in entry.get("output", ""):
        fail(f"completion output {entry.get('output')!r} lacks {output_part!r}")


def main():
    binary = Path(os.environ.get("EAGENT_BIN", DEFAULT_BINARY))
    if not binary.is_file() or not os.access(binary, os.X_OK):
        print(f"SKIP: required built binary is not executable: {binary}", file=sys.stderr)
        return 2
    run = ARTIFACT_ROOT / ("run-" + uuid.uuid4().hex)
    config = run / "config" / "e-agent"
    state = run / "state"
    workspace = run / "workspace"
    config.mkdir(parents=True)
    state.mkdir(parents=True)
    workspace.mkdir(parents=True)
    mock_port, server_port = free_port(), free_port()
    (config / "config.toml").write_text(
        "default = \"mock/title\"\n"
        "[providers.mock]\n"
        f"base_url = \"http://127.0.0.1:{mock_port}/v1\"\n"
        "api_key_env = \"E2E_MOCK_KEY\"\n"
        "[models.\"mock/title\"]\nmodel = \"title-mock\"\n",
        encoding="utf-8",
    )
    mock = ThreadingHTTPServer(("127.0.0.1", mock_port), MockHandler)
    mock_thread = threading.Thread(target=mock.serve_forever, daemon=True)
    mock_thread.start()
    env = os.environ.copy()
    env.update({"XDG_CONFIG_HOME": str(run / "config"), "XDG_STATE_HOME": str(state),
                "HOME": str(run / "home"), "E2E_MOCK_KEY": "local-only"})
    Path(env["HOME"]).mkdir()
    server_log = (run / "server.log").open("w", encoding="utf-8")
    server = subprocess.Popen([str(binary), "--serve", "--host", "127.0.0.1", "--port", str(server_port),
                               "--workspace", str(workspace)], env=env, stdout=server_log, stderr=subprocess.STDOUT)
    base = f"http://127.0.0.1:{server_port}"
    token_path = state / "e-agent" / "server.token"
    token = None
    live_sessions = []
    try:
        def ready():
            nonlocal token
            if server.poll() is not None:
                fail(f"server exited {server.returncode}; log={run / 'server.log'}")
            if not token_path.exists():
                return False
            token = token_path.read_text().strip()
            return request(base, token, "GET", "/api/tasks")[0] == 200
        wait_until("server readiness", ready)

        exact_sid = start_case(base, token, "exact-live")
        exact = assert_live_task(base, token, exact_sid, EXACT_TITLE, "sleep 30")
        live_sessions.append((exact_sid, exact["id"]))

        omitted_sid = start_case(base, token, "omitted")
        omitted = assert_live_task(base, token, omitted_sid, "sleep 30", "sleep 30")
        live_sessions.append((omitted_sid, omitted["id"]))

        blank_sid = start_case(base, token, "blank")
        blank = assert_live_task(base, token, blank_sid, "sleep 30", "sleep 30")
        live_sessions.append((blank_sid, blank["id"]))

        long_sid = start_case(base, token, "long-unicode")
        long_task = assert_live_task(base, token, long_sid, LONG_UNICODE_TITLE, "sleep 30")
        if len(long_task["label"]) != len(LONG_UNICODE_TITLE):
            fail("long Unicode title was truncated")
        live_sessions.append((long_sid, long_task["id"]))

        foreground_sid = start_case(base, token, "foreground")
        wait_until("foreground tool result", lambda: history_entries(base, token, foreground_sid))
        wait_until("foreground has no background task", lambda: task_for_session(base, token, foreground_sid) == [])

        completion_sid = start_case(base, token, "completion")
        assert_completion(base, token, completion_sid, EXACT_TITLE, "completion-title-ok")

        status, _ = request(base, token, "DELETE", f"/api/sessions/{exact_sid}/tasks/{exact['id']}")
        if status != 204:
            fail(f"cancel exact title task: HTTP {status}")
        assert_completion(base, token, exact_sid, EXACT_TITLE, "background task cancelled")
        live_sessions.remove((exact_sid, exact["id"]))
        print("PASS: exact title appears in start, task list, and cancellation completion")
        print("PASS: omitted and blank titles fall back to the command label")
        print("PASS: foreground title is ignored; long Unicode title is untruncated")
        print("PASS: task owner_session and full_command are retained")
        return 0
    finally:
        for session_id, task_id in live_sessions:
            try:
                request(base, token, "DELETE", f"/api/sessions/{session_id}/tasks/{task_id}")
            except (OSError, urllib.error.URLError):
                pass
        if server.poll() is None:
            server.terminate()
            try:
                server.wait(timeout=5)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait(timeout=5)
        server_log.close()
        mock.shutdown()
        mock.server_close()
        shutil.rmtree(run, ignore_errors=True)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except AssertionError as error:
        print("FAIL:", error, file=sys.stderr)
        raise SystemExit(1)
