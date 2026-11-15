#!/usr/bin/env python3
"""Real-server acceptance test for byte-identical delegate fork prefixes."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

REPO_ROOT = Path(__file__).resolve().parents[2]
BINARY = Path(os.environ.get("EAGENT_BIN", REPO_ROOT / "target/debug/e-agent")).resolve()


class SetupFailure(Exception):
    pass


def port():
    import socket
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def sse(response, model):
    chunk = {"id": "fork-acceptance", "object": "chat.completion.chunk", "model": model,
             "choices": [{"index": 0, "delta": response, "finish_reason": None}]}
    end = {"id": "fork-acceptance", "object": "chat.completion.chunk", "model": model,
           "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls" if "tool_calls" in response else "stop"}]}
    return b"".join(("data: " + json.dumps(item, separators=(",", ":")) + "\n\n").encode()
                   for item in (chunk, end)) + b"data: [DONE]\n\n"


def last_message_text(body):
    """Role and text of the final wire message; content may be a string or parts."""
    messages = body.get("messages") or []
    if not messages:
        return None, ""
    message = messages[-1]
    content = message.get("content")
    if isinstance(content, str):
        return message.get("role"), content
    if isinstance(content, list):
        parts = [part.get("text", "") for part in content
                 if isinstance(part, dict) and isinstance(part.get("text"), str)]
        return message.get("role"), "\n".join(parts)
    return message.get("role"), ""


def is_fork_child_request(body):
    """The fork child's OWN first request: its prompt starts with the task text.

    The parent keeps turning after the child's completion arrives, and the
    injected notice carries the task label, so a substring match would count
    the parent's continuation as a second child request.
    """
    role, text = last_message_text(body)
    return role == "user" and text.startswith("FORK_CHILD_TASK")


def has_fork_call(body):
    for message in body.get("messages") or []:
        for call in message.get("tool_calls") or []:
            function = call.get("function") or {}
            if function.get("name") == "delegate" and '"fork"' in (function.get("arguments") or ""):
                return True
    return False


def main():
    if not BINARY.is_file() or not os.access(BINARY, os.X_OK):
        print(f"SETUP ERROR: executable e-agent binary not found: {BINARY}; build it first", file=sys.stderr)
        return 2

    root = Path(tempfile.mkdtemp(prefix="delegate-fork-prefix-"))
    workspace = root / "workspace"
    workspace.mkdir()
    config_home = root / "config"
    (config_home / "e-agent").mkdir(parents=True)
    for name in ("home", "state", "cache"):
        (root / name).mkdir()
    mock_port = port()
    api_port = port()
    requests = []
    mock_errors = []
    lock = threading.Lock()

    class Mock(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_POST(self):
            try:
                if self.path != "/v1/chat/completions":
                    raise AssertionError(f"unexpected provider path {self.path!r}")
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                with lock:
                    requests.append(body)
                    ordinal = len(requests)
                    (root / "provider-requests.json").write_text(json.dumps(requests, indent=2), encoding="utf-8")
                # Decide by content, not arrival order: the parent's
                # post-tool-result request can race the child's first request.
                role, text = last_message_text(body)
                if role == "user" and "FORK_CHILD_TASK" in text:
                    response = {"content": "CHILD_DONE"}
                elif text == "SECOND_TURN":
                    response = {"tool_calls": [{"index": 0, "id": "fork-prefix-call-unique", "type": "function",
                                 "function": {"name": "delegate", "arguments": json.dumps({
                                     "workspace": str(workspace), "task": "FORK_CHILD_TASK", "fork": True})}}]}
                elif text == "FIRST_TURN":
                    response = {"content": "FIRST_TURN_DONE"}
                else:
                    response = {"content": "PARENT_CONTINUED"}
                payload = sse(response, body.get("model", "mock"))
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Connection", "close")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)
                self.wfile.flush()
            except Exception as exc:
                with lock:
                    mock_errors.append(repr(exc))
                try:
                    self.send_error(500, str(exc))
                except OSError:
                    pass

    mock = ThreadingHTTPServer(("127.0.0.1", mock_port), Mock)
    mock_thread = threading.Thread(target=mock.serve_forever, daemon=True)
    proc = None
    passed = False
    try:
        mock_thread.start()
        (config_home / "e-agent/config.toml").write_text(
            f'default = "mock/fork-e2e"\n'
            f'[providers.mock]\nbase_url = "http://127.0.0.1:{mock_port}/v1"\n'
            'api_key_env = "FORK_E2E_KEY"\n'
            '[models."mock/fork-e2e"]\nmodel = "fork-e2e-model"\n'
            '[session]\nbackend = "jsonl"\n', encoding="utf-8")
        env = os.environ.copy()
        env.update({"HOME": str(root / "home"), "XDG_CONFIG_HOME": str(config_home),
                    "XDG_STATE_HOME": str(root / "state"), "XDG_CACHE_HOME": str(root / "cache"),
                    "FORK_E2E_KEY": "isolated-dummy-value", "NO_PROXY": "127.0.0.1,localhost",
                    "no_proxy": "127.0.0.1,localhost"})
        log = (root / "server.log").open("wb")
        proc = subprocess.Popen([str(BINARY), "--serve", "--host", "127.0.0.1", "--port", str(api_port),
                                 "--workspace", str(workspace)], cwd=workspace, env=env, stdout=log, stderr=subprocess.STDOUT)
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        token_path = root / "state/e-agent/server.token"
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline and not (token_path.exists() and token_path.stat().st_size):
            if proc.poll() is not None:
                raise SetupFailure(f"server exited during startup; see {root / 'server.log'}")
            time.sleep(.05)
        if not token_path.exists():
            raise SetupFailure(f"server token not created; see {root / 'server.log'}")
        token = token_path.read_text(encoding="utf-8").strip()
        base = f"http://127.0.0.1:{api_port}"

        def api(method, path, data=None):
            request = urllib.request.Request(base + path,
                data=json.dumps(data).encode() if data is not None else None, method=method,
                headers={"Authorization": "Bearer " + token, "Content-Type": "application/json"})
            with opener.open(request, timeout=10) as response:
                raw = response.read()
                return json.loads(raw) if raw else None

        def wait_for(predicate, label, timeout=40):
            end = time.monotonic() + timeout
            while time.monotonic() < end:
                if mock_errors:
                    raise AssertionError("mock provider failed: " + mock_errors[0])
                if proc.poll() is not None:
                    raise AssertionError(f"server exited while waiting for {label}")
                with lock:
                    if predicate():
                        return
                time.sleep(.05)
            raise AssertionError(f"timeout waiting for {label}; observed {len(requests)} provider requests")

        # Allow the HTTP server to finish initialization before creating sessions.
        for _ in range(100):
            try:
                request = urllib.request.Request(base + "/api/sessions", headers={"Authorization": "Bearer " + token})
                with opener.open(request, timeout=1):
                    break
            except (OSError, urllib.error.URLError):
                if proc.poll() is not None:
                    raise SetupFailure(f"server exited before API readiness; see {root / 'server.log'}")
                time.sleep(.05)
        api("POST", "/api/sessions", {"id": "fork-prefix-parent", "initial_prompt": "FIRST_TURN"})
        wait_for(lambda: len(requests) >= 1, "parent first provider request")
        def first_turn_done():
            history = api("GET", "/api/sessions/fork-prefix-parent/history")
            return "FIRST_TURN_DONE" in json.dumps(history)
        wait_for(first_turn_done, "completed first parent turn")
        # Goal-bearing case: the parent commits a live goal AFTER its turn
        # boundary, so the fork's cut prefix drops that update and only the
        # frozen parent goal can reproduce the identical system message.
        api("POST", "/api/sessions/fork-prefix-parent/goal", {
            "action": "set",
            "objective": "keep the fork prefix identical",
            "success_criteria": ["goal system message matches"],
        })
        def goal_committed():
            goal = api("GET", "/api/sessions/fork-prefix-parent/goal").get("goal")
            return isinstance(goal, dict) and goal.get("objective") == "keep the fork prefix identical"
        wait_for(goal_committed, "committed parent goal")
        api("POST", "/api/sessions/fork-prefix-parent/prompt", {"prompt": "SECOND_TURN"})
        def child_request_seen():
            # wait_for already holds `lock` around the predicate; taking it
            # again here would self-deadlock on the non-reentrant Lock.
            return any(is_fork_child_request(request) for request in requests)
        wait_for(child_request_seen, "fork child provider request")
        # The child can finish its response before the next poll; allow a brief observation window
        # to catch an accidental second child request before fixing the snapshot for assertions.
        time.sleep(.4)
        with lock:
            observed = list(requests)
        if mock_errors:
            raise AssertionError("mock provider failed: " + mock_errors[0])
        # The parent keeps turning after the child's completion notice lands,
        # so the request count is not fixed. Identify the two requests by
        # content instead: parent and child now share an identical tool list,
        # which is exactly what this test exists to prove.
        def is_user_text(request, wanted):
            role, text = last_message_text(request)
            return role == "user" and text == wanted

        matching_fork_call = [request for request in observed if is_user_text(request, "SECOND_TURN")]
        if len(matching_fork_call) != 1:
            raise AssertionError(f"4a: expected one parent request ending in the in-flight turn, got {len(matching_fork_call)}")
        request2 = matching_fork_call[0]
        if has_fork_call(request2):
            raise AssertionError("4a: the in-flight parent request already contains the fork call it is about to produce")
        child_requests = [request for request in observed if is_fork_child_request(request)]
        if len(child_requests) != 1:
            raise AssertionError(f"4a: expected one fork child request, got {len(child_requests)} of {len(observed)} provider requests")
        request3 = child_requests[0]
        def goal_messages(request):
            return [message for message in request.get("messages") or []
                    if message.get("role") == "system"
                    and "[session goal]" in str(message.get("content") or "")]
        parent_goals = goal_messages(request2)
        if len(parent_goals) != 1 or "keep the fork prefix identical" not in str(parent_goals[0].get("content")):
            raise AssertionError(f"goal case: parent request lacks its committed goal message: {parent_goals!r}")
        if len(goal_messages(request3)) != 1:
            raise AssertionError("goal case: the fork child request is missing the goal system message")
        request1 = next((request for request in observed if is_user_text(request, "FIRST_TURN")), None)
        if request1 is None:
            raise AssertionError("4a: request with the first parent turn not found")
        if not any(tool.get("function", {}).get("name") == "delegate" for tool in request2.get("tools", [])):
            raise AssertionError("4a: the in-flight parent request lost the delegate tool")
        delegate_tool = next(tool for tool in request2["tools"] if tool.get("function", {}).get("name") == "delegate")
        if "fork" not in delegate_tool["function"].get("parameters", {}).get("properties", {}):
            raise AssertionError("4a: the parent delegate tool lacks parameters.properties.fork")

        def compare_tools(left, right):
            a, b = left.get("tools", []), right.get("tools", [])
            for i in range(max(len(a), len(b))):
                av = a[i] if i < len(a) else "<missing>"
                bv = b[i] if i < len(b) else "<missing>"
                if av != bv:
                    return f"tool index {i}: request2={json.dumps(av, ensure_ascii=False, indent=2)}\nrequest3={json.dumps(bv, ensure_ascii=False, indent=2)}"
            return None

        mismatch = compare_tools(request2, request3)
        if mismatch:
            raise AssertionError("4b: tool lists differ:\n" + mismatch)
        if request2.get("model") != request3.get("model"):
            raise AssertionError(f"4c: model differs: request2={request2.get('model')!r}, request3={request3.get('model')!r}")
        n = len(request2.get("messages", [])) - 1
        if n < 0:
            raise AssertionError("4d: request 2 has no messages to exclude the in-flight user turn")
        prefix2, prefix3 = request2["messages"][:n], request3.get("messages", [])[:n]
        for i in range(max(len(prefix2), len(prefix3))):
            a = prefix2[i] if i < len(prefix2) else "<missing>"
            b = prefix3[i] if i < len(prefix3) else "<missing>"
            if a != b:
                raise AssertionError(f"4d: message prefix differs at index {i}:\nrequest2={json.dumps(a, ensure_ascii=False, indent=2)}\nrequest3={json.dumps(b, ensure_ascii=False, indent=2)}")
        mutated = dict(request2)
        mutated["tools"] = list(reversed(request2.get("tools", [])))
        if not compare_tools(request2, mutated):
            raise AssertionError("4e: negative control failed; reversed tool list was reported equal")
        # Repeat each comparison on the immutable captured requests to expose accidental
        # stateful/nondeterministic comparison behavior.
        if compare_tools(request2, request3) is not None:
            raise AssertionError("4b determinism: repeated tool comparison changed")
        if request2.get("model") != request3.get("model") or request2["messages"][:n] != request3.get("messages", [])[:n]:
            raise AssertionError("4c/4d determinism: repeated prefix comparison changed")
        passed = True
        print(f"PASS: delegate fork provider request prefix is identical; artifacts: {root}")
        return 0
    except SetupFailure as exc:
        print(f"FAIL: SETUP: {exc}; artifacts: {root}", file=sys.stderr)
        return 2
    except Exception as exc:
        print(f"FAIL: {exc}; artifacts: {root}", file=sys.stderr)
        return 1
    finally:
        if proc is not None:
            proc.terminate()
            try:
                proc.wait(timeout=8)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=8)
        mock.shutdown()
        mock.server_close()
        if not passed:
            print(f"Artifacts retained: {root}", file=sys.stderr)


if __name__ == "__main__":
    sys.exit(main())
