#!/usr/bin/env python3
"""会话列表用例（基于 05fc4c8 已合入功能）：

  * list_two_line —— 标题两行显示（title 一行 + 完整 id 小字一行；无 title 单行 id）
  * list_search   —— 搜索（title / id 子串；无匹配提示）
  * list_legacy   —— 旧 server 降级：无 title/label/active/pinned 字段；
                     列表/树回退显示 id；✎ 重命名 PUT 404 提示不崩
  * list_resume   —— inactive（历史）会话点击 -> resumeSession（POST /api/sessions {id}）
  * list_delete   —— 删除会话（confirm + DELETE，轮询重绘后行消失）
  * list_pin      —— 📌 置顶（列表 + 侧边栏树）：PUT /api/sessions/{id}/pin body、
                     .pinned/.pin-btn.on 高亮、旧 server 404 「服务器不支持置顶」不崩
  * list_archive  —— 🗄 归档（列表 + 侧边栏树）：归档会话列表默认隐藏、「显示归档」
                     开关显示灰化行、PUT /api/sessions/{id}/archive body、
                     侧边栏「归档」折叠分组、旧 server 404 「服务器不支持归档」不崩
"""
import json

import common

def S(id_, status="Idle", busy=False, parent=None, title=None, active=True, n=2,
      pinned=None, archived=None):
    d = {"id": id_, "model": "flash", "role": "fixer" if parent else "main",
         "status": status, "busy": busy, "active": active,
         "parent_session_id": parent, "title": title,
         "entry_count": n, "created_at": "2026-01-01T00:00:00Z"}
    if pinned is not None:
        d["pinned"] = pinned
    if archived is not None:
        d["archived"] = archived
    return d

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


async def run_list_pin(c):
    # pinned 状态由本端维护：mock 轮询返回的 pinned 跟随 PUT 结果（后端排序/
    # 持久化不可见，这里只测前端渲染与请求）。
    pinned = {"main-pinned": True, "main-normal": False}
    base = [
        S("main-pinned", title="置顶会话"),
        S("main-normal", title="普通会话"),
        {"id": "main-legacy", "model": "flash", "role": "main", "status": "Idle",
         "busy": False, "active": True, "parent_session_id": None,
         "title": "旧服务器会话", "entry_count": 1,
         "created_at": "2026-01-01T00:00:00Z"},          # 旧 server：无 pinned 字段
    ]
    def sessions():
        out = []
        for s in base:
            d = dict(s)
            if d["id"] in pinned:
                d["pinned"] = pinned[d["id"]]
            out.append(d)
        return out
    c.sessions = sessions

    async def on_pin(route, url, method):
        sid = url.split("/api/sessions/")[1].split("/pin")[0]
        body = json.loads(route.request.post_data or "{}")
        c.records["pin"].append((url, route.request.post_data))
        # 只有成功（2xx）才让 mock 状态跟随 PUT（与 on_archive 一致）：
        # 404/405 时状态不变，否则「失败后行状态不变」会被轮询污染。
        status = c.pin_status
        if 200 <= status < 300:
            pinned[sid] = bool(body.get("pinned"))
        await route.fulfill(status=status, content_type="application/json", body="{}")
    c.extra_handlers.append(
        (lambda url, method: method == "PUT" and url.endswith("/pin"), on_pin))

    await c.start()
    c.check("pin：列表渲染 3 行", await c.page.locator(".session-row").count() == 3, "")

    p_row = c.page.locator("#sessionList .session-row", has_text="main-pinned").first
    c.check("pin：pinned 行带 .pinned + 📌 高亮(.on)",
            "pinned" in (await p_row.get_attribute("class"))
            and await p_row.locator(".pin-btn.on").count() == 1,
            await p_row.get_attribute("class"))
    n_row = c.page.locator("#sessionList .session-row", has_text="main-normal").first
    c.check("pin：未置顶行无 .pinned、📌 不高亮",
            "pinned" not in (await n_row.get_attribute("class"))
            and await n_row.locator(".pin-btn.on").count() == 0,
            await n_row.get_attribute("class"))
    l_row = c.page.locator("#sessionList .session-row", has_text="main-legacy").first
    c.check("pin：旧 server 行（无 pinned 字段）视为未置顶",
            "pinned" not in (await l_row.get_attribute("class"))
            and await l_row.locator(".pin-btn").count() == 1, "")

    # 点 📌 置顶普通会话
    await n_row.locator(".pin-btn").click()
    await c.page.wait_for_timeout(500)
    ok = (len(c.records["pin"]) == 1
          and c.records["pin"][0][0].endswith("/api/sessions/main-normal/pin")
          and c.records["pin"][0][1] == '{"pinned":true}')
    c.check("pin：点 📌 -> PUT /api/sessions/main-normal/pin body {pinned:true}", ok,
            str(c.records["pin"]))
    c.check("pin：置顶后行带 .pinned + 📌 高亮",
            "pinned" in (await n_row.get_attribute("class"))
            and await n_row.locator(".pin-btn.on").count() == 1,
            await n_row.get_attribute("class"))

    # 再点取消置顶
    await n_row.locator(".pin-btn").click()
    await c.page.wait_for_timeout(500)
    ok = (len(c.records["pin"]) == 2 and c.records["pin"][1][1] == '{"pinned":false}')
    c.check("pin：再点 -> PUT body {pinned:false}", ok, str(c.records["pin"]))
    c.check("pin：取消后 .pinned 移除、📌 不高亮",
            "pinned" not in (await n_row.get_attribute("class"))
            and await n_row.locator(".pin-btn.on").count() == 0, "")

    # 侧边栏树：主会话根节点也有 📌（仅主会话；按 title 匹配）
    await c.open_sidebar()
    await c.page.wait_for_selector("#sidebarTree .tree-row:not(.tasks-group-head)", timeout=5000)
    tp = c.page.locator("#sidebarTree .tree-row", has_text="置顶会话").locator(".pin-btn")
    c.check("pin：侧边栏树 📌 存在 + pinned 高亮",
            await tp.count() == 1 and "on" in (await tp.get_attribute("class")),
            await tp.get_attribute("class") if await tp.count() else "no pin-btn")
    tn = c.page.locator("#sidebarTree .tree-row", has_text="普通会话").locator(".pin-btn")
    await tn.click()
    await c.page.wait_for_timeout(500)
    ok = (len(c.records["pin"]) == 3
          and c.records["pin"][2][0].endswith("/main-normal/pin")
          and c.records["pin"][2][1] == '{"pinned":true}')
    c.check("pin：树点 📌 -> PUT {pinned:true}", ok, str(c.records["pin"]))
    c.check("pin：树行置顶后带 .pinned",
            "pinned" in (await c.page.locator("#sidebarTree .tree-row",
                                              has_text="普通会话").get_attribute("class")), "")
    await c.close_sidebar()
    await c.page.wait_for_timeout(300)

    # 旧 server：无 pin 端点（404）-> 提示「服务器不支持置顶」不崩、状态不变
    c.pin_status = 404
    await l_row.locator(".pin-btn").click()
    await c.page.wait_for_timeout(500)
    b = await c.ev("els.banner.textContent")
    c.check("pin：旧 server 404 -> 「服务器不支持置顶」", "服务器不支持置顶" in b, b)
    c.check("pin：提示后页面不崩", await c.ev("1+1") == 2, "")
    c.check("pin：失败后行状态不变（仍未置顶）",
            "pinned" not in (await l_row.get_attribute("class"))
            and await l_row.locator(".pin-btn.on").count() == 0, "")


async def run_list_archive(c):
    # archived 状态由本端维护：mock 轮询返回的 archived 跟随 PUT 结果
    # （后端排序/持久化不可见，这里只测前端渲染与请求）。
    archived = {"main-archived": True, "main-normal": False}
    base = [
        S("main-normal", title="普通会话"),
        S("main-archived", title="归档会话", archived=True),
        {"id": "main-legacy", "model": "flash", "role": "main", "status": "Idle",
         "busy": False, "active": True, "parent_session_id": None,
         "title": "旧服务器会话", "entry_count": 1,
         "created_at": "2026-01-01T00:00:00Z"},          # 旧 server：无 archived 字段
    ]
    def sessions():
        out = []
        for s in base:
            d = dict(s)
            if d["id"] in archived:
                d["archived"] = archived[d["id"]]
            out.append(d)
        return out
    c.sessions = sessions

    async def on_archive(route, url, method):
        sid = url.split("/api/sessions/")[1].split("/archive")[0]
        body = json.loads(route.request.post_data or "{}")
        c.records["archive"].append((url, route.request.post_data))
        # 只有成功（2xx）才让 mock 状态跟随 PUT —— 404/405 时状态必须保持
        # 不变（真实 server 不会在失败时改状态），否则「失败后行状态不变」
        # 断言会被下一轮轮询污染。
        status = c.archive_status
        if 200 <= status < 300:
            archived[sid] = bool(body.get("archived"))
        await route.fulfill(status=status, content_type="application/json", body="{}")
    c.extra_handlers.append(
        (lambda url, method: method == "PUT" and url.endswith("/archive"), on_archive))

    await c.start()
    # 列表页核心需求：归档会话默认不显示（只有未归档 2 行）
    c.check("archive：列表默认隐藏归档行（显示 2 行）",
            await c.page.locator(".session-row").count() == 2, "")
    c.check("archive：归档行不在列表中",
            await c.page.locator("#sessionList .session-row",
                                 has_text="main-archived").count() == 0, "")

    n_row = c.page.locator("#sessionList .session-row", has_text="main-normal").first
    c.check("archive：普通行有 🗄 按钮且未高亮",
            await n_row.locator(".archive-btn").count() == 1
            and await n_row.locator(".archive-btn.on").count() == 0, "")

    # 点 🗄 归档普通会话 → 行从列表消失
    await n_row.locator(".archive-btn").click()
    await c.page.wait_for_timeout(500)
    ok = (len(c.records["archive"]) == 1
          and c.records["archive"][0][0].endswith("/api/sessions/main-normal/archive")
          and c.records["archive"][0][1] == '{"archived":true}')
    c.check("archive：点 🗄 -> PUT /api/sessions/main-normal/archive body {archived:true}", ok,
            str(c.records["archive"]))
    c.check("archive：归档后列表只剩 1 行（普通会话也隐藏）",
            await c.page.locator(".session-row").count() == 1, "")

    # 「显示归档」开关：打开后所有会话可见（3 行 = 2 个已归档 + 1 个未归档
    # 的旧 server 行；旧 server 行没有 archived 字段、视为未归档，开关打开
    # 时同样显示）。
    await c.page.locator("#showArchiveBtn").click()
    await c.page.wait_for_timeout(500)
    c.check("archive：开关打开后全部会话可见（3 行）",
            await c.page.locator(".session-row").count() == 3, "")
    a_row = c.page.locator("#sessionList .session-row", has_text="main-archived").first
    c.check("archive：归档行带 .archived 灰化 + 🗄 高亮(.on)",
            "archived" in (await a_row.get_attribute("class"))
            and await a_row.locator(".archive-btn.on").count() == 1,
            await a_row.get_attribute("class"))
    # 再点 🗄 = 恢复 → PUT {archived:false}
    await a_row.locator(".archive-btn").click()
    await c.page.wait_for_timeout(500)
    ok = (len(c.records["archive"]) == 2
          and c.records["archive"][1][0].endswith("/api/sessions/main-archived/archive")
          and c.records["archive"][1][1] == '{"archived":false}')
    c.check("archive：恢复 PUT body {archived:false}", ok, str(c.records["archive"]))
    c.check("archive：恢复后行 .archived 移除、🗄 不高亮",
            "archived" not in (await a_row.get_attribute("class"))
            and await a_row.locator(".archive-btn.on").count() == 0, "")

    # 侧边栏：归档会话收进折叠的「归档 (N)」分组；未归档会话不在分组里
    # （main-normal 此时仍处于归档状态，直接开侧边栏断言；不能再点它的 🗄
    # —— toggle 语义会把已归档行恢复，归档分组反而消失）
    await c.open_sidebar()
    await c.page.wait_for_selector("#sidebarTree .tree-row:not(.tasks-group-head)", timeout=5000)
    ag = c.page.locator("#sidebarTree .tree-group", has_text="归档")
    c.check("archive：侧边栏「归档」分组存在",
            await ag.count() == 1, "groups=" +
            " | ".join(await c.page.locator("#sidebarTree .tree-group").all_text_contents()))
    c.check("archive：分组默认折叠（归档行不可见）",
            await c.page.locator("#sidebarTree .tree-row.archived:visible").count() == 0, "")
    # 展开分组：toggle 只挂在行内 .tree-toggle 按钮上（点 label 不切换）
    await c.page.locator("#sidebarTree .tree-row.tree-archive-row .tree-toggle").click()
    await c.page.wait_for_timeout(300)
    c.check("archive：展开后归档行可见（带 .archived + 🗄）",
            await c.page.locator("#sidebarTree .tree-row.archived").count() == 1
            and await c.page.locator("#sidebarTree .tree-row.archived .archive-btn").count() == 1, "")
    # 点击归档分组内会话可正常打开
    await c.page.locator("#sidebarTree .tree-row.archived").first.click()
    await c.page.wait_for_timeout(500)
    c.check("archive：点击归档分组内会话可打开",
            await c.ev("state.sessionId") == "main-normal" and await c.ev("state.view") == "chat",
            "sid=" + await c.ev("state.sessionId"))
    await c.close_sidebar()
    await c.page.wait_for_timeout(300)

    # 旧 server：无 archive 端点（404）-> 提示「服务器不支持归档」不崩
    c.archive_status = 404
    # 刷新必须回到列表视图：不能 goto(c.page.url) —— 点开归档会话后 URL 是
    # /?session=main-normal，刷新会触发深链重新打开聊天视图，#showArchiveBtn
    # 不在 DOM 里。裸 URL 无深链参数，刷新后回列表（mock 状态保持）。
    await c.page.goto(common.BASE + "/")
    await c.page.wait_for_timeout(500)
    c.check("archive：刷新后回到列表视图（?session= 深链不残留）",
            await c.ev("state.view") == "list", "view=" + await c.ev("state.view"))
    # 打开「显示归档」以拿到旧 server 行
    await c.page.locator("#showArchiveBtn").click()
    await c.page.wait_for_timeout(300)
    l_row = c.page.locator("#sessionList .session-row", has_text="main-legacy").first
    await l_row.locator(".archive-btn").click()
    await c.page.wait_for_timeout(500)
    b = await c.ev("els.banner.textContent")
    c.check("archive：旧 server 404 -> 「服务器不支持归档」", "服务器不支持归档" in b, b)
    c.check("archive：提示后页面不崩", await c.ev("1+1") == 2, "")
    c.check("archive：失败后行状态不变（未归档）",
            "archived" not in (await l_row.get_attribute("class"))
            and await l_row.locator(".archive-btn.on").count() == 0, "")


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
    {"name": "list_pin", "desc": "列表/侧边栏 📌 置顶（PUT /pin body、.pinned 高亮、旧 server 404 提示）",
     "run": run_list_pin},
    {"name": "list_archive", "desc": "列表/侧边栏 🗄 归档（默认隐藏、显示归档开关、PUT /archive body、侧边栏归档分组、旧 server 404 提示）",
     "run": run_list_archive},
]
