#!/usr/bin/env python3
"""斜杠命令用例（基于 05fc4c8 已合入功能）：/compact、/rename、/btw 拦截。

全部走 route 拦截 mock 端点，断言请求 url/body：
  * /compact  -> POST /api/sessions/{id}/compact，不 POST /prompt，输入清空
  * /rename   -> 用法提示（裸命令）；PUT /api/sessions/{id}/title（新标题 / 空=清除）
  * /btw      -> 用法提示（裸命令）；POST /api/sessions/{id}/btw；404/500 降级
  * 未知 /foo -> 照常 POST /prompt
"""
import json

import common

def S(id_, parent=None, title=None):
    return {"id": id_, "model": "flash", "role": "fixer" if parent else "main",
            "status": "Idle", "busy": False, "active": True,
            "parent_session_id": parent, "title": title,
            "entry_count": 2, "created_at": "2026-01-01T00:00:00Z"}

async def run_commands(c):
    def sessions():
        lst = [S("main-idle", None, "主会话")]
        if c.records["btw"]:                       # /btw 成功后：树里出现新 subagent
            lst.append(S("sub-btw-1", "main-idle", "btw 子代理"))
        return lst
    c.sessions = sessions

    await c.start()
    # 输入框 title 提示含三个命令
    title_attr = await c.page.get_attribute("#promptInput", "title")
    c.check("输入框 title 提示含 /compact /rename /btw",
            "/compact" in (title_attr or "") and "/rename" in (title_attr or "")
            and "/btw" in (title_attr or ""), title_attr or "None")

    await c.open_sidebar()
    await c.page.locator("#sidebarTree .tree-row", has_text="main-idle").first.locator(".tree-id").click()
    await c.page.wait_for_function("() => state.sessionId === 'main-idle'")
    await c.close_sidebar()

    def banner():
        return c.ev("els.banner.textContent")

    # ---------- /compact ----------
    await c.page.fill("#promptInput", "/compact")
    await c.page.click("#sendBtn")
    await c.page.wait_for_timeout(900)
    c.check("/compact：POST /compact 发出（不 POST /prompt）",
            len(c.records["compact"]) == 1
            and c.records["compact"][0][0].endswith("/main-idle/compact")
            and len(c.records["prompt"]) == 0,
            str(c.records["compact"]))
    c.check("/compact：输入框清空", await c.ev("els.promptInput.value") == "", "")
    c.check("/compact：提示「压缩请求已提交」",
            "压缩请求已提交" in await c.ev("els.messages.textContent"), "")

    # ---------- /rename 裸命令 -> 用法 ----------
    await c.page.fill("#promptInput", "/rename")
    await c.page.click("#sendBtn")
    await c.page.wait_for_timeout(400)
    c.check("/rename 裸命令：用法 banner + 不发 PUT",
            "用法：/rename <标题>" in await banner() and len(c.records["title"]) == 0,
            await banner())
    c.check("/rename 裸命令：输入保留", await c.ev("els.promptInput.value") == "/rename", "")

    # ---------- /rename 新标题 ----------
    await c.page.fill("#promptInput", "/rename 新标题甲")
    await c.page.press("#promptInput", "Enter")
    await c.page.wait_for_timeout(600)
    ok = (len(c.records["title"]) == 1
          and c.records["title"][0][0].endswith("/main-idle/title")
          and c.records["title"][0][1] == '{"title":"新标题甲"}')
    c.check("/rename 新标题：PUT body 正确", ok, str(c.records["title"]))
    c.check("/rename 新标题：输入框清空", await c.ev("els.promptInput.value") == "", "")

    # ---------- /rename 空标题（清除） ----------
    await c.page.fill("#promptInput", "/rename ")
    await c.page.press("#promptInput", "Enter")
    await c.page.wait_for_timeout(600)
    ok = (len(c.records["title"]) == 2
          and c.records["title"][1][0].endswith("/main-idle/title")
          and c.records["title"][1][1] == '{"title":""}')
    c.check("/rename 空标题：PUT {title:''}（清除）", ok, str(c.records["title"]))

    # ---------- /btw 裸命令 -> 用法 ----------
    await c.page.fill("#promptInput", "/btw")
    await c.page.click("#sendBtn")
    await c.page.wait_for_timeout(400)
    c.check("/btw 裸命令：用法 banner + 不发 POST",
            "用法：/btw <问题>" in await banner() and len(c.records["btw"]) == 0,
            await banner())

    # ---------- /btw 问题 -> 成功 ----------
    await c.page.fill("#promptInput", "/btw 帮我探讨一下这个方向")
    await c.page.press("#promptInput", "Enter")
    await c.page.wait_for_timeout(700)
    ok = (len(c.records["btw"]) == 1
          and c.records["btw"][0][0].endswith("/main-idle/btw")
          and c.records["btw"][0][1] == '{"prompt":"帮我探讨一下这个方向"}')
    c.check("/btw 问题：POST url + body prompt", ok, str(c.records["btw"]))
    c.check("/btw 成功：banner 含 subagent id", "已创建 btw subagent：sub-btw-1" in await banner(),
            await banner())
    c.check("/btw 成功：输入框清空", await c.ev("els.promptInput.value") == "", "")
    # 侧边栏树出现新 subagent
    await c.page.wait_for_timeout(500)
    await c.open_sidebar()
    await c.page.locator("#sidebarTree .tree-row", has_text="主会话").locator("button.tree-toggle").click()
    await c.page.wait_for_timeout(300)
    c.check("/btw 成功：侧边栏树出现新 subagent",
            await c.page.locator(".tree-row-child", has_text="btw 子代理").count() >= 1, "")
    await c.close_sidebar()
    await c.page.wait_for_timeout(300)

    # ---------- /btw 404 / 500 降级 ----------
    c.btw_status = 404
    await c.page.fill("#promptInput", "/btw 旧服务器问题")
    await c.page.press("#promptInput", "Enter")
    await c.page.wait_for_timeout(500)
    c.check("/btw 404：服务器不支持 /btw", "服务器不支持 /btw" in await banner(), await banner())
    c.btw_status = 500
    await c.page.fill("#promptInput", "/btw 服务器内部错误")
    await c.page.press("#promptInput", "Enter")
    await c.page.wait_for_timeout(500)
    c.check("/btw 500：创建失败 banner", "创建 btw subagent 失败" in await banner(), await banner())
    c.btw_status = 201

    # ---------- 未知 /foo -> 照常 POST /prompt ----------
    btw_before = len(c.records["btw"])
    await c.page.fill("#promptInput", "/foo bar")
    await c.page.press("#promptInput", "Enter")
    await c.page.wait_for_timeout(600)
    ok = (len(c.records["prompt"]) == 1
          and c.records["prompt"][0][0].endswith("/main-idle/prompt")
          and c.records["prompt"][0][1] == '{"text":"/foo bar"}')
    c.check("未知 /foo：照常 POST /prompt", ok, str(c.records["prompt"]))
    c.check("未知 /foo：未新增 /btw 请求", len(c.records["btw"]) == btw_before,
            f"btw_calls={len(c.records['btw'])}")

CASES = [
    {"name": "commands", "desc": "斜杠命令 /compact /rename /btw（mock 端点 + 降级）+ 未知命令走 /prompt",
     "run": run_commands},
]
