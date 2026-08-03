#!/usr/bin/env python3
"""手机视口用例：390px 不横向溢出（聊天空状态 / 聊天 / 侧边栏展开）。

手机视口（≤600px）行为：侧边栏仍为覆盖式（遮罩显示、内容不位移）；
桌面挤压布局见 sidebar_squeeze / sidebar_persist。
"""
import common

def S(id_, parent=None, title=None, label=None, active=True, busy=False):
    d = {"id": id_, "model": "flash", "role": "fixer" if parent else "main",
         "status": "Busy" if busy else "Idle", "busy": busy,
         "parent_session_id": parent, "title": title,
         "entry_count": 2, "created_at": "2026-01-01T00:00:00Z"}
    if active is not None:
        d["active"] = active
    if label is not None:
        d["label"] = label
    return d

SESSIONS = [
    S("root-a", None, "根会话A"),
    S("root-b", None, "根会话B"),
    S("sub-a1", "root-a", "子任务", label="任务A-1"),
    S("sub-a2", "root-a", "旧任务", active=False, label="任务A-2"),
    S("orphan-1", "ghost", "孤儿", label="孤儿任务"),
]

async def run_mobile_390(c):
    c.sessions = SESSIONS
    await c.start()

    def overflow_ok():
        return c.ev("[document.documentElement.scrollWidth <= window.innerWidth,"
                    " document.documentElement.scrollWidth, window.innerWidth]")

    w = await overflow_ok()
    c.check("聊天空状态 390px 不横向溢出", w[0] is True, str(w[1:]))
    c.check("未选会话显示空状态", await c.ev("state.sessionId === null && els.chatView.classList.contains('no-session')"), "")

    # 打开侧边栏（覆盖式），通过唯一导航打开会话
    await c.open_sidebar()
    await c.page.wait_for_timeout(400)
    w = await overflow_ok()
    c.check("侧边栏打开 390px 不横向溢出", w[0] is True, str(w[1:]))
    sw = await c.ev("els.sidebar.getBoundingClientRect().width")
    c.check("侧边栏宽度 <= 85vw(331.5)", sw <= 331.5, f"w={sw}")
    c.check("手机覆盖式：内容不右移",
            await c.ev("els.chatView.getBoundingClientRect().x") == 0, "")
    c.check("手机遮罩显示",
            await c.ev("!els.sidebarOverlay.hidden && getComputedStyle(els.sidebarOverlay).display !== 'none'"), "")

    # 树行打开会话：侧边栏保持打开，聊天可见
    tree_rows = c.page.locator("#sidebarTree .tree-row:not(.tasks-group-head)")
    await tree_rows.first.locator(".tree-id").click()      # 点文本区，避开行内按钮
    await c.page.wait_for_function("() => state.sessionId === 'root-a'")
    await c.page.wait_for_timeout(300)
    c.check("树行切会话后侧边栏仍打开", await c.ev("state.sidebar.open") is True, "")
    w = await overflow_ok()
    c.check("聊天视图 390px 不横向溢出", w[0] is True, str(w[1:]))
    c.check("切会话后仍不横向溢出", w[0] is True, str(w[1:]))

    # 点遮罩关闭
    await c.page.mouse.click(370, 400)
    await c.page.wait_for_timeout(500)
    c.check("手机点遮罩关闭侧边栏", await c.ev("!state.sidebar.open"), "")

CASES = [
    {"name": "mobile_390", "desc": "手机视口 390px：聊天空状态/聊天/侧边栏无横向溢出 + 遮罩关闭",
     "mobile": True, "viewport": {"width": 390, "height": 844}, "run": run_mobile_390},
]
