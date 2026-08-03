#!/usr/bin/env python3
"""真实 server 冒烟（可选，--real 启用）。

与核心 mock 用例不同，本用例连宿主 server（EAGENT_BASE，默认 18766）走真实 API：
  * 主页仍用当前 checkout 拼装的 HTML（保证测的是工作区前端）；
  * GET /api/sessions 真实数据 -> 侧边栏树渲染；
  * 若有会话则从侧边栏打开第一个 -> 真实 history + SSE。
只读操作，不创建/删除任何数据。server 不可达或 token 文件缺失 -> SKIP（不算 FAIL）。
"""
import json
import urllib.request

import common

async def run_smoke(c):
    token = common.read_token()
    if not token:
        raise common.SkipCase("token 文件不存在：%s" % common.TOKEN_FILE)
    c.token = token
    # server 可达性预检
    try:
        with urllib.request.urlopen(common.BASE, timeout=3) as r:
            if r.status != 200:
                raise common.SkipCase("server 返回 HTTP %d" % r.status)
    except Exception as e:
        raise common.SkipCase("server 不可达：%s" % e)

    page = c.page
    # 同 start()：init script 在任何页面脚本运行前种 token —— 首次加载时
    # initWorkspaces 会把带 token 的默认 workspace 落盘，刷新后 state.token
    # 才不会被空 token 的旧默认 workspace 覆盖。
    await page.add_init_script(
        "localStorage.setItem('eagent_token', %s)" % json.dumps(token))
    await page.goto(common.BASE + "/", wait_until="load")
    await page.reload(wait_until="load")
    # 等首轮真实轮询落入会话缓存，再打开侧边栏检查唯一导航。
    await page.wait_for_function(
        "() => Array.isArray(state.lastList) && state.workspaceErrors[state.workspace.id] !== undefined",
        timeout=10000)
    n = await c.ev("state.lastList.length")
    await c.open_sidebar()
    c.check("真实 API：侧边栏会话树渲染（%d 条缓存）" % n,
            (await page.locator("#sidebarTree .tree-row .tree-id").count()) >= min(n, 1), "")

    if n > 0:
        sid = await c.ev("state.lastList.find(s => !s.parent_session_id)?.id || state.lastList[0].id")
        row = page.locator("#sidebarTree .tree-row").filter(has=page.locator(".tree-id", has_text=sid)).first
        await row.locator(".tree-id").click()
        await page.wait_for_function("id => state.sessionId === id", arg=sid, timeout=8000)
        await page.wait_for_timeout(1500)
        c.check("真实 API：从侧边栏打开会话", await c.ev("state.sessionId") == sid,
                await c.ev("state.sessionId"))
        c.check("真实 API：聊天视图可见且非空状态",
                await c.ev("!els.chatView.classList.contains('hidden') && !els.chatView.classList.contains('no-session')"), "")

CASES = [
    {"name": "smoke_real_api",
     "desc": "真实 server 冒烟：侧边栏树渲染 + 打开会话（--real 启用，只读）",
     "requires_server": True, "real_api": True, "token": None, "run": run_smoke},
]
