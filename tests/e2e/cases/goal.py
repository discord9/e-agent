#!/usr/bin/env python3
"""Session goal 用例（oracle 三轮终审修复后的 Web 契约）：

  * goal_menu        —— slash 菜单含 /goal；输入框 title 提示含 /goal
  * goal_crud        —— /goal set → pause → resume → clear 全程 POST /goal
                        （不 POST /prompt）；resume body {"action":"resume"}；
                        202 只宣称「请求已接受」；GoalBar 从 GET /goal 刷新
  * goal_sse         —— SSE GoalUpdated live 事件（wire 契约）刷新 GoalBar + notice
  * goal_switch      —— 会话切换/恢复：GET /goal 重新初始化 GoalBar；切换开始
                        立即清空旧 GoalBar
  * goal_resume      —— 历史会话（active=false）resume：POST /api/sessions
                        {id} → openSession → GET /goal 初始化 GoalBar
  * goal_stale_get   —— 旧会话延迟 GET /goal 响应绝不覆盖新会话 GoalBar
  * goal_resync      —— SSE resync 重放把 wire goal_updated 映射到 GoalUpdated：
                        set 刷新 GoalBar，clear（goal:null）隐藏 GoalBar
  * goal_resync_clear—— resync 只含 clear 墓碑：GoalBar 隐藏 + notice
  * goal_finished_409—— POST /goal 409：错误 banner、输入保留、无 accepted、无 prompt
  * goal_post_stale_202 —— A 发 set 后切 B：A 迟到 202 对 B 完全 no-op
                        （B 专属 banner/draft/GoalBar/会话身份快照逐字段不变、
                        无额外 GET B、无 prompt）
  * goal_post_stale_409 —— A 迟到 409 对 B 同样完全 no-op（快照逐字段不变）
"""
import json

import common


def S(id_, parent=None, title=None, active=True):
    return {"id": id_, "model": "flash", "role": "fixer" if parent else "main",
            "status": "Idle", "busy": False, "active": active,
            "parent_session_id": parent, "title": title,
            "entry_count": 2, "created_at": "2026-01-01T00:00:00Z"}


def goal_bar_text(c):
    return c.ev("(els.goalBar.hidden ? '' : els.goalBar.textContent) || ''")


def banner_text(c):
    return c.ev("els.banner.textContent")


async def capture_b_snapshot(c):
    """B 的完整渲染快照（迟到响应到达前后必须逐字段完全相同）：

    banner（text/class/hidden）、prompt draft、GoalBar（text/class/hidden）、
    会话渲染身份（sessionId/workspace/epoch/status）。任何一项变化都算
    no-op 被破坏 —— 包括 banner 被清空或被换成任意「良性的」文案。
    """
    return await c.ev("""() => ({
        sessionId: state.sessionId,
        workspace: state.workspace.id,
        epoch: sessionOpenEpoch,
        status: state.status,
        prompt: els.promptInput.value,
        bannerText: els.bannerText.textContent,
        bannerClass: els.banner.className,
        bannerHidden: els.banner.hidden,
        goalBarText: els.goalBar.textContent,
        goalBarClass: els.goalBar.className,
        goalBarHidden: els.goalBar.hidden,
    })""")


async def open_main(c, sid="main-idle"):
    await c.open_sidebar()
    await c.page.locator("#sidebarTree .tree-row", has_text=sid).first.locator(".tree-id").click()
    await c.page.wait_for_function("() => state.sessionId === '" + sid + "'")
    await c.close_sidebar()


async def switch_to(c, sid):
    await c.open_sidebar()
    await c.page.locator("#sidebarTree .tree-row", has_text=sid).first.locator(".tree-id").click()
    await c.page.wait_for_function("() => state.sessionId === '" + sid + "'")
    await c.close_sidebar()


def snap(objective, status="active", revision=1, sid="goal-1", criteria=None):
    return {"id": sid, "revision": revision, "objective": objective,
            "success_criteria": criteria or [], "status": status,
            "progress": "", "evidence": [], "blocked_reason": None}


async def run_goal_menu(c):
    c.sessions = [S("main-idle", None, "主会话")]
    await c.start()
    await open_main(c)
    # slash 菜单含 /goal：输入 "/go" 弹出候选（需先打开会话，输入框才可见）
    await c.page.fill("#promptInput", "/go")
    await c.page.wait_for_timeout(300)
    names = await c.ev("""() => Array.from(els.slashMenu.querySelectorAll('.slash-name'))
                          .map((n) => n.textContent)""")
    c.check("slash 菜单含 /goal", "/goal" in names, str(names))
    title_attr = await c.page.get_attribute("#promptInput", "title")
    c.check("输入框 title 提示含 /goal", "/goal" in (title_attr or ""), title_attr or "None")
    await c.page.fill("#promptInput", "")


async def run_goal_crud(c):
    c.sessions = [S("main-idle", None, "主会话")]
    await c.start()
    await open_main(c)

    # ---------- /goal 裸命令 -> GET /goal + 提示（不 POST） ----------
    await c.page.fill("#promptInput", "/goal")
    await c.page.click("#sendBtn")
    await c.page.wait_for_timeout(500)
    c.check("/goal 裸命令：GET 显示「无 goal」+ 不发 POST",
            "当前无 goal" in await banner_text(c) and len(c.records["goal"]) == 0
            and len(c.records["prompt"]) == 0, await banner_text(c))

    # ---------- /goal set 新目标 -> POST /goal（不 POST /prompt） ----------
    await c.page.fill("#promptInput", "/goal set 修复 oracle blockers")
    await c.page.press("#promptInput", "Enter")
    await c.page.wait_for_timeout(600)
    ok = (len(c.records["goal"]) == 1
          and c.records["goal"][0][0].endswith("/main-idle/goal")
          and json.loads(c.records["goal"][0][1]) ==
              {"action": "set", "objective": "修复 oracle blockers"}
          and len(c.records["prompt"]) == 0)
    c.check("/goal set：POST /goal body 正确（不 POST /prompt）", ok,
            str(c.records["goal"]) + " prompt=" + str(c.records["prompt"]))
    c.check("/goal set：202 文案只宣称「请求已接受」",
            "请求已接受" in await banner_text(c) and "已创建" not in await banner_text(c),
            await banner_text(c))
    c.check("/goal set：GoalBar 显示新 goal",
            "修复 oracle blockers" in await goal_bar_text(c)
            and "[active]" in await goal_bar_text(c), await goal_bar_text(c))
    c.check("/goal set：输入框清空", await c.ev("els.promptInput.value") == "", "")

    # ---------- /goal pause -> POST /goal action=pause ----------
    await c.page.fill("#promptInput", "/goal pause")
    await c.page.press("#promptInput", "Enter")
    await c.page.wait_for_timeout(500)
    ok = (len(c.records["goal"]) == 2
          and json.loads(c.records["goal"][1][1]) == {"action": "pause"})
    c.check("/goal pause：POST /goal action=pause", ok, str(c.records["goal"]))
    c.check("/goal pause：GoalBar 显示 paused",
            "[paused]" in await goal_bar_text(c), await goal_bar_text(c))

    # ---------- /goal resume -> POST /goal action=resume，bar 回 active ----------
    await c.page.fill("#promptInput", "/goal resume")
    await c.page.press("#promptInput", "Enter")
    await c.page.wait_for_timeout(500)
    ok = (len(c.records["goal"]) == 3
          and json.loads(c.records["goal"][2][1]) == {"action": "resume"}
          and len(c.records["prompt"]) == 0)
    c.check("/goal resume：POST /goal action=resume（不 POST /prompt）", ok,
            str(c.records["goal"]) + " prompt=" + str(c.records["prompt"]))
    c.check("/goal resume：GoalBar 回 active",
            "[active]" in await goal_bar_text(c), await goal_bar_text(c))

    # ---------- /goal clear -> POST /goal action=clear，GoalBar 隐藏 ----------
    await c.page.fill("#promptInput", "/goal clear")
    await c.page.press("#promptInput", "Enter")
    await c.page.wait_for_timeout(500)
    ok = (len(c.records["goal"]) == 4
          and json.loads(c.records["goal"][3][1]) == {"action": "clear"})
    c.check("/goal clear：POST /goal action=clear", ok, str(c.records["goal"]))
    c.check("/goal clear：GoalBar 隐藏", await c.ev("els.goalBar.hidden"), "")

    # ---------- 裸 /goal set 显示用法 ----------
    await c.page.fill("#promptInput", "/goal set")
    await c.page.press("#promptInput", "Enter")
    await c.page.wait_for_timeout(400)
    c.check("/goal set 裸命令：用法 banner + 不发 POST",
            "用法：/goal set <目标>" in await banner_text(c) and len(c.records["goal"]) == 4,
            await banner_text(c))


async def run_goal_sse(c):
    c.sessions = [S("main-idle", None, "主会话")]
    # live wire 契约：event: GoalUpdated + 扁平 payload {"goal": snapshot}
    c.sse_body = "\n\n".join([
        "event: snapshot\ndata: []",
        "event: GoalUpdated\ndata: {\"goal\":{\"id\":\"goal-sse\", \"revision\":1,"
        "\"objective\":\"SSE 推送的 goal\",\"success_criteria\":[],\"status\":\"active\","
        "\"progress\":\"\",\"evidence\":[],\"blocked_reason\":null}}",
    ]) + "\n\n"
    await c.start()
    await open_main(c)
    await c.page.wait_for_timeout(800)
    c.check("SSE GoalUpdated live：GoalBar 更新", "SSE 推送的 goal" in await goal_bar_text(c),
            await goal_bar_text(c))
    c.check("SSE GoalUpdated live：notice 行",
            "SSE 推送的 goal" in await c.ev("els.messages.textContent"), "")


async def run_goal_switch(c):
    c.sessions = [S("main-idle", None, "主会话"), S("other-sess", None, "另一会话")]
    await c.start()
    await open_main(c)
    # 先无 goal：GoalBar 隐藏
    c.check("切换前：无 goal 时 GoalBar 隐藏", await c.ev("els.goalBar.hidden"), "")
    # 给 other-sess 预置一个 goal（模拟服务器已有快照）
    c.goals["other-sess"] = snap("另一个会话的目标", status="blocked", revision=3,
                                 sid="goal-other", criteria=["跑通"])
    await switch_to(c, "other-sess")
    await c.page.wait_for_timeout(500)
    c.check("会话切换：GET /goal 初始化 GoalBar（含完整字段）",
            "另一个会话的目标" in await goal_bar_text(c)
            and "[blocked]" in await goal_bar_text(c)
            and "rev 3" in await goal_bar_text(c),
            await goal_bar_text(c))
    # 切回 main-idle（无 goal）：GoalBar 隐藏（GET /goal 返回 null）
    await switch_to(c, "main-idle")
    await c.page.wait_for_timeout(500)
    c.check("会话切回：无 goal 的会话 GoalBar 隐藏",
            await c.ev("els.goalBar.hidden"), await goal_bar_text(c))


async def run_goal_resume(c):
    # 历史会话（active=false）：点击 → POST /api/sessions {id} 恢复 →
    # openSession → GET /goal 初始化 GoalBar（恢复路径的只读初始化）。
    c.sessions = [S("main-idle", None, "主会话"), S("old-sess", None, "历史会话", active=False)]
    c.goals["old-sess"] = snap("历史会话的目标", status="paused", revision=2, sid="goal-old")
    await c.start()
    await open_main(c)
    c.check("恢复前：main-idle 无 goal 时 GoalBar 隐藏", await c.ev("els.goalBar.hidden"), "")
    await switch_to(c, "old-sess")
    await c.page.wait_for_timeout(600)
    c.check("resume：POST /api/sessions 恢复历史会话",
            any("old-sess" in (p or "") for p in c.records["create"]),
            str(c.records["create"]))
    c.check("resume：GoalBar 从 GET /goal 初始化",
            "历史会话的目标" in await goal_bar_text(c)
            and "[paused]" in await goal_bar_text(c), await goal_bar_text(c))


async def run_goal_stale_get(c):
    # 同一会话内 GET 与 GoalUpdated 的先后顺序：GET 结果必须服从较新的
    # live projection generation，而不是覆盖它。
    c.sessions = [S("main-idle", None, "主会话")]
    c.goals["main-idle"] = snap("GET 的旧目标")
    c.sse_body = 'event: snapshot\ndata: []\n\nevent: status\ndata: {"status":"Idle"}\n\n'
    await c.start()
    await open_main(c)
    await c.page.wait_for_timeout(300)
    # Delay the real GET, then attach a real SSE response carrying the newer
    # projection. The GET returns the old server value after the SSE frame.
    c.goal_delay["main-idle"] = (0, 0.5)
    await c.page.evaluate("fetchGoal('main-idle', state.workspace.id, sessionOpenEpoch)")
    await c.page.wait_for_timeout(50)
    c.sse_body = "\n\n".join([
        "event: snapshot\ndata: []",
        "event: GoalUpdated\ndata: " + json.dumps({"goal": snap("SSE newer")}),
    ]) + "\n\n"
    await c.page.evaluate("connectSSE(state.sessionId, state.workspace.id, sessionOpenEpoch)")
    await c.page.wait_for_timeout(700)
    c.check("同会话 stale GET：GoalUpdated set 不被旧 GET 覆盖",
            "SSE newer" in await goal_bar_text(c), await goal_bar_text(c))
    # Repeat with the opposite ordering: a delayed non-null GET must not
    # resurrect a goal after the live clear tombstone.
    c.goals["main-idle"] = snap("GET stale non-null")
    c.goal_delay["main-idle"] = (0, 0.5)
    await c.page.evaluate("fetchGoal('main-idle', state.workspace.id, sessionOpenEpoch)")
    await c.page.wait_for_timeout(50)
    c.sse_body = "\n\n".join([
        "event: snapshot\ndata: []",
        "event: GoalUpdated\ndata: {\"goal\":null}",
    ]) + "\n\n"
    await c.page.evaluate("connectSSE(state.sessionId, state.workspace.id, sessionOpenEpoch)")
    await c.page.wait_for_timeout(700)
    c.check("同会话 stale GET：GoalUpdated clear 不被旧 GET 覆盖",
            await c.ev("els.goalBar.hidden"), await goal_bar_text(c))

    # Reset the fixture's per-session delay counters before the independent
    # cross-session stale-GET scenario; the same browser above already used
    # main-idle for the same-session ordering checks.
    c.goal_delay = {"main-idle": (1, 1.0), "other-sess": (0, 0.6)}
    # 旧会话的 GET /goal 延迟响应：切到新会话后到达，必须被 stale guard
    # 丢弃，绝不覆盖新会话的 GoalBar。A 的第一次 GET（openSession 初始化）
    # 不延迟（先让 A 的 GoalBar 显示出来），第二次（/goal 裸命令再拉）延迟
    # 1s；B 的 GET 全部延迟 0.6s，保证「切换瞬间旧 GoalBar 已清空」可观测。
    c.sessions = [S("main-idle", None, "主会话"), S("other-sess", None, "另一会话")]
    c.goals["main-idle"] = snap("A 的旧目标")
    c.goals["other-sess"] = snap("B 的新目标", sid="goal-b")
    c.sse_body = 'event: snapshot\\ndata: []\\n\\nevent: status\\ndata: {"status":"Idle"}\\n\\n'
    c.goal_delay = {"main-idle": (1, 1.0), "other-sess": (0, 0.6)}  # A 第二次 GET 延迟
    c.goal_delay["other-sess"] = (0, 0.6)    # 所有 GET /goal 延迟 0.6s
    await c.start()
    await open_main(c)                       # 打开 A：首次 GET 无延迟 → bar 显示 A
    c.check("stale 前置：A 的 GoalBar 已显示",
            "A 的旧目标" in await goal_bar_text(c), await goal_bar_text(c))
    # 触发 A 的第二次 GET /goal（延迟 1s），随后立即切到 B
    await c.page.fill("#promptInput", "/goal")
    await c.page.click("#sendBtn")
    await c.page.wait_for_timeout(100)
    await switch_to(c, "other-sess")
    # 切换开始即清空旧 GoalBar；B 的 GET 还在途（0.6s）→ 此刻必然隐藏
    c.check("切换瞬间：旧 GoalBar 立即清空",
            await c.ev("els.goalBar.hidden"), await goal_bar_text(c))
    # B 的 GET（0.6s）先返回 → B 的 goal
    await c.page.wait_for_timeout(700)
    c.check("stale GET：B 先返回，GoalBar 显示 B", "B 的新目标" in await goal_bar_text(c),
            await goal_bar_text(c))
    # 越过 A 的 1s 延迟：A 的陈旧响应到达即被丢弃
    await c.page.wait_for_timeout(800)
    c.check("stale GET：A 的延迟响应被丢弃，不覆盖 B",
            "B 的新目标" in await goal_bar_text(c)
            and "A 的旧目标" not in await goal_bar_text(c),
            await goal_bar_text(c))


async def run_goal_resync(c):
    # resync 重放把 wire goal_updated 映射到 GoalUpdated：先 live set 显示
    # goal，再 resync clear（goal:null）→ GoalBar 隐藏 + 「goal cleared」notice
    #（重放也走 applyLiveEvent，set/clear 都更新 GoalBar）。
    c.sessions = [S("main-idle", None, "主会话")]
    c.sse_body = "\n\n".join([
        "event: snapshot\ndata: []",
        "event: GoalUpdated\ndata: {\"goal\":{\"id\":\"goal-r\", \"revision\":1,"
        "\"objective\":\"resync 前的 goal\",\"success_criteria\":[],\"status\":\"active\","
        "\"progress\":\"\",\"evidence\":[],\"blocked_reason\":null}}",
        "event: resync\ndata: [{\"type\":\"goal_updated\",\"data\":{\"goal\":null}},"
        "{\"type\":\"goal_updated\",\"data\":{\"goal\":{\"id\":\"goal-r2\",\"revision\":2,"
        "\"objective\":\"resync 后的 goal\",\"success_criteria\":[],\"status\":\"active\","
        "\"progress\":\"\",\"evidence\":[],\"blocked_reason\":null}}}]",
    ]) + "\n\n"
    await c.start()
    await open_main(c)
    await c.page.wait_for_timeout(800)
    # resync 重放的两个 goal_updated：先 clear 再 set → 最终 bar = set 的 goal
    c.check("SSE resync：wire goal_updated 映射到 GoalUpdated（set 刷新 GoalBar）",
            "resync 后的 goal" in await goal_bar_text(c)
            and "resync 前的 goal" not in await goal_bar_text(c),
            await goal_bar_text(c))
    c.check("SSE resync：重放 notice 行含 goal 状态",
            "resync 后的 goal" in await c.ev("els.messages.textContent")
            and "goal cleared" in await c.ev("els.messages.textContent"),
            "")


async def run_goal_resync_clear(c):
    # resync 只含 clear 墓碑：GoalBar 隐藏 + notice「goal cleared」。
    c.sessions = [S("main-idle", None, "主会话")]
    c.sse_body = "\n\n".join([
        "event: snapshot\ndata: []",
        "event: GoalUpdated\ndata: {\"goal\":{\"id\":\"goal-c\", \"revision\":1,"
        "\"objective\":\"将被清除的 goal\",\"success_criteria\":[],\"status\":\"active\","
        "\"progress\":\"\",\"evidence\":[],\"blocked_reason\":null}}",
        "event: resync\ndata: [{\"type\":\"goal_updated\",\"data\":{\"goal\":null}}]",
    ]) + "\n\n"
    await c.start()
    await open_main(c)
    await c.page.wait_for_timeout(800)
    c.check("SSE resync clear：GoalBar 隐藏", await c.ev("els.goalBar.hidden"),
            await goal_bar_text(c))
    c.check("SSE resync clear：notice「goal cleared」",
            "goal cleared" in await c.ev("els.messages.textContent"), "")


async def run_goal_finished_409(c):
    # finished/closed 服务端 → POST /goal 409：错误 banner、输入保留、
    # 无「请求已接受」、无 /prompt（mock 空 body → banner 显示 HTTP 409）。
    c.sessions = [S("main-idle", None, "主会话")]
    c.goal_post_status = 409
    await c.start()
    await open_main(c)
    await c.page.fill("#promptInput", "/goal set 被拒绝的目标")
    await c.page.press("#promptInput", "Enter")
    await c.page.wait_for_timeout(600)
    c.check("409：POST /goal 已发出（不 POST /prompt）",
            len(c.records["goal"]) == 1
            and c.records["goal"][0][0].endswith("/main-idle/goal")
            and len(c.records["prompt"]) == 0, str(c.records["goal"]))
    c.check("409：错误 banner（HTTP 409）",
            "⚠" in await banner_text(c) and "409" in await banner_text(c),
            await banner_text(c))
    c.check("409：输入保留（未清空）",
            await c.ev("els.promptInput.value") == "/goal set 被拒绝的目标",
            await c.ev("els.promptInput.value"))
    c.check("409：无「请求已接受」", "请求已接受" not in await banner_text(c),
            await banner_text(c))


async def run_goal_post_stale_202(c):
    # A 发 /goal set（POST 延迟 1.5s）后切到 B：给 B 装上专属非空 banner，
    # 抓取 B 的完整渲染快照；A 的迟到 202 必须让 B 逐字段完全不变
    # （banner 被清空/换成任何文案、draft 被清、GoalBar 被换、会话身份
    # 变化都直接 FAIL），且无额外 GET B、无 /prompt。
    c.sessions = [S("main-idle", None, "主会话"), S("other-sess", None, "另一会话")]
    c.goals["other-sess"] = snap("B 自己的目标", sid="goal-b")
    c.goal_post_delay = 1.5
    await c.start()
    await open_main(c)                      # A
    await c.page.fill("#promptInput", "/goal set 迟到的目标")
    await c.page.press("#promptInput", "Enter")
    await c.page.wait_for_timeout(100)      # POST 已发出（mock 延迟 1.5s）
    await switch_to(c, "other-sess")        # 迟到 202 到达前切到 B
    # 等 B 的 GoalBar 由 GET /goal 初始化完成，再装专属 banner、抓快照
    await c.page.wait_for_function(
        "() => !els.goalBar.hidden && els.goalBar.textContent.includes('B 自己的目标')")
    await c.page.fill("#promptInput", "B 的草稿")
    await c.ev('setBanner("B 专属 banner：等待 A 的迟到 202", true)')  # warn：永不自动消失
    await c.page.wait_for_timeout(100)
    before = await capture_b_snapshot(c)
    c.check("迟到 202：前置：B 的专属 banner 已安装且非空",
            before["bannerText"] == "B 专属 banner：等待 A 的迟到 202"
            and before["bannerHidden"] is False
            and before["bannerClass"] == "banner warn",
            json.dumps(before, ensure_ascii=False))
    await c.page.wait_for_timeout(1900)     # 越过 A 的迟到 202
    after = await capture_b_snapshot(c)
    c.check("迟到 202：B 渲染快照完全不变（banner 文本/class/可见性、draft、GoalBar、会话身份）",
            after == before, json.dumps(after, ensure_ascii=False))
    c.check("迟到 202：mutation 只发给 A（1 次 set）",
            len(c.records["goal"]) == 1
            and c.records["goal"][0][0].endswith("/main-idle/goal")
            and json.loads(c.records["goal"][0][1]) ==
                {"action": "set", "objective": "迟到的目标"},
            str(c.records["goal"]))
    c.check("迟到 202：无额外 GET B（仅切换那 1 次）",
            len([u for u in c.records["goal_get"] if u.endswith("/other-sess/goal")]) == 1,
            str(c.records["goal_get"]))
    c.check("迟到 202：无 /prompt", len(c.records["prompt"]) == 0,
            str(c.records["prompt"]))


async def run_goal_post_stale_409(c):
    # 同 202：A 发 set（POST 延迟 + 返回 409）后切到 B；B 的专属非空
    # banner、draft、GoalBar、会话身份在 A 的迟到 409 后逐字段完全不变，
    # 无任何错误 banner、无额外 GET B、无 /prompt。
    c.sessions = [S("main-idle", None, "主会话"), S("other-sess", None, "另一会话")]
    c.goals["other-sess"] = snap("B 的目标", sid="goal-b")
    c.goal_post_status = 409
    c.goal_post_delay = 1.5
    await c.start()
    await open_main(c)                      # A
    await c.page.fill("#promptInput", "/goal set 会被拒绝的目标")
    await c.page.press("#promptInput", "Enter")
    await c.page.wait_for_timeout(100)      # POST 已发出（mock 延迟 + 409）
    await switch_to(c, "other-sess")
    await c.page.wait_for_function(
        "() => !els.goalBar.hidden && els.goalBar.textContent.includes('B 的目标')")
    await c.page.fill("#promptInput", "B 的草稿")
    await c.ev('setBanner("B 专属 banner：等待 A 的迟到 409", true)')  # warn：永不自动消失
    await c.page.wait_for_timeout(100)
    before = await capture_b_snapshot(c)
    c.check("迟到 409：前置：B 的专属 banner 已安装且非空",
            before["bannerText"] == "B 专属 banner：等待 A 的迟到 409"
            and before["bannerHidden"] is False
            and before["bannerClass"] == "banner warn",
            json.dumps(before, ensure_ascii=False))
    await c.page.wait_for_timeout(1900)     # 越过 A 的迟到 409
    after = await capture_b_snapshot(c)
    c.check("迟到 409：B 渲染快照完全不变（banner 文本/class/可见性、draft、GoalBar、会话身份）",
            after == before, json.dumps(after, ensure_ascii=False))
    c.check("迟到 409：mutation 只发给 A（1 次 set）",
            len(c.records["goal"]) == 1
            and c.records["goal"][0][0].endswith("/main-idle/goal")
            and json.loads(c.records["goal"][0][1]) ==
                {"action": "set", "objective": "会被拒绝的目标"},
            str(c.records["goal"]))
    c.check("迟到 409：无额外 GET B（仅切换那 1 次）",
            len([u for u in c.records["goal_get"] if u.endswith("/other-sess/goal")]) == 1,
            str(c.records["goal_get"]))
    c.check("迟到 409：无 /prompt", len(c.records["prompt"]) == 0,
            str(c.records["prompt"]))


CASES = [
    {"name": "goal_menu", "desc": "slash 菜单含 /goal + 输入框 title 提示", "run": run_goal_menu},
    {"name": "goal_crud", "desc": "/goal set→pause→resume→clear 走 POST /goal（不 POST /prompt）+ GoalBar + 202 文案",
     "run": run_goal_crud},
    {"name": "goal_sse", "desc": "SSE GoalUpdated live 事件（wire 契约）刷新 GoalBar + notice", "run": run_goal_sse},
    {"name": "goal_switch", "desc": "会话切换从 GET /goal 初始化 GoalBar + 切换即清空", "run": run_goal_switch},
    {"name": "goal_resume", "desc": "历史会话 resume 后 GET /goal 初始化 GoalBar", "run": run_goal_resume},
    {"name": "goal_stale_get", "desc": "旧会话延迟 GET /goal 不覆盖新会话 GoalBar", "run": run_goal_stale_get},
    {"name": "goal_resync", "desc": "SSE resync 把 goal_updated 映射到 GoalUpdated（set/clear）", "run": run_goal_resync},
    {"name": "goal_resync_clear", "desc": "SSE resync clear 墓碑隐藏 GoalBar", "run": run_goal_resync_clear},
    {"name": "goal_finished_409", "desc": "POST /goal 409：错误 banner、输入保留、无 accepted、无 prompt",
     "run": run_goal_finished_409},
    {"name": "goal_post_stale_202", "desc": "A 迟到 202 对 B 完全 no-op（draft/banner/bar 保留、无额外 GET B）",
     "run": run_goal_post_stale_202},
    {"name": "goal_post_stale_409", "desc": "A 迟到 409 对 B 完全 no-op", "run": run_goal_post_stale_409},
]
