#!/usr/bin/env python3
"""侧边栏用例（基于 05fc4c8 已合入功能）：

  * sidebar_tree   —— 开合（按钮/×/Escape/遮罩）+ 会话树渲染：
                       主会话根节点、subagent 活跃/历史分组、label 优先、
                       旧 server 兼容（无 label/active）、busy 提示、孤儿组。
  * sidebar_filter —— 筛选：主会话按 title/id 匹配，匹配的展开显示全部子会话；
                       孤儿组按自身 title/id 匹配；无匹配提示。
  * sidebar_limit  —— 15 条限制 + 「+N 个更早的会话」展开全部。
  * sidebar_squeeze —— 挤压布局（桌面 >600px）：内容右移 280px、遮罩 display:none、
                       点内容区不关（旧覆盖式点遮罩才关）。
  * sidebar_persist —— 持续打开：切会话/返回列表保持打开 + localStorage 跨刷新恢复。
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
    await c.start()
    n_rows = await c.page.locator(".session-row").count()
    c.check("启动：列表渲染 12 行（3 根 + 7 子 + 2 孤儿）", n_rows == 12, f"rows={n_rows}")

    # ---------- 开合 ----------
    await c.open_sidebar()
    c.check("打开：.open + 侧边栏可见",
            await c.page.locator("#sidebar.open").count() == 1 and await c.ev("!els.sidebar.hidden"), "")
    c.check("桌面打开：遮罩 display:none（挤压布局，不遮挡内容）",
            await c.ev("getComputedStyle(els.sidebarOverlay).display") == "none", "")
    n_roots = await c.page.locator(
        "#sidebarTree > .tree-node:not(.tasks-group) > .tree-row").count()
    c.check("默认全部折叠：3 根 + 未关联 = 4 行（不含隐藏的「运行中任务」组）",
            n_roots == 4, f"rows={n_roots}")

    tree = lambda: c.page.locator("#sidebarTree")

    # ---------- A：root-a（活跃 label / 非活跃 label / 旧 server 兼容 / busy） ----------
    root_a = tree().locator(".tree-row", has_text="根会话A").first
    await root_a.locator("button.tree-toggle").click()
    await c.page.wait_for_timeout(200)
    a_node = root_a.locator("xpath=..")
    lbl = a_node.locator(".tree-row-child:not(.tree-hist)", has_text="任务A-1")
    c.check("A1 活跃 subagent 显示 label（非 id）",
            await lbl.count() == 1 and await lbl.locator(".tree-id").text_content() == "任务A-1",
            f"count={await lbl.count()}")
    c.check("A1 不显示 label 之外的回退（id 不出现）",
            await a_node.locator(".tree-row-child", has_text="sub-a1-").count() == 0, "")
    c.check("A2 非活跃 subagent 不在默认列表",
            await a_node.locator(".tree-row-child:not(.tree-hist)", has_text="任务A-2").count() == 0, "")
    hist_a = a_node.locator(".tree-hist-row")
    c.check("A2 非活跃收进「历史子会话 (1)」折叠组",
            await hist_a.count() == 1
            and await hist_a.locator(".tree-hist-label").text_content() == "历史子会话 (1)",
            f"n={await hist_a.count()}")
    c.check("A2 历史组默认收起（children hidden）",
            await hist_a.locator("xpath=..").locator(".tree-children").get_attribute("hidden") is not None, "")
    await hist_a.locator("button.tree-toggle").click()
    await c.page.wait_for_timeout(200)
    c.check("A2 点击展开后非活跃可见",
            await a_node.locator(".tree-row-child.tree-hist", has_text="任务A-2").count() == 1
            and await a_node.locator(".tree-row-child.tree-hist", has_text="任务A-2").is_visible(), "")
    c.check("A3 旧 server 兼容（无 label 无 active -> 直接显示 + title 回退）",
            await a_node.locator(".tree-row-child:not(.tree-hist)", has_text="旧服务器子会话").count() == 1, "")
    busy_row = a_node.locator(".tree-row-child:not(.tree-hist)", has_text="忙碌任务A-4")
    c.check("A4 busy 活跃 subagent 直接显示 + label + busy 点",
            await busy_row.count() == 1
            and await busy_row.locator(".busy-dot.busy").count() == 1, "")
    c.check("A4 忙碌 subagent 行 title 提示可发送消息",
            "可发送消息" in (await busy_row.get_attribute("title")), "")

    # ---------- B：root-b 全非活跃 -> 只显示历史组 ----------
    root_b = tree().locator(".tree-row", has_text="根会话B").first
    await root_b.locator("button.tree-toggle").click()
    await c.page.wait_for_timeout(200)
    b_node = root_b.locator("xpath=..")
    c.check("B 全非活跃父节点：无直接活跃行",
            await b_node.locator(".tree-row-child:not(.tree-hist)").count() == 0, "")
    hist_b = b_node.locator(".tree-hist-row")
    c.check("B 只显示「历史子会话 (1)」分组",
            await hist_b.count() == 1 and "历史子会话 (1)" in (await hist_b.text_content()), "")

    # ---------- C：root-c 无 label 回退 ----------
    root_c = tree().locator(".tree-row", has_text="根会话C").first
    await root_c.locator("button.tree-toggle").click()
    await c.page.wait_for_timeout(200)
    c_node = root_c.locator("xpath=..")
    c.check("C 无 label 活跃 subagent -> title 回退",
            await c_node.locator(".tree-row-child:not(.tree-hist)", has_text="无标签子C").count() == 1, "")
    hist_c = c_node.locator(".tree-hist-row")
    c.check("C 无 label 非活跃 -> 进历史组",
            await hist_c.count() == 1 and "历史子会话 (1)" in (await hist_c.text_content()), "")
    await hist_c.locator("button.tree-toggle").click()
    await c.page.wait_for_timeout(200)
    c.check("C 无 label 无 title 非活跃 -> shortId 回退",
            await c_node.locator(".tree-row-child.tree-hist", has_text="sub-c2-a…").count() == 1, "")

    # ---------- D：孤儿「未关联」同样处理 ----------
    unrel = tree().locator(".tree-row", has_text="未关联").first
    await unrel.locator("button.tree-toggle").click()
    await c.page.wait_for_timeout(200)
    u_node = unrel.locator("xpath=..")
    c.check("D 未关联组默认只显示活跃孤儿（label）",
            await u_node.locator(".tree-row-child:not(.tree-hist)", has_text="孤儿活跃标签").count() == 1
            and await u_node.locator(".tree-row-child:not(.tree-hist)", has_text="孤儿历史标签").count() == 0, "")
    hist_u = u_node.locator(".tree-hist-row")
    c.check("D 未关联内非活跃孤儿收进「历史子会话 (1)」",
            await hist_u.count() == 1 and "历史子会话 (1)" in (await hist_u.text_content()), "")
    await hist_u.locator("button.tree-toggle").click()
    await c.page.wait_for_timeout(200)
    c.check("D 展开后孤儿历史可见",
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
    roots = await c.page.locator("#sidebarTree > .tree-node:not(.tasks-group) > .tree-row").count()
    c.check("筛选命中 1 个主会话", roots == 1, f"roots={roots}")
    # root-b 全非活跃：子会话收进历史组（筛选时父节点自动展开）
    hist = await c.page.locator("#sidebarTree .tree-hist-row").count()
    c.check("筛选时匹配父节点的子会话展开（历史组可见）", hist == 1, f"hist={hist}")

    # 按孤儿 title 匹配（app 的筛选逻辑是 s.title || s.id，有 title 时只搜 title）
    # -> 「未关联」组出现
    await c.page.fill("#sidebarFilter", "孤A")
    await c.page.wait_for_timeout(200)
    g = await c.page.locator("#sidebarTree > .tree-node > .tree-row", has_text="未关联").count()
    c.check("筛选命中孤儿 -> 未关联组显示", g == 1, f"groups={g}")
    unrel = c.page.locator("#sidebarTree > .tree-node > .tree-row", has_text="未关联").first
    await unrel.locator("button.tree-toggle").click()
    await c.page.wait_for_timeout(200)
    c.check("孤儿组展开后命中孤儿可见",
            await c.page.locator(".tree-row-child", has_text="孤儿活跃标签").is_visible(), "")

    # 清空 -> 恢复默认 4 行
    await c.page.fill("#sidebarFilter", "")
    await c.page.wait_for_timeout(200)
    n = await c.page.locator("#sidebarTree > .tree-node:not(.tasks-group) > .tree-row").count()
    c.check("清空筛选恢复 3 根 + 未关联", n == 4, f"rows={n}")

async def run_sidebar_limit(c):
    # 18 个主会话（无子会话）：默认只渲染最近 8 条 + 「+10 个更早的会话」按钮
    sessions = [S("root-%02d" % i, None, "主会话%02d" % i, True) for i in range(18)]
    c.sessions = sessions
    await c.start()
    await c.open_sidebar()
    await c.page.wait_for_selector("#sidebarTree .tree-row:not(.tasks-group-head)", timeout=5000)
    n = await c.page.locator("#sidebarTree > .tree-node:not(.tasks-group) > .tree-row").count()
    c.check("默认只渲染最近 8 个主会话", n == 8, f"rows={n}")
    more = c.page.locator("#sidebarTree .tree-more")
    c.check("显示「+10 个更早的会话」按钮",
            await more.count() == 1 and await more.text_content() == "+10 个更早的会话",
            f"n={await more.count()}")
    await more.click()
    await c.page.wait_for_timeout(300)
    n2 = await c.page.locator("#sidebarTree > .tree-node:not(.tasks-group) > .tree-row").count()
    c.check("点击后展开全部 18 个主会话", n2 == 18, f"rows={n2}")


async def run_sidebar_squeeze(c):
    """桌面视口（>600px）：打开侧边栏时挤压内容——#topbar/#listView/#chatView 右移
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
    c.check("桌面打开：#topbar margin-left = 280px", ml == "280px", ml)
    x = await c.ev("els.listView.getBoundingClientRect().x")
    c.check("桌面打开：#listView 左缘 ≈ 280px", abs(x - 280) <= 2, f"x={x}")
    ov = await c.ev("getComputedStyle(els.sidebarOverlay).display")
    c.check("桌面打开：遮罩 display:none（不遮挡内容）", ov == "none", ov)

    # 点内容区（遮罩已 display:none，不会被拦截）→ 不关
    await c.page.mouse.click(1000, 850)          # 列表卡片底部空白处
    await c.page.wait_for_timeout(400)
    c.check("桌面点内容区：侧边栏保持打开", await c.ev("state.sidebar.open"), "")

    # 聊天视图同样右移
    await c.page.locator("#sessionList .session-row", has_text="根会话A").first.click()
    await c.page.wait_for_function("() => state.view === 'chat'")
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
    """切会话/返回列表保持打开 + localStorage 跨刷新持久化（仅手动关）。"""
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

    # 列表点会话行进入聊天：侧边栏保持打开
    await c.page.locator("#sessionList .session-row", has_text="根会话A").first.click()
    await c.page.wait_for_function("() => state.view === 'chat' && state.sessionId === 'root-a'")
    await c.page.wait_for_timeout(300)
    c.check("切会话后侧边栏保持打开", await c.ev("state.sidebar.open"), "")

    # 返回列表：侧边栏保持打开
    await c.page.click("#backBtn")
    await c.page.wait_for_function("() => state.view === 'list'")
    await c.page.wait_for_timeout(300)
    c.check("返回列表后侧边栏保持打开", await c.ev("state.sidebar.open"), "")

    # 侧边栏树里切会话：也保持打开
    await c.page.locator("#sidebarTree .tree-row", has_text="根会话B").first.click()
    await c.page.wait_for_function("() => state.view === 'chat' && state.sessionId === 'root-b'")
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

CASES = [
    {"name": "sidebar_tree", "desc": "侧边栏开合 + 会话树（活跃/历史分组、label 优先、旧 server 兼容、孤儿）",
     "run": run_sidebar_tree},
    {"name": "sidebar_filter", "desc": "侧边栏筛选（主会话匹配 + 子会话随父展开、无匹配提示）",
     "run": run_sidebar_filter},
    {"name": "sidebar_limit", "desc": "侧边栏 15 条限制 + 展开全部",
     "run": run_sidebar_limit},
    {"name": "sidebar_squeeze", "desc": "侧边栏挤压布局：桌面内容右移 280px、遮罩隐藏、点内容区不关",
     "run": run_sidebar_squeeze},
    {"name": "sidebar_persist", "desc": "侧边栏持续打开：切会话/返回列表保持 + localStorage 刷新恢复",
     "run": run_sidebar_persist},
]
