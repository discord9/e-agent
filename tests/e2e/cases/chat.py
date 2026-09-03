#!/usr/bin/env python3
"""聊天视图用例（基于 05fc4c8 已合入功能）：

  * chat_open_sse     —— 侧边栏树打开会话、加载历史；
                          SSE 基本流（mock 事件）：status/UserPrompt/AssistantDelta/
                          ReasoningDelta/ToolCall/ToolResult/AssistantText/Notice/
                          Usage/Error 的渲染；Busy 状态取消可用。
  * chat_state_preserved —— 状态保留：切走再切回，草稿 / 滚动位置 / 消息缓存
                          原样恢复，且不重新加载历史。
  * chat_error_render —— 错误渲染：statusLabel('Failed(xx)')=失败、error chip、
                          appendError 前缀、Finished 禁输入。
  * conflict_card    —— 并发写冲突（按当前合入现状）：Failed 状态 -> 「失败」chip、
                         冲突错误消息渲染为「错误: 」普通行（无 .msg-error.conflict
                         友好卡片 —— 卡片未合入，用例登记现状）。
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
            {"type": "message", "message": {"Assistant": {"content": "好的，`<workspace>/.e-agent/agents/*.md` 和 <workspace>。",
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
        "event: AssistantText\ndata: {\"type\":\"assistant_text\",\"session_id\":\"s1\",\"seq\":7,\"text\":\"出错了，**换个方式**，`<workspace>/.e-agent/agents/*.md` 和 <workspace>。\"}",
        "event: Notice\ndata: {\"type\":\"notice\",\"session_id\":\"s1\",\"seq\":8,\"text\":\"系统提示行\"}",
        "event: Usage\ndata: {\"type\":\"usage\",\"session_id\":\"s1\",\"seq\":9,\"context_input\":1234,\"session\":{\"input_tokens\":100,\"output_tokens\":50}}",
        "event: Error\ndata: {\"type\":\"error\",\"session_id\":\"s1\",\"seq\":10,\"error\":\"回合失败\"}",
    ]) + "\n\n"

    await c.start()
    await c.open_sidebar()
    await c.page.locator("#sidebarTree .tree-row", has_text="main-idle").first.locator(".tree-id").click()
    await c.page.wait_for_function("() => state.sessionId === 'main-idle'")
    await c.page.wait_for_timeout(800)
    c.check("openSession：进入当前会话", await c.ev("state.sessionId") == "main-idle",
            await c.ev("state.sessionId"))
    c.check("openSession：标题与会话 id 显示",
            "主会话" in (await c.page.locator("#chatSessionId").text_content())
            and "main-idle" in (await c.page.locator("#chatSessionId").text_content()), "")
    c.check("openSession：chatView 可见且退出空状态",
            await c.ev("!els.chatView.classList.contains('hidden') && !els.chatView.classList.contains('no-session')"), "")

    # history 渲染
    t = await c.ev("els.messages.textContent")
    c.check("历史：用户消息", "你好，帮我看看" in t, "")
    c.check("历史：助手消息", "好的，" in t and "<workspace>/.e-agent/agents/*.md" in t
            and "<workspace>" in t, "")
    history_body = c.page.locator("#messages .msg-assistant .msg-body").first
    c.check("历史：占位符 DOM 文本无嵌套 HTML",
            await history_body.locator("code").first.text_content() == "<workspace>/.e-agent/agents/*.md"
            and await history_body.locator("code").first.locator("*").count() == 0,
            await history_body.inner_html())
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
    live_body = c.page.locator("#messages .msg-assistant .msg-body").last
    c.check("SSE：占位符 DOM 文本无嵌套 HTML",
            await live_body.locator("code").first.text_content() == "<workspace>/.e-agent/agents/*.md"
            and await live_body.locator("code").first.locator("*").count() == 0
            and "<workspace>" in await live_body.text_content(),
            await live_body.inner_html())
    c.check("SSE：Notice 渲染", "系统提示行" in t, "")
    usage_text = await c.ev("els.usageInfo.textContent")
    c.check("SSE：Usage 无 window 回退为完整上下文（不显示累计输入/输出）",
            "上下文 1234 tok" in usage_text
            and "输入" not in usage_text and "输出" not in usage_text
            and await c.ev("els.usageInfo.querySelectorAll('.usage-pct').length") == 0
            and await c.ev("els.usageInfo.querySelectorAll('.usage-detail').length") == 1,
            usage_text)

    # 有 context_window：桌面同时显示短百分比与完整详情；窄屏只隐藏详情，
    # 不通过整串 ellipsis 冒险丢掉末尾百分比。
    await c.ev("applyUsage({context_input:132865, context_window:512000, session:{input_tokens:999, output_tokens:888}})")
    usage_text = await c.ev("els.usageInfo.textContent")
    c.check("Usage 桌面：百分比短标签 DOM 存在",
            await c.ev("els.usageInfo.querySelector('.usage-pct').textContent") == "26%", usage_text)
    c.check("Usage 桌面：完整详情可见且不含累计输入/输出",
            await c.ev("els.usageInfo.querySelector('.usage-detail').textContent.includes('上下文 132865/512000 tok')")
            and await c.ev("getComputedStyle(els.usageInfo.querySelector('.usage-detail')).display !== 'none'")
            and "输入" not in usage_text and "输出" not in usage_text, usage_text)
    c.check("Usage DOM 阅读顺序：百分比优先、详情随后",
            await c.ev("els.usageInfo.children[0].classList.contains('usage-pct')")
            and await c.ev("els.usageInfo.children[1].classList.contains('usage-detail')"), "")

    await c.page.set_viewport_size({"width": 390, "height": 844})
    c.check("Usage 手机：保留百分比并隐藏详情",
            await c.ev("getComputedStyle(els.usageInfo.querySelector('.usage-pct')).display !== 'none'")
            and await c.ev("getComputedStyle(els.usageInfo.querySelector('.usage-detail')).display === 'none'")
            and await c.ev("els.usageInfo.querySelector('.usage-pct').textContent") == "26%", "")
    await c.ev("applyUsage({context_input:410000, context_window:512000})")
    c.check("Usage >=80%：整体高用量红色语义保持",
            await c.ev("els.usageInfo.classList.contains('usage-high')")
            and await c.ev("els.usageInfo.querySelector('.usage-pct').textContent") == "80%", "")
    await c.ev("applyUsage({context_input:1234})")
    c.check("Usage 手机无 window：仍显示上下文 fallback",
            await c.ev("els.usageInfo.querySelectorAll('.usage-pct').length") == 0
            and await c.ev("getComputedStyle(els.usageInfo.querySelector('.usage-detail')).display !== 'none'")
            and "上下文 1234 tok" in await c.ev("els.usageInfo.textContent"), "")
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
    await c.open_sidebar()
    await c.page.locator("#sidebarTree .tree-row", has_text="sess-a").first.locator(".tree-id").click()
    await c.page.wait_for_selector("#messages .msg-user", timeout=8000)
    n = await c.page.locator("#messages .msg-user").count()
    c.check("打开 sess-a：历史渲染", n == 30, f"msgs={n}")

    # 写草稿 + 上滚
    await c.page.fill("#promptInput", "草稿甲")
    await c.ev("els.promptInput.dispatchEvent(new Event('input'))")
    saved_scroll = await c.ev("els.messages.scrollTop = 300; els.messages.scrollTop")
    c.check("会话可滚动（scrollTop 生效）", saved_scroll > 0, f"scrollTop={saved_scroll}")

    # 通过唯一导航切到 sess-b（首次打开 -> 拉历史）
    await c.page.locator("#sidebarTree .tree-row", has_text="sess-b").first.locator(".tree-id").click()
    await c.page.wait_for_function("() => state.sessionId === 'sess-b'")
    c.check("切到 sess-b：进入聊天", await c.ev("state.sessionId") == "sess-b", "")

    # 通过侧边栏切回 sess-a：草稿/滚动/缓存恢复，不重新拉历史
    await c.page.locator("#sidebarTree .tree-row", has_text="sess-a").first.locator(".tree-id").click()
    await c.page.wait_for_function("() => state.sessionId === 'sess-a'")
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
    c.check("切回 sess-a：缓存先恢复并补拉最新历史", len(hist_a) == 2, f"fetches={len(hist_a)}")

async def run_chat_error_render(c):
    c.sessions = [
        S("main-failed", "Failed", False, None, "失败会话"),
        S("main-finished", "Finished", False, None, "结束会话"),
        S("sub-finished", "Finished", False, "main-finished", "子任务已完"),
    ]
    await c.start()

    c.check("Failed 状态标签与样式映射",
            await c.ev("statusLabel('Failed')") == "失败"
            and await c.ev("statusChipClass('Failed')") == "error", "")

    # 从侧边栏打开失败会话 -> applyStatus('Failed(xx)') -> 失败
    await c.open_sidebar()
    await c.page.locator("#sidebarTree .tree-row", has_text="main-failed").first.locator(".tree-id").click()
    await c.page.wait_for_function("() => state.sessionId === 'main-failed'")
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


async def run_conflict_card(c):
    """并发写冲突现状（feat/conflict-friendly-web 只合入了「Failed→失败」一行；
    友好卡片未合入）。冲突回合在服务端表现为 Failed 状态 + 错误消息走 SSE Error，
    前端渲染为普通「错误: 」行 —— 用例按现状断言，不假设 .conflict 卡片存在。"""
    c.sessions = [
        S("main-conflict", "Failed", False, None, "冲突会话"),
        S("main-ok", "Idle", False, None, "正常会话"),
    ]
    c.history = {
        "entries": [
            {"type": "message", "message": {"User": {"content": "写日志", "images": []}}},
        ],
        "next_before_seq": None,
    }
    c.sse_body = ("event: status\ndata: {\"status\":\"Failed(concurrent write conflict)\"}\n\n"
                  "event: Error\ndata: {\"type\":\"error\",\"session_id\":\"s1\","
                  "\"seq\":1,\"error\":\"会话被其他客户端占用，已停止写入以避免数据冲突。\"}\n\n")

    await c.start()
    # statusLabel：Failed* -> 失败（现状唯一合入的友好化改动）
    c.check("statusLabel('Failed(concurrent write conflict)') === '失败'",
            await c.ev("statusLabel('Failed(concurrent write conflict)')") == "失败", "")

    # 从侧边栏打开会话：SSE status=Failed + Error 事件（冲突提示）渲染
    await c.open_sidebar()
    await c.page.locator("#sidebarTree .tree-row", has_text="main-conflict").first.locator(".tree-id").click()
    await c.page.wait_for_function("() => state.sessionId === 'main-conflict'")
    await c.page.wait_for_timeout(1000)
    c.check("冲突回合：聊天状态 chip 文本=失败",
            (await c.page.locator("#chatStatus").text_content()) == "失败",
            await c.page.locator("#chatStatus").text_content())
    t = await c.ev("els.messages.textContent")
    c.check("冲突错误消息渲染（错误: 前缀）",
            "错误: 会话被其他客户端占用，已停止写入以避免数据冲突。" in t, "")
    n_err = await c.ev("els.messages.querySelectorAll('.msg-error').length")
    n_conf = await c.ev("els.messages.querySelectorAll('.msg-error.conflict').length")
    c.check("现状：错误行是 .msg-error 普通行（无 .conflict 子类卡片）",
            n_err >= 1 and n_conf == 0, f"msg-error={n_err} conflict={n_conf}")

DIAGRAM_FIXTURE = """┌────────────┐              ┌────────────┐              ┌────────────┐
│    用户    │              │  Web 服务  │              │   数据库   │
└─────┬──────┘              └─────┬──────┘              └─────┬──────┘
      │─────────发送请求──────────>                           │
      <──────────返回结果─────────│                           │
      │                           │─────────查询数据──────────>
      │                           <──────────响应数据─────────│
ASCII W │ ─ ← → — 用"""


async def run_markdown_cjk_diagram(c):
    c.sessions = [S("diagram", "Idle", False, None, "图表排版")]
    markdown = ("普通 **Markdown** 保持渲染；行内代码 `代码 示例 普通 文本 仍然 可以 自动 换行` "
                "后面的正文也可正常换行。\n\n```text\n" + DIAGRAM_FIXTURE + "\n```")
    c.history = {"entries": [{"type": "message", "message": {
        "Assistant": {"content": markdown, "tool_calls": [], "reasoning": None}}}],
        "next_before_seq": None}

    font_path = (common.REPO_ROOT / "src/ui/fonts/"
                 "SarasaFixedSC-Regular-0eef5142e058644f.woff2")
    async def bundled_font(route, url, method):
        await route.fulfill(status=200, content_type="font/woff2",
                            body=font_path.read_bytes())
    c.extra_handlers.append((lambda url, method: "/fonts/SarasaFixedSC-Regular-" in url,
                             bundled_font))
    await c.start()
    await c.open_sidebar()
    await c.page.locator("#sidebarTree .tree-row", has_text="diagram").first.locator(".tree-id").click()
    await c.page.wait_for_function("() => state.sessionId === 'diagram'")
    await c.page.wait_for_selector(".msg-assistant pre code")
    await c.ev("document.fonts.ready")

    measure_js = """() => {
      const code = document.querySelector('.msg-assistant pre code');
      const pre = code.parentElement;
      const text = code.textContent.replace(/\\n$/, '');
      const lines = text.split('\\n');
      const starts = []; let off = 0;
      for (const line of lines) { starts.push(off); off += line.length + 1; }
      function rect(line, index) {
        const r = new Range();
        r.setStart(code.firstChild, starts[line] + index);
        r.setEnd(code.firstChild, starts[line] + index + 1);
        const b = r.getBoundingClientRect();
        return {x: b.left, width: b.width};
      }
      function nth(line, ch, n) {
        let from = -1;
        for (let i = 0; i <= n; i++) from = lines[line].indexOf(ch, from + 1);
        return rect(line, from);
      }
      const spread = v => Math.max(...v) - Math.min(...v);
      const rightBorderDelta = Math.max(
        spread([nth(0, '┐', 0).x, nth(1, '│', 1).x, nth(2, '┘', 0).x]),
        spread([nth(0, '┐', 1).x, nth(1, '│', 3).x, nth(2, '┘', 1).x]),
        spread([nth(0, '┐', 2).x, nth(1, '│', 5).x, nth(2, '┘', 2).x]));
      const endpointDelta = Math.max(
        spread([nth(2, '┬', 0).x, nth(3, '│', 0).x, nth(4, '<', 0).x,
                nth(5, '│', 0).x, nth(6, '│', 0).x]),
        spread([nth(2, '┬', 1).x, nth(3, '>', 0).x, nth(4, '│', 0).x,
                nth(5, '│', 1).x, nth(6, '<', 0).x]),
        spread([nth(2, '┬', 2).x, nth(3, '│', 1).x, nth(4, '│', 1).x,
                nth(5, '>', 0).x, nth(6, '│', 1).x]));
      const cs = getComputedStyle(code), ps = getComputedStyle(pre);
      return {
        ascii: rect(1, lines[1].indexOf('W')).width,
        box: rect(0, lines[0].indexOf('─')).width,
        cjk: rect(1, lines[1].indexOf('用')).width,
        arrow: rect(7, lines[7].indexOf('→')).width,
        rightBorderDelta, endpointDelta, fontFamily: cs.fontFamily,
        whiteSpace: ps.whiteSpace, tabSize: ps.tabSize, overflowX: ps.overflowX,
        preClientWidth: pre.clientWidth, preScrollWidth: pre.scrollWidth,
        pageClientWidth: document.documentElement.clientWidth,
        pageScrollWidth: document.documentElement.scrollWidth
      };
    }"""
    code = c.page.locator(".msg-assistant pre code")
    await code.evaluate("el => el.style.fontFamily = 'inherit'")
    before = await c.page.evaluate(measure_js)
    await code.evaluate("el => el.style.fontFamily = ''")
    after = await c.page.evaluate(measure_js)
    print("  [MEASURE] before=" + json.dumps(before, ensure_ascii=False, sort_keys=True))
    print("  [MEASURE] after-desktop=" + json.dumps(after, ensure_ascii=False, sort_keys=True))

    c.check("Sarasa Fixed SC 与 ASCII/框线/CJK 1:1:2 advance",
            after["fontFamily"].lstrip().startswith('"Sarasa Fixed SC"')
            and abs(after["box"] - after["ascii"]) <= 0.25
            and abs(after["arrow"] - after["ascii"]) <= 0.25
            and abs(after["cjk"] - 2 * after["ascii"]) <= 0.5,
            json.dumps(after, ensure_ascii=False))
    c.check("桌面边框与端点偏差 <=1px",
            after["rightBorderDelta"] <= 1 and after["endpointDelta"] <= 1,
            "right=%.3f endpoint=%.3f" % (after["rightBorderDelta"], after["endpointDelta"]))
    c.check("pre 空白/tab/内部滚动且桌面无页面横溢",
            after["whiteSpace"] == "pre" and after["tabSize"] == "4"
            and after["overflowX"] == "auto"
            and after["pageScrollWidth"] <= after["pageClientWidth"] + 1,
            json.dumps(after))

    inline = await c.page.locator(".msg-assistant code:not(pre code)").evaluate(
        "el => { const s=getComputedStyle(el); return {display:s.display, whiteSpace:s.whiteSpace, font:s.fontFamily}; }")
    c.check("普通 Markdown 与 inline code 未受影响",
            await c.ev("!!document.querySelector('.msg-assistant strong')")
            and inline["display"] == "inline" and inline["whiteSpace"] == "normal"
            and "Sarasa Fixed SC" not in inline["font"],
            json.dumps(inline, ensure_ascii=False))

    await c.page.set_viewport_size({"width": 390, "height": 844})
    mobile = await c.page.evaluate(measure_js)
    print("  [MEASURE] after-mobile=" + json.dumps(mobile, ensure_ascii=False, sort_keys=True))
    c.check("手机对齐、内部滚动且无页面横溢",
            mobile["rightBorderDelta"] <= 1 and mobile["endpointDelta"] <= 1
            and abs(mobile["box"] - mobile["ascii"]) <= 0.25
            and abs(mobile["arrow"] - mobile["ascii"]) <= 0.25
            and abs(mobile["cjk"] - 2 * mobile["ascii"]) <= 0.5
            and mobile["preScrollWidth"] > mobile["preClientWidth"]
            and mobile["pageScrollWidth"] <= mobile["pageClientWidth"] + 1,
            json.dumps(mobile))


CASES = [
    {"name": "markdown_cjk_diagram", "desc": "Markdown 中文框线图本地复合字体与响应式滚动",
     "run": run_markdown_cjk_diagram},
    {"name": "chat_open_sse", "desc": "openSession + SSE 基本流（mock 事件渲染）+ Busy 状态",
     "run": run_chat_open_sse},
    {"name": "chat_state_preserved", "desc": "切会话状态保留（草稿/滚动/缓存，不重拉历史）",
     "run": run_chat_state_preserved},
    {"name": "chat_error_render", "desc": "错误渲染（Failed->失败、错误行、Finished 禁输入）",
     "run": run_chat_error_render},
    {"name": "conflict_card", "desc": "并发写冲突现状：Failed->失败 chip、冲突错误消息行（无 .conflict 卡片）",
     "run": run_conflict_card},
]
