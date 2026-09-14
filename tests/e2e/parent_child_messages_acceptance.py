#!/usr/bin/env python3
"""Isolated real --serve parent/child send_message acceptance (stdlib only)."""
import hashlib
import json
import os
from pathlib import Path
import re
import socket
import subprocess
import sys
import tempfile
import threading
import time
import traceback
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = Path(__file__).resolve().parents[2]
NEW = (HERE / ".e-agent/message-acceptance/e-agent-new-dd1cc4d").resolve()
OLD = (HERE / ".e-agent/message-acceptance/e-agent-old-bca5941").resolve()
ARTIFACTS = HERE / ".e-agent/message-acceptance/runs"
ARTIFACTS.mkdir(parents=True, exist_ok=True)
PARENT = "acceptance-parent"
P2C = "P2C-BEGIN\nFull parent message: café 中文 \"quoted\".\nP2C-END"
C2P = "C2P-BEGIN\nFull child message: café 中文 \"quoted\".\nC2P-END"


def port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


ROOT = Path(tempfile.mkdtemp(prefix="parent-child-messages-", dir=ARTIFACTS))
WORK = ROOT / "workspace"
for d in (WORK, ROOT / "home", ROOT / "config/e-agent", ROOT / "state/e-agent"):
    d.mkdir(parents=True, exist_ok=True)

requests, errors = [], []
lock = threading.Lock()
child_entered = threading.Event()
release_child = threading.Event()
parent_queued = threading.Event()
child_id = None
counts = {"parent": 0, "child": 0}


def dump(name, value):
    (ROOT / name).write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")


def record_requests():
    with lock:
        dump("provider-requests.json", requests)


def delta_tool(name, arguments, call_id):
    return {"tool_calls": [{"index": 0, "id": call_id, "type": "function",
             "function": {"name": name, "arguments": json.dumps(arguments, ensure_ascii=False)}}]}


def content_list(request):
    return [m["content"] for m in request.get("messages", []) if isinstance(m.get("content"), str)]


def notice(request, sender, body):
    return f"[agent message from {sender}]\n{body}" in "\n".join(content_list(request))


class Mock(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        global child_id
        try:
            if self.path.rstrip("/") != "/v1/chat/completions":
                raise AssertionError("unexpected provider path: " + self.path)
            raw = self.rfile.read(int(self.headers["Content-Length"]))
            request = json.loads(raw)
            tools = request.get("tools", [])
            names = {x.get("function", {}).get("name") for x in tools}
            text = "\n".join(content_list(request))
            latest = content_list(request)[-1] if content_list(request) else ""
            # Parent owns delegate; child has only child builtin tools.
            role = "parent" if "delegate" in names else "child" if "send_message" in names else "other"
            with lock:
                requests.append({"role": role, "request": request})
                counts[role] = counts.get(role, 0) + 1
                call = counts[role]
            record_requests()

            # Explicit post-restart prompts have priority over old context.
            if latest == "OLD_APPEND_ACCEPTANCE":
                response = {"content": "OLD_APPEND_RECORDED"}
            elif latest == "NEW_APPEND_ACCEPTANCE":
                response = {"content": "NEW_APPEND_RECORDED"}
            elif role == "parent" and call == 1:
                send = next(x["function"] for x in tools if x["function"]["name"] == "send_message")
                assert set(send["parameters"]["required"]) == {"target", "message"}
                assert send["parameters"]["additionalProperties"] is False
                response = delta_tool("delegate", {"workspace": str(WORK), "label": "acceptance-child",
                    "task": "CHILD_ACCEPTANCE_TASK: wait for and answer the parent message."}, "delegate-1")
            elif role == "parent" and call == 2:
                assert child_entered.wait(30), "child provider call did not reach barrier"
                match = re.search(r"subagent session:\s*(sub-[A-Za-z0-9_-]+)", text)
                assert match, "delegate receipt did not provide child session id"
                child_id = match.group(1)
                response = delta_tool("send_message", {"target": child_id, "message": P2C}, "parent-send-1")
            elif role == "parent" and call == 3:
                receipt = [m for m in request["messages"] if m.get("role") == "tool" and m.get("tool_call_id") == "parent-send-1"]
                assert len(receipt) == 1 and receipt[0].get("content") == "message queued", "parent receipt was not exact message queued"
                parent_queued.set()
                response = {"content": "PARENT_FINISHED_INITIAL_RESPONSE"}
            elif role == "child" and call == 1:
                child_entered.set()
                assert release_child.wait(45), "child release barrier timed out"
                response = {"content": "CHILD_INITIAL_RESPONSE"}
            elif role == "child" and call == 2:
                assert notice(request, PARENT, P2C), "child next request lacks attributed complete parent Notice"
                response = delta_tool("send_message", {"target": "parent", "message": C2P}, "child-send-1")
            elif role == "child" and call == 3:
                receipt = [m for m in request["messages"] if m.get("role") == "tool" and m.get("tool_call_id") == "child-send-1"]
                assert len(receipt) == 1 and receipt[0].get("content") == "message queued", "child receipt was not exact message queued"
                response = {"content": "CHILD_REACTED_TO_PARENT_MESSAGE"}
            elif role == "parent":
                assert child_id and notice(request, child_id, C2P), "parent reaction request lacks attributed complete child Notice"
                response = {"content": "PARENT_REACTED_TO_CHILD_MESSAGE"}
            else:
                response = {"content": "UNEXPECTED_AUXILIARY_RESPONSE"}

            finish = "tool_calls" if "tool_calls" in response else "stop"
            def chunk(delta, end):
                return {"id": "acceptance", "object": "chat.completion.chunk", "model": request.get("model", "mock"),
                        "choices": [{"index": 0, "delta": delta, "finish_reason": end}]}
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Connection", "close")
            self.end_headers()
            for item in (chunk(response, None), chunk({}, finish)):
                self.wfile.write(("data: " + json.dumps(item, ensure_ascii=False) + "\n\n").encode())
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        except Exception as exc:
            with lock:
                errors.append(traceback.format_exc())
            try:
                self.send_error(500, repr(exc))
            except Exception:
                pass


def sha(path):
    h = hashlib.sha256()
    with path.open("rb") as f:
        for block in iter(lambda: f.read(1024 * 1024), b""):
            h.update(block)
    return h.hexdigest()


mock = ThreadingHTTPServer(("127.0.0.1", 0), Mock)
threading.Thread(target=mock.serve_forever, daemon=True).start()
MOCK_PORT = mock.server_address[1]
(ROOT / "config/e-agent/config.toml").write_text(f'''default = "mock/acceptance"
[providers.mock]
base_url = "http://127.0.0.1:{MOCK_PORT}/v1"
api_key_env = "ACCEPTANCE_MOCK_KEY"
[models."mock/acceptance"]
model = "acceptance"
[session]
backend = "jsonl"
''', encoding="utf-8")
env = {"PATH": "/usr/bin:/bin", "HOME": str(ROOT / "home"), "XDG_CONFIG_HOME": str(ROOT / "config"),
       "XDG_STATE_HOME": str(ROOT / "state"), "XDG_CACHE_HOME": str(ROOT / "cache"),
       "ACCEPTANCE_MOCK_KEY": "isolated-dummy-not-a-credential", "NO_PROXY": "127.0.0.1,localhost"}
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
proc = None
token = None
api_port = None


def start(binary, label):
    global proc, token, api_port
    api_port = port()
    command = [str(binary), "--serve", "--host", "127.0.0.1", "--port", str(api_port), "--workspace", str(WORK)]
    log = (ROOT / f"server-{label}.log").open("wb")
    proc = subprocess.Popen(command, cwd=WORK, env=env, stdout=log, stderr=subprocess.STDOUT)
    dump(f"run-{label}.json", {"command": command, "binary_sha256": sha(binary), "root": str(ROOT)})
    return log


def stop(log):
    global proc
    if proc:
        proc.terminate()
        try: proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill(); proc.wait(timeout=10)
    proc = None
    log.close()


def api(method, path, body=None, status=200):
    data = json.dumps(body).encode() if body is not None else None
    request = urllib.request.Request(f"http://127.0.0.1:{api_port}{path}", data=data, method=method,
        headers={"Authorization": "Bearer " + token, "Content-Type": "application/json"})
    try:
        with opener.open(request, timeout=8) as response:
            assert response.status == status, (response.status, path)
            raw = response.read()
            return json.loads(raw) if raw else None
    except urllib.error.HTTPError as exc:
        actual = exc.read().decode(errors="replace")
        if exc.code != status: raise AssertionError(f"{method} {path}: {exc.code}, expected {status}: {actual}")
        return None


def until(fn, label, timeout=50):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if errors: raise AssertionError(errors[0])
        if proc.poll() is not None: raise AssertionError(f"server exited during {label}")
        try:
            value = fn()
            if value: return value
        except (OSError, urllib.error.URLError, json.JSONDecodeError):
            pass
        time.sleep(.1)
    raise AssertionError("timeout: " + label)


def history(sid):
    return api("GET", f"/api/sessions/{sid}/history")["entries"]


def assert_transcript(entries, source, received, sent, call_id):
    # SessionEntry and Message use serde's externally-tagged JSON form:
    # {"message":{"Assistant":{"tool_calls":[{"name","arguments"}]}}}.
    wanted = f"[agent message from {source}]\n{received}"
    assert sum(e.get("type") == "notice" and e.get("text") == wanted for e in entries) == 1, "Notice missing, duplicate, or body changed"
    for e in entries:
        user = e.get("message", {}).get("User")
        if user: assert received not in user.get("content", ""), "received message became human User"
    calls = [c for e in entries for c in e.get("message", {}).get("Assistant", {}).get("tool_calls", []) if c.get("id") == call_id]
    assert len(calls) == 1 and json.loads(calls[0]["arguments"])["message"] == sent, "full sender tool arguments missing"


result = {"verdict": "fail", "artifact_root": str(ROOT), "binary_sha256": {"new": sha(NEW), "old": sha(OLD)}}
print("ARTIFACT_ROOT=" + str(ROOT), flush=True)
try:
    assert NEW.is_file() and os.access(NEW, os.X_OK), "required exact new binary unavailable"
    assert OLD.is_file() and os.access(OLD, os.X_OK), "required exact old binary unavailable"
    log = start(NEW, "new-basic")
    token_path = ROOT / "state/e-agent/server.token"
    until(lambda: token_path.exists() and token_path.stat().st_size, "server token")
    token = token_path.read_text().strip()
    until(lambda: api("GET", "/api/sessions") is not None, "server API")
    api("POST", "/api/sessions", {"id": PARENT, "initial_prompt": "START_PARENT_CHILD_ACCEPTANCE"}, 201)
    until(lambda: parent_queued.is_set(), "parent exact queued receipt")
    assert child_id, "child id absent"
    held = history(child_id)
    dump("child-while-provider-held.json", held)
    release_child.set()
    def both_done():
        p, c = history(PARENT), history(child_id)
        return (p, c) if "PARENT_REACTED_TO_CHILD_MESSAGE" in json.dumps(p, ensure_ascii=False) and "CHILD_REACTED_TO_PARENT_MESSAGE" in json.dumps(c, ensure_ascii=False) else None
    parent_history, child_history = until(both_done, "bidirectional reactions")
    dump("parent-history-basic.json", parent_history); dump("child-history-basic.json", child_history)
    assert_transcript(parent_history, child_id, C2P, P2C, "parent-send-1")
    assert_transcript(child_history, PARENT, P2C, C2P, "child-send-1")
    assert any(r["role"] == "parent" and notice(r["request"], child_id, C2P) for r in requests), "parent provider context never consumed child Notice"
    assert any(r["role"] == "child" and notice(r["request"], PARENT, P2C) for r in requests), "child provider context never consumed parent Notice"
    for sid, expected in ((PARENT, parent_history), (child_id, child_history)):
        raw = [json.loads(x) for x in (WORK / ".e-agent/sessions" / f"{sid}.jsonl").read_text(encoding="utf-8").splitlines() if x]
        assert raw == expected, f"raw JSONL/API history mismatch for {sid}"
    basic_requests = len(requests)
    stop(log)

    # Historical loads are passive.  Resume explicitly with a prompt only after proving no request occurred.
    log = start(OLD, "old-resume")
    until(lambda: api("GET", f"/api/sessions/{PARENT}/history") is not None, "old historical history")
    old_parent_before, old_child_before = history(PARENT), history(child_id)
    assert old_parent_before == parent_history and old_child_before == child_history, "old changed original records/order on passive load"
    time.sleep(.4)
    assert len(requests) == basic_requests, "historical replay made unsolicited provider reaction"
    api("POST", "/api/sessions", {"id": PARENT, "initial_prompt": "OLD_APPEND_ACCEPTANCE"}, 201)
    def old_appended():
        h = history(PARENT)
        return h if "OLD_APPEND_RECORDED" in json.dumps(h) else None
    old_parent_after = until(old_appended, "old append")
    assert old_parent_after[:len(parent_history)] == parent_history, "old append did not preserve original parent order"
    dump("parent-history-old.json", old_parent_after)
    stop(log)

    log = start(NEW, "new-after-old")
    until(lambda: api("GET", f"/api/sessions/{PARENT}/history") is not None, "new historical history")
    before_new = history(PARENT)
    assert before_new == old_parent_after, "new altered old-written records/order on passive load"
    n_before = len(requests)
    time.sleep(.4)
    assert len(requests) == n_before, "new historical replay made unsolicited provider reaction"
    api("POST", "/api/sessions", {"id": PARENT, "initial_prompt": "NEW_APPEND_ACCEPTANCE"}, 201)
    def new_appended():
        h = history(PARENT)
        return h if "NEW_APPEND_RECORDED" in json.dumps(h) else None
    final_parent = until(new_appended, "new append")
    assert final_parent[:len(old_parent_after)] == old_parent_after, "new append did not preserve new-old records/order"
    dump("parent-history-final.json", final_parent)
    result.update({"verdict": "pass", "established": ["real --serve API/factory/delegate", "busy child queued bidirectional full multiline Unicode messages", "exact message queued receipts", "attributed Notice provider context and API/JSONL persistence", "new→old→new passive replay and append preserve API record order"], "uncovered": ["unknown/finished/sibling target rejection", "resumed child current-parent endpoint", "/btw", "SSE event classification", "SQLite/Greptime/browser"]})
    print("PASS: parent/child messaging and new-old-new JSONL acceptance", flush=True)
except Exception as exc:
    result["error"] = repr(exc)
    result["traceback"] = traceback.format_exc()
    print("FAIL: " + repr(exc), file=sys.stderr, flush=True)
    raise
finally:
    dump("result.json", result)
    release_child.set()
    if proc: stop(log)
    mock.shutdown(); mock.server_close()
