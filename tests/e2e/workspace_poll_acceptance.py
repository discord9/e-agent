#!/usr/bin/env python3
"""Playwright DOM acceptance for independent workspace polling.

This deliberately assembles the shipped UI exactly as ``server.rs::assemble`` does,
then mocks only browser routes.  It never starts an e-agent server or accesses a
real workspace/token store.

Run from the repository root:
  uv run --with playwright python tests/e2e/workspace_poll_acceptance.py

Set EAGENT_CHROME if Chromium is installed somewhere other than the documented
Playwright cache path.
"""

import asyncio
import json
import os
import shutil
import sys
import tempfile
import time
from pathlib import Path

try:
    from playwright.async_api import async_playwright
except ImportError as exc:  # pragma: no cover - makes the command's prerequisite clear
    raise SystemExit("Playwright is required; run: uv run --with playwright python " + __file__) from exc


ROOT = Path(__file__).resolve().parents[2]
CHROME = os.environ.get(
    "EAGENT_CHROME",
    "/home/discord9/.cache/ms-playwright/chromium-1234/chrome-linux64/chrome",
)
POLL_WINDOW_SECONDS = 4.0  # deliberately below the UI's 10 second request timeout
SLOW_RESPONSE_SECONDS = 2.5


class AcceptanceFailure(Exception):
    """A recorded DOM assertion failed."""


def assemble_ui() -> str:
    """Mirror the replacement order and asset concatenation in server.rs::assemble."""
    ui = ROOT / "src" / "ui"
    required = [
        "index.html", "style.css", "app.js", "render.js", "sessions.js", "tasks.js",
        "sse.js", "usage-dashboard.js", "pet.html", "vendor/katex.min.css",
        "vendor/marked.min.js", "vendor/uPlot.min.css", "vendor/gridjs.mermaid.min.css",
        "vendor/uPlot.iife.min.js", "vendor/gridjs.umd.js",
    ]
    missing = [name for name in required if not (ui / name).is_file()]
    if missing:
        raise RuntimeError("cannot assemble UI; missing: " + ", ".join(missing))

    def read(name: str) -> str:
        return (ui / name).read_text(encoding="utf-8")

    skeleton = read("index.html")
    app_js = "\n".join(read(name) for name in ("app.js", "render.js", "sessions.js", "tasks.js", "sse.js")) + "\n"
    replacements = {
        "/*__KATEX_CSS__*/": read("vendor/katex.min.css"),
        "/*__USAGE_DASHBOARD_VENDOR_CSS__*/": read("vendor/uPlot.min.css") + "\n" + read("vendor/gridjs.mermaid.min.css"),
        "/*__CSS__*/": read("style.css"),
        "/*__JS_VENDOR__*/": read("vendor/marked.min.js"),
        "/*__USAGE_DASHBOARD_VENDOR_JS__*/": read("vendor/uPlot.iife.min.js") + "\n" + read("vendor/gridjs.umd.js"),
        "/*__JS_APP__*/": app_js,
        "/*__USAGE_DASHBOARD_JS__*/": read("usage-dashboard.js"),
        "<!--__PET__-->": read("pet.html"),
    }
    for placeholder, value in replacements.items():
        if placeholder not in skeleton:
            raise RuntimeError("server.rs UI placeholder absent from index.html: " + placeholder)
        skeleton = skeleton.replace(placeholder, value)
    return skeleton


def session(session_id: str, title: str, status: str = "Idle") -> dict:
    return {
        "id": session_id,
        "title": title,
        "status": status,
        "busy": status == "Busy",
        "active": True,
        "entry_count": 1,
        "last_active_at": "2025-01-01T00:00:00Z",
    }


def live_task(label: str) -> dict:
    return {
        "id": "healthy-task",
        "session_id": "healthy-session",
        "kind": "bash",
        "label": label,
        "command": "echo acceptance",
        "started_at": "2025-01-01T00:00:00Z",
    }


class Checks:
    def __init__(self) -> None:
        self.failed = False

    def emit(self, assertion: str, passed: bool, detail: str = "") -> None:
        print(json.dumps({"assertion": assertion, "passed": passed, "detail": detail}, ensure_ascii=False), flush=True)
        self.failed |= not passed

    async def eventually(self, assertion: str, predicate, timeout: float, detail: str) -> None:
        deadline = time.monotonic() + timeout
        last = ""
        while time.monotonic() < deadline:
            try:
                if await predicate():
                    self.emit(assertion, True, detail)
                    return
            except Exception as exc:  # locators may be transiently rebuilt by polling
                last = repr(exc)
            await asyncio.sleep(0.05)
        message = detail + ("; last error=" + last if last else "")
        self.emit(assertion, False, message)
        raise AcceptanceFailure(message)

    def require(self, assertion: str, condition: bool, detail: str) -> None:
        self.emit(assertion, condition, detail)
        if not condition:
            raise AcceptanceFailure(detail)


async def main() -> int:
    if not Path(CHROME).is_file():
        print(json.dumps({
            "assertion": "chromium prerequisite",
            "passed": False,
            "detail": "Chrome executable is unavailable: " + CHROME,
        }, ensure_ascii=False), flush=True)
        return 2

    html = assemble_ui()
    checks = Checks()
    runtime_parent = ROOT / ".workspace_poll_acceptance-runtime"
    runtime_parent.mkdir(mode=0o700, exist_ok=True)
    temp = tempfile.TemporaryDirectory(prefix="run-", dir=runtime_parent)
    old_env = {key: os.environ.get(key) for key in ("HOME", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME")}
    isolated = Path(temp.name)
    for key, leaf in (("HOME", "home"), ("XDG_CONFIG_HOME", "config"), ("XDG_CACHE_HOME", "cache"), ("XDG_DATA_HOME", "data"), ("XDG_STATE_HOME", "state")):
        path = isolated / leaf
        path.mkdir(mode=0o700)
        os.environ[key] = str(path)

    # Events describe route state, not implementation internals.  They make it
    # explicit that the successful healthy updates happened while the other
    # workspace requests were still unresolved.
    hung_started = asyncio.Event()
    slow_second_started = asyncio.Event()
    slow_fourth_started = asyncio.Event()
    release_hung = asyncio.Event()
    release_slow_fourth = asyncio.Event()
    finished_pending = asyncio.Event()
    release_finished = asyncio.Event()
    healthy_session_times: list[float] = []
    healthy_task_times: list[float] = []
    slow_session_calls = 0
    healthy_task_calls = 0
    # The test moves these stages only after observing each rendered result;
    # requests themselves remain the UI's real 2s timer-driven polls.
    healthy_task_stage = "add"
    healthy_remove_request = asyncio.Event()
    healthy_readd_request = asyncio.Event()

    async def ui_route(route):
        await route.fulfill(status=200, content_type="text/html; charset=utf-8", body=html)

    async def healthy_route(route):
        nonlocal healthy_task_calls
        path = route.request.url.split("healthy.test", 1)[1].split("?", 1)[0]
        if path == "/api/sessions":
            healthy_session_times.append(time.monotonic())
            status = "Busy" if len(healthy_session_times) % 2 else "Idle"
            await route.fulfill(json=[session("healthy-session", "healthy " + status, status)])
        elif path == "/api/sessions/healthy-session/history":
            await route.fulfill(json={"entries": [], "next_before_seq": None})
        elif path == "/api/sessions/healthy-session/events":
            # A minimal valid live SSE snapshot keeps the actual open-session
            # path intact without inventing product state.
            await route.fulfill(
                status=200,
                content_type="text/event-stream",
                body="event: snapshot\ndata: []\n\n",
            )
        elif path == "/api/sessions/healthy-session/goal":
            await route.fulfill(json={"goal": None})
        elif path == "/api/sessions/healthy-session/usage":
            await route.fulfill(json={"input_tokens": 0, "output_tokens": 0})
        elif path == "/api/tasks":
            healthy_task_calls += 1
            healthy_task_times.append(time.monotonic())
            if healthy_task_stage == "remove":
                healthy_remove_request.set()
                tasks = []
            elif healthy_task_stage == "readd":
                healthy_readd_request.set()
                tasks = [live_task("healthy live task")]
            else:
                tasks = [live_task("healthy live task")]
            await route.fulfill(json=tasks)
        elif path == "/api/tasks/finished":
            finished_pending.set()
            await release_finished.wait()
            await route.fulfill(json=[])
        else:
            await route.fulfill(status=404, body="unexpected healthy route")

    async def hung_route(route):
        hung_started.set()
        await release_hung.wait()
        await route.fulfill(status=503, body="closed after test")

    async def slow_route(route):
        nonlocal slow_session_calls
        path = route.request.url.split("slow.test", 1)[1]
        if path == "/api/tasks":
            await route.fulfill(json=[])
            return
        if path == "/api/tasks/finished":
            await route.fulfill(json=[])
            return
        if path != "/api/sessions":
            await route.fulfill(status=404, body="unexpected slow route")
            return
        slow_session_calls += 1
        if slow_session_calls == 1:
            await route.fulfill(json=[session("slow-old", "slow old")])
        elif slow_session_calls == 2:
            slow_second_started.set()
            await asyncio.sleep(SLOW_RESPONSE_SECONDS)
            await route.fulfill(json=[
                session("slow-complete-a", "slow complete A"),
                session("slow-complete-b", "slow complete B"),
            ])
        elif slow_session_calls == 3:
            await route.fulfill(status=503, body="slow workspace temporarily closed")
        else:
            slow_fourth_started.set()
            await release_slow_fourth.wait()
            # A late result must be ignored after the user removes this ws.
            await route.fulfill(json=[session("slow-zombie", "slow zombie")])

    async def text_has(locator, text: str) -> bool:
        return await locator.is_visible() and text in await locator.inner_text()

    async def healthy_status_is(status: str) -> bool:
        section = page.locator(".tree-ws-section").filter(has=page.locator(".ws-chip", has_text="Healthy"))
        node = section.locator(".tree-node").filter(has_text="healthy-session")
        if await node.count() != 1:
            return False
        dot = node.locator(".main-dot")
        classes = await dot.get_attribute("class") or ""
        return ("busy" in classes) == (status == "Busy") and status in await node.inner_text()

    async def is_hidden(locator) -> bool:
        return await locator.is_hidden()

    async def has_no_nodes(locator) -> bool:
        return await locator.count() == 0

    async def event_is_set(event: asyncio.Event) -> bool:
        return event.is_set()

    async def task_bar_shows_one() -> bool:
        return await text_has(task_bar, "运行中任务 (1)")

    async def task_panel_has_live_label() -> bool:
        return await text_has(page.locator("#composerTasks"), "healthy live task")

    async def healthy_orbit_count(expected: int) -> bool:
        section = page.locator(".tree-ws-section").filter(has=page.locator(".ws-chip", has_text="Healthy"))
        node = section.locator(".tree-node").filter(has_text="healthy-session")
        return await node.locator(".orbit-dot").count() == expected

    async def slow_complete_list_is_visible() -> bool:
        return await text_has(slow_section, "slow complete A") and await text_has(slow_section, "slow complete B")

    async def slow_failure_keeps_complete_list() -> bool:
        return (await text_has(slow_section, "无法连接")
                and await text_has(slow_section, "slow complete A")
                and await text_has(slow_section, "slow complete B"))

    async def slow_is_not_revived() -> bool:
        no_section = await page.locator(".tree-ws-section").filter(
            has=page.locator(".ws-chip", has_text="Slow")).count() == 0
        no_zombie = "slow zombie" not in await page.locator("#sidebarTree").inner_text()
        return no_section and no_zombie

    try:
        async with async_playwright() as playwright:
            browser = await playwright.chromium.launch(
                headless=True,
                executable_path=CHROME,
                env=os.environ.copy(),
            )
            context = await browser.new_context()
            # No production credentials: each isolated browser context receives
            # only these mock endpoint tokens before UI startup.
            await context.add_init_script("""
                localStorage.setItem('eagent.workspaces', JSON.stringify([
                  {id:'healthy', name:'Healthy', url:'http://healthy.test', token:'healthy-token'},
                  {id:'hung', name:'Hung', url:'http://hung.test', token:'hung-token'},
                  {id:'slow', name:'Slow', url:'http://slow.test', token:'slow-token'}
                ]));
                localStorage.setItem('eagent.activeWorkspace', 'healthy');
                localStorage.setItem('e-agent.sidebar.open', '1');
            """)
            page = await context.new_page()
            page.on("dialog", lambda dialog: asyncio.create_task(dialog.accept()))
            # Fail closed: the only permitted network origins are installed
            # below.  A new or accidental request cannot escape this fixture.
            await page.route("**/*", lambda route: route.abort())
            await page.route("http://ui.test/**", ui_route)
            await page.route("http://healthy.test/**", healthy_route)
            await page.route("http://hung.test/**", hung_route)
            await page.route("http://slow.test/**", slow_route)
            await page.goto("http://ui.test/", wait_until="domcontentloaded")

            # The sidebar is the real UI condition that enables session polling.
            # Startup restores the persisted open sidebar; do not toggle it shut.
            await page.locator("#sidebar").wait_for(state="visible")
            await checks.eventually(
                "hung workspace request is pending",
                lambda: asyncio.sleep(0, result=hung_started.is_set()),
                2.0,
                "the Hung API route has an unresolved request",
            )
            await checks.eventually(
                "healthy Busy session is rendered while Hung is pending",
                lambda: healthy_status_is("Busy"),
                POLL_WINDOW_SECONDS,
                "healthy Busy row reached the sidebar before the 10s request timeout",
            )
            await checks.eventually(
                "healthy automatically advances Busy to Idle while Hung is pending",
                lambda: healthy_status_is("Idle"),
                POLL_WINDOW_SECONDS,
                "a second automatic 2s session poll changed the visible Healthy row within 4s",
            )
            await checks.eventually(
                "healthy automatically advances Idle back to Busy while Hung is pending",
                lambda: healthy_status_is("Busy"),
                POLL_WINDOW_SECONDS,
                "a third automatic session poll changed the visible Healthy row",
            )
            checks.require(
                "healthy session cadence stays below timeout",
                len(healthy_session_times) >= 3 and healthy_session_times[2] - healthy_session_times[0] < 2 * POLL_WINDOW_SECONDS,
                "three Healthy /api/sessions responses arrived before a Hung request could reach the 10s timeout",
            )

            # Open via the real sidebar row. This uses the product session-open
            # path; it does not force-click or alter no-session CSS/classes.
            healthy_row = page.locator(".tree-ws-section").filter(
                has=page.locator(".ws-chip", has_text="Healthy")).locator(
                ".tree-row").filter(has_text="healthy-session")
            await healthy_row.click()
            await checks.eventually(
                "Healthy session was opened through visible sidebar navigation",
                lambda: page.locator("#chatView").evaluate(
                    "el => !el.classList.contains('no-session') && !!el.offsetParent"),
                2.0,
                "the visible chat view left no-session state through the Healthy row click",
            )
            await checks.eventually(
                "opened Healthy composer is visible",
                lambda: page.locator(".composer").is_visible(),
                2.0,
                "the product session-open flow exposed the composer",
            )

            await checks.eventually(
                "finished request is deliberately pending",
                lambda: asyncio.sleep(0, result=finished_pending.is_set()),
                2.0,
                "Healthy /api/tasks/finished is still held by the mock",
            )
            task_bar = page.locator("#tasksToggleBar")
            await checks.eventually(
                "live task add is visible despite slow finished endpoint",
                task_bar_shows_one,
                POLL_WINDOW_SECONDS,
                "live Healthy task count rendered while /api/tasks/finished remains pending",
            )
            await task_bar.click()
            await checks.eventually(
                "live task label is visible in expanded DOM",
                task_panel_has_live_label,
                1.0,
                "expanded task panel contains the live task",
            )
            await checks.eventually(
                "healthy task orbit has one dot",
                lambda: healthy_orbit_count(1),
                1.0,
                "one live bash task produces exactly one .orbit-dot on Healthy",
            )
            healthy_task_stage = "remove"
            await checks.eventually(
                "automatic live task removal is visible despite slow finished endpoint",
                lambda: is_hidden(task_bar),
                2 * POLL_WINDOW_SECONDS,
                "Healthy /api/tasks changed to an empty list through a real subsequent timer poll",
            )
            await checks.eventually(
                "automatic live task removal request was timer-driven",
                lambda: event_is_set(healthy_remove_request),
                1.0,
                "the mock's remove stage was consumed by /api/tasks rather than a product poll call",
            )
            await checks.eventually(
                "healthy task orbit clears after removal",
                lambda: healthy_orbit_count(0),
                1.0,
                "no .orbit-dot remains after the empty live task snapshot",
            )
            healthy_task_stage = "readd"
            await checks.eventually(
                "automatic live task re-add is visible",
                task_bar_shows_one,
                2 * POLL_WINDOW_SECONDS,
                "a later automatic Healthy /api/tasks response restored the live task indicator",
            )
            await checks.eventually(
                "automatic live task re-add request was timer-driven",
                lambda: event_is_set(healthy_readd_request),
                1.0,
                "the mock's re-add stage was consumed by /api/tasks",
            )
            await checks.eventually(
                "healthy task orbit returns to one dot",
                lambda: healthy_orbit_count(1),
                1.0,
                "the re-added live bash task restores exactly one .orbit-dot",
            )
            checks.require(
                "live tasks updated before finished response",
                finished_pending.is_set() and not release_finished.is_set() and len(healthy_task_times) >= 3,
                "timer-driven live task add/remove/re-add completed while finished was unresolved",
            )

            slow_section = page.locator(".tree-ws-section").filter(has=page.locator(".ws-chip", has_text="Slow"))
            await checks.eventually(
                "slow endpoint began delayed successful response",
                lambda: asyncio.sleep(0, result=slow_second_started.is_set()),
                2 * POLL_WINDOW_SECONDS,
                "the second Slow /api/sessions route is intentionally pending",
            )
            await checks.eventually(
                "slow successful response replaces data completely",
                slow_complete_list_is_visible,
                SLOW_RESPONSE_SECONDS + POLL_WINDOW_SECONDS,
                "both complete Slow sessions are visible after the delayed success",
            )
            checks.require(
                "slow successful response removed obsolete data",
                "slow old" not in await slow_section.inner_text(),
                "the complete Slow list replaced rather than appended to its old list",
            )
            await checks.eventually(
                "failed Slow refresh keeps its own old DOM data",
                slow_failure_keeps_complete_list,
                2 * POLL_WINDOW_SECONDS,
                "HTTP 503 marks Slow degraded but preserves its last complete list",
            )
            await checks.eventually(
                "Slow deletion test has an in-flight request",
                lambda: asyncio.sleep(0, result=slow_fourth_started.is_set()),
                2 * POLL_WINDOW_SECONDS,
                "fourth Slow /api/sessions request remains held before deletion",
            )
            await slow_section.locator(".ws-del").click()
            await checks.eventually(
                "deleted workspace disappears immediately",
                lambda: has_no_nodes(slow_section),
                1.0,
                "the Slow workspace section was removed by the real delete control",
            )
            release_slow_fourth.set()
            await asyncio.sleep(0.5)
            checks.require(
                "late deleted-workspace response does not revive DOM",
                await slow_is_not_revived(),
                "late Slow response was ignored after workspace deletion",
            )

            await browser.close()
    except AcceptanceFailure:
        return 1
    finally:
        release_hung.set()
        release_slow_fourth.set()
        release_finished.set()
        for key, value in old_env.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value
        temp.cleanup()
        # A failed browser launch should not leave an e2e profile in the checkout.
        if runtime_parent.exists():
            shutil.rmtree(runtime_parent)

    checks.emit("acceptance summary", not checks.failed, "all assertions emitted as JSON")
    return 0 if not checks.failed else 1


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
