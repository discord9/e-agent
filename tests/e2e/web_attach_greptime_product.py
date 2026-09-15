#!/usr/bin/env python3
"""Isolated GreptimeDB product acceptance for web attach using supplied binaries.

This intentionally does not intercept browser requests: Chromium drives the real
web app, server, history endpoint, and SSE endpoint.  A local OpenAI-compatible
provider is the only mock; it controls a real runner's delegate and a held
streaming completion.
"""
import asyncio
import hashlib
import http.client
import json
import os
import signal
import socket
import subprocess
import sys
import threading
import time
import traceback
import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
ARTIFACT_ROOT = ROOT / ".e-agent" / "greptime-web-acceptance"
BINARY = Path(os.environ.get("EAGENT_PRODUCT_BINARY", str(ARTIFACT_ROOT / "bin" / "e-agent-new")))
CHROME = Path(os.environ.get("EAGENT_CHROME", "/home/discord9/.cache/ms-playwright/chromium-1234/chrome-linux64/chrome"))
PREFIX = "PARENT FINAL: held-prefix"
SUFFIX = " and released-suffix."
ANSWER = PREFIX + SUFFIX
CHILD = "CHILD FINAL REPORT"
INITIAL = "PARENT INITIAL REPORT"


ARTIFACT_HASH_EXCLUDE = ("browser-home/", "browser-xdg/")


def sha256(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def safe_env(env):
    # Artifact provenance records isolation overrides, never credential values.
    return {key: env[key] for key in ("HOME", "XDG_CONFIG_HOME", "XDG_STATE_HOME", "PRODUCT_DUMMY_KEY", "RUST_LOG") if key in env and key != "PRODUCT_DUMMY_KEY"}


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def sse_chunk(delta=None, finish=None, ident="mock"):
    choice = {"index": 0, "delta": delta or {}, "finish_reason": finish}
    return "data: " + json.dumps({"id": ident, "object": "chat.completion.chunk", "model": "mock-model", "choices": [choice]}, separators=(",", ":")) + "\n\n"


class ProviderState:
    def __init__(self, log, workspace):
        self.log = log
        self.workspace = workspace
        self.parent_tool_seen = threading.Event()
        self.prefix_sent = threading.Event()
        self.release_suffix = threading.Event()
        self.lock = threading.Lock()
        self.requests = []

    def record(self, kind, body):
        with self.lock:
            self.requests.append({"kind": kind, "body": body, "at": time.time()})
            self.log.write(json.dumps(self.requests[-1], sort_keys=True) + "\n")
            self.log.flush()


class Provider(BaseHTTPRequestHandler):
    state = None

    def log_message(self, fmt, *args):
        return

    def do_POST(self):
        size = int(self.headers.get("content-length", "0"))
        raw = self.rfile.read(size)
        try:
            body = json.loads(raw)
        except Exception:
            body = {"invalid": raw.decode("utf-8", "replace")}
        messages = body.get("messages", []) if isinstance(body, dict) else []
        text = json.dumps(body, ensure_ascii=False)
        user_messages = [str(m.get("content", "")) for m in messages if m.get("role") == "user"]
        user_text = "\n".join(user_messages)
        latest_user = user_messages[-1] if user_messages else ""
        has_delegate_call = any(m.get("tool_calls") for m in messages if m.get("role") == "assistant")
        # The markers are carried through the real runner's provider wire.
        # Classify by user content and actual assistant calls, never by the
        # serialized tool schema (which naturally repeats CHILD_TASK_TRIGGER).
        if user_text == "PARENT_TRIGGER" and not has_delegate_call:
            kind = "parent_initial"
            args = json.dumps({"workspace": self.state.workspace, "task": "CHILD_TASK_TRIGGER"}, separators=(",", ":"))
            # A real two-call batch exercises server/bootstrap pairing across
            # a delegate completion and an ordinary tool result.
            chunks = [
                sse_chunk({"role": "assistant", "tool_calls": [
                    {"index": 0, "id": "call-delegate-product", "type": "function", "function": {"name": "delegate", "arguments": args}},
                    {"index": 1, "id": "call-goal-product", "type": "function", "function": {"name": "get_goal", "arguments": "{}"}},
                ]}),
                sse_chunk({}, "tool_calls"),
            ]
        elif user_text == "CHILD_TASK_TRIGGER":
            kind = "child"
            self.state.record(kind, body)
            # Ensure the parent has actually consumed the delegate tool result
            # before this child becomes a background completion.
            if not self.state.parent_tool_seen.wait(20):
                self.send_error(500, "parent tool result did not arrive")
                return
            chunks = [sse_chunk({"role": "assistant", "content": CHILD}), sse_chunk({}, "stop")]
        elif latest_user == "COMPAT_OLD_APPEND":
            kind = "compat_old"
            chunks = [sse_chunk({"role": "assistant", "content": "COMPAT_OLD_RESPONSE"}), sse_chunk({}, "stop")]
        elif latest_user == "COMPAT_NEW_APPEND":
            kind = "compat_new"
            chunks = [sse_chunk({"role": "assistant", "content": "COMPAT_NEW_RESPONSE"}), sse_chunk({}, "stop")]
        elif CHILD in text:
            kind = "parent_completion"
            chunks = [sse_chunk({"role": "assistant", "content": PREFIX})]
        elif "call-delegate-product" in text:
            kind = "parent_after_tool"
            self.state.parent_tool_seen.set()
            chunks = [sse_chunk({"role": "assistant", "content": INITIAL}), sse_chunk({}, "stop")]
        else:
            kind = "unexpected"
            chunks = [sse_chunk({"role": "assistant", "content": "UNEXPECTED PROVIDER TURN"}), sse_chunk({}, "stop")]
        if kind != "child":
            self.state.record(kind, body)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        for chunk in chunks:
            self.wfile.write(chunk.encode())
            self.wfile.flush()
        if kind == "parent_completion":
            self.state.prefix_sent.set()
            if not self.state.release_suffix.wait(30):
                return
            self.wfile.write(sse_chunk({"content": SUFFIX}).encode())
            self.wfile.write(sse_chunk({}, "stop").encode())
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        else:
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()


def wait_for(path, timeout=20):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if path.exists() and path.read_text().strip():
            return path.read_text().strip()
        time.sleep(.05)
    raise RuntimeError("timed out waiting for " + str(path))


def http_json(port, token, method, path, body=None):
    c = http.client.HTTPConnection("127.0.0.1", port, timeout=20)
    headers = {"Authorization": "Bearer " + token}
    if body is not None:
        headers["Content-Type"] = "application/json"
    c.request(method, path, body=body, headers=headers)
    r = c.getresponse()
    data = r.read()
    c.close()
    return r.status, data


def api_json(port, token, method, path, payload=None):
    body = None if payload is None else json.dumps(payload)
    status, raw = http_json(port, token, method, path, body)
    if not 200 <= status < 300:
        raise RuntimeError("%s %s -> %s: %s" % (method, path, status, raw.decode("utf-8", "replace")))
    return json.loads(raw) if raw else None


def wait_until(pred, timeout=30, note="condition"):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = pred()
        if value:
            return value
        time.sleep(.05)
    raise RuntimeError("timed out waiting for " + note)


def capture_bootstrap(port, token, sid, out):
    """Read the actual server's first SSE event while it is live and held."""
    c = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
    c.request("GET", "/api/sessions/%s/events?view=web" % sid,
              headers={"Authorization": "Bearer " + token, "Accept": "text/event-stream"})
    r = c.getresponse()
    if r.status != 200:
        raise RuntimeError("bootstrap SSE status %s" % r.status)
    # http.client exposes the HTTP/1.1 chunk framing on r.fp. Read through
    # its decoded response API so the captured record is protocol SSE, not
    # transport chunk-size bytes.
    raw = b""
    while b"\n\n" not in raw:
        block = r.read(1)
        if not block:
            raise RuntimeError("SSE closed before bootstrap")
        raw += block
    c.close()
    raw = raw.decode("utf-8")
    out.write_text(raw)
    if not raw.startswith("event: bootstrap\n"):
        raise AssertionError("first actual SSE frame is not bootstrap: " + raw[:160])
    return raw


async def run_basic(run_dir, server_port, token, browser_env):
    from playwright.async_api import async_playwright
    url = "http://127.0.0.1:%d/" % server_port
    browser_errors = []
    requests = []
    second_responses = []

    async def capture_second_response(response):
        if "/history" not in response.url:
            return
        try:
            second_responses.append({"url": response.url, "status": response.status,
                                     "body": (await response.text())})
        except Exception as error:
            second_responses.append({"url": response.url, "status": response.status,
                                     "read_error": str(error)})

    async with async_playwright() as p:
        browser = await p.chromium.launch(executable_path=str(CHROME), headless=True, env=browser_env)
        context = await browser.new_context(viewport={"width": 1280, "height": 900})
        page = await context.new_page()
        page.on("pageerror", lambda exc: browser_errors.append(str(exc)))
        page.on("request", lambda req: requests.append({"method": req.method, "url": req.url}))
        await page.add_init_script("localStorage.setItem('eagent_token', %s)" % json.dumps(token))
        await page.goto(url, wait_until="load")
        await page.wait_for_function("() => Array.isArray(state.lastList)", timeout=15000)
        # Use the product's sidebar create button rather than manufacturing a
        # session by direct API calls.
        await page.click("#sidebarBtn")
        await page.wait_for_selector("#sidebar.open")
        adds = page.locator("button.ws-add")
        if await adds.count() < 1:
            raise AssertionError("product sidebar has no create-session button")
        await adds.first.click()
        await page.wait_for_function("() => !!state.sessionId", timeout=15000)
        sid = await page.evaluate("state.sessionId")
        await page.locator("#promptInput").fill("PARENT_TRIGGER")
        await page.click("#sendBtn")
        # Wait through real runner -> delegate child -> background completion ->
        # real parent request whose provider is holding its suffix.
        await asyncio.to_thread(PROVIDER_STATE.prefix_sent.wait, 30)
        if not PROVIDER_STATE.prefix_sent.is_set():
            raise AssertionError("provider never sent held parent prefix")
        try:
            await page.wait_for_function("t => document.querySelector('#messages').innerText.includes(t)", arg=PREFIX, timeout=15000)
        except Exception:
            (run_dir / "prefix-timeout-dom.json").write_text(json.dumps({
                "messages": await page.locator("#messages").inner_text(),
                "state": await page.evaluate("({id:state.sessionId,status:state.status,source:state.initSource})"),
                "html": await page.content(),
            }, ensure_ascii=False, indent=2))
            raise
        prefix_dom = await page.locator("#messages").inner_text()
        if SUFFIX in prefix_dom:
            raise AssertionError("suffix visible before controlled release")

        # A second actual app attachment fetches actual history and actual SSE;
        # it is not routed or supplied fabricated bootstrap/history data.
        second = await context.new_page()
        second.on("pageerror", lambda exc: browser_errors.append("second: " + str(exc)))
        second.on("response", lambda response: asyncio.create_task(capture_second_response(response)))
        await second.goto(url + "?session=" + sid, wait_until="load")
        await second.wait_for_function("id => state.sessionId === id", arg=sid, timeout=15000)
        await second.wait_for_function("t => document.querySelector('#messages').innerText.includes(t)", arg=PREFIX, timeout=15000)
        second_prefix = await second.locator("#messages").inner_text()
        if SUFFIX in second_prefix:
            raise AssertionError("reattached page saw suffix before release")

        wire = await asyncio.to_thread(capture_bootstrap, server_port, token, sid, run_dir / "wire-bootstrap.sse")
        if PREFIX not in wire:
            raise AssertionError("actual bootstrap does not carry held prefix")
        PROVIDER_STATE.release_suffix.set()
        for target in (page, second):
            await target.wait_for_function("t => document.querySelector('#messages').innerText.includes(t)", arg=ANSWER, timeout=20000)
        final1 = await page.locator("#messages").inner_text()
        final2 = await second.locator("#messages").inner_text()
        second_cards_before_child = await second.locator("#messages details.tool-card").count()
        # Count exact logical answer occurrences, not substrings in source logs.
        tool_cards = await page.locator("#messages details.tool-card").count()
        orphan_cards = await page.locator("#messages details.tool-card").filter(has_text="工具结果").count()
        expected_tool_cards = 2
        checks = {
            "prefix_visible_before_release": PREFIX in prefix_dom and SUFFIX not in prefix_dom,
            "reattach_visible_before_release": PREFIX in second_prefix and SUFFIX not in second_prefix,
            "final_exact_once_original": final1.count(ANSWER) == 1,
            "final_exact_once_reattached": final2.count(ANSWER) == 1,
            "child_final_visible_parent": final1.count(CHILD) == 1,
            "initial_parent_report_visible": final1.count(INITIAL) == 1,
            "two_tool_batch_cards_rendered": tool_cards == expected_tool_cards,
            "no_orphan_tool_result_card": orphan_cards == 0,
            "no_browser_errors": not browser_errors,
        }
        # Completed child must remain readable even after it no longer has a
        # live runner. Discover its real id through actual session metadata.
        status, raw = await asyncio.to_thread(http_json, server_port, token, "GET", "/api/sessions")
        sessions = json.loads(raw)
        children = [s for s in sessions if s.get("parent_session_id") == sid]
        if len(children) != 1:
            raise AssertionError("expected one real child session, got %r" % children)
        child_id = children[0]["id"]
        await second.goto(url + "?session=" + child_id, wait_until="load")
        await second.wait_for_function("id => state.sessionId === id", arg=child_id, timeout=30000)
        try:
            await second.wait_for_function("t => document.querySelector('#messages').innerText.includes(t)", arg=CHILD, timeout=30000)
        except Exception:
            # A just-finished child can race the deep-link event endpoint;
            # preserve the real browser state and retry the product navigation.
            (run_dir / "child-navigation-timeout.json").write_text(json.dumps({"messages": await second.locator("#messages").inner_text(), "state": await second.evaluate("({id:state.sessionId,status:state.status})")}, indent=2))
            await second.goto(url + "?session=" + child_id, wait_until="load")
            await second.wait_for_function("t => document.querySelector('#messages').innerText.includes(t)", arg=CHILD, timeout=30000)
        child_dom = await second.locator("#messages").inner_text()
        checks["finished_child_history_readable"] = child_dom.count(CHILD) == 1
        # The delegate tool card uses a real runner event; child transcript is
        # rendered independently after it has finished.
        checks["parent_child_order"] = final1.find(CHILD) < final1.find(ANSWER)
        checks["no_browser_errors_after_child_navigation"] = not browser_errors
        payload = {
            "session_id": sid, "child_session_id": child_id, "checks": checks,
            "dom": {"prefix_original": prefix_dom, "prefix_reattached": second_prefix,
                    "parent_final_original": final1, "parent_final_second_page": final2,
                    "child_final": child_dom},
            "cards": {"original_count": tool_cards, "original_orphan_count": orphan_cards,
                      "second_count_before_child_navigation": second_cards_before_child},
            "browser_requests": requests, "second_page_history_responses": second_responses,
            "browser_errors": browser_errors,
        }
        (run_dir / "dom.json").write_text(json.dumps(payload, ensure_ascii=False, indent=2) + "\n")
        await browser.close()
    return payload


async def run_gap(run_dir, server_port, token, browser_env):
    """Actual persisted >2-page history, loaded then reconnected as a live runner."""
    from playwright.async_api import async_playwright
    # Each marker is its own real runner turn, yielding 606 durable message
    # entries: initial 200-entry head plus two already-loaded older pages.
    records = ["GAP-%03d" % n for n in range(302)]
    turns = records
    sid = api_json(server_port, token, "POST", "/api/sessions", {"initial_prompt": turns[0]})["id"]
    # Product persistence reader used solely to wait for runner completion.
    def durable_count():
        cursor, count = None, 0
        while True:
            query = "?limit=200" + (("&before_seq=%s" % cursor) if cursor is not None else "")
            page = api_json(server_port, token, "GET", "/api/sessions/%s/history%s" % (sid, query))
            count += len(page.get("entries", []))
            cursor = page.get("next_before_seq")
            if cursor is None:
                return count
    wait_until(lambda: durable_count() >= 2, timeout=20, note="first gap turn durable")
    # Wait between submissions so web's queue batches cannot combine markers
    # into one User entry; every turn is a real runner/provider turn.
    for index, text in enumerate(turns[1:], 1):
        api_json(server_port, token, "POST", "/api/sessions/%s/prompt" % sid, {"text": text})
        wait_until(lambda: durable_count() >= (index + 1) * 2, timeout=20, note="gap turn %d durable" % index)
    wait_until(lambda: durable_count() >= len(turns) * 2, timeout=20, note="all gap turns durable")
    url = "http://127.0.0.1:%d/?session=%s" % (server_port, sid)
    errors, history = [], []
    async with async_playwright() as p:
        browser = await p.chromium.launch(executable_path=str(CHROME), headless=True, env=browser_env)
        context = await browser.new_context(viewport={"width": 1280, "height": 900})
        page = await context.new_page()
        page.on("pageerror", lambda e: errors.append(str(e)))
        async def response(response):
            if "/history" in response.url:
                try: history.append({"url": response.url, "status": response.status, "body": await response.text()})
                except Exception as e: history.append({"url": response.url, "error": str(e)})
        page.on("response", lambda r: asyncio.create_task(response(r)))
        await page.add_init_script("localStorage.setItem('eagent_token', %s)" % json.dumps(token))
        await page.goto(url, wait_until="load")
        await page.wait_for_function("id => state.sessionId === id", arg=sid, timeout=30000)
        await page.wait_for_function("() => document.querySelector('#messages').innerText.includes('GAP-301')", timeout=30000)
        # Invoke the shipped UI's loadOlder function. It performs its own
        # authenticated /history request and rendering; no frame/history is
        # supplied by this harness. This avoids a hidden load-more affordance
        # being pruned from an intentionally large DOM.
        for _ in range(3):
            cursor = await page.evaluate("state.nextBeforeSeq")
            await page.evaluate("loadOlder()")
            await page.wait_for_function("c => state.nextBeforeSeq !== c && !state.loadingOlder", arg=cursor, timeout=30000)
            await page.wait_for_timeout(300)
        await page.wait_for_function("() => document.querySelector('#messages').innerText.includes('GAP-000')", timeout=30000)
        before = await page.locator("#messages").inner_text()
        # Reconnect actual SSE through the UI's public function; this forces
        # bootstrap reconciliation while earlier pages are retained.
        await page.evaluate("restartTransport()")
        await page.wait_for_function("() => state.initSource === 'history'", timeout=30000)
        bootstrap_one = await asyncio.to_thread(capture_bootstrap, server_port, token, sid, run_dir / "gap-bootstrap-reconnect-1.sse")
        after = await page.locator("#messages").inner_text()
        await page.evaluate("restartTransport()")
        await page.wait_for_function("() => state.initSource === 'history'", timeout=30000)
        bootstrap_two = await asyncio.to_thread(capture_bootstrap, server_port, token, sid, run_dir / "gap-bootstrap-reconnect-2.sse")
        after_second = await page.locator("#messages").inner_text()
        await asyncio.sleep(.1)
        await browser.close()
    expected = records
    def order_ok(text):
        return all(text.find(expected[i]) < text.find(expected[i + 1]) for i in range(len(expected) - 1))
    checks = {"three_or_more_actual_history_pages": sum("before_seq=" in x.get("url", "") for x in history) >= 3,
              "all_records_before_reconnect": all(x in before for x in expected),
              "all_records_after_reconnect": all(x in after for x in expected),
              "all_records_after_second_reconnect": all(x in after_second for x in expected),
              "exact_once_after_reconnect": all(after.count(x) == 1 for x in expected),
              "exact_once_after_second_reconnect": all(after_second.count(x) == 1 for x in expected),
              "order_after_reconnect": order_ok(after), "order_after_second_reconnect": order_ok(after_second),
              "actual_bootstrap_after_each_reconnect": "event: bootstrap\n" in bootstrap_one and "event: bootstrap\n" in bootstrap_two,
              "no_browser_errors": not errors}
    payload = {"session_id": sid, "checks": checks, "history_responses": history,
               "bootstrap_receipts": {"reconnect_1": bootstrap_one, "reconnect_2": bootstrap_two},
               "dom": {"before_reconnect": before, "after_reconnect": after, "after_second_reconnect": after_second},
               "browser_errors": errors}
    (run_dir / "gap-dom.json").write_text(json.dumps(payload, indent=2) + "\n")
    return payload


async def run_disjoint(run_dir, server_port, token, browser_env):
    """Retain loaded old pages, advance live head while disconnected, then bridge gap."""
    from playwright.async_api import async_playwright
    old = ["OLD-%03d" % n for n in range(302)]
    sid = api_json(server_port, token, "POST", "/api/sessions", {"initial_prompt": old[0]})["id"]
    def durable_count():
        cursor, count = None, 0
        while True:
            q = "?limit=200" + (("&before_seq=%s" % cursor) if cursor is not None else "")
            data = api_json(server_port, token, "GET", "/api/sessions/%s/history%s" % (sid, q))
            count += len(data.get("entries", [])); cursor = data.get("next_before_seq")
            if cursor is None: return count
    wait_until(lambda: durable_count() >= 2, 20, "first old turn")
    for n, text in enumerate(old[1:], 1):
        api_json(server_port, token, "POST", "/api/sessions/%s/prompt" % sid, {"text": text})
        wait_until(lambda: durable_count() >= (n + 1) * 2, 20, "old turn %d" % n)
    url = "http://127.0.0.1:%d/?session=%s" % (server_port, sid)
    history, errors = [], []
    async with async_playwright() as p:
        browser = await p.chromium.launch(executable_path=str(CHROME), headless=True, env=browser_env)
        context = await browser.new_context(viewport={"width": 1280, "height": 900})
        page = await context.new_page(); page.on("pageerror", lambda e: errors.append(str(e)))
        async def seen(response):
            if "/history" in response.url:
                try: history.append({"url": response.url, "status": response.status, "body": await response.text()})
                except Exception as e: history.append({"url": response.url, "error": str(e)})
        page.on("response", lambda r: asyncio.create_task(seen(r)))
        await page.add_init_script("localStorage.setItem('eagent_token', %s)" % json.dumps(token))
        await page.goto(url, wait_until="load")
        await page.wait_for_function("() => document.querySelector('#messages').innerText.includes('OLD-301')", timeout=30000)
        # Load the two actual old pages before disconnecting transport.
        for _ in range(3):
            cursor = await page.evaluate("state.nextBeforeSeq")
            if cursor is None: break
            await page.evaluate("loadOlder()")
            await page.wait_for_function("c => state.nextBeforeSeq !== c && !state.loadingOlder", arg=cursor, timeout=30000)
        await page.wait_for_function("() => document.querySelector('#messages').innerText.includes('OLD-000')", timeout=30000)
        before_disconnect = await page.locator("#messages").inner_text()
        await page.evaluate("stopSSE(); stopPolling()")
        # Generate >2 new-head intervals through real provider/runner turns
        # while browser retains old loaded DOM and has no stream attached.
        fresh = ["NEW-%03d" % n for n in range(202)]
        for n, text in enumerate(fresh):
            api_json(server_port, token, "POST", "/api/sessions/%s/prompt" % sid, {"text": text})
            wait_until(lambda n=n: durable_count() >= (len(old) + n + 1) * 2, 20, "new turn %d" % n)
        # Product reconnect bootstrap advances the bounded head. The client now
        # owns older pages and must drain webGapCursor before its oldest cursor.
        await page.evaluate("restartTransport()")
        await page.wait_for_function("() => state.initSource === 'history' && document.querySelector('#messages').innerText.includes('NEW-201')", timeout=30000)
        gap_cursor = await page.evaluate("state.webGapCursor")
        for _ in range(3):
            cursor = await page.evaluate("state.webGapCursor !== null ? state.webGapCursor : state.nextBeforeSeq")
            if cursor is None: break
            await page.evaluate("loadOlder()")
            await page.wait_for_function("c => (state.webGapCursor !== c && state.nextBeforeSeq !== c) && !state.loadingOlder", arg=cursor, timeout=30000)
        await page.wait_for_function("() => document.querySelector('#messages').innerText.includes('OLD-000')", timeout=30000)
        after_bridge = await page.locator("#messages").inner_text()
        await page.evaluate("restartTransport()")
        await page.wait_for_function("() => state.initSource === 'history'", timeout=30000)
        await page.wait_for_timeout(300)
        after_second = await page.locator("#messages").inner_text()
        await asyncio.sleep(.1); await browser.close()
    markers = old + fresh
    ordered = lambda text: all(text.find(markers[i]) < text.find(markers[i+1]) for i in range(len(markers)-1))
    checks = {"old_pages_loaded_before_disconnect": all(x in before_disconnect for x in old),
              "advanced_head_while_disconnected": gap_cursor is not None,
              "actual_gap_history_requests": sum("before_seq=" in x.get("url", "") for x in history) >= 4,
              "full_records_after_gap_bridge": all(x in after_bridge for x in markers),
              "full_records_after_second_reconnect": all(x in after_second for x in markers),
              "exact_once_after_gap_bridge": all(after_bridge.count(x) == 1 for x in markers),
              "exact_once_after_second_reconnect": all(after_second.count(x) == 1 for x in markers),
              "chronological_after_gap_bridge": ordered(after_bridge), "chronological_after_second_reconnect": ordered(after_second),
              "no_browser_errors": not errors}
    payload = {"session_id": sid, "gap_cursor_after_reconnect": gap_cursor, "checks": checks,
               "history_responses": history, "dom": {"before_disconnect": before_disconnect, "after_gap_bridge": after_bridge, "after_second_reconnect": after_second}, "browser_errors": errors}
    (run_dir / "disjoint-gap-dom.json").write_text(json.dumps(payload, indent=2) + "\n")
    return payload


def main():
    parser = argparse.ArgumentParser(description="real --serve web attach product acceptance")
    parser.add_argument("--case", choices=("basic", "gap", "disjoint"), default="basic")
    parser.add_argument("--conn", required=True, help="isolated localhost Greptime PostgreSQL connection")
    args = parser.parse_args()
    if "127.0.0.1" not in args.conn:
        print("BLOCKED: --conn must name isolated localhost GreptimeDB", file=sys.stderr)
        return 2
    if not BINARY.is_file():
        print("BLOCKED: candidate binary unavailable: %s" % BINARY, file=sys.stderr)
        return 2
    if not CHROME.is_file():
        print("BLOCKED: supplied Chromium unavailable: %s" % CHROME, file=sys.stderr)
        return 2
    stamp = "%s-%s" % (args.case, time.strftime("run-%Y%m%dT%H%M%SZ", time.gmtime()))
    run_dir = ARTIFACT_ROOT / stamp
    run_dir.mkdir(parents=True, exist_ok=False)
    home, xdg_config, xdg_state, workspace, browser_home, browser_xdg = (run_dir / x for x in ("home", "config", "state", "workspace", "browser-home", "browser-xdg"))
    for d in (home, xdg_config / "e-agent", xdg_state, workspace, browser_home, browser_xdg): d.mkdir(parents=True, exist_ok=True)
    provider_port, server_port = free_port(), free_port()
    (xdg_config / "e-agent" / "config.toml").write_text(
        'default = "mock/product"\n[providers.mock]\nbase_url = "http://127.0.0.1:%d/v1"\napi_key_env = "PRODUCT_DUMMY_KEY"\n[models."mock/product"]\nmodel = "mock-model"\n[session]\nbackend = "greptime"\nconn = "%s"\n' % (provider_port, args.conn))
    provider_log = open(run_dir / "provider-requests.jsonl", "w", encoding="utf-8")
    global PROVIDER_STATE
    PROVIDER_STATE = ProviderState(provider_log, str(workspace))
    Provider.state = PROVIDER_STATE
    provider = ThreadingHTTPServer(("127.0.0.1", provider_port), Provider)
    threading.Thread(target=provider.serve_forever, daemon=True).start()
    env = os.environ.copy()
    env.update({"HOME": str(home), "XDG_CONFIG_HOME": str(xdg_config), "XDG_STATE_HOME": str(xdg_state),
                "PRODUCT_DUMMY_KEY": "dummy-local-only", "RUST_LOG": "info"})
    browser_env = os.environ.copy()
    browser_env.update({"HOME": str(browser_home), "XDG_CONFIG_HOME": str(browser_xdg), "XDG_STATE_HOME": str(browser_xdg / "state")})
    argv = [str(BINARY), "--serve", "--host", "127.0.0.1", "--port", str(server_port), "--workspace", str(workspace)]
    provenance = {"case": args.case, "binary": {"resolved_path": str(BINARY.resolve()), "sha256_before": sha256(BINARY)},
                  "server_argv": argv, "server_isolation_overrides": safe_env(env), "greptime_conn": args.conn,
                  "browser": {"executable": str(CHROME.resolve()), "sha256": sha256(CHROME), "isolation_overrides": safe_env(browser_env)}}
    (run_dir / "provenance-before.json").write_text(json.dumps(provenance, indent=2) + "\n")
    proc = subprocess.Popen(argv, env=env, cwd=str(workspace),
                            stdout=open(run_dir / "server.stdout.log", "w"), stderr=open(run_dir / "server.stderr.log", "w"))
    outcome = None
    try:
        token = wait_for(xdg_state / "e-agent" / "server.token")
        runner = {"basic": run_basic, "gap": run_gap, "disjoint": run_disjoint}[args.case]
        outcome = asyncio.run(runner(run_dir, server_port, token, browser_env))
        if not all(outcome["checks"].values()):
            raise AssertionError("failed checks: " + repr(outcome["checks"]))
        result = {"status": "passed", "run_dir": str(run_dir), "checks": outcome["checks"]}
    except Exception as exc:
        traceback.print_exc()
        result = {"status": "failed", "run_dir": str(run_dir), "error": str(exc)}
    finally:
        PROVIDER_STATE.release_suffix.set()
        provider.shutdown()
        provider.server_close()
        if proc.poll() is None:
            proc.send_signal(signal.SIGINT)
            try: proc.wait(timeout=5)
            except subprocess.TimeoutExpired: proc.kill(); proc.wait()
        provider_log.close()
        # This run-local bearer token is not evidence and must not survive in
        # the artifact bundle; DOM/API evidence is already captured.
        token_path = xdg_state / "e-agent" / "server.token"
        if token_path.exists():
            token_path.unlink()
        provenance["binary"]["sha256_after"] = sha256(BINARY)
        provenance["binary"]["unchanged"] = provenance["binary"]["sha256_before"] == provenance["binary"]["sha256_after"]
        (run_dir / "provenance-after.json").write_text(json.dumps(provenance, indent=2) + "\n")
        result["provenance"] = {"binary_sha256_before": provenance["binary"]["sha256_before"], "binary_unchanged": provenance["binary"]["unchanged"]}
        hashes = {}
        for path in sorted(run_dir.rglob("*")):
            rel = str(path.relative_to(run_dir))
            if path.is_file() and path.name != "manifest.json" and not rel.startswith(ARTIFACT_HASH_EXCLUDE):
                hashes[rel] = hashlib.sha256(path.read_bytes()).hexdigest()
        result["sha256"] = hashes
        (run_dir / "manifest.json").write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps(result, ensure_ascii=False, indent=2))
    return 0 if result["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
