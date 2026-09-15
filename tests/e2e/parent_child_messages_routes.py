#!/usr/bin/env python3
"""Focused JSONL-route acceptance for live parent/child messaging and /btw.

Uses the exact candidate binary with an isolated --serve process and an actual
OpenAI-compatible streaming provider.  This deliberately covers the bounded
route/SSE gaps only; it is not a Greptime acceptance run.
"""
import hashlib
import json
import os
from pathlib import Path
import queue
import socket
import subprocess
import tempfile
import threading
import time
import traceback
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = Path(__file__).resolve().parents[2]
BINARY = (HERE / ".e-agent/message-acceptance/e-agent-new-dd1cc4d").resolve()
GREPTIME = (HERE / ".e-agent/message-acceptance/bin/greptime").resolve()
BACKEND = os.environ.get("PARENT_CHILD_BACKEND", "jsonl")
assert BACKEND in {"jsonl", "greptime"}, "PARENT_CHILD_BACKEND must be jsonl or greptime"
ARTIFACTS = HERE / ".e-agent/message-acceptance/runs"
ARTIFACTS.mkdir(parents=True, exist_ok=True)
ROOT = Path(tempfile.mkdtemp(prefix="parent-child-routes-", dir=ARTIFACTS))
WORK = ROOT / "workspace"
for directory in (WORK, ROOT / "home", ROOT / "config/e-agent", ROOT / "state/e-agent", ROOT / "greptime-data", ROOT / "greptime-log"):
    directory.mkdir(parents=True, exist_ok=True)

MAIN_A, MAIN_B = "routes-parent-a", "routes-parent-b"
UNKNOWN = "no-such-live-child"
P2BTW = "P2BTW-BEGIN\nparent queued while btw provider busy\nP2BTW-END"
BTW2P = "BTW2P-BEGIN\nbtw reply to parent\nBTW2P-END"


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


GT_HTTP, GT_GRPC, GT_MYSQL, GT_PG = (free_port() for _ in range(4))
GREPTIME_CONN = f"host=127.0.0.1 port={GT_PG} user=postgres dbname=public"


def sha(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def dump(name, value):
    (ROOT / name).write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")


state_lock = threading.Lock()
provider_requests, provider_errors = [], []
ids = {"a_own": None, "a_finished": None, "b_sibling": None, "btw": None}
reject_stage = 0
btw_parent_sent = False
btw_entered, release_btw = threading.Event(), threading.Event()


def tool(name, arguments, call_id):
    return {"tool_calls": [{"index": 0, "id": call_id, "type": "function",
                              "function": {"name": name, "arguments": json.dumps(arguments)}}]}


def strings(request):
    return [item.get("content", "") for item in request.get("messages", []) if isinstance(item.get("content"), str)]


def tool_result(request, call_id):
    return [item.get("content") for item in request.get("messages", [])
            if item.get("role") == "tool" and item.get("tool_call_id") == call_id]


def error_receipt(request, call_id):
    values = tool_result(request, call_id)
    assert len(values) == 1 and values[0] == "ERROR: target is not a live child session", (call_id, values)


def delegate_id(request, call_id):
    values = tool_result(request, call_id)
    assert len(values) == 1, (call_id, values)
    match = next((line for line in values[0].splitlines() if line.startswith("subagent session: ")), None)
    assert match, (call_id, values)
    return match.split(": ", 1)[1]


def has_notice(request, source, message):
    return f"[agent message from {source}]\n{message}" in "\n".join(strings(request))


class Provider(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        global reject_stage, btw_parent_sent
        try:
            assert self.path.rstrip("/") == "/v1/chat/completions", self.path
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            names = {item.get("function", {}).get("name") for item in request.get("tools", [])}
            latest = strings(request)[-1] if strings(request) else ""
            role = "main" if "delegate" in names else "child" if "send_message" in names else "other"
            with state_lock:
                provider_requests.append({"role": role, "request": request})
                dump("provider-requests-live.json", provider_requests)

            # The summary listener calls the same configured provider without tools.
            if role == "other":
                response = {"content": "SUMMARY"}
            # These explicit route probes must outrank retained delegate receipts.
            elif role == "main" and latest == "REJECT_ROUTES":
                response = tool("send_message", {"target": UNKNOWN, "message": "unknown"}, "reject-unknown")
            elif role == "main" and reject_stage == 0 and tool_result(request, "reject-unknown"):
                error_receipt(request, "reject-unknown")
                reject_stage = 1
                with state_lock:
                    sibling = ids["b_sibling"]
                assert sibling
                response = tool("send_message", {"target": sibling, "message": "sibling"}, "reject-sibling")
            elif role == "main" and reject_stage == 1 and tool_result(request, "reject-sibling"):
                error_receipt(request, "reject-sibling")
                reject_stage = 2
                with state_lock:
                    finished = ids["a_finished"]
                assert finished
                response = tool("send_message", {"target": finished, "message": "finished"}, "reject-finished")
            elif role == "main" and reject_stage == 2 and tool_result(request, "reject-finished"):
                error_receipt(request, "reject-finished")
                reject_stage = 3
                response = {"content": "REJECTIONS_RECORDED"}
            elif role == "main" and latest == "SEND_TO_BTW":
                with state_lock:
                    btw = ids["btw"]
                assert btw
                response = tool("send_message", {"target": btw, "message": P2BTW}, "parent-send-btw")
            elif role == "main" and has_notice(request, ids.get("btw"), BTW2P):
                btw_parent_sent = True
                response = {"content": "PARENT_RECEIVED_BTW"}
            elif role == "main" and tool_result(request, "parent-send-btw"):
                assert tool_result(request, "parent-send-btw") == ["message queued"]
                response = {"content": "PARENT_SENT_TO_BUSY_BTW"}
            # Main B supplies a real sibling child registered under another parent.
            elif role == "main" and latest == "START_B":
                response = tool("delegate", {"workspace": str(WORK), "label": "sibling",
                                               "task": "SIBLING_HOLD"}, "b-delegate")
            elif role == "main" and tool_result(request, "b-delegate"):
                with state_lock:
                    ids["b_sibling"] = delegate_id(request, "b-delegate")
                response = {"content": "B_READY"}
            elif role == "child" and latest == "SIBLING_HOLD":
                # The session remains a live sibling through all A rejections.
                release_btw.wait(45)
                response = {"content": "SIBLING_DONE"}

            # Main A creates an own live child and a child that promptly finishes.
            elif role == "main" and latest == "START_A":
                response = tool("delegate", {"workspace": str(WORK), "label": "own",
                                               "task": "OWN_HOLD"}, "a-own-delegate")
            elif role == "main" and tool_result(request, "a-finished-delegate"):
                with state_lock:
                    ids["a_finished"] = delegate_id(request, "a-finished-delegate")
                response = {"content": "A_READY"}
            elif role == "main" and tool_result(request, "a-own-delegate"):
                with state_lock:
                    ids["a_own"] = delegate_id(request, "a-own-delegate")
                response = tool("delegate", {"workspace": str(WORK), "label": "finished",
                                               "task": "FINISHED_NOW"}, "a-finished-delegate")
            elif role == "child" and latest == "OWN_HOLD":
                release_btw.wait(45)
                response = {"content": "OWN_DONE"}
            elif role == "child" and latest == "FINISHED_NOW":
                response = {"content": "FINISHED_DONE"}

            # A /btw fork is a child endpoint. Hold its first call so A sends while it is Busy.
            elif role == "child" and latest == "BTW_START":
                btw_entered.set()
                assert release_btw.wait(45), "btw release timed out"
                response = tool("send_message", {"target": "parent", "message": BTW2P}, "btw-send-parent")
            elif role == "child" and has_notice(request, MAIN_A, P2BTW):
                # The direct Notice can arrive in the same next request that
                # retains the send receipt; it is the delivery acceptance.
                assert tool_result(request, "btw-send-parent") == ["message queued"]
                response = {"content": "BTW_RECEIVED_PARENT"}
            else:
                raise AssertionError(f"unexpected provider request role={role} latest={latest!r}")

            finish = "tool_calls" if "tool_calls" in response else "stop"
            def frame(delta, reason):
                return {"id": "routes", "object": "chat.completion.chunk", "model": "routes",
                        "choices": [{"index": 0, "delta": delta, "finish_reason": reason}]}
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Connection", "close")
            self.end_headers()
            for item in (frame(response, None), frame({}, finish)):
                self.wfile.write(("data: " + json.dumps(item) + "\n\n").encode())
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        except Exception:
            with state_lock:
                provider_errors.append(traceback.format_exc())
            self.send_error(500, "provider assertion failed")


class SseTap:
    """Actual authenticated /events connection; captures parsed SSE frames."""
    def __init__(self, url, token):
        self.frames, self.errors = [], []
        self.ready = threading.Event()
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._run, args=(url, token), daemon=True)
        self._thread.start()

    def _run(self, url, token):
        try:
            request = urllib.request.Request(url, headers={"Authorization": "Bearer " + token})
            with OPENER.open(request, timeout=50) as response:
                self.ready.set()
                event, data = None, []
                while not self._stop.is_set():
                    line = response.readline().decode("utf-8").rstrip("\r\n")
                    if not line:
                        if event is not None:
                            try:
                                payload = json.loads("\n".join(data))
                            except json.JSONDecodeError:
                                payload = "\n".join(data)
                            self.frames.append((event, payload))
                            dump("parent-sse-live.json" if "/routes-parent-a/events" in url else "btw-sse-live.json", self.frames)
                        event, data = None, []
                    elif line.startswith("event: "):
                        event = line[7:]
                    elif line.startswith("data: "):
                        data.append(line[6:])
        except Exception as exc:
            if not self._stop.is_set():
                self.errors.append(repr(exc))

    def close(self):
        self._stop.set()


mock = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
threading.Thread(target=mock.serve_forever, daemon=True).start()
MOCK_PORT = mock.server_address[1]
backend_config = 'backend = "jsonl"' if BACKEND == "jsonl" else f'''backend = "greptime"
conn = "{GREPTIME_CONN}"'''
(ROOT / "config/e-agent/config.toml").write_text(f'''default = "mock/routes"
[providers.mock]
base_url = "http://127.0.0.1:{MOCK_PORT}/v1"
api_key_env = "ROUTES_MOCK_KEY"
[models."mock/routes"]
model = "routes"
[session]
{backend_config}
''', encoding="utf-8")
env = {"PATH": "/usr/bin:/bin", "HOME": str(ROOT / "home"), "XDG_CONFIG_HOME": str(ROOT / "config"),
       "XDG_STATE_HOME": str(ROOT / "state"), "XDG_CACHE_HOME": str(ROOT / "cache"),
       "ROUTES_MOCK_KEY": "isolated-dummy-not-a-credential", "NO_PROXY": "127.0.0.1,localhost"}
OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))
proc = None
greptime_proc = None
token = None
api_port = free_port()


def start_greptime():
    global greptime_proc
    if BACKEND != "greptime": return
    command = [str(GREPTIME), "standalone", "start", "--data-home", str(ROOT / "greptime-data"),
               "--http-addr", f"127.0.0.1:{GT_HTTP}", "--grpc-bind-addr", f"127.0.0.1:{GT_GRPC}",
               "--mysql-addr", f"127.0.0.1:{GT_MYSQL}", "--postgres-addr", f"127.0.0.1:{GT_PG}",
               "--log-dir", str(ROOT / "greptime-log")]
    log = (ROOT / "greptime.log").open("wb")
    greptime_proc = subprocess.Popen(command, cwd=WORK, env=env, stdout=log, stderr=subprocess.STDOUT)
    dump("greptime.json", {"command": command, "binary_sha256": sha(GREPTIME), "conn": GREPTIME_CONN,
                            "ports": {"http": GT_HTTP, "grpc": GT_GRPC, "mysql": GT_MYSQL, "postgres": GT_PG}})
    until(lambda: OPENER.open(f"http://127.0.0.1:{GT_HTTP}/health", timeout=1).read() is not None, "Greptime health", 45)


def api(method, path, body=None, expected=200):
    data = json.dumps(body).encode() if body is not None else None
    request = urllib.request.Request(f"http://127.0.0.1:{api_port}{path}", data=data, method=method,
                                     headers={"Authorization": "Bearer " + token, "Content-Type": "application/json"})
    try:
        with OPENER.open(request, timeout=8) as response:
            assert response.status == expected, (response.status, path)
            raw = response.read()
            return json.loads(raw) if raw else None
    except urllib.error.HTTPError as exc:
        raw = exc.read().decode(errors="replace")
        if exc.code != expected:
            raise AssertionError(f"{method} {path}: {exc.code}, expected {expected}: {raw}")
        return raw


def until(check, label, timeout=35):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        with state_lock:
            if provider_errors:
                raise AssertionError(provider_errors[0])
        if proc and proc.poll() is not None:
            raise AssertionError(f"server exited during {label}")
        value = check()
        if value:
            return value
        time.sleep(.05)
    raise AssertionError("timeout: " + label)


def history(session):
    return api("GET", f"/api/sessions/{session}/history")["entries"]


def text_present(session, needle):
    return needle in json.dumps(history(session), ensure_ascii=False)


def events_include(tap, event, needle):
    return any(kind == event and isinstance(payload, dict) and needle in payload.get("text", "")
               for kind, payload in tap.frames)


result = {"verdict": "fail", "backend": BACKEND, "scope": f"isolated {BACKEND} route/SSE acceptance",
          "artifact_root": str(ROOT), "binary_sha256": {"new": sha(BINARY), **({"greptime": sha(GREPTIME)} if BACKEND == "greptime" else {})}}
print("ARTIFACT_ROOT=" + str(ROOT), flush=True)
try:
    assert BINARY.is_file() and os.access(BINARY, os.X_OK), "required exact candidate binary unavailable"
    if BACKEND == "greptime":
        assert GREPTIME.is_file() and os.access(GREPTIME, os.X_OK), "required isolated Greptime binary unavailable"
        start_greptime()
    server_log = (ROOT / "server.log").open("wb")
    proc = subprocess.Popen([str(BINARY), "--serve", "--host", "127.0.0.1", "--port", str(api_port),
                             "--workspace", str(WORK)], cwd=WORK, env=env, stdout=server_log, stderr=subprocess.STDOUT)
    token_path = ROOT / "state/e-agent/server.token"
    until(lambda: token_path.exists() and token_path.stat().st_size, "server token")
    token = token_path.read_text().strip()
    until(lambda: api("GET", "/api/sessions") is not None, "server API")

    api("POST", "/api/sessions", {"id": MAIN_B, "initial_prompt": "START_B"}, 201)
    until(lambda: ids["b_sibling"] and text_present(MAIN_B, "B_READY"), "sibling live registration")
    api("POST", "/api/sessions", {"id": MAIN_A, "initial_prompt": "START_A"}, 201)
    until(lambda: ids["a_finished"] and text_present(MAIN_A, "A_READY"), "A children registration")
    # A completed delegate must no longer be attachable/live before targeting it.
    finished = ids["a_finished"]
    until(lambda: api("GET", f"/api/sessions/{finished}/events", expected=404) is not None, "finished child removal")

    api("POST", f"/api/sessions/{MAIN_A}/prompt", {"prompt": "REJECT_ROUTES"}, 202)
    until(lambda: text_present(MAIN_A, "REJECTIONS_RECORDED"), "unknown/sibling/finished rejection receipts")
    rejection_history = history(MAIN_A)
    dump("parent-rejections-history.json", rejection_history)
    for call_id in ("reject-unknown", "reject-sibling", "reject-finished"):
        receipts = [entry["message"]["Tool"]["content"] for entry in rejection_history
                    if entry.get("message", {}).get("Tool", {}).get("call_id") == call_id]
        assert receipts == ["target is not a live child session"], (call_id, receipts)

    # Inspect the actual /btw route: it accepts a live main parent (201), but not a subagent (409).
    btw = api("POST", f"/api/sessions/{MAIN_A}/btw", {"prompt": "BTW_START"}, 201)["id"]
    with state_lock:
        ids["btw"] = btw
    assert btw.startswith("btw-"), btw
    assert btw_entered.wait(20), "btw provider did not enter held busy call"
    assert api("POST", f"/api/sessions/{btw}/btw", {"prompt": "nested"}, 409).find("cannot fork subagent session") >= 0
    # Attach before either direct message so frames prove live provenance.
    parent_tap = SseTap(f"http://127.0.0.1:{api_port}/api/sessions/{MAIN_A}/events", token)
    child_tap = SseTap(f"http://127.0.0.1:{api_port}/api/sessions/{btw}/events", token)
    assert parent_tap.ready.wait(5) and child_tap.ready.wait(5), "SSE streams did not attach"
    api("POST", f"/api/sessions/{MAIN_A}/prompt", {"prompt": "SEND_TO_BTW"}, 202)
    until(lambda: text_present(MAIN_A, "PARENT_SENT_TO_BUSY_BTW"), "parent send receipt while btw busy")
    release_btw.set()
    until(lambda: btw_parent_sent and text_present(btw, "BTW_RECEIVED_PARENT"),
          "mutual BTW delivery")
    until(lambda: events_include(parent_tap, "Notice", BTW2P) and events_include(child_tap, "Notice", P2BTW),
          "live Notice SSE frames")
    dump("parent-sse.json", parent_tap.frames); dump("btw-sse.json", child_tap.frames)
    for tap, message in ((parent_tap, BTW2P), (child_tap, P2BTW)):
        assert not events_include(tap, "UserPrompt", message), f"received agent message appeared as UserPrompt: {message}"
        assert events_include(tap, "Notice", message), f"missing Notice: {message}"
    parent_tap.close(); child_tap.close()

    parent_history, btw_history = history(MAIN_A), history(btw)
    dump("parent-btw-history.json", parent_history); dump("btw-history.json", btw_history)
    assert sum(entry.get("type") == "notice" and entry.get("text") == f"[agent message from {btw}]\n{BTW2P}"
               for entry in parent_history) == 1
    assert sum(entry.get("type") == "notice" and entry.get("text") == f"[agent message from {MAIN_A}]\n{P2BTW}"
               for entry in btw_history) == 1
    result.update({"verdict": "pass", "established": [
        "actual /btw 201 main-session fork and 409 subagent rejection",
        "actual SendMessage tool receipts reject unknown, finished, and sibling targets",
        "parent sent to a busy /btw child and /btw child sent to idle parent",
        "both received messages persisted as one attributed Notice and appeared as live SSE Notice, never SSE UserPrompt"],
        "not_run": []})
    print("PASS: routes, /btw mutual delivery, and SSE Notice provenance", flush=True)
finally:
    dump("result.json", result)
    release_btw.set()
    if proc:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill(); proc.wait(timeout=10)
    if greptime_proc:
        greptime_proc.terminate()
        try:
            greptime_proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            greptime_proc.kill(); greptime_proc.wait(timeout=10)
    try:
        server_log.close()
    except NameError:
        pass
    mock.shutdown(); mock.server_close()
