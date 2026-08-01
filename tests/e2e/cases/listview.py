#!/usr/bin/env python3
"""会话列表用例（基于 05fc4c8 已合入功能）：

  * list_two_line —— 标题两行显示（title 一行 + 完整 id 小字一行；无 title 单行 id）
  * list_search   —— 搜索（title / id 子串；无匹配提示）
  * list_legacy   —— 旧 server 降级：无 title/label/active/pinned 字段；
                     列表/树回退显示 id；✎ 重命名 PUT 404 提示不崩
  * list_resume   —— inactive（历史）会话点击 -> resumeSession（POST /api/sessions {id}）
  * list_delete   —— 删除会话（confirm + DELETE，轮询重绘后行消失）

pin 按钮（feat/pin-frontend）未合入 05fc4c8，见 cases/skipped.py（TODO）。
"""
import json

import common

def S(id_, status="Idle", busy=False, parent=None, title=None, active=True, n=2):
    return {"id": id_, "model": "flash", "role": "fixer" if parent else "main",
            "status": status, "busy": busy, "active": active,
            "parent_session_id": parent, "title": title,
            "entry_count": n, "created_at": "2026-01-01T00:00:00Z"}

async def run_list_two_line(c):
    c.sessions = [
        S("main-titled", title="标题会话"),
        {"id": "no-title-session", "model": "flash", "role": "main", "status": "Idle",
         "busy": False, "active": True, "parent_session_id": None,
         "entry_count": 1, "created_at": "2026-01-01T00:00:00Z"},   # 旧 server：无 title
    ]
    await c.start()
    n_rows = await c.page.locator(".session-row").count()
    c.check("启动：列表渲染 2 行", n_rows == 2, f"rows={n_rows}")

    titled = c.page.locator("#sessionList .session-row", has_text="main-titled").first
    c.check("有标题行：.sid.has-title 结构",
            await titled.locator(".sid.has-title").count() == 1, "")
    c.check("有标题行：.sid-title 显示 title", 
            await titled.locator(".sid-title").text_content() == "标题会话", "")
    c.check("有标题行：.sid-id 显示完整 id",
            await titled.locator(".sid-id").text_content() == "main-titled", "")
    c.check("有标题行：✎ 重命名按钮存在",
            await titled.locator(".tree-edit").count() == 1, "")

    notitle = c.page.locator("#sessionList .session-row", has_text="no-title-session").first
    c.check("无标题行：单行显示完整 id（无 has-title）",
            await notitle.locator(".sid:not(.has-title)").count() == 1
            and (await notitle.locator(".sid").text_content()) == "no-title-session", "")
    meta = await c.page.locator("#listMeta").text_content()
    c.check("列表 meta：共 2 个", "共 2 个" in meta, meta)

async def run_list_search(c):
    c.sessions = [
        S("main-titled", title="标题会话"),
        S("main-idle", title="普通会话"),
        S("no-title", title=None),
    ]
    await c.start()

    await c.page.fill("#searchInput", "标题会话")          # 按 title 命中
    await c.page.wait_for_timeout(300)
    n = await c.page.locator(".session-row").count()
    sid_txt = await c.page.locator(".session-row .sid-id").first.text_content()
    c.check("搜索按 title 命中 1 行", n == 1 and sid_txt == "main-titled",
            f"n={n} sid={sid_txt}")

    await c.page.fill("#searchInput", "main-ti")            # 按 id 子串命中
    await c.page.wait_for_timeout(300)
    n = await c.page.locator(".session-row").count()
    c.check("搜索按 id 子串命中", n == 1,
            f"n={n}")

    await c.page.fill("#searchInput", "zzz-不存在")
    await c.page.wait_for_timeout(300)
    n = await c.page.locator(".session-row").count()
    hint = await c.page.locator("#listHint").text_content()
    c.check("搜索无匹配：0 行 + 提示", n == 0 and "没有匹配的会话" in hint,
            f"n={n} hint={hint}")

    await c.page.fill("#searchInput", "")
    await c.page.wait_for_timeout(300)
    n = await c.page.locator(".session-row").count()
    c.check("清空搜索恢复全部", n == 3, f"n={n}")

async def run_list_legacy(c):
    # 旧 server：只有基础字段，无 title/label/active/pinned
    c.sessions = [
        {"id": "legacy-main", "model": "flash", "role": "main", "status": "Idle",
         "busy": False, "parent_session_id": None, "entry_count": 1,
         "created_at": "2026-01-01T00:00:00Z"},
        {"id": "legacy-sub-1", "model": "flash", "role": "fixer", "status": "Idle",
         "busy": False, "parent_session_id": "legacy-main", "entry_count": 1,
         "created_at": "2026-01-01T00:00:00Z"},
    ]
    await c.start()
    c.check("旧 server：列表渲染 2 行", await c.page.locator(".session-row").count() == 2, "")
    row = c.page.locator("#sessionList .session-row", has_text="legacy-main").first
    c.check("旧 server：无 title -> 单行完整 id",
            (await row.locator(".sid").text_content()) == "legacy-main"
            and await row.locator(".sid.has-title").count() == 0, "")
    c.check("旧 server：行 chip 空闲 + ✎ 仍显示",
            (await row.locator(".status-chip").text_content()) == "空闲"
            and await row.locator(".tree-edit").count() == 1, "")

    # 侧边栏树：根/子都回退 shortId
    await c.open_sidebar()
    c.check("旧 server：树根节点 shortId 回退",
            await c.page.locator("#sidebarTree .tree-row", has_text="legacy-m…").count() >= 1, "")
    await c.page.locator("#sidebarTree .tree-row", has_text="legacy-m…").locator("button.tree-toggle").click()
    await c.page.wait_for_timeout(200)
    c.check("旧 server：树子节点 shortId 回退",
            await c.page.locator(".tree-row-child", has_text="legacy-s…").count() >= 1, "")
    await c.close_sidebar()
    await c.page.wait_for_timeout(300)

    # ✎ 重命名 -> PUT 404 -> 提示「服务器不支持重命名」，编辑框保留、不崩
    c.title_status = 404
    await row.locator(".tree-edit").click()
    await c.page.wait_for_timeout(200)
    await c.page.fill(".session-row .rename-input", "新名字")
    await c.page.locator(".rename-save").click()
    await c.page.wait_for_timeout(500)
    b = await c.ev("els.banner.textContent")
    c.check("旧 server：PUT 404 -> 服务器不支持重命名", "服务器不支持重命名" in b, b)
    c.check("旧 server：PUT 404 -> 编辑框保留、页面不崩",
            await c.page.locator(".rename-input").count() == 1 and await c.ev("1+1") == 2, "")
    await c.page.press(".rename-input", "Escape")

async def run_list_resume(c):
    c.sessions = [
        S("active-main", title="活跃会话"),
        S("inactive-hist", status="Idle", title="历史会话", active=False),
    ]
    await c.start()

    # inactive 行：灰显 + 点击触发 resumeSession
    row = c.page.locator("#sessionList .session-row", has_text="inactive-hist").first
    c.check("inactive 行带 .inactive 样式",
            "inactive" in (await row.get_attribute("class")), await row.get_attribute("class"))

    async def on_create(route, url, method):
        body = route.request.post_data
        c.records["create"].append(body)
        sid = json.loads(body or "{}").get("id", "sess-new")
        await route.fulfill(status=201, content_type="application/json",
                            body=json.dumps({"id": sid, "status": "Idle"}))
    c.extra_handlers.append(
        (lambda url, method: url.rstrip("/").endswith("/api/sessions") and method == "POST",
         on_create))

    await row.click()
    await c.page.wait_for_function("() => state.view === 'chat'")
    c.check("inactive 点击 -> resumeSession POST {id}",
            len(c.records["create"]) == 1
            and c.records["create"][0] == '{"id":"inactive-hist"}',
            str(c.records["create"]))
    c.check("resume 成功后打开会话", await c.ev("state.sessionId") == "inactive-hist", "")

async def run_list_delete(c):
    deleted = set()
    base = [S("main-del-1", title="删除目标"), S("main-keep", title="保留会话")]
    def sessions():
        return [s for s in base if s["id"] not in deleted]
    c.sessions = sessions

    async def on_delete(route, url, method):
        sid = url.split("/api/sessions/")[1]
        deleted.add(sid)
        c.records["delete"].append(url)
        await route.fulfill(status=204, content_type="application/json", body="")
    c.extra_handlers.append(
        (lambda url, method: method == "DELETE" and "/api/sessions/" in url, on_delete))

    await c.start()
    c.check("删除前：2 行", await c.page.locator(".session-row").count() == 2, "")
    await c.page.locator("#sessionList .session-row", has_text="main-del-1").locator(".del").click()
    await c.page.wait_for_function("() => document.querySelectorAll('.session-row').length === 1",
                                   timeout=5000)
    c.check("删除：DELETE 请求发出", len(c.records["delete"]) == 1
            and c.records["delete"][0].endswith("/api/sessions/main-del-1"),
            str(c.records["delete"]))
    c.check("删除：轮询重绘后行消失（剩 1 行）",
            await c.page.locator(".session-row").count() == 1, "")
    c.check("删除：未误删其他行",
            await c.page.locator("#sessionList .session-row", has_text="main-keep").count() == 1, "")

CASES = [
    {"name": "list_two_line", "desc": "列表：标题两行显示（title + 完整 id）",
     "run": run_list_two_line},
    {"name": "list_search", "desc": "列表：搜索（title/id 子串、无匹配提示）",
     "run": run_list_search},
    {"name": "list_legacy", "desc": "兼容：旧 server 无 title/label/active/pinned 字段降级",
     "run": run_list_legacy},
    {"name": "list_resume", "desc": "列表：inactive 历史会话点击 -> resumeSession",
     "run": run_list_resume},
    {"name": "list_delete", "desc": "列表：删除会话（confirm + DELETE + 行消失）",
     "run": run_list_delete},
]
