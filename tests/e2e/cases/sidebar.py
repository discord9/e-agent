#!/usr/bin/env python3
"""侧边栏用例（基于 05fc4c8 已合入功能）：

  * sidebar_tree   —— 开合（按钮/×/Escape/遮罩）+ 会话树渲染：
                       主会话根节点、subagent 活跃/历史分组、label 优先、
                       旧 server 兼容（无 label/active）、busy 提示、孤儿组。
  * sidebar_filter —— 筛选：主会话按 title/id 匹配，匹配的展开显示全部子会话；
                       孤儿组按自身 title/id 匹配；无匹配提示。
  * sidebar_limit  —— 8 条限制 + 「+N 个更早的会话」展开全部。
  * sidebar_squeeze —— 挤压布局（桌面 >600px）：内容右移 280px、遮罩 display:none、
                       点内容区不关（旧覆盖式点遮罩才关）。
  * sidebar_persist —— 持续打开：切会话保持打开 + localStorage 跨刷新恢复。
  * sidebar_actions —— 唯一导航中的搜索、恢复、删除、置顶、归档与旧 server 降级。
"""
import json

import common

def S(id_, parent, title, active, label=None, busy=False, status="Idle", model="flash"):
    d = {"id": id_, "model": model, "role": "fixer" if parent else "main",
         "status": status, "busy": busy, "parent_session_id": parent,
         "title": title, "entry_count": 2, "created_at": "2026-01-01T00:00:00Z"}
    if active is not None:
        d["active"] = active
    if label is not None:
        d["label"] = label
    return d

# root-a: 活跃带 label + 非活跃带 label + 旧 server 兼容（无 label 无 active）
# root-b: 全非活跃 -> 只显示历史组
# root-c: 无 label（title 回退）+ 无 label 无 title（shortId 回退，非活跃）
# 孤儿：活跃带 label + 非活跃带 label（parent 不在列表）
SESSIONS = [
    S("root-a", None, "根会话A", True),
    S("root-b", None, "根会话B", True),
    S("root-c", None, "根会话C", True),
    S("sub-a1-cccc", "root-a", "子A1标题", True, label="任务A-1"),
    S("sub-a2-dddd", "root-a", "子A2标题", False, label="任务A-2"),
    S("sub-a3-9999", "root-a", "旧服务器子会话", None),          # 旧 server：无 label 无 active
    S("sub-a4-busy", "root-a", "子A4标题", True, label="忙碌任务A-4", busy=True),
    S("sub-b1-eeee", "root-b", "子B1标题", False, label="过期任务B"),
    S("sub-c1-ffff", "root-c", "无标签子C", True),               # 无 label -> title 回退
    S("sub-c2-abcd", "root-c", None, False),                     # 无 label 无 title -> shortId 回退
    S("orph-a1-1111", "ghost-parent", "孤A", True, label="孤儿活跃标签"),
    S("orph-a2-2222", "ghost-parent", "孤B", False, label="孤儿历史标签"),
]

async def run_sidebar_tree(c):
    c.sessions = SESSIONS
    # resume 拦截：POST /api/sessions 按请求的 id 原样返回（默认 handler 固定
    # 回 sess-new，无法区分恢复的是哪个会话）；同时记录进 c.records["create"]，
    # 供「live 点击不 POST resume、inactive 才 POST resume」断言。
    async def on_create(route, url, method):
        body = route.request.post_data
        c.records["create"].append(body)
        sid = json.loads(body or "{}").get("id", "sess-new")
        await route.fulfill(status=201, content_type="application/json",
                            body=json.dumps({"id": sid, "status": "Idle", "active": True}))
    c.extra_handlers.append(
        (lambda url, method: method == "POST" and url.rstrip("/").endswith("/api/sessions"), on_create))
    await c.start()
    # ---------- 开合 ----------
    await c.open_sidebar()
    c.check("打开：.open + 侧边栏可见",
            await c.page.locator("#sidebar.open").count() == 1 and await c.ev("!els.sidebar.hidden"), "")
    c.check("桌面打开：遮罩 display:none（挤压布局，不遮挡内容）",
            await c.ev("getComputedStyle(els.sidebarOverlay).display") == "none", "")
    n_roots = await c.page.locator(
        "#sidebarTree .tree-ws-body > .tree-node:not(.tasks-group) > .tree-row").count()
    c.check("默认全部折叠：3 根 + 未关联 = 4 行（不含隐藏的「运行中任务」组）",
            n_roots == 4, f"rows={n_roots}")

    tree = lambda: c.page.locator("#sidebarTree")

    # ---------- A：root-a（live 直显：busy + Idle 存活 + 旧 server 兼容；
    #              inactive（active === false）收进历史组） ----------
    root_a = tree().locator(".tree-row", has_text="根会话A").first
    await root_a.locator("button.tree-toggle").click()
    await c.page.wait_for_timeout(200)
    a_node = root_a.locator("xpath=..")
    busy_row = a_node.locator(".tree-row-child:not(.tree-hist)", has_text="忙碌任务A-4")
    c.check("A busy subagent 直接显示 + label + busy 点",
            await busy_row.count() == 1 and await busy_row.locator(".busy-dot.busy").count() == 1, "")
    c.check("A busy subagent 行 title 提示可发送消息",
            "可发送消息" in (await busy_row.get_attribute("title")), "")
    live_idle = a_node.locator(".tree-row-child:not(.tree-hist)", has_text="任务A-1")
    c.check("A Idle 存活 subagent 直显且无红点",
            await live_idle.count() == 1
            and await live_idle.locator(".busy-dot.busy").count() == 0, "")
    legacy_live = a_node.locator(".tree-row-child:not(.tree-hist)", has_text="旧服务器子会话")
    c.check("A 旧 server（无 active 字段）subagent 视为 live 直显",
            await legacy_live.count() == 1, "")
    hist_a = a_node.locator(".tree-hist-row")
    c.check("A 仅 inactive 子会话收进历史组",
            await hist_a.count() == 1 and "历史子会话 (1)" in (await hist_a.text_content()), "")
    await hist_a.locator("button.tree-toggle").click()
    await c.page.wait_for_timeout(200)
    c.check("A 历史组只含 inactive 子会话（label 可见，live 的不进历史）",
            await a_node.locator(".tree-row-child.tree-hist", has_text="任务A-2").count() == 1
            and await a_node.locator(".tree-row-child.tree-hist", has_text="任务A-1").count() == 0
            and await a_node.locator(".tree-row-child.tree-hist", has_text="旧服务器子会话").count() == 0, "")
    # Idle 存活行点击：直接打开，不 POST resume。
    await live_idle.locator(".tree-id").click()
    await c.page.wait_for_function("() => state.sessionId === 'sub-a1-cccc'")
    c.check("A Idle 存活 subagent 点击直接打开（不 POST resume）",
            c.records["create"] == [], str(c.records["create"]))
    # inactive 历史行点击：先 POST resume 再打开（重绘后历史组默认收起，重新展开）。
    await a_node.locator(".tree-hist-row button.tree-toggle").click()
    await c.page.wait_for_timeout(200)
    await a_node.locator(".tree-row-child.tree-hist", has_text="任务A-2").locator(".tree-id").click()
    await c.page.wait_for_function("() => state.sessionId === 'sub-a2-dddd'")
    c.check("A inactive subagent 点击先 POST resume 再打开",
            c.records["create"] == ['{"id":"sub-a2-dddd"}'], str(c.records["create"]))

    # ---------- B：root-b 全 idle -> 只显示历史组 ----------
    root_b = tree().locator(".tree-row", has_text="根会话B").first
    await root_b.locator("button.tree-toggle").click()
    await c.page.wait_for_timeout(200)
    b_node = root_b.locator("xpath=..")
    c.check("B 全 idle 父节点：无直接 busy 行",
            await b_node.locator(".tree-row-child:not(.tree-hist)").count() == 0, "")
    c.check("B 只显示历史子会话分组",
            "历史子会话 (1)" in (await b_node.locator(".tree-hist-row").text_content()), "")

    # ---------- C：无 label 时 title/id 回退 ----------
    root_c = tree().locator(".tree-row", has_text="根会话C").first
    await root_c.locator("button.tree-toggle").click()
    await c.page.wait_for_timeout(200)
    c_node = root_c.locator("xpath=..")
    live_c1 = c_node.locator(".tree-row-child:not(.tree-hist)", has_text="无标签子C")
    c.check("C active 无 label subagent 直显（title 回退，无红点）",
            await live_c1.count() == 1 and await live_c1.locator(".busy-dot.busy").count() == 0, "")
    hist_c = c_node.locator(".tree-hist-row")
    c.check("C 仅 inactive 子会话进历史组", "历史子会话 (1)" in (await hist_c.text_content()), "")
    await hist_c.locator("button.tree-toggle").click()
    await c.page.wait_for_timeout(200)
    c.check("C 历史组无 label 无 title -> 完整 id 回退",
            await c_node.locator(".tree-row-child.tree-hist", has_text="sub-c2-abcd").count() == 1, "")

    # ---------- D：孤儿「未关联」同样 live 直显 / inactive 进历史分组 ----------
    unrel = tree().locator(".tree-row", has_text="未关联").first
    await unrel.locator("button.tree-toggle").click()
    await c.page.wait_for_timeout(200)
    u_node = unrel.locator("xpath=..")
    live_orph = u_node.locator(".tree-row-child:not(.tree-hist)", has_text="孤儿活跃标签")
    c.check("D 未关联 active 孤儿直显（无红点）",
            await live_orph.count() == 1 and await live_orph.locator(".busy-dot.busy").count() == 0, "")
    hist_u = u_node.locator(".tree-hist-row")
    c.check("D 未关联仅 inactive 收进历史组",
            await hist_u.count() == 1 and "历史子会话 (1)" in (await hist_u.text_content()), "")
    await hist_u.locator("button.tree-toggle").click()
    await c.page.wait_for_timeout(200)
    c.check("D 展开后历史孤儿 label 可见",
            await u_node.locator(".tree-row-child.tree-hist", has_text="孤儿历史标签").is_visible(), "")

    # ---------- E：关闭方式（× / Escape / 遮罩） ----------
    await c.close_sidebar()
    await c.page.wait_for_timeout(400)
    c.check("E × 关闭侧边栏", await c.ev("!state.sidebar.open"), "")

    await c.open_sidebar()
    await c.page.keyboard.press("Escape")
    await c.page.wait_for_timeout(400)
    c.check("E Escape 关闭侧边栏", await c.ev("!state.sidebar.open"), "")

    await c.open_sidebar()
    await c.page.mouse.click(1000, 450)          # 桌面挤压：点内容区（遮罩 display:none）
    await c.page.wait_for_timeout(400)
    c.check("E 桌面点内容区不关（挤压布局；手机点遮罩关闭见 mobile_390）",
            await c.ev("state.sidebar.open"), "")

async def run_sidebar_filter(c):
    c.sessions = SESSIONS
    await c.start()
    await c.open_sidebar()
    await c.page.wait_for_selector("#sidebarTree .tree-row:not(.tasks-group-head)", timeout=5000)

    # 无匹配 -> 空态提示
    await c.page.fill("#sidebarFilter", "zzz-不存在")
    await c.page.wait_for_timeout(200)
    empty = await c.page.locator("#sidebarTree .tree-empty").count()
    c.check("筛选无匹配 -> 「无匹配会话」", empty == 1
            and await c.page.locator("#sidebarTree .tree-empty").text_content() == "无匹配会话",
            f"empty={empty}")

    # 按主会话 title 匹配 -> 1 根 + 其子会话随父展开显示
    await c.page.fill("#sidebarFilter", "根会话B")
    await c.page.wait_for_timeout(200)
    roots = await c.page.locator("#sidebarTree .tree-ws-body > .tree-node:not(.tasks-group) > .tree-row").count()
    c.check("筛选命中 1 个主会话", roots == 1, f"roots={roots}")
    # root-b 全非活跃：子会话收进历史组（筛选时父节点自动展开）
    hist = await c.page.locator("#sidebarTree .tree-hist-row").count()
    c.check("筛选时匹配父节点的子会话展开（历史组可见）", hist == 1, f"hist={hist}")

    # 按孤儿 title 匹配 -> 「未关联」组出现
    await c.page.fill("#sidebarFilter", "孤A")
    await c.page.wait_for_timeout(200)
    g = await c.page.locator("#sidebarTree .tree-ws-body > .tree-node > .tree-row", has_text="未关联").count()
    c.check("筛选命中孤儿 -> 未关联组显示", g == 1, f"groups={g}")
    unrel = c.page.locator("#sidebarTree .tree-ws-body > .tree-node > .tree-row", has_text="未关联").first
    await unrel.locator("button.tree-toggle").click()
    await c.page.wait_for_timeout(200)
    c.check("孤儿组命中孤儿直显（live）",
            await c.page.locator(".tree-row-child:not(.tree-hist)", has_text="孤儿活跃标签").is_visible(), "")

    # 有标题的会话仍必须能按完整 id 命中（输入框承诺 title / ID）。
    await c.page.fill("#sidebarFilter", "root-a")
    await c.page.wait_for_timeout(200)
    c.check("有标题会话按 ID 仍可命中",
            await c.page.locator("#sidebarTree .tree-row", has_text="根会话A").count() == 1, "")
    await c.page.fill("#sidebarFilter", "根会话A")
    await c.page.wait_for_timeout(200)
    c.check("有标题会话按标题可命中",
            await c.page.locator("#sidebarTree .tree-row", has_text="根会话A").count() == 1, "")

    # 清空 -> 恢复默认 4 行
    await c.page.fill("#sidebarFilter", "")
    await c.page.wait_for_timeout(200)
    n = await c.page.locator("#sidebarTree .tree-ws-body > .tree-node:not(.tasks-group) > .tree-row").count()
    c.check("清空筛选恢复 3 根 + 未关联", n == 4, f"rows={n}")

async def run_sidebar_limit(c):
    # 18 个主会话（无子会话）：默认只渲染最近 8 条 + 「+10 个更早的会话」按钮
    sessions = [S("root-%02d" % i, None, "主会话%02d" % i, True) for i in range(18)]
    c.sessions = sessions
    await c.start()
    await c.open_sidebar()
    await c.page.wait_for_selector("#sidebarTree .tree-row:not(.tasks-group-head)", timeout=5000)
    n = await c.page.locator("#sidebarTree .tree-ws-body > .tree-node:not(.tasks-group) > .tree-row").count()
    c.check("默认只渲染最近 8 个主会话", n == 8, f"rows={n}")
    more = c.page.locator("#sidebarTree .tree-more")
    c.check("显示「+10 个更早的会话」按钮",
            await more.count() == 1 and await more.text_content() == "+10 个更早的会话",
            f"n={await more.count()}")
    await more.click()
    await c.page.wait_for_timeout(300)
    n2 = await c.page.locator("#sidebarTree .tree-ws-body > .tree-node:not(.tasks-group) > .tree-row").count()
    c.check("点击后展开全部 18 个主会话", n2 == 18, f"rows={n2}")


async def run_sidebar_squeeze(c):
    """桌面视口（>600px）：打开侧边栏时挤压内容——#topbar/#chatView 右移
    280px、遮罩 display:none；点内容区不关（遮罩不拦截）。"""
    c.sessions = [
        S("root-a", None, "根会话A", True),
        S("root-b", None, "根会话B", True),
        S("root-c", None, "根会话C", True),
    ]
    await c.start()
    await c.open_sidebar()
    await c.page.wait_for_timeout(400)          # 等 margin-left 0.2s 过渡结束

    ml = await c.ev("getComputedStyle(document.getElementById('topbar')).marginLeft")
    c.check("桌面打开：顶栏保持全宽固定", ml == "0px", ml)
    x = await c.ev("els.chatView.getBoundingClientRect().x")
    c.check("桌面打开：#chatView 左缘 ≈ 280px", abs(x - 280) <= 2, f"x={x}")
    ov = await c.ev("getComputedStyle(els.sidebarOverlay).display")
    c.check("桌面打开：遮罩 display:none（不遮挡内容）", ov == "none", ov)

    # 点内容区（遮罩已 display:none，不会被拦截）→ 不关
    await c.page.mouse.click(1000, 850)          # 聊天空状态底部空白处
    await c.page.wait_for_timeout(400)
    c.check("桌面点内容区：侧边栏保持打开", await c.ev("state.sidebar.open"), "")

    # 从已打开的侧边栏进入会话，聊天视图仍右移
    await c.page.locator("#sidebarTree .tree-row", has_text="根会话A").first.locator(".tree-id").click()
    await c.page.wait_for_function("() => state.sessionId === 'root-a'")
    await c.page.wait_for_timeout(400)
    xc = await c.ev("els.chatView.getBoundingClientRect().x")
    c.check("聊天视图：#chatView 左缘 ≈ 280px", abs(xc - 280) <= 2, f"x={xc}")
    await c.page.mouse.click(1000, 450)          # 消息区空白处
    await c.page.wait_for_timeout(400)
    c.check("聊天视图点消息区：侧边栏保持打开", await c.ev("state.sidebar.open"), "")

    # 关闭后恢复
    await c.close_sidebar()
    await c.page.wait_for_timeout(400)
    x0 = await c.ev("els.chatView.getBoundingClientRect().x")
    c.check("关闭侧边栏：内容左缘回到 0", abs(x0) <= 2, f"x={x0}")

async def run_sidebar_persist(c):
    """切会话保持打开 + localStorage 跨刷新持久化（仅手动关）。"""
    c.sessions = [
        S("root-a", None, "根会话A", True),
        S("root-b", None, "根会话B", True),
        S("root-c", None, "根会话C", True),
    ]
    await c.start()
    await c.open_sidebar()
    await c.page.wait_for_timeout(300)
    ls = await c.ev("localStorage.getItem('e-agent.sidebar.open')")
    c.check("打开：localStorage 记录 1", ls == "1", str(ls))

    # 侧边栏树打开会话：侧边栏保持打开
    await c.page.locator("#sidebarTree .tree-row", has_text="根会话A").first.locator(".tree-id").click()
    await c.page.wait_for_function("() => state.sessionId === 'root-a'")
    await c.page.wait_for_timeout(300)
    c.check("切会话后侧边栏保持打开", await c.ev("state.sidebar.open"), "")

    # 侧边栏树里切会话：也保持打开
    await c.page.locator("#sidebarTree .tree-row", has_text="根会话B").first.click()
    await c.page.wait_for_function("() => state.sessionId === 'root-b'")
    await c.page.wait_for_timeout(300)
    c.check("侧边栏树切会话后保持打开", await c.ev("state.sidebar.open"), "")

    # 刷新：localStorage 恢复打开
    await c.page.reload(wait_until="load")
    await c.page.wait_for_function("() => typeof state !== 'undefined' && state.sidebar.open === true", timeout=8000)
    await c.page.wait_for_selector("#sidebar.open", timeout=5000)
    c.check("刷新后：侧边栏自动恢复打开", True, "")
    c.check("刷新后：#sidebar 可见", await c.ev("!els.sidebar.hidden"), "")

    # 关闭 -> localStorage 记录 0；再刷新 -> 保持关闭
    await c.close_sidebar()
    await c.page.wait_for_timeout(300)
    ls0 = await c.ev("localStorage.getItem('e-agent.sidebar.open')")
    c.check("关闭：localStorage 记录 0", ls0 == "0", str(ls0))
    await c.page.reload(wait_until="load")
    await c.page.wait_for_timeout(800)
    c.check("刷新后：保持关闭", await c.ev("!state.sidebar.open"), "")


async def run_sidebar_actions(c):
    """列表页移除后的功能迁移：所有会话操作都从侧边栏完成。"""
    pinned = {"main-pinned": True, "main-normal": False, "main-legacy": False}
    archived = {"main-archived": True, "main-normal": False, "main-legacy": False}
    deleted = set()
    base = [
        S("main-pinned", None, "置顶会话", True),
        S("main-normal", None, "普通会话", True),
        S("main-archived", None, "归档会话", True),
        S("inactive-hist", None, "历史会话", False),
        S("main-legacy", None, "旧服务器会话", True),
    ]

    def sessions():
        out = []
        for item in base:
            if item["id"] in deleted:
                continue
            row = dict(item)
            if row["id"] in pinned:
                row["pinned"] = pinned[row["id"]]
            if row["id"] in archived:
                row["archived"] = archived[row["id"]]
            out.append(row)
        return out
    c.sessions = sessions

    async def on_pin(route, url, method):
        sid = url.split("/api/sessions/")[1].split("/pin")[0]
        body = json.loads(route.request.post_data or "{}")
        c.records["pin"].append((url, route.request.post_data))
        if 200 <= c.pin_status < 300:
            pinned[sid] = bool(body.get("pinned"))
        await route.fulfill(status=c.pin_status, content_type="application/json", body="{}")

    async def on_archive(route, url, method):
        sid = url.split("/api/sessions/")[1].split("/archive")[0]
        body = json.loads(route.request.post_data or "{}")
        c.records["archive"].append((url, route.request.post_data))
        if 200 <= c.archive_status < 300:
            archived[sid] = bool(body.get("archived"))
        await route.fulfill(status=c.archive_status, content_type="application/json", body="{}")

    async def on_create(route, url, method):
        body = route.request.post_data
        c.records["create"].append(body)
        sid = json.loads(body or "{}").get("id", "sess-new")
        await route.fulfill(status=201, content_type="application/json",
                            body=json.dumps({"id": sid, "status": "Idle"}))

    async def on_delete(route, url, method):
        sid = url.split("/api/sessions/")[1]
        deleted.add(sid)
        c.records["delete"].append(url)
        await route.fulfill(status=204, content_type="application/json", body="")

    c.extra_handlers.extend([
        (lambda url, method: method == "PUT" and url.endswith("/pin"), on_pin),
        (lambda url, method: method == "PUT" and url.endswith("/archive"), on_archive),
        (lambda url, method: method == "POST" and url.rstrip("/").endswith("/api/sessions"), on_create),
        (lambda url, method: method == "DELETE" and "/api/sessions/" in url, on_delete),
    ])

    await c.start()
    await c.open_sidebar()

    # 搜索：有标题时 title 和 id 都可命中。
    await c.page.fill("#sidebarFilter", "普通会话")
    await c.page.wait_for_timeout(200)
    c.check("sidebar actions：按标题搜索", await c.page.locator(".tree-row", has_text="普通会话").count() == 1, "")
    await c.page.fill("#sidebarFilter", "main-normal")
    await c.page.wait_for_timeout(200)
    c.check("sidebar actions：有标题仍可按 ID 搜索", await c.page.locator(".tree-row", has_text="普通会话").count() == 1, "")
    await c.page.fill("#sidebarFilter", "")

    # inactive：树行点击先 POST resume，再打开。
    await c.page.locator("#sidebarTree .tree-row", has_text="历史会话").first.locator(".tree-id").click()
    await c.page.wait_for_function("() => state.sessionId === 'inactive-hist'")
    c.check("sidebar actions：inactive 会话 POST resume",
            c.records["create"] == ['{"id":"inactive-hist"}'], str(c.records["create"]))

    # pin：置顶分组内按钮取消，再在普通 workspace 行恢复置顶。
    pin_row = c.page.locator("#sidebarTree .tree-row", has_text="置顶会话").first
    c.check("sidebar actions：置顶行高亮", await pin_row.locator(".pin-btn.on").count() == 1, "")
    await pin_row.locator(".pin-btn").click()
    await c.page.wait_for_timeout(300)
    c.check("sidebar actions：pin PUT false",
            c.records["pin"][-1][1] == '{"pinned":false}', str(c.records["pin"]))

    # archive：归档组可展开并恢复；普通会话可归档。
    archive_header = c.page.locator("#sidebarTree .tree-archive-row .tree-toggle").first
    await archive_header.click()
    archived_row = c.page.locator("#sidebarTree .tree-row.archived", has_text="归档会话").first
    c.check("sidebar actions：归档分组内行可见", await archived_row.count() == 1, "")
    await archived_row.locator(".archive-btn").click()
    await c.page.wait_for_timeout(300)
    c.check("sidebar actions：archive PUT false",
            c.records["archive"][-1][1] == '{"archived":false}', str(c.records["archive"]))
    normal_row = c.page.locator("#sidebarTree .tree-row", has_text="普通会话").first
    await normal_row.locator(".archive-btn").click()
    await c.page.wait_for_timeout(300)
    c.check("sidebar actions：archive PUT true",
            c.records["archive"][-1][1] == '{"archived":true}', str(c.records["archive"]))

    # delete：树行删除后缓存和 DOM 都移除。
    legacy_row = c.page.locator("#sidebarTree .tree-row", has_text="旧服务器会话").first
    await legacy_row.locator(".tree-del").click()
    await c.page.wait_for_timeout(300)
    c.check("sidebar actions：DELETE 发出", c.records["delete"][-1].endswith("/main-legacy"), str(c.records["delete"]))
    c.check("sidebar actions：删除后树行消失",
            await c.page.locator("#sidebarTree .tree-row", has_text="旧服务器会话").count() == 0, "")

    # 旧 server 降级提示不崩。
    c.pin_status = 404
    await c.page.locator("#sidebarTree .tree-row", has_text="归档会话").first.locator(".pin-btn").click()
    await c.page.wait_for_timeout(300)
    c.check("sidebar actions：旧 server pin 404 提示", "服务器不支持置顶" in await c.ev("els.banner.textContent"), "")
    c.archive_status = 404
    await c.page.locator("#sidebarTree .tree-archive-row .tree-toggle").first.click()
    await c.page.locator("#sidebarTree .tree-row.archived", has_text="普通会话").first.locator(".archive-btn").click()
    await c.page.wait_for_timeout(300)
    c.check("sidebar actions：旧 server archive 404 提示", "服务器不支持归档" in await c.ev("els.banner.textContent"), "")

CASES = [
    {"name": "sidebar_tree", "desc": "侧边栏开合 + 会话树（活跃/历史分组、label 优先、旧 server 兼容、孤儿）",
     "run": run_sidebar_tree},
    {"name": "sidebar_filter", "desc": "侧边栏筛选（主会话匹配 + 子会话随父展开、无匹配提示）",
     "run": run_sidebar_filter},
    {"name": "sidebar_limit", "desc": "侧边栏 8 条限制 + 展开全部",
     "run": run_sidebar_limit},
    {"name": "sidebar_squeeze", "desc": "侧边栏挤压布局：桌面聊天内容右移 280px、遮罩隐藏、点内容区不关",
     "run": run_sidebar_squeeze},
    {"name": "sidebar_persist", "desc": "侧边栏持续打开：切会话保持 + localStorage 刷新恢复",
     "run": run_sidebar_persist},
    {"name": "sidebar_actions", "desc": "侧边栏唯一导航：搜索、恢复、删除、置顶、归档与旧 server 降级",
     "run": run_sidebar_actions},
]
