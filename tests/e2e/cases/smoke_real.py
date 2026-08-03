#!/usr/bin/env python3
"""真实 server 冒烟（可选，--real 启用）。

与核心 mock 用例不同，本用例连宿主 server（EAGENT_BASE，默认 18766）走真实 API：
  * 主页仍用当前 checkout 拼装的 HTML（保证测的是工作区前端）；
  * GET /api/sessions 真实列表 -> 渲染；
  * 若有会话则打开第一个 -> 真实 history + SSE。
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
    # 等首轮真实轮询渲染完成（有行或空态提示）
    await page.wait_for_function(
        "() => document.querySelectorAll('.session-row').length > 0 || "
        "(els.listHint && els.listHint.textContent.includes('暂无会话'))",
        timeout=10000)
    n = await page.locator(".session-row").count()
    c.check("真实 API：会话列表渲染（%d 行）" % n, n >= 0, "")

    if n > 0:
        await page.locator("#sessionList .session-row").first.click()
        await page.wait_for_function("() => state.view === 'chat'", timeout=8000)
        await page.wait_for_timeout(1500)
        c.check("真实 API：打开会话进入聊天视图", await c.ev("state.view") == "chat",
                await c.ev("state.sessionId"))
        c.check("真实 API：聊天视图可见", await c.ev("!els.chatView.classList.contains('hidden')"), "")

CASES = [
    {"name": "smoke_real_api",
     "desc": "真实 server 冒烟：列表渲染 + 打开会话（--real 启用，只读）",
     "requires_server": True, "real_api": True, "token": None, "run": run_smoke},
]
