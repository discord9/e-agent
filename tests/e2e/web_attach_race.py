#!/usr/bin/env python3
"""Chromium transport-boundary reproduction for Web session reattach.

This fixture uses valid SessionEntry history and valid AgentEvent serde snapshot
objects.  Its barriers model the only relevant race without timing guesses:

  history read -> completion commits -> browser attaches SSE snapshot

The current client deliberately has no exact reconciliation for that interval:
when history rendered first, it ignores snapshot non-assistant events.  The
between-history-and-attach check is therefore expected to fail before a
transport-boundary design exists.  No text-based deduplication is tested or
suggested here.
"""
import asyncio
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import common

ALREADY = "already-parent"
PARENT = "between-parent"
CHILD = "child-historical"
OTHER = "other"
GAP = "gap-parent"
FINAL = "FINAL CHILD REPORT"
DISPLAY = "LIVE DISPLAY RETAINED"


def assistant(text):
    return {"type": "message", "message": {"Assistant": {
        "content": text, "tool_calls": [], "reasoning": None}}}


def completion():
    return {"type": "background_completion", "id": 17,
            "output": "subagent session: child-historical\n" + FINAL,
            "label": "delegate"}


def completion_notice_event():
    # Exact AgentEvent serde shape: #[serde(tag="type", content="data",
    # rename_all="snake_case")].  It is intentionally not a SessionEntry.
    return {"type": "background_completion_notice", "data": {
        "id": 17,
        "output": "subagent session: child-historical\n" + FINAL,
        "label": "delegate",
    }}


def sse_event(name, data):
    return "event: %s\ndata: %s\n\n" % (name, json.dumps(data))

def locations(entries):
    # Wire-level WebEntryLocation shape: backend + existing physical key only.
    # The same fixture entry keeps the same physical identity across pages.
    return [{"backend": "jsonl", "key": {"Jsonl": {"ordinal": id(entry)}}}
            for entry in entries]


async def main():
    from playwright.async_api import async_playwright

    sessions = [
        {"id": ALREADY, "model": "flash", "role": "main", "status": "Busy", "busy": True,
         "active": True, "parent_session_id": None, "title": "already", "entry_count": 3,
         "created_at": "2026-01-01T00:00:00Z"},
        {"id": PARENT, "model": "flash", "role": "main", "status": "Busy", "busy": True,
         "active": True, "parent_session_id": None, "title": "between", "entry_count": 3,
         "created_at": "2026-01-01T00:00:00Z"},
        {"id": CHILD, "model": "flash", "role": "fixer", "status": "Finished", "busy": False,
         "active": False, "parent_session_id": PARENT, "title": "child", "entry_count": 2,
         "created_at": "2026-01-01T00:00:00Z"},
        {"id": OTHER, "model": "flash", "role": "main", "status": "Idle", "busy": False,
         "active": True, "parent_session_id": None, "title": "other", "entry_count": 1,
         "created_at": "2026-01-01T00:00:00Z"},
        {"id": GAP, "model": "flash", "role": "main", "status": "Busy", "busy": True,
         "active": True, "parent_session_id": None, "title": "gap", "entry_count": 600,
         "created_at": "2026-01-01T00:00:00Z"},
    ]
    two_calls = {"type": "message", "message": {"Assistant": {
        "content": None, "reasoning": None, "tool_calls": [
            {"id": "pair-a", "name": "read_file", "arguments": "{\"path\":\"a.txt\"}"},
            {"id": "pair-b", "name": "read_file", "arguments": "{\"path\":\"b.txt\"}"},
        ]}}}
    base_history = {
        "entries": [
            two_calls,
            assistant("PERSISTED COMPLETE SEGMENT"),
            # Identical complete outputs are two real logical messages.
            assistant("IDENTICAL COMPLETE OUTPUT"),
            assistant("IDENTICAL COMPLETE OUTPUT"),
            {"type": "compaction", "summary": "retained compaction", "retained": []},
        ],
        "next_before_seq": 40,
    }
    already_history = {"entries": base_history["entries"] + [completion()], "next_before_seq": 40}
    committed_history = {"entries": base_history["entries"] + [completion()], "next_before_seq": 40}
    older = {"entries": [assistant("OLDER HISTORY SURVIVES")], "next_before_seq": None}
    child_tail = {"entries": [assistant("CHILD INTERMEDIATE ONLY")], "next_before_seq": 5}
    child_older = {"entries": [assistant("CHILD OLDER HISTORY")], "next_before_seq": None}
    other_history = {"entries": [assistant("OTHER")], "next_before_seq": None}
    # 0..199 is fully loaded with its true oldest cursor exhausted. The
    # reconnect head jumps to 400..599, leaving a physical 200..399 interval.
    gap_old = {"entries": [assistant("GAP-%03d" % i) for i in range(200)], "next_before_seq": None}
    gap_middle = {"entries": [assistant("GAP-%03d" % i) for i in range(200, 400)], "next_before_seq": None}
    gap_new = {"entries": [assistant("GAP-%03d" % i) for i in range(400, 600)], "next_before_seq": 400}
    gap_event_reads = 0

    # Explicit request barriers, not sleeps: second parent history response is
    # the stale durable read; it commits only after fulfilling that response;
    # its following /events request is held until that commit is observable.
    history_read = asyncio.Event()
    completion_committed = asyncio.Event()
    parent_history_reads = 0
    parent_event_reads = 0
    records = []

    async def route(route):
        nonlocal parent_history_reads, parent_event_reads, gap_event_reads
        url, method = route.request.url, route.request.method
        base = url.split("?", 1)[0].rstrip("/")
        if base == common.BASE.rstrip("/"):
            await route.fulfill(status=200, content_type="text/html; charset=utf-8",
                                body=common.assemble_html())
            return
        if base.endswith("/api/sessions") and method == "GET":
            await route.fulfill(status=200, content_type="application/json", body=json.dumps(sessions))
            return
        if "/usage" in base:
            await route.fulfill(status=404, content_type="application/json", body="{}")
            return
        if "/goal" in base:
            await route.fulfill(status=200, content_type="application/json", body=json.dumps({"goal": None}))
            return
        if "/history" in base:
            records.append("history " + url)
            if "/" + ALREADY + "/history" in base:
                payload = older if "before_seq=" in url else already_history
            elif "/" + PARENT + "/history" in base:
                if "before_seq=" in url:
                    payload = older
                else:
                    parent_history_reads += 1
                    payload = committed_history if parent_history_reads >= 3 else base_history
            elif "/" + CHILD + "/history" in base:
                payload = child_older if "before_seq=" in url else child_tail
            elif "/" + OTHER + "/history" in base:
                payload = other_history
            elif "/" + GAP + "/history" in base:
                payload = gap_middle if "before_seq=400" in url else gap_old
            else:
                payload = {"entries": [], "next_before_seq": None}
            await route.fulfill(status=200, content_type="application/json",
                                body=json.dumps(dict(payload, locations=locations(payload["entries"]))))
            # The second parent history response is now in the browser.  The
            # completion commits after that read and before SSE attach.
            if "/" + PARENT + "/history" in base and "before_seq=" not in url and parent_history_reads == 2:
                history_read.set()
                completion_committed.set()
            return
        if base.endswith("/events"):
            records.append("events " + url)
            if "/" + CHILD + "/events" in base:
                await route.fulfill(status=404, content_type="text/plain", body="historical session")
                return
            # Web attach is one SSE request: its bootstrap frame contains the
            # authoritative head and only runner-owned bridge presentation.
            if "view=web" in url:
                if "/" + ALREADY + "/events" in base:
                    head, replay = already_history, []
                elif "/" + PARENT + "/events" in base:
                    parent_event_reads += 1
                    if parent_event_reads == 2:
                        # The controlled commit occurs after the stale head
                        # is frozen and before the bridge cut is emitted.
                        history_read.set()
                        completion_committed.set()
                        head = base_history
                        replay = [{"event": completion_notice_event(), "after_head": True,
                                   "transient": False}]
                    elif parent_event_reads >= 3:
                        head, replay = committed_history, []
                    else:
                        head, replay = base_history, []
                    if parent_event_reads == 1:
                        # Bootstrap may contain an unmatched call while its
                        # result is the first live frame; pairing must use
                        # the model call id, not arrival order.
                        replay.append({"event": {"type":"tool_call", "data": {
                            "name":"read_file", "arguments":"{\"path\":\"a.txt\"}",
                            "call_id":"pair-a"}}, "after_head": True, "transient": False})
                    replay += [
                        # A fully consumed queue is presentation state, not
                        # transcript history. Every bootstrap folds this pair
                        # from an empty queue and must end empty.
                        {"event": {"type":"prompt_queued", "data":"consumed queued prompt"},
                         "after_head": False, "transient": True},
                        {"event": {"type":"prompt_consumed", "data":None},
                         "after_head": False, "transient": True},
                        {"event": {"type":"display", "data": DISPLAY}, "after_head": False,
                         "transient": True},
                        {"event": {"type":"usage", "data": {"context_input":42,
                         "context_window":100, "session":{"input_tokens":1,"output_tokens":2}}},
                         "after_head": False, "transient": True},
                    ]
                elif "/" + GAP + "/events" in base:
                    gap_event_reads += 1
                    head, replay = (gap_old if gap_event_reads == 1 else gap_new), []
                else:
                    head, replay = other_history, []
                body = sse_event("bootstrap", {"entries": head["entries"],
                    "locations": locations(head["entries"]),
                    "next_before_seq": head["next_before_seq"], "replay": replay})
                body += sse_event("status", {"status":"Busy"})
                if "/" + PARENT + "/events" in base and parent_event_reads == 1:
                    body += sse_event("ToolResult", {"is_error": False, "content": "first result",
                                                       "call_id": "pair-a"})
                    body += sse_event("ToolResult", {"is_error": False, "content": "second result",
                                                       "call_id": "pair-b"})
                await route.fulfill(status=200, headers={"content-type":"text/event-stream"}, body=body)
                return
            await route.fulfill(status=200, headers={"content-type":"text/event-stream"}, body="retry: 3000\n\n")
            return
        if base.endswith("/api/tasks"):
            await route.fulfill(status=200, content_type="application/json", body="[]")
            return
        await route.fulfill(status=200, content_type="application/json", body="{}")

    async with async_playwright() as p:
        browser = await p.chromium.launch(executable_path=common.EXE)
        page = await browser.new_page(viewport={"width": 1280, "height": 900})
        errors = []
        page.on("pageerror", lambda e: errors.append(str(e)))
        await page.add_init_script("localStorage.setItem('eagent_token', 'fixture-token')")
        await page.route("**/*", route)
        await page.goto(common.BASE + "/", wait_until="load")
        await page.reload(wait_until="load")
        await page.wait_for_function("() => Array.isArray(state.lastList) && state.lastList.length === 5")

        async def open_session(sid):
            await page.evaluate("sid => openSession(sid)", sid)
            await page.wait_for_function("sid => state.sessionId === sid && state.initSource === 'history'", arg=sid)

        failures = []
        def check(name, ok, detail=""):
            print("[%s] %s%s" % ("PASS" if ok else "FAIL", name, (" | " + detail) if detail else ""))
            if not ok:
                failures.append(name)

        # Completion already durable: the identical snapshot must not create a
        # second visible logical completion.
        await open_session(ALREADY)
        text = await page.locator("#messages").text_content()
        check("completion already in history plus snapshot renders once", text.count(FINAL) == 1,
              "count=%d" % text.count(FINAL))

        await open_session(PARENT)
        text = await page.locator("#messages").text_content()
        check("identical assistant outputs are retained", text.count("IDENTICAL COMPLETE OUTPUT") == 2,
              "count=%d" % text.count("IDENTICAL COMPLETE OUTPUT"))
        check("valid compaction history retained", "retained compaction" in text)
        check("real live Display retained", DISPLAY in text)
        check("real live Usage retained", "42/100 tok" in await page.locator("#usageInfo").text_content())
        check("consumed queue folds empty on bootstrap", await page.evaluate("state.queue.length === 0"))
        await page.wait_for_function("""() => {
          const cards = [...document.querySelectorAll('.tool-card')].filter((x) =>
            x.textContent.includes('a.txt') || x.textContent.includes('b.txt'));
          return cards.length === 2 && cards.every((card) => card.querySelector('.tool-state').textContent === '完成');
        }""")
        check("bootstrap ToolCall pairs with live ToolResult by call_id",
              await page.locator(".tool-card", has_text="a.txt").count() == 1
              and await page.locator(".tool-card", has_text="b.txt").count() == 1)

        # Cache a genuine in-flight DOM tail, switch away/back, and force the
        # explicit history-read -> commit -> snapshot-attach interval.
        await page.evaluate("appendAssistantDelta('CACHED INCOMPLETE TAIL', state.acc)")
        await open_session(OTHER)
        await open_session(PARENT)
        await page.wait_for_function("() => state.sessionId === 'between-parent'")
        text = await page.locator("#messages").text_content()
        check("reattach does not duplicate complete segments", text.count("PERSISTED COMPLETE SEGMENT") == 1,
              "count=%d" % text.count("PERSISTED COMPLETE SEGMENT"))
        check("reattach drops stale incomplete cached tail", "CACHED INCOMPLETE TAIL" not in text)
        check("barrier history read then completion commit", history_read.is_set() and completion_committed.is_set())
        check("completion between history and attach is visible once", text.count(FINAL) == 1,
              "count=%d text=%r" % (text.count(FINAL), text))

        await page.evaluate("loadOlder()")
        await page.wait_for_function("() => els.messages.textContent.includes('OLDER HISTORY SURVIVES')")
        check("parent pagination remains accessible", "OLDER HISTORY SURVIVES" in await page.locator("#messages").text_content())
        anchor = await page.locator(".msg-assistant", has_text="OLDER HISTORY SURVIVES").evaluate(
            "node => node.dataset.entryLocation")
        await page.evaluate("els.messages.scrollTop = 0; restartTransport()")
        await page.wait_for_function("() => state.sessionId === 'between-parent' && state.initSource === 'history'")
        text = await page.locator("#messages").text_content()
        anchor_after = await page.locator(".msg-assistant", has_text="OLDER HISTORY SURVIVES").evaluate(
            "node => node.dataset.entryLocation")
        check("loaded physical older page survives reconnect once", text.count("OLDER HISTORY SURVIVES") == 1,
              "count=%d" % text.count("OLDER HISTORY SURVIVES"))
        check("consumed queue remains empty after repeated bootstrap", await page.evaluate("state.queue.length === 0"))
        check("loaded older reader anchor keeps its physical identity", anchor and anchor == anchor_after,
              "before=%r after=%r" % (anchor, anchor_after))

        # Separate gap cursor: old 0..199 is already exhausted, then the
        # Web head jumps to 400..599. The 200..399 interval must be fetched
        # without reopening either retained page or losing its reader anchor.
        await open_session(GAP)
        old_anchor = await page.locator(".msg-assistant", has_text="GAP-000").evaluate(
            "node => node.dataset.entryLocation")
        await page.evaluate("els.messages.scrollTop = 0; restartTransport()")
        await page.wait_for_function("() => state.webGapCursor === 400 && state.nextBeforeSeq === null")
        check("disjoint gap retains exhausted oldest cursor",
              await page.evaluate("state.webGapCursor === 400 && state.nextBeforeSeq === null"))
        await page.evaluate("loadOlder()")
        await page.wait_for_function("() => els.messages.textContent.includes('GAP-200') && state.webGapCursor === null")
        gap = await page.evaluate(r"""() => {
          const rows = [...document.querySelectorAll('.msg-assistant')]
            .filter((node) => node.textContent.includes("GAP-"));
          return { count: rows.length, keys: new Set(rows.map((node) => node.dataset.entryLocation)).size,
                   old: rows.find((node) => node.textContent.includes('GAP-000'))?.dataset.entryLocation };
        }""")
        check("disjoint gap loads all 600 existing physical rows once",
              gap["count"] == 600 and gap["keys"] == 600,
              "count=%d keys=%d" % (gap["count"], gap["keys"]))
        check("disjoint gap keeps retained oldest anchor", old_anchor and gap["old"] == old_anchor,
              "before=%r after=%r" % (old_anchor, gap["old"]))

        await open_session(CHILD)
        text = await page.locator("#messages").text_content()
        check("historical child survives events 404", "CHILD INTERMEDIATE ONLY" in text)
        await page.evaluate("loadOlder()")
        await page.wait_for_function("() => els.messages.textContent.includes('CHILD OLDER HISTORY')")
        check("historical child pagination survives events 404", "CHILD OLDER HISTORY" in await page.locator("#messages").text_content())

        # A later durable read includes the flat SessionEntry completion.  The
        # snapshot still overlaps it, and current behavior visibly renders it
        # exactly once because snapshot is skipped after history.
        await open_session(OTHER)
        await open_session(PARENT)
        text = await page.locator("#messages").text_content()
        check("subsequent committed history is flat background_completion once", text.count(FINAL) == 1,
              "count=%d" % text.count(FINAL))
        check("no browser page errors", not errors, repr(errors))
        print("request barriers:", "history_read=%s completion_committed=%s" %
              (history_read.is_set(), completion_committed.is_set()))
        print("requests:", records)
        await browser.close()

    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(asyncio.run(main()))
