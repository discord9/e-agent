#!/usr/bin/env python3
"""Real-product cancellation-source acceptance harness (GreptimeDB fixture).

Run from the repository root of the *task worktree*::

    uv run --with playwright python tests/e2e/task_cancel_source_greptime.py \
      --greptimedb-bin /home/discord9/.local/share/e-agent/greptimedb/greptime \
      --candidate-binary .e-agent/acceptance/task-cancel-source/bin/e-agent-candidate \
      --candidate-src    .e-agent/acceptance/task-cancel-source/src/candidate-<sha> \
      --old-binary       .e-agent/acceptance/task-cancel-source/bin/e-agent-old-60220f7 \
      --old-src          .e-agent/acceptance/task-cancel-source/src/old-60220f7 \
      --browser require --worktree "$PWD"

(`--pythonpath` is only needed when the interpreter running this file cannot
import playwright itself; a plain ``uv run --with playwright python`` does not
need it.)

What this proves, against the REAL product surface (no fixture inside the
product, no synthetic API rows):

1. Actual ``e-agent --serve`` HTTP API driven by a stdlib fake OpenAI SSE
   provider, over an isolated GreptimeDB instance created by the real
   ``greptime`` binary in an isolated ``--data-home``. Every tested completion
   comes from a real background bash process that exited, was cancelled, or
   timed out.
2. Cancellation sources over the real API/DB: user (API ``DELETE``), agent
   (the model really calls ``cancel_background_task``), system (configured
   background timeout), a normal exit with no source, and a legacy row with
   no source written by the old 60220f7 binary.
3. Browser-visible labels from the actual session: Playwright drives the real
   assembled UI against the real ``/api/tasks/finished`` rows of this session
   and asserts every sourced row renders ``cancelled by <source>`` while a
   source-less (legacy/normal) row renders no source text.
4. Durable cross-version continuity on ONE isolated GreptimeDB:
   new(candidate) -> old(60220f7) -> new(candidate) -> restart(candidate) ->
   final restart. Every stage resumes the same session id and appends real
   entries; the full entry list and its order must equal the concatenation of
   the stages, and each earlier entry must read back field-for-field
   unchanged at every later stage (old and new readers alike). The only
   difference an old reader may show is the documented one: 60220f7 has no
   ``cancellation_source`` field, so its view omits that single key. Those
   omissions are recorded as exact evidence (index/id/type/source), never
   accepted as a weaker assertion on the durable rows.

Non-goals: no crash-recovery, ownership, stall-warning, subagent or pet/UI
scenario; no production config, credential, database or service is read or
written; no host-global process state is asserted (only processes this run
spawns are tracked, and ``/proc`` scans are restricted to this run's unique
marker).

Provenance: the old binary is accepted only when its embedded build commit
(build.rs ``EAGENT_COMMIT``) matches the HEAD of ``--old-src`` and that HEAD
matches ``--expect-old-commit`` (default 60220f7a9ee559ff86a1116720eb909e36298017).
Timestamps are recorded, never trusted. The report records the exact HEAD and
``git status --porcelain`` of ``--worktree`` at run time.
"""

import argparse
import hashlib
import json
import os
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.error import HTTPError, URLError
from urllib.parse import urlparse
from urllib.request import ProxyHandler, Request, build_opener

REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_ARTIFACTS = REPO_ROOT / ".e-agent/acceptance/task-cancel-source/runs"
# Production/default listeners this harness must never touch.
FORBIDDEN_PORTS = {80, 15400, 15401, 15402, 15403}
OLD_COMMIT_FULL = "60220f7a9ee559ff86a1116720eb909e36298017"
SESSION_ID = "e2e-cancel-source"
GREPTIME_READY_SECONDS = 120.0
SERVER_READY_SECONDS = 30.0
ENTRY_SECONDS = 30.0
BACKGROUND_TIMEOUT_SECS = 2
DEFAULT_CHROME = [
    "/mnt/nvme_rust/cargo-home/playwright-browsers/chromium_headless_shell-1228/chrome-headless-shell-linux64/chrome-headless-shell",
    "/home/discord9/.cache/ms-playwright/chromium_headless_shell-1234/chrome-linux64/headless_shell",
    "/usr/bin/chromium-headless-shell",
]
DEFAULT_PLAYWRIGHT_PATHS = [
    "/home/discord9/.cache/e-agent-browser-python",
]

# Child processes must not inherit an outbound proxy.
for _proxy in ("http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "all_proxy", "NO_PROXY", "no_proxy"):
    os.environ.pop(_proxy, None)
HTTP = build_opener(ProxyHandler({}))


def log(message):
    print(message, flush=True)


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def choose_port():
    """Kernel-assigned localhost port; never a reserved/production listener."""
    while True:
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        if port not in FORBIDDEN_PORTS:
            return port


def sse(payload):
    return "data: " + json.dumps(payload, separators=(",", ":")) + "\n\n"


def wait_for(description, predicate, seconds):
    deadline = time.monotonic() + seconds
    last = None
    while time.monotonic() < deadline:
        last = predicate()
        if last:
            return last
        time.sleep(0.10)
    raise AssertionError(
        "timed out after %.1fs waiting for %s%s"
        % (seconds, description, ": " + repr(last)[:400] if last is not None else "")
    )


# ---------------------------------------------------------------------------
# Fake OpenAI-compatible SSE provider (stdlib only).
#
# The harness announces one scenario per turn (after the previous turn's
# durable completion landed and the session went Idle), and the provider keeps
# its own per-scenario response counter for the step. That is exact without
# reading the product's request body at all: it never has to guess a scenario
# from prompt text (which stays in the transcript forever) or from "the last
# request won". `seen` records what the provider served for the report.
# ---------------------------------------------------------------------------

SCENARIOS = {
    "normal": "sleep 0.3; echo %(marker)s-normal-output",
    "user": "exec -a %(marker)s-user sleep 300",
    "agent": "exec -a %(marker)s-agent sleep 300",
    "timeout": "exec -a %(marker)s-timeout sleep 300",
    "legacy": "exec -a %(marker)s-legacy sleep 300",
    "old-normal": "sleep 0.3; echo %(marker)s-old-normal-output",
    "user2": "exec -a %(marker)s-user2 sleep 300",
    "live": "exec -a %(marker)s-live sleep 300",
    "normal2": "sleep 0.3; echo %(marker)s-normal2-output",
}
TOKEN = "mockstep-%s-%d"


def step_token(scenario, step):
    return TOKEN % (scenario, step)


class Provider(BaseHTTPRequestHandler):
    marker = ""
    current = ""  # scenario the harness announced for the in-flight turn
    counts = {}  # scenario -> number of responses this provider generated for it
    agent_continue = threading.Event()
    seen = []
    lock = threading.Lock()

    def log_message(self, _format, *_args):
        pass

    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(b'{"ok":true}')

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        body = raw.decode("utf-8", "replace")
        with Provider.lock:
            # The step is the provider's own per-scenario response count: the
            # harness serializes turns (durable completion + Idle + stable
            # history) before announcing the next scenario, so this is exact
            # and independent of how the transcript replay is bounded.
            active = Provider.current or "unscoped"
            step = Provider.counts.get(active, 0)
            Provider.counts[active] = step + 1
            Provider.seen.append({
                "scenario": active, "step": step, "path": urlparse(self.path).path,
                "prompt_in_replay": ("[e2e-%s]" % active) in body,
                "body_bytes": len(raw),
            })
        encoded = ("".join(sse(chunk) for chunk in self.chunks(active, step)) + "data: [DONE]\n\n").encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Content-Length", str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)
        self.wfile.flush()

    def chunks(self, scenario, step):
        token = step_token(scenario, min(step, 9))
        if step == 0 and scenario in SCENARIOS:
            command = SCENARIOS[scenario] % {"marker": Provider.marker}
            return self.tool_call(token, "bash", {"command": command, "background": True})
        if scenario == "agent" and step == 1:
            # Deterministic registry id: a fresh server process whose only
            # background task is this scenario's bash task (id 1).
            Provider.agent_continue.wait(timeout=ENTRY_SECONDS)
            return self.tool_call(token, "cancel_background_task", {"id": 1})
        return self.text("fixture " + token + " turn complete")

    @staticmethod
    def tool_call(call_id, name, arguments):
        return [
            {"id": "e2e", "object": "chat.completion.chunk", "model": "mock-e2e", "choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [{"index": 0, "id": call_id, "type": "function", "function": {"name": name, "arguments": json.dumps(arguments, separators=(",", ":"))}}]}, "finish_reason": None}]},
            {"id": "e2e", "object": "chat.completion.chunk", "model": "mock-e2e", "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]},
        ]

    @staticmethod
    def text(content):
        return [
            {"id": "e2e", "object": "chat.completion.chunk", "model": "mock-e2e", "choices": [{"index": 0, "delta": {"role": "assistant", "content": content}, "finish_reason": None}]},
            {"id": "e2e", "object": "chat.completion.chunk", "model": "mock-e2e", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
        ]


# ---------------------------------------------------------------------------
# HTTP + process helpers.
# ---------------------------------------------------------------------------


def request(url, token=None, method="GET", body=None, timeout=8):
    headers = {"Accept": "application/json"}
    if token:
        headers["Authorization"] = "Bearer " + token
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        headers["Content-Type"] = "application/json"
    req = Request(url, data=data, headers=headers, method=method)
    try:
        with HTTP.open(req, timeout=timeout) as response:
            raw = response.read().decode("utf-8", "replace")
            return response.status, (json.loads(raw) if raw else None)
    except HTTPError as error:
        raw = error.read().decode("utf-8", "replace")
        raise AssertionError("HTTP %s %s %s: %s" % (method, error.code, url, raw[:600])) from error
    except URLError as error:
        raise AssertionError("HTTP %s %s failed: %s" % (method, url, error)) from error


def marker_pids(marker):
    """PIDs of processes whose argv carries this run's unique marker."""
    found = []
    proc = Path("/proc")
    if not proc.exists():
        return found
    for child in proc.iterdir():
        if not child.name.isdigit():
            continue
        try:
            command = (child / "cmdline").read_bytes().replace(b"\0", b" ").decode("utf-8", "replace")
        except OSError:
            continue
        if marker in command:
            found.append(int(child.name))
    return found


class Fixture:
    """Isolated GreptimeDB + fake provider + e-agent servers for one run."""

    def __init__(self, root, greptimedb_bin):
        self.root = root
        self.greptimedb_bin = greptimedb_bin
        self.workspace = root / "workspace"
        self.config = root / "config"
        self.state = root / "state"
        self.home = root / "home"
        self.tmp = root / "tmp"
        self.db_data_home = root / "greptime-data"
        for path in (self.workspace / ".e-agent", self.config / "e-agent", self.state, self.home, self.tmp, self.db_data_home):
            path.mkdir(parents=True, exist_ok=True)
        self.marker = "eagent-cancel-source-%d" % os.getpid()
        self.mock_port = choose_port()
        self.db_http, self.db_grpc, self.db_mysql, self.db_pg = (choose_port() for _ in range(4))
        self.processes = []
        self.servers = []
        self.greptime = None
        self.mock = None

    # --- isolated GreptimeDB ------------------------------------------------
    def start_greptime(self):
        log_path = self.root / "greptime.log"
        env = {"HOME": str(self.home), "TMPDIR": str(self.tmp), "PATH": os.environ.get("PATH", "")}
        self.greptime = subprocess.Popen(
            [
                self.greptimedb_bin, "standalone", "start",
                "--data-home", str(self.db_data_home),
                "--http-addr", "127.0.0.1:%d" % self.db_http,
                "--grpc-bind-addr", "127.0.0.1:%d" % self.db_grpc,
                "--mysql-addr", "127.0.0.1:%d" % self.db_mysql,
                "--postgres-addr", "127.0.0.1:%d" % self.db_pg,
            ],
            stdin=subprocess.DEVNULL, stdout=log_path.open("wb"), stderr=subprocess.STDOUT,
            env=env, start_new_session=True, cwd=str(self.root),
        )
        self.processes.append(self.greptime)
        pg_env = dict(os.environ, PGCONNECT_TIMEOUT="2", HOME=str(self.home))

        def ready():
            if self.greptime.poll() is not None:
                raise AssertionError("greptime exited; inspect %s" % log_path)
            try:
                done = subprocess.run(
                    ["psql", "-h", "127.0.0.1", "-p", str(self.db_pg), "-U", "postgres", "-d", "public", "-Atqc", "select 1"],
                    stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, timeout=5, env=pg_env,
                )
                if done.returncode == 0 and done.stdout.strip() == b"1":
                    return True
            except (subprocess.SubprocessError, OSError):
                pass
            return False

        wait_for("isolated greptime (postgres 127.0.0.1:%d)" % self.db_pg, ready, GREPTIME_READY_SECONDS)

    # --- fake provider ------------------------------------------------------
    def start_mock(self):
        Provider.marker = self.marker
        Provider.current = ""
        Provider.counts = {}
        Provider.seen = []
        Provider.agent_continue.clear()
        self.mock = ThreadingHTTPServer(("127.0.0.1", self.mock_port), Provider)
        threading.Thread(target=self.mock.serve_forever, daemon=True).start()
        wait_for("fake provider", lambda: request("http://127.0.0.1:%d/health" % self.mock_port)[0] == 200, 10.0)

    # --- config / servers ---------------------------------------------------
    def write_config(self, timeout_secs):
        (self.config / "e-agent" / "config.toml").write_text(
            """default = "mock/e2e"
[providers.mock]
base_url = "http://127.0.0.1:%d/v1"
api_key_env = "E2E_FAKE_PROVIDER_KEY"
[models."mock/e2e"]
model = "mock-e2e"
[session]
backend = "greptime"
conn = "host=127.0.0.1 port=%d dbname=public"
[background]
timeout_secs = %d
""" % (self.mock_port, self.db_pg, timeout_secs),
            encoding="utf-8",
        )

    def start_server(self, label, binary, timeout_secs):
        server = Server(self, label, binary, timeout_secs).start()
        self.servers.append(server)
        return server

    def cleanup(self):
        failures = []
        for server in self.servers:
            try:
                server.stop()
            except Exception as error:  # noqa: BLE001 - cleanup must report every failure
                failures.append("server %s stop: %s" % (server.label, error))
        for process in self.processes:
            if process.poll() is None:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    failures.append("pid %d did not reap" % process.pid)
        for pid in marker_pids(self.marker):
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        try:
            wait_for("run marker process cleanup", lambda: not marker_pids(self.marker), 15.0)
        except AssertionError as error:
            failures.append(str(error))
        if self.mock is not None:
            self.mock.shutdown()
            self.mock.server_close()
        return failures


class Server:
    """One `e-agent --serve` process against the shared isolated fixture."""

    def __init__(self, fixture, label, binary, timeout_secs):
        self.fixture = fixture
        self.label = label
        self.binary = binary
        self.timeout_secs = timeout_secs
        self.process = None
        self.port = None
        self.token = None
        self.log_path = None

    def api(self, path):
        return "http://127.0.0.1:%d%s" % (self.port, path)

    def start(self):
        # A config timeout of 0 means "no timeout" in the product.
        self.fixture.write_config(self.timeout_secs)
        self.port = choose_port()
        self.log_path = self.fixture.root / ("server-%s-%d.log" % (self.label, self.port))
        env = {
            "HOME": str(self.fixture.home),
            "XDG_CONFIG_HOME": str(self.fixture.config),
            "XDG_STATE_HOME": str(self.fixture.state),
            "TMPDIR": str(self.fixture.tmp),
            "E2E_FAKE_PROVIDER_KEY": "fixture-only-not-a-secret",
            "PATH": os.environ.get("PATH", ""),
        }
        self.process = subprocess.Popen(
            [self.binary, "--serve", "--host", "127.0.0.1", "--port", str(self.port), "--workspace", str(self.fixture.workspace)],
            stdin=subprocess.DEVNULL, stdout=self.log_path.open("wb"), stderr=subprocess.STDOUT,
            env=env, start_new_session=True, cwd=str(self.fixture.workspace),
        )
        self.fixture.processes.append(self.process)
        token_file = self.fixture.state / "e-agent" / "server.token"

        def ready():
            if self.process.poll() is not None:
                raise AssertionError("e-agent %s exited; inspect retained fixture log" % self.label)
            if not token_file.exists() or not token_file.read_text().strip():
                return False
            self.token = token_file.read_text().strip()
            try:
                return request(self.api("/api/sessions"), self.token)[0] == 200
            except AssertionError:
                return False

        wait_for("e-agent --serve (%s)" % self.label, ready, SERVER_READY_SECONDS)
        return self

    def resume(self):
        status, meta = request(self.api("/api/sessions"), self.token, "POST", {"id": SESSION_ID})
        assert status == 201, "resume status %s" % status
        return meta

    def prompt(self, scenario):
        assert scenario in SCENARIOS, scenario
        Provider.current = scenario
        status, _ = request(
            self.api("/api/sessions/%s/prompt" % SESSION_ID), self.token, "POST",
            {"text": "[e2e-%s] start a real background bash task" % scenario},
        )
        assert status == 202, "prompt status %s" % status

    def tasks(self):
        _status, tasks = request(self.api("/api/tasks"), self.token)
        return tasks

    def session_tasks(self):
        return [t for t in self.tasks() if t.get("session_id") == SESSION_ID and t.get("kind") == "bash"]

    def running_task(self):
        return wait_for("running bash task for %s" % SESSION_ID, lambda: (self.session_tasks() or [False])[0], ENTRY_SECONDS)

    def cancel(self, task_id, expect=204):
        status, _ = request(self.api("/api/sessions/%s/tasks/%d" % (SESSION_ID, task_id)), self.token, "DELETE")
        assert status == expect, "cancel status %s (expected %s) for task %d" % (status, expect, task_id)
        return status

    def history(self):
        status, payload = request(self.api("/api/sessions/%s/history" % SESSION_ID), self.token)
        assert status == 200, "history status %s" % status
        return payload["entries"]

    def completions(self):
        return [e for e in self.history() if e.get("type") == "background_completion"]

    def await_idle(self):
        """The session must be Idle before the next prompt is issued."""
        def idle():
            _status, sessions = request(self.api("/api/sessions"), self.token)
            meta = next((s for s in sessions if s.get("id") == SESSION_ID), None)
            if meta is None:
                return False
            if meta.get("busy"):
                return False
            return meta.get("status") == "Idle"
        wait_for("session %s to go Idle" % SESSION_ID, idle, ENTRY_SECONDS)

    def await_new_completion(self, previous_count, source, note):
        """Wait for exactly one more durable completion, with the expected source.

        `source` None means "no recorded source" (normal exit or legacy row).
        """
        def find():
            entries = self.completions()
            if len(entries) <= previous_count:
                return False
            return entries[-1] if entries[-1].get("cancellation_source") == source else False

        entry = wait_for("durable completion (%s)" % note, find, ENTRY_SECONDS)
        total = len(self.completions())
        assert total == previous_count + 1, "%s: expected one new completion, got %d" % (note, total)
        return entry

    def settle(self):
        """Wait until no turn is in flight and the transcript stopped growing.

        Called between scenarios: the provider's per-scenario step counter is
        only exact if the previous scenario's completion turn (and later its
        cancellation-completion turn) has fully finished.
        """
        self.await_idle()
        previous = json.dumps(self.history(), sort_keys=True)
        for _ in range(4):
            time.sleep(0.4)
            current = json.dumps(self.history(), sort_keys=True)
            if current == previous:
                return
            previous = current
        raise AssertionError("history never settled for session %s" % SESSION_ID)

    def drain(self):
        """Cancel leftover tasks and wait for a stable idle transcript."""
        for task in self.session_tasks():
            self.cancel_any(task["id"])
        wait_for("session task drain", lambda: not self.session_tasks(), ENTRY_SECONDS)
        self.await_idle()
        # A cancelled wrapper's durable completion is appended by the runner;
        # consecutive identical reads are a settling heuristic, not proof that
        # no append remains in flight. Later restart comparisons check durability.
        previous = json.dumps(self.history(), sort_keys=True)
        for _ in range(3):
            time.sleep(0.4)
            current = json.dumps(self.history(), sort_keys=True)
            if current == previous:
                return
            previous = current
        raise AssertionError("history never settled after drain")

    def cancel_any(self, task_id):
        status, _ = request(self.api("/api/sessions/%s/tasks/%d" % (SESSION_ID, task_id)), self.token, "DELETE")
        assert status in (204, 404), "drain cancel status %s for task %d" % (status, task_id)

    def stop(self):
        process, self.process = self.process, None
        if process is None or process.poll() is not None:
            return
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=5)


# ---------------------------------------------------------------------------
# Journal: canonical durable entries + explicit reader normalization.
# ---------------------------------------------------------------------------


def comparable(entry, viewer):
    """Comparison form of one entry as read by `viewer`.

    60220f7 has no `cancellation_source` field at all, so an old read omits
    that single key. Every other field must match exactly; the omission itself
    is reported as evidence by `Journal.verify_prefix`.
    """
    if viewer == "old" and entry.get("type") == "background_completion":
        return {key: value for key, value in entry.items() if key != "cancellation_source"}
    return entry


class Journal:
    def __init__(self):
        self.entries = []  # canonical entries, captured from candidate reads
        self.stages = []

    def verify_prefix(self, entries, viewer, stage):
        """Require `entries` to start with every canonical entry, in order."""
        assert len(entries) >= len(self.entries), \
            "%s: %s reader sees %d entries, canonical has %d (history shrank)" % (
                stage, viewer, len(entries), len(self.entries))
        dropped = []
        for index, canonical in enumerate(self.entries):
            read = entries[index]
            assert comparable(read, viewer) == comparable(canonical, viewer), (
                "%s: entry %d changed as read by %s\ncanonical=%s\nread=%s"
                % (stage, index, viewer,
                   json.dumps(canonical, sort_keys=True, ensure_ascii=False)[:600],
                   json.dumps(read, sort_keys=True, ensure_ascii=False)[:600]))
            if viewer == "old" and canonical.get("cancellation_source") is not None:
                dropped.append({
                    "index": index, "id": canonical.get("id"), "type": canonical.get("type"),
                    "source": canonical["cancellation_source"],
                    "old_read_has_key": "cancellation_source" in read,
                })
        return dropped

    def extend(self, entries, stage):
        """Verify the canonical prefix, then adopt this stage's new tail.

        `/history` returns `SessionEntry` values (no `seq`), so ordering is
        positional: every stage must reproduce the earlier entries at the same
        indices before new entries may be appended after them.
        """
        self.verify_prefix(entries, "new", stage)
        appended = entries[len(self.entries):]
        self.entries.extend(appended)
        self.stages.append({"stage": stage, "appended": len(appended)})
        return appended

    def verify_exact(self, entries, stage):
        self.verify_prefix(entries, "new", stage)
        assert len(entries) == len(self.entries), \
            "%s: entry count %d != canonical %d" % (stage, len(entries), len(self.entries))
        assert entries == self.entries, "%s: entries differ from the canonical journal" % stage


# ---------------------------------------------------------------------------
# Browser assertion: real UI, real API rows, real session.
# ---------------------------------------------------------------------------

FINISHED_ROW_SIG_SCRIPT = """
(() => Array.from(document.querySelectorAll('.task-row-finished')).map((row) => ({
  sig: row.getAttribute('data-finished'),
  text: row.innerText,
  chips: Array.from(row.querySelectorAll('.task-meta')).map((n) => n.innerText),
})))()
"""


def finished_row_sig(row):
    """Mirror of src/ui/tasks.js `finishedKeySig` for one real API row."""
    def blank(value):
        return value if value is not None else ""

    return json.dumps([
        row.get("session_id") or "", blank(row.get("seq")), blank(row.get("id")),
        row.get("label") or "", row.get("kind") or "", row.get("status") or "",
        blank(row.get("output")), blank(row.get("exit_code")), row.get("signal") or "",
        row.get("cancellation_source") or "", blank(row.get("duration_ms")),
    ], separators=(",", ":"), ensure_ascii=False)


def import_sync_playwright(pythonpath):
    """Prefer the interpreter's own playwright; fall back to `--pythonpath`.

    uv-managed runs provide playwright in their own environment, and inserting
    an unrelated site dir first would shadow it with an ABI-mismatched build,
    so the extra path is only consulted after a plain import fails.
    """
    try:
        from playwright.sync_api import sync_playwright
        return sync_playwright, None
    except ImportError as plain:
        if not pythonpath:
            return None, str(plain)
        sys.path[:0] = [p for p in pythonpath.split(os.pathsep) if p]
        try:
            from playwright.sync_api import sync_playwright
            return sync_playwright, None
        except ImportError as pathed:
            return None, "%s; with --pythonpath %s: %s" % (plain, pythonpath, pathed)


def browser_check(server, chrome, pythonpath, required, evidence_root):
    if not chrome or not Path(chrome).is_file():
        if required:
            raise AssertionError("--browser require needs a Chromium executable (see --chrome)")
        return "skipped-unverified: Chromium executable unavailable"
    sync_playwright, import_error = import_sync_playwright(pythonpath)
    if sync_playwright is None:
        if required:
            raise AssertionError(
                "browser=require cannot import playwright (run with `uv run --with playwright python`, "
                "or pass --pythonpath): %s" % import_error)
        return "skipped-unverified: Python Playwright unavailable (%s)" % import_error

    _status, rows = request(server.api("/api/tasks/finished"), server.token)
    assert isinstance(rows, list) and rows, "/api/tasks/finished returned no rows"
    expected = {finished_row_sig(row): row for row in rows if row.get("session_id") == SESSION_ID}
    assert expected, "no finished rows for %s in /api/tasks/finished" % SESSION_ID
    sources = sorted({row.get("cancellation_source") or "" for row in expected.values()})
    assert {"agent", "system", "user"} <= set(sources), \
        "/api/tasks/finished does not carry all three sources: %s" % sources
    source_less = [row for row in expected.values() if not row.get("cancellation_source")]
    assert source_less, "no source-less row available for the negative browser assertion"
    live = server.running_task()

    origin = "http://127.0.0.1:%d" % server.port
    with sync_playwright() as playwright:
        browser = context = page = None
        page_errors = []
        try:
            browser = playwright.chromium.launch(executable_path=chrome, timeout=15_000)
            context = browser.new_context(viewport={"width": 1400, "height": 1000})
            page = context.new_page()
            page.on("pageerror", lambda error: page_errors.append(str(error)))
            page.goto(origin, wait_until="domcontentloaded", timeout=15_000)
            page.evaluate("token => localStorage.setItem('eagent_token', token)", server.token)
            page.goto(origin + "?session=" + SESSION_ID, wait_until="domcontentloaded", timeout=15_000)
            page.locator("#tasksToggleBar").wait_for(state="visible", timeout=15_000)
            page.locator("#tasksToggleBar").click(timeout=15_000)
            page.locator(".tasks-finished-header").click(timeout=15_000)
            page.locator(".task-row-finished").first.wait_for(state="visible", timeout=15_000)

            def rendered():
                found = page.evaluate(FINISHED_ROW_SIG_SCRIPT)
                if not found:
                    return False
                rendered_sigs = {item["sig"] for item in found}
                return found if set(expected) <= rendered_sigs else False

            items = wait_for("every real finished row rendered in the assembled UI", rendered, 20.0)
            observed = {item["sig"]: item for item in items}
            for sig, row in expected.items():
                item = observed[sig]
                text = item["text"]
                session_id = row.get("session_id")
                assert ("会话 " + session_id) in text, "row does not show its session: %r" % text
                source = row.get("cancellation_source")
                if source:
                    assert ("cancelled by " + source) in text, \
                        "row %s#%s missing rendered 'cancelled by %s': %r" % (session_id, row.get("id"), source, text)
                else:
                    assert "cancelled by" not in text, \
                        "source-less row %s#%s rendered a cancellation source: %r" % (session_id, row.get("id"), text)
            assert not page_errors, "page errors: %s" % page_errors
        except Exception:
            if page is not None:
                try:
                    page.screenshot(path=str(evidence_root / "browser-failure.png"), timeout=5_000)
                except Exception:  # noqa: BLE001 - evidence is best effort
                    pass
                (evidence_root / "browser-failure.txt").write_text(
                    "url=%s\npage_errors=%s\n" % (page.url, page_errors), encoding="utf-8")
            raise
        finally:
            if context is not None:
                context.close()
            if browser is not None:
                browser.close()
    return (
        "passed: %d real rows of session %s rendered by the assembled UI "
        "(sources=%s, one row with no source asserted clean, live task #%d kept the panel open)"
        % (len(expected), SESSION_ID, sources, live["id"])
    )


# ---------------------------------------------------------------------------
# Binary provenance.
# ---------------------------------------------------------------------------


def git_info(path):
    def run(*args):
        done = subprocess.run(["git", "-C", str(path), *args], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        assert done.returncode == 0, "git %s in %s failed: %s" % (" ".join(args), path, done.stdout.strip())
        return done.stdout

    return {
        "path": str(Path(path).resolve()),
        "head": run("rev-parse", "HEAD").strip(),
        "status_porcelain": [line for line in run("status", "--porcelain").splitlines() if line],
    }


def binary_identity(path, expect_src, expect_commit, label):
    path = Path(path).resolve()
    assert path.is_file() and os.access(path, os.X_OK), "%s is not an executable: %s" % (label, path)
    done = subprocess.run([str(path), "--version"], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    assert done.returncode == 0, "%s --version failed: %s" % (label, done.stdout)
    version = done.stdout.strip()
    match = re.match(r"^e-agent \S+ \(([0-9a-f]+)(-dirty)?\)$", version)
    assert match, "%s --version does not embed a git commit: %r" % (label, version)
    commit, dirty = match.group(1), bool(match.group(2))
    src = git_info(expect_src)
    assert not dirty, "%s binary carries a dirty build marker" % label
    assert not src["status_porcelain"], "%s source checkout is dirty" % label
    assert src["head"].startswith(commit), \
        "%s --version commit %s is not the HEAD of %s (%s)" % (label, commit, expect_src, src["head"])
    if expect_commit:
        assert src["head"].startswith(expect_commit), \
            "%s source HEAD %s does not match expected %s" % (label, src["head"], expect_commit)
    stat = path.stat()
    return {
        "label": label,
        "path": str(path),
        "version": version,
        "embedded_commit": commit,
        "dirty_marker": dirty,
        "src": src,
        "size": stat.st_size,
        "mtime": time.strftime("%Y-%m-%dT%H:%M:%S%z", time.localtime(stat.st_mtime)),
        "sha256": sha256(path),
    }


# ---------------------------------------------------------------------------
# Stages.
# ---------------------------------------------------------------------------


def stage_new_initial(fixture, candidate_bin, journal, evidence):
    """A: candidate creates the session and records every source kind."""
    evidence["stage"] = "A new(candidate) initial"
    fixture_record = {}
    server = fixture.start_server("A-normal", candidate_bin, 0)
    server.resume()

    server.prompt("normal")
    normal = server.await_new_completion(0, None, "normal background exit")
    server.settle()

    server.prompt("user")
    user_task = server.running_task()
    server.cancel(user_task["id"])
    user = server.await_new_completion(1, "user", "API DELETE user cancel")
    server.settle()
    server.drain()
    server.stop()

    server = fixture.start_server("A-agent", candidate_bin, 0)
    server.resume()
    server.prompt("agent")
    agent_task = server.running_task()
    fixture_record["agent_task"] = {"id": agent_task["id"], "command": agent_task.get("full_command")}
    Provider.agent_continue.set()
    agent = server.await_new_completion(2, "agent", "model cancel_background_task tool")
    assert agent.get("status") == "killed", "agent-cancelled task status: %r" % agent.get("status")
    assert fixture_record["agent_task"]["id"] == 1, (
        "agent cancel was aimed at id 1 but the live task was %s" % fixture_record["agent_task"])
    server.settle()
    server.drain()
    server.stop()

    server = fixture.start_server("A-timeout", candidate_bin, BACKGROUND_TIMEOUT_SECS)
    server.resume()
    server.prompt("timeout")
    server.running_task()
    timeout = server.await_new_completion(3, "system", "configured background timeout")
    server.settle()
    server.drain()
    entries = server.history()
    server.stop()

    journal.extend(entries, evidence["stage"])
    fixture_record["sources"] = {
        "normal": normal.get("cancellation_source"),
        "user": user.get("cancellation_source"),
        "agent": agent.get("cancellation_source"),
        "timeout": timeout.get("cancellation_source"),
    }
    fixture_record["normal_entry"] = {k: v for k, v in normal.items() if k != "type"}
    fixture_record["user_entry"] = {k: v for k, v in user.items() if k != "type"}
    fixture_record["timeout_entry"] = {k: v for k, v in timeout.items() if k != "type"}
    fixture_record["completions"] = len([e for e in entries if e.get("type") == "background_completion"])
    return fixture_record


def stage_old(fixture, old_binary, journal, evidence):
    """B: old 60220f7 resumes/reads the candidate rows, then appends its own."""
    evidence["stage"] = "B old(%s)" % OLD_COMMIT_FULL[:7]
    server = fixture.start_server("B-old", old_binary, 0)
    meta = server.resume()
    before = server.history()
    dropped = journal.verify_prefix(before, "old", evidence["stage"] + " resume read")

    server.prompt("legacy")
    legacy_task = server.running_task()
    server.cancel(legacy_task["id"])
    legacy = server.await_new_completion(
        len([e for e in before if e.get("type") == "background_completion"]), None, "legacy API cancel (old binary)")
    server.settle()
    server.prompt("old-normal")
    old_normal = server.await_new_completion(
        len([e for e in before if e.get("type") == "background_completion"]) + 1, None, "legacy normal exit (old binary)")
    server.settle()
    server.drain()
    after = server.history()
    server.stop()

    journal.verify_prefix(after, "old", evidence["stage"] + " append read")
    tail = after[len(journal.entries):]
    assert tail, "%s: old stage appended no entries" % evidence["stage"]
    assert legacy.get("cancellation_source") is None, "old binary recorded a cancellation source"
    assert old_normal.get("cancellation_source") is None, "old binary recorded a cancellation source"
    return {
        "resume_meta": meta,
        "old_view_dropped_optional_field": dropped,
        "old_view_verdict": (
            "old reader matched every canonical entry field-for-field; it omits only the optional "
            "cancellation_source key on %d sourced rows (recorded above)" % len(dropped)
        ),
        "old_read_tail": tail,
        "old_appended": [
            {"type": e.get("type"), "id": e.get("id"),
             "cancellation_source": e.get("cancellation_source"), "output": e.get("output")}
            for e in tail
        ],
    }


def stage_new_after_old(fixture, candidate_bin, journal, evidence, old_stage):
    """C: candidate reads the old-appended rows, appends a user cancel + live task."""
    evidence["stage"] = "C new(candidate) after old"
    server = fixture.start_server("C-new", candidate_bin, 0)
    server.resume()
    entries = server.history()
    dropped = journal.verify_prefix(entries, "new", evidence["stage"] + " resume read")
    assert not dropped, "candidate view lost canonical entries"
    # The old binary's own appends must read back identically to what it wrote.
    old_tail = old_stage["old_read_tail"]
    new_tail = entries[len(journal.entries):len(journal.entries) + len(old_tail)]
    assert len(new_tail) == len(old_tail), \
        "candidate missed %d entries appended by the old binary" % (len(old_tail) - len(new_tail))
    assert new_tail == old_tail, (
        "old-written entries read back differently under the candidate:\nold=%s\nnew=%s"
        % (json.dumps(old_tail, sort_keys=True, ensure_ascii=False)[:800],
           json.dumps(new_tail, sort_keys=True, ensure_ascii=False)[:800]))
    journal.extend(entries, evidence["stage"] + " resume read")

    sources = [e.get("cancellation_source") for e in entries if e.get("type") == "background_completion"]
    assert sources.count("user") == 1 and sources.count("agent") == 1 and sources.count("system") == 1, sources
    assert sources.count(None) == 3, (
        "normal exit and both old-binary rows must stay source-less: %s" % sources)
    evidence["sources_after_old"] = sources

    server.prompt("user2")
    task = server.running_task()
    server.cancel(task["id"])
    server.await_new_completion(len([e for e in entries if e.get("type") == "background_completion"]), "user", "second user cancel")
    server.settle()
    journal.extend(server.history(), evidence["stage"] + " user2 append")

    server.prompt("live")
    live = server.running_task()
    evidence["live_task"] = {"id": live["id"], "command": live.get("full_command")}
    return server


def run_stages(args, fixture, evidence, run_dir):
    candidate_bin = str(Path(args.candidate_binary).resolve())
    old_bin = str(Path(args.old_binary).resolve())
    journal = Journal()
    evidence["stage_a"] = stage_new_initial(fixture, candidate_bin, journal, evidence)
    old_stage = stage_old(fixture, old_bin, journal, evidence)
    evidence["stage_b"] = old_stage
    server = stage_new_after_old(fixture, candidate_bin, journal, evidence, old_stage)

    # Browser assertion with a real live task holding the panel open, then that
    # live task is cancelled through the API (another real user source row).
    chrome = args.chrome or next((p for p in DEFAULT_CHROME if Path(p).is_file()), "")
    pythonpath = args.pythonpath or next((p for p in DEFAULT_PLAYWRIGHT_PATHS if Path(p).is_dir()), "")
    evidence["browser_config"] = {"mode": args.browser, "chrome": chrome, "pythonpath": pythonpath}
    if args.browser == "skip":
        result = "skipped by --browser skip (downgrade requested on the command line)"
    else:
        result = browser_check(server, chrome, pythonpath, args.browser == "require", run_dir)
    if result.startswith("passed"):
        evidence["results"].append("browser: " + result)
    else:
        evidence["unverified"].append("browser: " + result)

    entries = server.history()
    journal.extend(entries, "C browser stage read")
    live_ids = [t["id"] for t in server.session_tasks()]
    assert live_ids, "live task vanished before the post-browser append"
    server.cancel(live_ids[0])
    server.await_new_completion(len([e for e in entries if e.get("type") == "background_completion"]), "user", "post-browser user cancel")
    server.settle()
    journal.extend(server.history(), "C live-task cancel append")
    server.drain()
    server.stop()

    # D: restart on the same isolated DB -> resume, read, append, read.
    evidence["stage"] = "D restart(candidate) + append"
    server = fixture.start_server("D-restart", candidate_bin, 0)
    server.resume()
    entries = server.history()
    journal.verify_exact(entries, "D resume read")
    before = len([e for e in entries if e.get("type") == "background_completion"])
    server.prompt("normal2")
    final_normal = server.await_new_completion(before, None, "final normal exit")
    server.settle()
    journal.extend(server.history(), "D append")
    server.drain()
    server.stop()
    evidence["stage_d"] = {"final_normal_source": final_normal.get("cancellation_source")}

    # D-final: a further restart must read the frozen end state identically,
    # and the finished-task API must expose the same rows and sources.
    evidence["stage"] = "D-final restart(candidate)"
    server = fixture.start_server("D-final", candidate_bin, 0)
    server.resume()
    entries = server.history()
    journal.verify_exact(entries, "D-final resume read")
    _status, rows = request(server.api("/api/tasks/finished"), server.token)
    session_rows = [r for r in rows if r.get("session_id") == SESSION_ID]
    session_rows.sort(key=lambda r: r.get("seq"))
    seqs = [r.get("seq") for r in session_rows]
    assert len(set(seqs)) == len(seqs), "finished rows are not one row per seq: %s" % seqs
    completions = [e for e in entries if e.get("type") == "background_completion"]
    assert len(session_rows) == len(completions), (
        "finished-task API has %d rows for %s, history has %d completions"
        % (len(session_rows), SESSION_ID, len(completions)))
    checked = 0
    pairs = []
    for entry, row in zip(completions, session_rows):
        key = "seq=%s id=%s" % (row.get("seq"), row.get("id"))
        for field in ("id", "output", "label", "started_at_ms", "duration_ms", "exit_code", "signal", "status", "kind"):
            assert row.get(field) == entry.get(field), \
                "finished row %s field %s: api=%r entry=%r" % (key, field, row.get(field), entry.get(field))
        assert row.get("cancellation_source") == entry.get("cancellation_source"), \
            "finished row %s source: api=%r entry=%r" % (key, row.get("cancellation_source"), entry.get("cancellation_source"))
        pairs.append({"seq": row.get("seq"), "id": row.get("id"), "source": row.get("cancellation_source"),
                      "status": row.get("status"), "label": row.get("label")})
        checked += 1
    evidence["finished_rows"] = pairs
    server.drain()
    server.stop()

    evidence["finished_rows_checked"] = checked
    evidence["entry_count"] = len(journal.entries)
    evidence["sources_final"] = [e.get("cancellation_source") for e in entries if e.get("type") == "background_completion"]
    evidence["journal_stages"] = journal.stages
    evidence["entry_order"] = [
        {"index": index, "type": e.get("type"), "id": e.get("id"),
         "source": e.get("cancellation_source"), "label": e.get("label")}
        for index, e in enumerate(entries)
    ]
    evidence["results"].append(
        "durable chain new->old(%s)->new->restart->final: %d entries preserved in order across %d appends"
        % (OLD_COMMIT_FULL[:7], len(journal.entries), len(journal.stages)))
    return journal


def run(args):
    started = time.time()
    run_dir = Path(args.artifacts_root).resolve() / time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
    run_dir.mkdir(parents=True, exist_ok=True)
    fixture_root = Path(tempfile.mkdtemp(prefix="fixture-", dir=run_dir))
    evidence = {
        "harness": "tests/e2e/task_cancel_source_greptime.py",
        "session_id": SESSION_ID,
        "expect_old_commit": args.expect_old_commit,
        "run_dir": str(run_dir),
        "fixture_root": str(fixture_root),
        "results": [],
        "unverified": [],
        "blockers": [],
    }
    fixture = Fixture(fixture_root, str(Path(args.greptimedb_bin).resolve()))
    keep = args.keep
    try:
        evidence["worktree"] = git_info(args.worktree)
        evidence["greptimedb"] = {
            "path": str(Path(args.greptimedb_bin).resolve()),
            "version": subprocess.run([args.greptimedb_bin, "--version"], stdout=subprocess.PIPE,
                                      stderr=subprocess.STDOUT, text=True).stdout.strip().replace("\n", " | "),
        }
        evidence["candidate"] = binary_identity(args.candidate_binary, args.candidate_src, None, "candidate")
        evidence["old"] = binary_identity(args.old_binary, args.old_src, args.expect_old_commit, "old")
        assert evidence["old"]["src"]["head"] == args.expect_old_commit, (
            "old source HEAD %s != expected full commit %s" % (evidence["old"]["src"]["head"], args.expect_old_commit))

        evidence["ports"] = {
            "greptime_http": fixture.db_http, "greptime_grpc": fixture.db_grpc,
            "greptime_mysql": fixture.db_mysql, "greptime_pg": fixture.db_pg,
            "mock_provider": fixture.mock_port,
            "forbidden": sorted(FORBIDDEN_PORTS),
        }
        for label in ("greptime_http", "greptime_grpc", "greptime_mysql", "greptime_pg", "mock_provider"):
            assert evidence["ports"][label] not in FORBIDDEN_PORTS, "reserved port used: %s" % label
        fixture.start_greptime()
        fixture.start_mock()
        log("fixture: greptime=http://127.0.0.1:%d pg=127.0.0.1:%d provider=http://127.0.0.1:%d workspace=%s"
            % (fixture.db_http, fixture.db_pg, fixture.mock_port, fixture.workspace))
        journal = run_stages(args, fixture, evidence, run_dir)
        evidence["journal_entries"] = len(journal.entries)
        evidence["provider_requests"] = list(Provider.seen)
        evidence["provider_step_counts"] = dict(Provider.counts)
    except Exception as error:  # noqa: BLE001 - evidence-first failure path
        keep = True
        evidence["failed"] = "%s: %s" % (type(error).__name__, error)
        raise
    finally:
        failures = fixture.cleanup()
        evidence["blockers"].extend(failures)
        if failures:
            keep = True
        evidence["leftover_marker_processes"] = marker_pids(fixture.marker)
        evidence["duration_seconds"] = round(time.time() - started, 1)
        evidence["status"] = "FAIL" if "failed" in evidence else ("PASS" if not evidence["blockers"] else "PASS-WITH-BLOCKERS")
        (run_dir / "report.json").write_text(json.dumps(evidence, indent=2, sort_keys=True), encoding="utf-8")
        (Path(args.artifacts_root).resolve() / "latest").write_text(str(run_dir) + "\n", encoding="utf-8")
        if not keep:
            shutil.rmtree(fixture_root, ignore_errors=True)
        else:
            log("evidence retained at %s" % run_dir)
    return evidence


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--greptimedb-bin", required=True, help="real standalone greptime binary (isolated --data-home)")
    parser.add_argument("--candidate-binary", required=True, help="rebuilt candidate e-agent binary")
    parser.add_argument("--candidate-src", required=True, help="git checkout the candidate binary was built from")
    parser.add_argument("--old-binary", required=True, help="e-agent binary built from the old commit")
    parser.add_argument("--old-src", required=True, help="git checkout the old binary was built from")
    parser.add_argument("--expect-old-commit", default=OLD_COMMIT_FULL, help="full old commit binary and source must match")
    parser.add_argument("--worktree", default=str(REPO_ROOT), help="worktree whose HEAD/porcelain is recorded in the report")
    parser.add_argument("--artifacts-root", default=str(DEFAULT_ARTIFACTS), help="workspace-local evidence parent")
    parser.add_argument("--keep", action="store_true", help="retain the successful fixture directory as evidence")
    parser.add_argument("--browser", choices=("auto", "skip", "require"), default="auto")
    parser.add_argument("--chrome", default=os.environ.get("EAGENT_CHROME", ""), help="Chromium/headless-shell executable override")
    parser.add_argument("--pythonpath", default=os.environ.get("EAGENT_PLAYWRIGHT_PATH", ""), help="Python path providing playwright")
    args = parser.parse_args()

    for name in ("candidate_binary", "old_binary", "greptimedb_bin", "candidate_src", "old_src"):
        value = getattr(args, name)
        if name.endswith("_bin") or name.endswith("_binary"):
            if not (Path(value).is_file() and os.access(value, os.X_OK)):
                parser.error("--%s is not an executable file: %s" % (name.replace("_", "-"), value))
        elif not Path(value).is_dir():
            parser.error("--%s is not a directory: %s" % (name.replace("_", "-"), value))
    if args.browser == "require":
        chrome = args.chrome or next((p for p in DEFAULT_CHROME if Path(p).is_file()), "")
        if not chrome:
            parser.error("--browser require needs --chrome or a cached EAGENT_CHROME")

    try:
        evidence = run(args)
    except Exception as error:  # noqa: BLE001 - report exact failure
        log("FAIL %s: %s" % (type(error).__name__, error))
        latest = Path(args.artifacts_root).resolve() / "latest"
        if latest.is_file():
            log("run report: %s/report.json" % latest.read_text().strip())
        return 1
    summary = {
        "status": evidence["status"],
        "run_dir": evidence["run_dir"],
        "candidate": evidence["candidate"]["version"],
        "candidate_sha256": evidence["candidate"]["sha256"],
        "old": evidence["old"]["version"],
        "old_sha256": evidence["old"]["sha256"],
        "old_src_head": evidence["old"]["src"]["head"],
        "worktree_head": evidence["worktree"]["head"],
        "worktree_porcelain": evidence["worktree"]["status_porcelain"],
        "entries": evidence.get("entry_count"),
        "sources_final": evidence.get("sources_final"),
        "finished_rows_checked": evidence.get("finished_rows_checked"),
        "unverified": evidence["unverified"],
        "blockers": evidence["blockers"],
    }
    log(json.dumps(summary, indent=2, ensure_ascii=False))
    for line in evidence["results"]:
        log("PASS " + line)
    for line in evidence["unverified"]:
        log("UNVERIFIED " + line)
    return 0 if evidence["status"] == "PASS" else 1


if __name__ == "__main__":
    sys.exit(main())
