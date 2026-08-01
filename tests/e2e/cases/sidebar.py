#!/usr/bin/env python3
"""侧边栏用例（基于 05fc4c8 已合入功能）：

  * sidebar_tree   —— 开合（按钮/×/Escape/遮罩）+ 会话树渲染：
                       主会话根节点、subagent 活跃/历史分组、label 优先、
                       旧 server 兼容（无 label/active）、busy 提示、孤儿组。
  * sidebar_filter —— 筛选：主会话按 title/id 匹配，匹配的展开显示全部子会话；
                       孤儿组按自身 title/id 匹配；无匹配提示。
  * sidebar_limit  —— 15 条限制 + 「+N 个更早的会话」展开全部。

未合入（见 cases/skipped.py，TODO）：挤压布局、持续打开/持久化。
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
    overlay_ok = await c.ev(
        "!els.sidebarOverlay.hidden && getComputedStyle(els.sidebarOverlay).display !== 'none'")
    c.check("打开：.open + 遮罩显示", overlay_ok, "")
    n_roots = await c.page.locator("#sidebarTree > .tree-node > .tree-row").count()
    c.check("默认全部折叠：3 根 + 未关联 = 4 行", n_roots == 4, f"rows={n_roots}")

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
    await c.page.mouse.click(1000, 450)          # 点遮罩区域
    await c.page.wait_for_timeout(400)
    c.check("E 点遮罩关闭侧边栏", await c.ev("!state.sidebar.open"), "")

async def run_sidebar_filter(c):
    c.sessions = SESSIONS
    await c.start()
    await c.open_sidebar()
    await c.page.wait_for_selector("#sidebarTree .tree-row", timeout=5000)

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
    roots = await c.page.locator("#sidebarTree > .tree-node > .tree-row").count()
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
    n = await c.page.locator("#sidebarTree > .tree-node > .tree-row").count()
    c.check("清空筛选恢复 3 根 + 未关联", n == 4, f"rows={n}")

async def run_sidebar_limit(c):
    # 18 个主会话（无子会话）：默认只渲染 15 条 + 「+3 个更早的会话」按钮
    sessions = [S("root-%02d" % i, None, "主会话%02d" % i, True) for i in range(18)]
    c.sessions = sessions
    await c.start()
    await c.open_sidebar()
    await c.page.wait_for_selector("#sidebarTree .tree-row", timeout=5000)
    n = await c.page.locator("#sidebarTree > .tree-node > .tree-row").count()
    c.check("默认只渲染最近 15 个主会话", n == 15, f"rows={n}")
    more = c.page.locator("#sidebarTree .tree-more")
    c.check("显示「+3 个更早的会话」按钮",
            await more.count() == 1 and await more.text_content() == "+3 个更早的会话",
            f"n={await more.count()}")
    await more.click()
    await c.page.wait_for_timeout(300)
    n2 = await c.page.locator("#sidebarTree > .tree-node > .tree-row").count()
    c.check("点击后展开全部 18 个主会话", n2 == 18, f"rows={n2}")

CASES = [
    {"name": "sidebar_tree", "desc": "侧边栏开合 + 会话树（活跃/历史分组、label 优先、旧 server 兼容、孤儿）",
     "run": run_sidebar_tree},
    {"name": "sidebar_filter", "desc": "侧边栏筛选（主会话匹配 + 子会话随父展开、无匹配提示）",
     "run": run_sidebar_filter},
    {"name": "sidebar_limit", "desc": "侧边栏 15 条限制 + 展开全部",
     "run": run_sidebar_limit},
]
