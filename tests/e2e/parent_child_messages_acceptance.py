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
GREPTIME = (HERE / ".e-agent/message-acceptance/bin/greptime").resolve()
BACKEND = os.environ.get("PARENT_CHILD_BACKEND", "jsonl")
assert BACKEND in {"jsonl", "greptime"}, "PARENT_CHILD_BACKEND must be jsonl or greptime"
ARTIFACTS = HERE / ".e-agent/message-acceptance/runs"
ARTIFACTS.mkdir(parents=True, exist_ok=True)
PARENT = "acceptance-parent"
RESUME_PARENT = "resume-parent"
P2C = "P2C-BEGIN\nFull parent message: café 中文 \"quoted\".\nP2C-END"
C2P = "C2P-BEGIN\nFull child message: café 中文 \"quoted\".\nC2P-END"
C2NEW = "C2NEW-BEGIN\nResumed child reply: café 中文 \"new parent\".\nC2NEW-END"


def port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


GT_HTTP, GT_GRPC, GT_MYSQL, GT_PG = (port() for _ in range(4))
GREPTIME_CONN = f"host=127.0.0.1 port={GT_PG} user=postgres dbname=public"


ROOT = Path(tempfile.mkdtemp(prefix="parent-child-messages-", dir=ARTIFACTS))
WORK = ROOT / "workspace"
for d in (WORK, ROOT / "home", ROOT / "config/e-agent", ROOT / "state/e-agent", ROOT / "greptime-data", ROOT / "greptime-log"):
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
            elif latest == "PROMPTLESS_ACTIVATION":
                response = {"content": "PROMPTLESS_ACTIVATED"}
            elif role == "parent" and latest == "START_RESUME_PARENT":
                response = delta_tool("delegate", {"workspace": str(WORK), "label": "resume-child",
                    "resume": child_id, "task": "RESUME_CHILD_TASK: send the requested reply to your current parent."}, "resume-delegate-1")
            elif role == "parent" and notice(request, child_id, C2NEW):
                response = {"content": "NEW_PARENT_REACTED_TO_RESUMED_CHILD"}
            elif role == "parent" and any(m.get("tool_call_id") == "resume-delegate-1" for m in request.get("messages", [])):
                response = {"content": "RESUME_PARENT_READY"}
            elif role == "child" and latest == "RESUME_CHILD_TASK: send the requested reply to your current parent.":
                response = delta_tool("send_message", {"target": "parent", "message": C2NEW}, "child-send-new-parent")
            elif role == "child" and any(m.get("tool_call_id") == "child-send-new-parent" and m.get("content") == "message queued" for m in request.get("messages", [])):
                response = {"content": "RESUMED_CHILD_REPLY"}
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
backend_config = 'backend = "jsonl"' if BACKEND == "jsonl" else f'''backend = "greptime"
conn = "{GREPTIME_CONN}"'''
(ROOT / "config/e-agent/config.toml").write_text(f'''default = "mock/acceptance"
[providers.mock]
base_url = "http://127.0.0.1:{MOCK_PORT}/v1"
api_key_env = "ACCEPTANCE_MOCK_KEY"
[models."mock/acceptance"]
model = "acceptance"
[session]
{backend_config}
''', encoding="utf-8")
env = {"PATH": "/usr/bin:/bin", "HOME": str(ROOT / "home"), "XDG_CONFIG_HOME": str(ROOT / "config"),
       "XDG_STATE_HOME": str(ROOT / "state"), "XDG_CACHE_HOME": str(ROOT / "cache"),
       "ACCEPTANCE_MOCK_KEY": "isolated-dummy-not-a-credential", "NO_PROXY": "127.0.0.1,localhost"}
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
proc = None
greptime_proc = None
token = None
api_port = None


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
    until(lambda: opener.open(f"http://127.0.0.1:{GT_HTTP}/health", timeout=1).read() is not None, "Greptime health", 45)


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
        if proc and proc.poll() is not None: raise AssertionError(f"server exited during {label}")
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


def canonical(entry):
    return json.dumps(entry, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def history_probe(before, after):
    before_rows, after_rows = [canonical(row) for row in before], [canonical(row) for row in after]
    first_difference = next((index for index, pair in enumerate(zip(before_rows, after_rows)) if pair[0] != pair[1]), None)
    if first_difference is None and len(before_rows) != len(after_rows):
        first_difference = min(len(before_rows), len(after_rows))
    if before_rows == after_rows:
        classification = "identical"
    elif after_rows[:len(before_rows)] == before_rows:
        classification = "snapshot_prefix_plus_late_appends"
    elif before_rows[:len(after_rows)] == after_rows:
        classification = "loss_or_truncation_prefix"
    else:
        classification = "loss_or_reorder_or_changed_record"
    return {"before_length": len(before), "after_length": len(after),
            "before_canonical_sha256": hashlib.sha256("\n".join(before_rows).encode()).hexdigest(),
            "after_canonical_sha256": hashlib.sha256("\n".join(after_rows).encode()).hexdigest(),
            "before_types": [row.get("type") for row in before], "after_types": [row.get("type") for row in after],
            "classification": classification, "first_difference_index": first_difference,
            "before_entry_at_difference": before[first_difference] if first_difference is not None and first_difference < len(before) else None,
            "after_entry_at_difference": after[first_difference] if first_difference is not None and first_difference < len(after) else None}


result = {"verdict": "fail", "backend": BACKEND, "artifact_root": str(ROOT), "binary_sha256": {"new": sha(NEW), "old": sha(OLD), **({"greptime": sha(GREPTIME)} if BACKEND == "greptime" else {})}}
print("ARTIFACT_ROOT=" + str(ROOT), flush=True)
try:
    assert NEW.is_file() and os.access(NEW, os.X_OK), "required exact new binary unavailable"
    assert OLD.is_file() and os.access(OLD, os.X_OK), "required exact old binary unavailable"
    if BACKEND == "greptime":
        assert GREPTIME.is_file() and os.access(GREPTIME, os.X_OK), "required isolated Greptime binary unavailable"
        start_greptime()
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
    if BACKEND == "jsonl":
        for sid, expected in ((PARENT, parent_history), (child_id, child_history)):
            raw = [json.loads(x) for x in (WORK / ".e-agent/sessions" / f"{sid}.jsonl").read_text(encoding="utf-8").splitlines() if x]
            assert raw == expected, f"raw JSONL/API history mismatch for {sid}"
    else:
        dump("greptime-api-basic-records.json", {"parent": parent_history, "child": child_history})
    # Freeze only after the normal delegate completion boundary: the child
    # has finished, its completion record is durable in the parent, the parent
    # has made its completion reaction, and no owned task remains.
    def completion_boundary():
        p, c = history(PARENT), history(child_id)
        child_finished = "CHILD_REACTED_TO_PARENT_MESSAGE" in json.dumps(c, ensure_ascii=False)
        completion = [entry for entry in p if entry.get("type") == "background_completion"
                      and entry.get("output", "").startswith(f"subagent session: {child_id}\nCHILD_REACTED_TO_PARENT_MESSAGE")]
        # A normal parent reaction is durable as the assistant record after
        # the completion entry; provider request payloads do not necessarily
        # expose the completion body as a string.
        completion_index = p.index(completion[0]) if completion else -1
        parent_reacted = completion_index >= 0 and any(
            "PARENT_REACTED_TO_CHILD_MESSAGE" in json.dumps(entry, ensure_ascii=False)
            for entry in p[completion_index + 1:])
        sessions = api("GET", "/api/sessions")
        parent_meta = next((item for item in sessions if item.get("id") == PARENT), None)
        parent_idle = parent_meta and parent_meta.get("status") == "Idle"
        # `/api/tasks` is global task-panel data; a delegate is owned by the
        # parent but carries its subagent session id, so match that identity.
        no_owned_task = not any(item.get("session_id") == child_id for item in api("GET", "/api/tasks"))
        dump("completion-boundary-observation.json", {"child_finished": child_finished,
             "completion_count": len(completion), "parent_reacted": parent_reacted,
             "parent_status": parent_meta.get("status") if parent_meta else None,
             "parent_idle": bool(parent_idle), "no_owned_task": no_owned_task})
        return (p, c) if child_finished and len(completion) == 1 and parent_reacted and parent_idle and no_owned_task else None
    parent_history, child_history = until(completion_boundary, "delegate completion boundary")
    dump("parent-history-completion-boundary.json", parent_history)
    dump("child-history-completion-boundary.json", child_history)
    basic_requests = len(requests)
    transition_times = {"before_capture_ns": time.time_ns()}
    dump("parent-history-before-shutdown.json", parent_history)
    dump("child-history-before-shutdown.json", child_history)
    transition_times["before_shutdown_ns"] = time.time_ns()
    stop(log)
    transition_times["after_shutdown_ns"] = time.time_ns()

    # Historical loads are passive.  Resume explicitly with a prompt only after proving no request occurred.
    transition_times["before_old_start_ns"] = time.time_ns()
    log = start(OLD, "old-resume")
    transition_times["after_old_start_ns"] = time.time_ns()
    until(lambda: api("GET", f"/api/sessions/{PARENT}/history") is not None, "old historical history")
    old_parent_before, old_child_before = history(PARENT), history(child_id)
    transition_times["old_get_ns"] = time.time_ns()
    dump("old-transition-timestamps.json", transition_times)
    dump("parent-history-old-before-comparison.json", old_parent_before)
    dump("child-history-old-before-comparison.json", old_child_before)
    comparison = {"parent": history_probe(parent_history, old_parent_before), "child": history_probe(child_history, old_child_before), "timestamps": transition_times}
    dump("old-passive-comparison.json", comparison)
    assert old_parent_before == parent_history and old_child_before == child_history, "old changed original records/order"
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
    stop(log)

    # Fourth product launch: history-only reads must exactly retain both original
    # transcripts.  A promptless POST /sessions is the actual runner lifecycle
    # boundary (it installs a live WaitForInput runner); its historical Notice
    # must not create a provider request before an explicit activation prompt.
    log = start(NEW, "new-fourth-history-resume")
    until(lambda: api("GET", f"/api/sessions/{PARENT}/history") is not None, "fourth API history")
    fourth_parent = history(PARENT)
    fourth_child = history(child_id)
    assert fourth_parent == final_parent, "fourth launch changed final parent records/order"
    assert fourth_child == child_history, "fourth launch changed original child records/order"
    dump("parent-history-fourth-before-resume.json", fourth_parent)
    dump("child-history-fourth-before-resume.json", fourth_child)
    requests_before_resume = len(requests)
    resumed = api("POST", "/api/sessions", {"id": PARENT}, 201)
    assert resumed["status"] == "Idle" and resumed["active"] is True, "promptless resume did not establish live Idle runner"
    time.sleep(.5)
    assert len(requests) == requests_before_resume, "historical Notice reacted during promptless live resume"
    assert history(PARENT) == final_parent, "promptless resume appended records"
    api("POST", f"/api/sessions/{PARENT}/prompt", {"prompt": "PROMPTLESS_ACTIVATION"}, 202)
    def promptless_activated():
        h = history(PARENT)
        return h if "PROMPTLESS_ACTIVATED" in json.dumps(h) else None
    promptless_parent = until(promptless_activated, "promptless resume activation")
    assert promptless_parent[:len(final_parent)] == final_parent, "activation failed to preserve final parent order"
    dump("parent-history-promptless-activated.json", promptless_parent)

    # Resume the finished child through a fresh real parent delegate.  The
    # child's `parent` endpoint must point at this new parent, not the old one.
    api("POST", "/api/sessions", {"id": RESUME_PARENT, "initial_prompt": "START_RESUME_PARENT"}, 201)
    def resumed_delivery():
        h = history(RESUME_PARENT)
        return h if "NEW_PARENT_REACTED_TO_RESUMED_CHILD" in json.dumps(h) else None
    resume_parent_history = until(resumed_delivery, "resumed child delivery to new parent")
    resumed_child_history = history(child_id)
    assert sum(e.get("type") == "notice" and e.get("text") == f"[agent message from {child_id}]\n{C2NEW}" for e in resume_parent_history) == 1, "resumed child did not reach new parent as one complete Notice"
    old_parent_after_delivery = history(PARENT)
    dump("old-parent-history-after-resumed-delivery.json", old_parent_after_delivery)
    assert not any(e.get("type") == "notice" and C2NEW in e.get("text", "") for e in old_parent_after_delivery), "resumed child incorrectly reached old parent"
    resumed_calls = [c for e in resumed_child_history for c in e.get("message", {}).get("Assistant", {}).get("tool_calls", []) if c.get("id") == "child-send-new-parent"]
    assert len(resumed_calls) == 1 and json.loads(resumed_calls[0]["arguments"]) == {"target": "parent", "message": C2NEW}, "resumed child did not use current parent endpoint"
    dump("resume-parent-history.json", resume_parent_history)
    dump("child-history-resumed.json", resumed_child_history)
    stop(log)

    # A final restart proves the child append is durable and API-readable.
    log = start(NEW, "new-fifth-child-durability")
    until(lambda: api("GET", f"/api/sessions/{child_id}/history") is not None, "resumed child final API history")
    durable_child = history(child_id)
    assert durable_child == resumed_child_history, "restart changed resumed child records/order"
    dump("child-history-final-durable.json", durable_child)
    result.update({"verdict": "pass", "established": [f"isolated real {BACKEND} backend", "real --serve API/factory/delegate", "busy child queued bidirectional full multiline Unicode messages", "exact message queued receipts", "attributed Notice provider context and API persistence", "new→old→new passive replay and append preserve API record order", "fourth-launch exact parent/child API replay and promptless live resume has no Notice-only reaction before activation", "resumed finished child uses current new-parent endpoint and its appended history survives restart"], "uncovered": ["unknown/finished/sibling target rejection", "/btw", "SSE event classification", "SQLite/Greptime/browser"]})
    print("PASS: parent/child messaging, compatibility, promptless resume, and resumed-child routing", flush=True)
except Exception as exc:
    result["error"] = repr(exc)
    result["traceback"] = traceback.format_exc()
    print("FAIL: " + repr(exc), file=sys.stderr, flush=True)
    raise
finally:
    dump("result.json", result)
    release_child.set()
    if proc: stop(log)
    if greptime_proc:
        greptime_proc.terminate()
        try: greptime_proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            greptime_proc.kill(); greptime_proc.wait(timeout=10)
    mock.shutdown(); mock.server_close()
