#!/usr/bin/env python3
"""端到端验证：web_search 通过 SearXNG 后端返回真实搜索结果。

需要一个已运行的 SearXNG 实例（默认 http://127.0.0.1:8088）。
通过 E_AGENT_SEARXNG_URL 环境变量覆盖。

用法（searxng-config worktree 根目录）：
    uv run python tests/e2e/searxng_acceptance.py

验证流程：
1. 启动 mock OpenAI provider（让模型调用 web_search）。
2. 用 feat/web-search-searxng 编译的 e-agent 二进制启动 --serve。
3. 创建 session，发送 prompt。
4. 验证 web_search 工具被调用且结果非空（来自真实 SearXNG）。
"""
import json
import os
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

HERE = Path(__file__).resolve().parents[2]
BINARY = HERE / "target" / "debug" / "e-agent"
if not BINARY.exists():
    BINARY = Path(os.environ.get(
        "E_AGENT_BINARY",
        "/mnt/nvme_rust/rust-targets/e-agent-searxng/debug/e-agent"))

SEARXNG_URL = os.environ.get("E_AGENT_SEARXNG_URL", "http://127.0.0.1:8088")
SESSION_ID = "searxng-e2e"

# ---------------------------------------------------------------------------
# Mock OpenAI provider: first call → web_search tool call; second → final text.
# ---------------------------------------------------------------------------

def sse(payload):
    return "data: " + json.dumps(payload, separators=(",", ":")) + "\n\n"


class Handler(BaseHTTPRequestHandler):
    calls = 0
    # Observations the end-to-end assertions rely on: the tool was really
    # offered to the model, and the SearXNG payload really came back to the
    # provider as a `role: "tool"` message on the follow-up request.
    saw_web_search_tool = False
    saw_search_result = False

    @classmethod
    def observe(cls, body):
        for tool in body.get("tools") or []:
            if isinstance(tool, dict) and tool.get("function", {}).get("name") == "web_search":
                cls.saw_web_search_tool = True
        for message in body.get("messages", []):
            if not isinstance(message, dict) or message.get("role") != "tool":
                continue
            content = message.get("content")
            if isinstance(content, str) and "tokio" in content.lower():
                cls.saw_search_result = True

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        body = json.loads(self.rfile.read(length))
        Handler.calls += 1
        Handler.observe(body)
        if Handler.calls == 1:
            arguments = json.dumps({"query": "rust tokio async runtime"}, separators=(",", ":"))
            chunks = [
                {
                    "id": "e2e-search-call",
                    "object": "chat.completion.chunk",
                    "model": "mock",
                    "choices": [{
                        "index": 0,
                        "delta": {
                            "role": "assistant",
                            "tool_calls": [{
                                "index": 0,
                                "id": "call-web-search",
                                "type": "function",
                                "function": {"name": "web_search", "arguments": arguments},
                            }],
                        },
                        "finish_reason": None,
                    }],
                },
                {
                    "id": "e2e-search-call",
                    "object": "chat.completion.chunk",
                    "model": "mock",
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
                },
            ]
        else:
            chunks = [
                {
                    "id": "e2e-search-final",
                    "object": "chat.completion.chunk",
                    "model": "mock",
                    "choices": [{
                        "index": 0,
                        "delta": {"role": "assistant", "content": "search done"},
                        "finish_reason": None,
                    }],
                },
                {
                    "id": "e2e-search-final",
                    "object": "chat.completion.chunk",
                    "model": "mock",
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                },
            ]
        payload = "".join(sse(c) for c in chunks) + "data: [DONE]\n\n"
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        self.wfile.write(payload.encode())

    def log_message(self, fmt, *args):
        pass


def choose_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def request(url, token=None, method="GET", body=None):
    req = urllib.request.Request(url, method=method)
    if token:
        req.add_header("Authorization", "Bearer " + token)
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, data, timeout=15) as resp:
            raw = resp.read()
            try:
                return resp.status, json.loads(raw)
            except ValueError:
                # Endpoints like /prompt answer 202 with an empty body.
                return resp.status, {}
    except urllib.error.HTTPError as e:
        body = e.read()
        try:
            return e.code, json.loads(body)
        except ValueError:
            return e.code, {}
    except urllib.error.URLError:
        # Not listening yet (server startup race) or already gone.
        return 0, {}


def wait_for(desc, fn, timeout=15):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if fn():
            return
        time.sleep(0.3)
    raise AssertionError("timeout: " + desc)


def unwrap_entry(entry):
    """Split one `/history` entry into `(kind, payload)`.

    Persisted entries are the serde shape of the session's `SessionEntry`
    enum, i.e. `{"type": "message", "message": {"Tool": {...}}}` with the
    inner `Message` variant as an externally-tagged key (`User`,
    `Assistant`, `Tool`). Flat shapes are tolerated so the reader does not
    hard-code the nesting depth.
    """
    if not isinstance(entry, dict):
        return None, {}
    message = entry.get("message")
    if isinstance(message, dict) and len(message) == 1:
        (kind, payload), = message.items()
        return str(kind).lower(), payload if isinstance(payload, dict) else {}
    kind = entry.get("type")
    return (str(kind).lower() if isinstance(kind, str) else None), entry


def assistant_text(entry):
    kind, payload = unwrap_entry(entry)
    return payload.get("content") or "" if kind == "assistant" else ""


def tool_payloads(entries):
    """Every tool-result payload in the history, newest wire shapes first."""
    payloads = []
    for entry in entries:
        kind, payload = unwrap_entry(entry)
        if kind in ("tool", "tool_result"):
            payloads.append(payload)
    return payloads


def main():
    # 0. Verify SearXNG is reachable
    try:
        with urllib.request.urlopen(SEARXNG_URL + "/", timeout=3) as r:
            assert r.status == 200
    except Exception as e:
        print("SKIP: SearXNG not reachable at %s: %s" % (SEARXNG_URL, e))
        sys.exit(0)  # skip, not fail

    # 1. Mock provider
    mock_port = choose_port()
    mock = ThreadingHTTPServer(("127.0.0.1", mock_port), Handler)
    threading.Thread(target=mock.serve_forever, daemon=True).start()

    # 2. Isolated fixture
    root = Path(tempfile.mkdtemp(prefix="searxng-e2e-", dir=HERE / ".e-agent"))
    home = root / "home"
    config = root / "config" / "e-agent"
    state = root / "state" / "e-agent"
    workspace = root / "workspace"
    for d in (home, config, state, workspace):
        d.mkdir(parents=True)

    (config / "config.toml").write_text(f"""\
default = "mock/m1"

[models."mock/m1"]
model = "m1"

[providers.mock]
base_url = "http://127.0.0.1:{mock_port}"
api_key_env = "E2E_KEY"

[web_search]
provider = "searxng"
base_url = "{SEARXNG_URL}"
""")

    # 3. Start e-agent
    port = choose_port()
    log_path = root / "server.log"
    env = {
        "HOME": str(home),
        "XDG_CONFIG_HOME": str(root / "config"),
        "XDG_STATE_HOME": str(root / "state"),
        "E2E_KEY": "fixture-only",
        "PATH": os.environ.get("PATH", ""),
    }
    proc = subprocess.Popen(
        [str(BINARY), "--serve", "--host", "127.0.0.1", "--port", str(port), "--workspace", str(workspace)],
        stdin=subprocess.DEVNULL, stdout=log_path.open("wb"), stderr=subprocess.STDOUT,
        env=env, start_new_session=True, cwd=str(workspace),
    )
    base = f"http://127.0.0.1:{port}"

    try:
        # The server writes the token before it binds the listener
        # (`load_or_create_token` runs first in `server::run`), so the token
        # file alone is not readiness: also probe the authenticated API so
        # the first create-session POST cannot race the bind. Path: the real
        # location is `$XDG_STATE_HOME/e-agent/server.token` (the state dir
        # helper appends `e-agent`), so `state` above already ends in
        # `e-agent` — no second segment.
        token_file = state / "server.token"
        token = {}

        def ready():
            if proc.poll() is not None:
                raise AssertionError(
                    f"e-agent exited rc={proc.returncode}; log:\n{log_path.read_text(errors='replace')}")
            if not token_file.exists():
                return False
            value = token_file.read_text().strip()
            if not value:
                return False
            token["value"] = value
            status, _ = request(base + "/api/sessions", value)
            return status == 200

        wait_for("e-agent serve", ready, 30)
        base_token = token["value"]

        # 4. Create session + send prompt
        status, session = request(base + "/api/sessions", base_token, "POST", {"id": SESSION_ID})
        assert status == 201, f"create session: {status} {session}"

        status, _ = request(base + f"/api/sessions/{SESSION_ID}/prompt", base_token, "POST", {"text": "search rust tokio"})
        assert status == 202, f"prompt: {status}"

        # 5. Wait for the turn to complete (mock provider gives 2 responses)
        def done():
            s, entries = request(base + f"/api/sessions/{SESSION_ID}/history", base_token)
            if s != 200:
                return False
            return any("search done" in assistant_text(e) for e in entries.get("entries", []))

        wait_for("turn completes", done, 30)

        # 6. Verify web_search was called and returned SearXNG results
        s, entries = request(base + f"/api/sessions/{SESSION_ID}/history", base_token)
        assert s == 200
        tool_results = [p for p in tool_payloads(entries.get("entries", [])) if p.get("name") == "web_search"]
        assert tool_results, "no web_search tool_result in history"
        search_result = tool_results[0].get("content", "")
        assert not tool_results[0].get("is_error"), f"web_search failed: {search_result[:200]}"
        assert len(search_result) > 50, f"search result too short: {search_result[:100]}"
        assert "tokio" in search_result.lower(), f"expected 'tokio' in result: {search_result[:200]}"
        assert "http" in search_result.lower(), f"expected result URLs: {search_result[:200]}"
        # The result must have reached the provider (mock saw it as a
        # `role: "tool"` follow-up message), not just landed in history.
        assert Handler.calls >= 2, f"provider was called {Handler.calls} time(s)"
        assert Handler.saw_web_search_tool, "web_search was never offered to the model"
        assert Handler.saw_search_result, f"provider never received the SearXNG result (calls={Handler.calls})"

        print(f"PASS: web_search via SearXNG returned {len(search_result)} chars")
        print(f"  first 200 chars: {search_result[:200]}")
        print(f"  artifact: {root}")

    finally:
        proc.kill()
        proc.wait(timeout=5)
        mock.shutdown()

    sys.exit(0)


if __name__ == "__main__":
    main()
