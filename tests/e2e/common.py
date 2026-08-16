#!/usr/bin/env python3
"""e-agent Web 端回归测试套件 —— 共享基础设施。

提供：
  * assemble_html()：从 src/ui/ 读取 index.html 骨架 + style.css + app.js +
    vendor 文件，替换占位符拼出自包含单页 HTML（与 .e-agent/ 下散落脚本同手法，
    保证测的是当前 checkout 的前端，而不是 server 内置的旧版本）。
  * Case：单个用例的运行上下文（mock 数据、拦截 handler、check 记录、
    JS 错误收集）。
  * run_case()：每个用例独立启动一个浏览器（状态隔离），统一 30s 超时，
    失败时打印关键 DOM 快照（html 片段）便于调试。

设计约定：
  * 核心用例全部走 page.route 拦截（mock），不依赖真实 server —— 回归要稳。
    真实 server 冒烟（cases/smoke_real.py）可选，用 --real 启用；
    server/token 不可达时该用例自动 SKIP。
  * 环境变量覆盖：EAGENT_CHROME（浏览器可执行文件）、EAGENT_BASE（server 地址）、
    EAGENT_TOKEN_FILE（真实冒烟用 token 文件）、EAGENT_CASE_TIMEOUT（单用例超时）。
"""
import asyncio
import json
import os
from pathlib import Path

# ---------------------------------------------------------------------------
# 环境配置
# ---------------------------------------------------------------------------
REPO_ROOT = Path(__file__).resolve().parents[2]          # tests/e2e/ -> 仓库根
UI_DIR = REPO_ROOT / "src" / "ui"

BASE = os.environ.get("EAGENT_BASE", "http://127.0.0.1:18766")
EXE = os.environ.get(
    "EAGENT_CHROME",
    "/usr/bin/chromium-headless-shell",
)
TOKEN_FILE = Path(os.environ.get(
    "EAGENT_TOKEN_FILE", str(Path.home() / ".local/state/e-agent/server.token"))).expanduser()
CASE_TIMEOUT = float(os.environ.get("EAGENT_CASE_TIMEOUT", "30"))

MOBILE_UA = ("Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) "
             "AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 "
             "Mobile/15E148 Safari/604.1")

_HTML_CACHE = None


def assemble_html():
    """从 src/ui/ 拼装自包含主页 HTML（骨架 + 内联 CSS/JS/vendor）。"""
    global _HTML_CACHE
    if _HTML_CACHE is None:
        def rd(name):
            return (UI_DIR / name).read_text(encoding="utf-8")
        _HTML_CACHE = (rd("index.html")
                       .replace("/*__KATEX_CSS__*/", rd("vendor/katex.min.css"))
                       .replace("/*__CSS__*/", rd("style.css"))
                       .replace("/*__JS_VENDOR__*/", rd("vendor/marked.min.js"))
                       # 与 server.rs 一致：按序拼接拆分后的 JS 文件（同一 <script> 内
                       # 顶层声明全局可见；事件绑定与 init 在 sse.js 尾部执行）
                       .replace("/*__JS_APP__*/", "\n".join(
                           rd(f) for f in ["app.js", "render.js", "sessions.js",
                                           "tasks.js", "sse.js"])))
    return _HTML_CACHE


def read_token():
    """读取真实 server 的访问 token（冒烟用例用）；文件缺失返回 None。"""
    try:
        return TOKEN_FILE.read_text(encoding="utf-8").strip()
    except OSError:
        return None


class SkipCase(Exception):
    """用例主动跳过（如真实 server 不可达）。runner 记为 SKIPPED，不算 FAIL。"""


class Case:
    """单个用例的运行上下文。"""

    def __init__(self, name, desc):
        self.name = name
        self.desc = desc
        # ---- mock 数据（默认拦截 handler 读取；sessions/history 可为 callable）----
        self.sessions = []
        self.history = {"entries": [], "next_before_seq": None}
        self.history_older = {"entries": [], "next_before_seq": None}
        self.sse_body = "retry: 3000\n\n"
        self.title_status = 204              # PUT /title 的 mock 状态码
        self.pin_status = 200                # PUT /pin 的 mock 状态码
        self.archive_status = 200            # PUT /archive 的 mock 状态码
        self.btw_status = 201                # POST /btw 的 mock 状态码
        self.goals = {}                      # sid -> goal 快照（None=无）；GET /goal 按会话返回
        self.goal_delay = {}                 # sid -> (skip, 秒)：前 skip 次 GET /goal 不延迟，之后延迟（stale 测试）
        self.goal_post_status = 202          # POST /goal 返回的状态码（409 = finished/closed mock）
        self.goal_post_delay = None          # 秒；POST /goal 延迟响应（跨会话 stale 测试）
        self.records = {
            "prompt": [], "title": [], "pin": [], "archive": [], "btw": [],
            "compact": [], "history": [], "create": [], "delete": [], "goal": [],
            "goal_get": [],
        }
        self.extra_handlers = []             # [(predicate(url, method), async handler(route,url,method))]
        # ---- 浏览器 ----
        self.viewport = {"width": 1280, "height": 900}
        self.mobile = False
        self.real_api = False                # True：只拦主页 HTML，其余走真实 server
        self.token = "test-token-123"
        self.js_check = True                 # 用例末尾自动加「0 JS 错误」检查
        self.page = None
        # ---- 结果 ----
        self.checks = []
        self.errors = []
        self.elapsed = 0.0

    # ---- 断言 ----
    def check(self, name, ok, detail=""):
        self.checks.append((name, bool(ok), detail))
        print("  [%s] %s%s" % ("PASS" if ok else "FAIL", name,
                               ("   | " + str(detail) if detail else "")))

    def fail(self, name, detail=""):
        self.check(name, False, detail)

    def failed(self):
        return any(not ok for _, ok, _ in self.checks)

    # ---- 便捷 ----
    def ev(self, js):
        return self.page.evaluate(js)

    async def start(self, wait_rows=1):
        """加载主页、注入 token，并等待会话缓存首轮轮询完成。

        wait_rows: 期望的会话缓存条数；0 表示允许空列表。侧边栏是唯一导航，
        因此就绪条件读 state.lastList，而不依赖抽屉是否打开或树 DOM 是否已渲染。
        """
        page = self.page
        # 必须在页面任何脚本运行前把 token 种进 localStorage（add_init_script
        # 对每次导航/刷新都生效）：多工作区功能之后，initWorkspaces 会在首次
        # 加载时把「默认 workspace（空 token）」落盘，刷新后它覆盖
        # eagent_token —— 原来的 goto→setItem→reload 顺序不再可靠（全部
        # mock 用例在 start() 处空列表超时）。
        await page.add_init_script(
            "localStorage.setItem('eagent_token', %s)" % json.dumps(self.token))
        await page.goto(BASE + "/", wait_until="load")
        await page.reload(wait_until="load")
        await page.wait_for_function(
            "n => Array.isArray(state.lastList) && "
            "(state.lastList.length >= n || (n === 0 && state.workspaceErrors[state.workspace.id] !== undefined))",
            arg=wait_rows, timeout=10000)

    async def open_sidebar(self):
        await self.page.click("#sidebarBtn")
        await self.page.wait_for_selector("#sidebar.open", timeout=4000)

    async def close_sidebar(self):
        await self.page.click("#sidebarCloseBtn")


# ---------------------------------------------------------------------------
# 路由拦截
# ---------------------------------------------------------------------------
def make_intercept(c):
    """全量 mock 拦截：主页 HTML + 全部 API 端点（核心用例不依赖真实 server）。"""
    html = assemble_html()

    async def intercept(route):
        url = route.request.url
        method = route.request.method
        base = url.split("?")[0].rstrip("/")   # 忽略 query（app 会用 /?session=<id> 深链）
        if base == BASE:
            return await route.fulfill(status=200, content_type="text/html; charset=utf-8",
                                       body=html)
        # 用例自定义 handler 优先
        for pred, handler in c.extra_handlers:
            if pred(url, method):
                return await handler(route, url, method)
        if url.endswith("/favicon.ico") or "/fonts/" in url:
            return await route.fulfill(status=200, content_type="application/octet-stream",
                                       body="")
        if method == "GET" and base.endswith("/api/sessions"):
            payload = c.sessions() if callable(c.sessions) else c.sessions
            return await route.fulfill(status=200, content_type="application/json",
                                       body=json.dumps(payload))
        if method == "POST" and base.endswith("/api/sessions"):
            c.records["create"].append(route.request.post_data)
            # 恢复历史会话（POST {id}）：按请求 id 原样返回（默认固定 sess-new
            # 无法区分恢复的是哪个会话）。
            sid = "sess-new"
            try:
                body = json.loads(route.request.post_data or "{}")
                if body.get("id"):
                    sid = body["id"]
            except Exception:
                pass
            return await route.fulfill(status=201, content_type="application/json",
                                       body=json.dumps({"id": sid, "status": "Idle", "active": True}))
        if method == "DELETE" and "/api/sessions/" in url:
            c.records["delete"].append(url)
            return await route.fulfill(status=204, content_type="application/json", body="")
        if method == "PUT" and url.endswith("/title"):
            c.records["title"].append((url, route.request.post_data))
            return await route.fulfill(status=c.title_status, content_type="application/json",
                                       body="")
        if method == "PUT" and url.endswith("/pin"):
            c.records["pin"].append((url, route.request.post_data))
            return await route.fulfill(status=c.pin_status, content_type="application/json",
                                       body="{}")
        if method == "PUT" and url.endswith("/archive"):
            c.records["archive"].append((url, route.request.post_data))
            return await route.fulfill(status=c.archive_status, content_type="application/json",
                                       body="{}")
        if method == "POST" and url.endswith("/prompt"):
            c.records["prompt"].append((url, route.request.post_data))
            return await route.fulfill(status=202, content_type="application/json", body="{}")
        if method == "POST" and url.endswith("/cancel"):
            return await route.fulfill(status=202, content_type="application/json", body="{}")
        if method == "POST" and url.endswith("/compact"):
            c.records["compact"].append((url, route.request.post_data))
            return await route.fulfill(status=202, content_type="application/json", body="{}")
        if method == "POST" and url.endswith("/btw"):
            c.records["btw"].append((url, route.request.post_data))
            return await route.fulfill(status=c.btw_status, content_type="application/json",
                                       body=json.dumps({"id": "sub-btw-1"}))
        if method == "GET" and url.endswith("/goal"):
            c.records["goal_get"].append(url)   # 记录 GET /goal（断言「无额外 GET B」）
            sid = url.split("/api/sessions/", 1)[1].split("/goal", 1)[0]
            spec = c.goal_delay.get(sid)
            if spec:
                skip, secs = spec
                if skip > 0:
                    c.goal_delay[sid] = (skip - 1, secs)   # 前 skip 次不延迟
                else:
                    await asyncio.sleep(secs)              # stale 测试：延迟响应
            return await route.fulfill(status=200, content_type="application/json",
                                       body=json.dumps({"goal": c.goals.get(sid)}))
        if method == "POST" and url.endswith("/goal"):
            # 模拟服务端 goal 状态：set/pause/resume/clear 就地更新该会话的
            # goals[sid]，使随后的 GET /goal（GoalBar 初始化/刷新）返回最新
            # 快照。goal_post_status != 202 时模拟 finished/closed（409，
            # 不更新状态）；goal_post_delay 延迟响应（跨会话 stale 测试）。
            c.records["goal"].append((url, route.request.post_data))
            sid = url.split("/api/sessions/", 1)[1].split("/goal", 1)[0]
            if c.goal_post_delay:
                await asyncio.sleep(c.goal_post_delay)
            if c.goal_post_status != 202:
                return await route.fulfill(status=c.goal_post_status,
                                           content_type="application/json", body="")
            try:
                body = json.loads(route.request.post_data or "{}")
                action = body.get("action")
                cur = c.goals.get(sid)
                if action == "set" and str(body.get("objective", "")).strip():
                    c.goals[sid] = {"id": "goal-1", "revision": 1,
                                    "objective": str(body.get("objective", "")).strip(),
                                    "success_criteria": [], "status": "active",
                                    "progress": "", "evidence": [], "blocked_reason": None}
                elif action in ("pause", "resume") and isinstance(cur, dict):
                    c.goals[sid] = dict(cur, status="paused" if action == "pause" else "active",
                                        revision=cur.get("revision", 1) + 1)
                elif action == "clear":
                    c.goals[sid] = None
            except Exception:
                pass
            return await route.fulfill(status=202, content_type="application/json", body="{}")
        if url.endswith("/events"):
            return await route.fulfill(status=200,
                                       headers={"content-type": "text/event-stream"},
                                       body=c.sse_body)
        if "/history" in url:
            c.records["history"].append((url, method))
            if "before_seq=" in url:
                payload = c.history_older() if callable(c.history_older) else c.history_older
            else:
                payload = c.history() if callable(c.history) else c.history
            return await route.fulfill(status=200, content_type="application/json",
                                       body=json.dumps(payload))
        if base.endswith("/api/tasks"):
            return await route.fulfill(status=200, content_type="application/json", body="[]")
        return await route.fulfill(status=200, content_type="application/json", body="{}")

    return intercept


def make_real_intercept(c):
    """真实 server 冒烟：只拦主页 HTML（用当前 checkout 的三件套），其余放行。"""
    html = assemble_html()

    async def intercept(route):
        if route.request.url.rstrip("/") == BASE:
            await route.fulfill(status=200, content_type="text/html; charset=utf-8",
                                body=html)
        else:
            await route.continue_()

    return intercept


# ---------------------------------------------------------------------------
# 用例执行
# ---------------------------------------------------------------------------
async def snapshot(c):
    """失败时抓取关键 DOM 快照（html 片段），便于定位问题。"""
    page = c.page
    try:
        return await page.evaluate("""() => {
          const g = (id) => { const el = document.getElementById(id);
            return el ? { html: el.innerHTML.slice(0, 1200),
                          hidden: el.hasAttribute("hidden"),
                          cls: el.className } : null; };
          let st = {};
          try {
            st = { sessionId: state.sessionId, status: state.status,
                   noSession: els.chatView.classList.contains("no-session"),
                   token: !!state.token, sidebarOpen: state.sidebar.open,
                   sidebarFilter: state.sidebar.filter,
                   promptValue: els.promptInput ? els.promptInput.value : null };
          } catch (e) { st = { eval_err: String(e) }; }
          return Object.assign(st, {
            url: location.href,
            banner: g("banner"), chatEmpty: g("chatEmpty"),
            sidebarTree: g("sidebarTree"),
            messages: g("messages"),
            scrollW: document.documentElement.scrollWidth,
            clientW: document.documentElement.clientWidth,
          });
        }""")
    except Exception as e:
        try:
            content = await page.content()
        except Exception:
            content = "<unreadable>"
        return {"eval_error": str(e), "content_prefix": content[:1500]}


async def run_case(c, fn, timeout):
    """启动独立浏览器执行用例；统一超时/异常处理；失败时打 DOM 快照。

    返回 (status, detail, snapshot_dump)；status ∈ {"PASS", "FAIL", "SKIPPED"}。
    """
    from playwright.async_api import async_playwright
    import time
    t0 = time.monotonic()
    status, detail, snap = "PASS", None, None
    timed_out = exc = skip_reason = False
    async with async_playwright() as p:
        browser = await p.chromium.launch(executable_path=EXE)
        ctx_kw = {"viewport": c.viewport}
        if c.mobile:
            ctx_kw.update(is_mobile=True, has_touch=True, user_agent=MOBILE_UA)
        ctx = await browser.new_context(**ctx_kw)
        page = await ctx.new_page()
        c.page = page
        page.set_default_timeout(5000)       # 断言失败后的后续步骤快速失败，不空等 30s
        page.on("pageerror", lambda e: c.errors.append("pageerror: " + str(e)))
        page.on("console", lambda m: c.errors.append("console.error: " + m.text)
                if m.type == "error" else None)

        async def on_dialog(dialog):
            await dialog.accept()            # confirm() 一律接受（/compact 等）
        page.on("dialog", on_dialog)

        await page.route("**/*",
                         make_intercept(c) if not c.real_api else make_real_intercept(c))
        try:
            await asyncio.wait_for(fn(c), timeout)
        except asyncio.TimeoutError:
            timed_out = True
            c.fail("用例超时", ">%ss" % timeout)
        except SkipCase as e:
            skip_reason = e.reason
        except Exception as e:
            exc = e
            c.fail("用例异常", "%s: %s" % (type(e).__name__, e))
        if skip_reason:
            status, detail = "SKIPPED", skip_reason
        else:
            if c.js_check:
                js_errs = [e for e in c.errors
                           if not e.startswith("console.error: Failed to load resource")]
                c.check("0 JS 错误（pageerror + 非资源类 console.error）", not js_errs,
                        "; ".join(js_errs[:5]) or "none")
            if timed_out or exc or c.failed():
                status = "FAIL"
                snap = await snapshot(c)
        await browser.close()
    c.elapsed = time.monotonic() - t0
    return status, detail, snap
