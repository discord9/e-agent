#!/usr/bin/env python3
"""聊天视图用例（基于 05fc4c8 已合入功能）：

  * chat_open_sse     —— openSession：列表行点开进入聊天视图、加载历史；
                          SSE 基本流（mock 事件）：status/UserPrompt/AssistantDelta/
                          ReasoningDelta/ToolCall/ToolResult/AssistantText/Notice/
                          Usage/Error 的渲染；Busy 状态取消可用。
  * chat_state_preserved —— 状态保留：切走再切回，草稿 / 滚动位置 / 消息缓存
                          原样恢复，且不重新加载历史。
  * chat_error_render —— 错误渲染：statusLabel('Failed(xx)')=失败、error chip、
                          appendError 前缀、Finished 禁输入。
"""
import json

import common

def S(id_, status="Idle", busy=False, parent=None, title=None, active=True):
    return {"id": id_, "model": "flash", "role": "fixer" if parent else "main",
            "status": status, "busy": busy, "active": active,
            "parent_session_id": parent, "title": title,
            "entry_count": 2, "created_at": "2026-01-01T00:00:00Z"}

async def run_chat_open_sse(c):
    c.sessions = [S("main-idle", "Idle", False, None, "主会话")]
    c.history = {
        "entries": [
            {"type": "message", "message": {"User": {"content": "你好，帮我看看", "images": []}}},
            {"type": "message", "message": {"Assistant": {"content": "好的，我来处理。",
                "tool_calls": [{"id": "call1", "name": "bash", "arguments": '{"command":"ls"}'}],
                "reasoning": None}}},
            {"type": "message", "message": {"Tool": {"call_id": "call1", "name": "bash",
                "content": "file1\nfile2", "is_error": False, "synthetic": False}}},
            {"type": "compaction", "summary": "早期内容已压缩", "retained": []},
        ],
        "next_before_seq": None,
    }
    c.sse_body = "\n\n".join([
        "event: status\ndata: {\"status\":\"Busy\"}",
        "event: UserPrompt\ndata: {\"type\":\"user_prompt\",\"session_id\":\"s1\",\"seq\":1,\"text\":\"再来一次\"}",
        "event: AssistantDelta\ndata: {\"type\":\"assistant_delta\",\"session_id\":\"s1\",\"seq\":2,\"delta\":\"正在\"}",
        "event: AssistantDelta\ndata: {\"type\":\"assistant_delta\",\"session_id\":\"s1\",\"seq\":3,\"delta\":\"处理\"}",
        "event: ReasoningDelta\ndata: {\"type\":\"reasoning_delta\",\"session_id\":\"s1\",\"seq\":4,\"delta\":\"推理中\"}",
        "event: ToolCall\ndata: {\"type\":\"tool_call\",\"session_id\":\"s1\",\"seq\":5,\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a.txt\\\"}\"}",
        "event: ToolResult\ndata: {\"type\":\"tool_result\",\"session_id\":\"s1\",\"seq\":6,\"is_error\":true,\"content\":\"文件不存在\"}",
        "event: AssistantText\ndata: {\"type\":\"assistant_text\",\"session_id\":\"s1\",\"seq\":7,\"text\":\"出错了，**换个方式**。\"}",
        "event: Notice\ndata: {\"type\":\"notice\",\"session_id\":\"s1\",\"seq\":8,\"text\":\"系统提示行\"}",
        "event: Usage\ndata: {\"type\":\"usage\",\"session_id\":\"s1\",\"seq\":9,\"context_input\":1234,\"session\":{\"input_tokens\":100,\"output_tokens\":50}}",
        "event: Error\ndata: {\"type\":\"error\",\"session_id\":\"s1\",\"seq\":10,\"error\":\"回合失败\"}",
    ]) + "\n\n"

    await c.start()
    await c.page.locator("#sessionList .session-row", has_text="main-idle").first.click()
    await c.page.wait_for_timeout(800)
    c.check("openSession：进入聊天视图", await c.ev("state.view") == "chat", await c.ev("state.view"))
    c.check("openSession：会话 id 显示",
            (await c.page.locator("#chatSessionId").text_content()) == "会话 main-idle", "")
    c.check("openSession：chatView 可见、listView 隐藏",
            await c.ev("!els.chatView.classList.contains('hidden') && els.listView.classList.contains('hidden')"), "")

    # history 渲染
    t = await c.ev("els.messages.textContent")
    c.check("历史：用户消息", "你好，帮我看看" in t, "")
    c.check("历史：助手消息", "好的，我来处理。" in t, "")
    c.check("历史：工具卡片（bash + 完成）",
            await c.ev("els.messages.querySelectorAll('.tool-card').length") >= 1
            and "bash" in t and "file1" in t, "")
    c.check("历史：压缩分界线", "—— 上下文已压缩 ——" in t and "早期内容已压缩" in t, "")

    # SSE live 事件
    c.check("SSE：UserPrompt 渲染", "再来一次" in t, "")
    c.check("SSE：AssistantDelta 累积（正在处理）", "正在" in t and "处理" in t, "")
    c.check("SSE：ReasoningDelta 进思考块", "推理中" in t, "")
    c.check("SSE：ToolCall + 错误 ToolResult（失败）",
            "read_file" in t and "文件不存在" in t
            and await c.ev("els.messages.querySelectorAll('.tool-result.err').length") == 1, "")
    html = await c.ev("els.messages.innerHTML")
    c.check("SSE：AssistantText markdown 渲染", "出错了，" in t and "<strong>换个方式</strong>" in html, "")
    c.check("SSE：Notice 渲染", "系统提示行" in t, "")
    c.check("SSE：Usage 渲染",
            "上下文 1234" in await c.ev("els.usageInfo.textContent")
            and "输入 100" in await c.ev("els.usageInfo.textContent")
            and "输出 50" in await c.ev("els.usageInfo.textContent"),
            await c.ev("els.usageInfo.textContent"))
    c.check("SSE：Error 渲染", "错误: 回合失败" in t, "")
    c.check("SSE：status Busy -> 处理中 + 取消可用",
            (await c.page.locator("#chatStatus").text_content()) == "处理中"
            and await c.ev("els.cancelBtn.disabled") is False
            and await c.ev("els.compactBtn.disabled") is True, "")

    # Busy -> Idle：取消/压缩恢复
    await c.ev("applyStatus('Idle')")
    c.check("回到 Idle：取消禁用、压缩可用",
            await c.ev("els.cancelBtn.disabled") is True and await c.ev("els.compactBtn.disabled") is False, "")

async def run_chat_state_preserved(c):
    c.sessions = [S("sess-a", "Idle", False, None), S("sess-b", "Idle", False, None)]
    entries = []
    for i in range(30):                                  # 足够长，让 messages 可滚动
        entries.append({"type": "message",
                        "message": {"User": {"content": "历史消息 %d" % i, "images": []}}})
    entries.append({"type": "message",
                    "message": {"Assistant": {"content": "长内容" * 60, "tool_calls": [],
                                              "reasoning": None}}})
    c.history = {"entries": entries, "next_before_seq": None}

    # 本用例不需要 SSE 事件：让 /events 流挂起（不结束），避免 app 的 3s 断线
    # 重连触发 loadHistory，干扰「不重新加载历史」的计数断言。
    import asyncio as _asyncio

    async def hold_events(route, url, method):
        await _asyncio.sleep(120)
        await route.fulfill(status=200, headers={"content-type": "text/event-stream"}, body="")
    c.extra_handlers.append((lambda url, method: url.endswith("/events"), hold_events))

    await c.start()
    await c.page.locator("#sessionList .session-row", has_text="sess-a").first.click()
    await c.page.wait_for_selector("#messages .msg-user", timeout=8000)
    n = await c.page.locator("#messages .msg-user").count()
    c.check("打开 sess-a：历史渲染", n == 30, f"msgs={n}")

    # 写草稿 + 上滚
    await c.page.fill("#promptInput", "草稿甲")
    await c.ev("els.promptInput.dispatchEvent(new Event('input'))")
    saved_scroll = await c.ev("els.messages.scrollTop = 300; els.messages.scrollTop")
    c.check("会话可滚动（scrollTop 生效）", saved_scroll > 0, f"scrollTop={saved_scroll}")

    # 切到 sess-b（首次打开 -> 拉历史）
    await c.page.click("#backBtn")
    await c.page.wait_for_function("() => state.view === 'list'")
    await c.page.locator("#sessionList .session-row", has_text="sess-b").first.click()
    await c.page.wait_for_function("() => state.view === 'chat' && state.sessionId === 'sess-b'")
    c.check("切到 sess-b：进入聊天", await c.ev("state.sessionId") == "sess-b", "")

    # 切回 sess-a：草稿/滚动/缓存恢复，不重新拉历史
    await c.page.click("#backBtn")
    await c.page.wait_for_function("() => state.view === 'list'")
    await c.page.locator("#sessionList .session-row", has_text="sess-a").first.click()
    await c.page.wait_for_function("() => state.view === 'chat' && state.sessionId === 'sess-a'")
    await c.page.wait_for_timeout(300)
    c.check("切回 sess-a：草稿恢复",
            await c.ev("els.promptInput.value") == "草稿甲",
            await c.ev("els.promptInput.value"))
    restored_scroll = await c.ev("els.messages.scrollTop")
    c.check("切回 sess-a：滚动位置恢复", abs(restored_scroll - saved_scroll) <= 2,
            f"saved={saved_scroll} restored={restored_scroll}")
    c.check("切回 sess-a：消息缓存恢复（不重新渲染）",
            await c.page.locator("#messages .msg-user").count() == 30, "")
    hist_a = [u for u, _ in c.records["history"] if "/sess-a/history" in u]
    c.check("切回 sess-a：不重新加载历史（仅首次 1 次）", len(hist_a) == 1, f"fetches={len(hist_a)}")

async def run_chat_error_render(c):
    c.sessions = [
        S("main-failed", "Failed", False, None, "失败会话"),
        S("main-finished", "Finished", False, None, "结束会话"),
        S("sub-finished", "Finished", False, "main-finished", "子任务已完"),
    ]
    await c.start()

    # 列表行：Failed -> 失败 chip
    row = c.page.locator("#sessionList .session-row", has_text="main-failed").first
    chip = row.locator(".status-chip")
    c.check("列表：Failed 会话 chip=失败 + error 样式",
            (await chip.text_content()) == "失败" and "error" in (await chip.get_attribute("class")),
            await chip.get_attribute("class"))

    # 打开失败会话 -> applyStatus('Failed(xx)') -> 失败
    await row.click()
    await c.page.wait_for_function("() => state.view === 'chat'")
    await c.ev("applyStatus('Failed(xx)')")
    c.check("applyStatus('Failed(xx)')：状态标签=失败",
            (await c.page.locator("#chatStatus").text_content()) == "失败", "")
    c.check("statusLabel('Failed(xx)') === '失败'",
            await c.ev("statusLabel('Failed(xx)')") == "失败", "")
    c.check("statusChipClass('Failed(xx)') === 'error'",
            await c.ev("statusChipClass('Failed(xx)')") == "error", "")
    c.check("Failed 非 Finished：输入仍可用",
            await c.ev("els.sendBtn.disabled") is False, "")

    # appendError 旧路径
    t = await c.ev("appendError('boom'); els.messages.textContent")
    c.check("appendError -> 「错误: 」前缀", "错误: boom" in t, "")

    # Finished 主会话：禁输入 + 会话已结束
    await c.ev("openSession('main-finished')")
    await c.page.wait_for_timeout(400)
    await c.ev("applyStatus('Finished')")
    c.check("Finished 主会话：sendBtn+输入框禁用 + 会话已结束",
            await c.ev("els.sendBtn.disabled") is True
            and await c.ev("els.promptInput.disabled") is True
            and await c.ev("els.promptInput.placeholder") == "会话已结束",
            await c.ev("els.promptInput.placeholder"))

    # Finished subagent：子任务提示
    await c.ev("openSession('sub-finished')")
    await c.page.wait_for_timeout(400)
    await c.ev("applyStatus('Finished')")
    c.check("Finished subagent：placeholder=子任务已结束",
            await c.ev("els.promptInput.placeholder") == "子任务已结束，无法继续发送",
            await c.ev("els.promptInput.placeholder"))

CASES = [
    {"name": "chat_open_sse", "desc": "openSession + SSE 基本流（mock 事件渲染）+ Busy 状态",
     "run": run_chat_open_sse},
    {"name": "chat_state_preserved", "desc": "切会话状态保留（草稿/滚动/缓存，不重拉历史）",
     "run": run_chat_state_preserved},
    {"name": "chat_error_render", "desc": "错误渲染（Failed->失败、错误行、Finished 禁输入）",
     "run": run_chat_error_render},
]
