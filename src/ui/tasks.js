/* =============================================================================
 * tasks.js — 运行中任务：/api/tasks 统一轮询（fetchTasks/pollTasks）、
 * composer 任务面板（renderComposerTasks/renderTaskList）、消息列表 bash
 * 输出块（ensureTaskOutputBlock/reconcileTaskOutputBlocks）、delegate 内嵌
 * SSE 流式（startTaskStream/handleTaskStreamBlock）、任务取消（cancelTask）。
 * 依赖 app.js + render.js + sessions.js（resolveSubagentSessionId）。
 * =============================================================================*/

/* =====================================================================
 * 运行中任务（两处显示，共用同一轮询）
 *   1. 聊天视图 composer 上方：#tasksToggleBar 折叠条（计数即徽标，
 *      有任务时高亮）+ #composerTasks 面板（默认收起，state.tasks.composerOpen）
 *   2. 消息列表（els.messages）末尾：当前会话每个运行中 bash 任务的
 *      「任务输出块」.task-output-block（默认展开实时看输出，见
 *      reconcileTaskOutputBlocks / ensureTaskOutputBlock）
 * 统一轮询 pollTasks()（2s 常驻）：一次 GET /api/tasks 同时更新两处。
 * ===================================================================*/
/* GET /api/tasks → 任务数组；失败/无 token 返回 null（调用方决定如何处理） */
async function fetchTasks() {
  if (!state.token) return null;
  try {
    const res = await api("/api/tasks");
    if (res.status === 401 || res.status === 403) {
      setBanner("⚠ 认证失败：请检查 Token。");
      return null;
    }
    if (!res.ok) throw new Error("HTTP " + res.status);
    const list = await res.json();
    return Array.isArray(list) ? list : [];
  } catch (e) {
    return null;   // 网络/404：静默，保持现状，不刷屏
  }
}

/* 统一任务轮询（防重入：seq 竞态序号只应用最新一次响应）。
   一次 fetchTasks 同时更新 composer 折叠条/面板 + 消息列表输出块
   （当前会话的 bash 任务）。 */
async function pollTasks() {
  const seq = ++state.tasks.seq;
  const tasks = await fetchTasks();
  if (tasks === null || seq !== state.tasks.seq) return;  // 过期响应丢弃
  state.tasks.list = tasks;
  renderComposerTasks();
  reconcileTaskOutputBlocks(tasks);   // 消息列表输出块：创建/续轮询/结束收起
}

/* composer 上方折叠条 + 面板：计数即徽标；有任务时高亮，无任务整条隐藏。
   面板内容仅在展开时渲染（收起时只更新计数/箭头/高亮）。 */
function renderComposerTasks() {
  const bar = els.tasksToggleBar;
  if (!bar) return;
  const n = (state.tasks.list || []).length;
  bar.hidden = n === 0;
  bar.classList.toggle("active", n > 0);
  bar.classList.toggle("open", state.tasks.composerOpen);
  const label = bar.querySelector(".tasks-toggle-label");
  if (label) label.textContent = "运行中任务 (" + n + ")";
  const chevron = bar.querySelector(".tasks-toggle-chevron");
  if (chevron) chevron.textContent = state.tasks.composerOpen ? "▾" : "▸";
  const panel = els.composerTasks;
  if (panel) {
    panel.hidden = !state.tasks.composerOpen;
    if (state.tasks.composerOpen) {
      renderTaskList(state.tasks.list || [], panel);
    }
  }
}

/* delegate 行点击现在切到 subagent 会话（见 renderTaskList）；就地流式区
   .task-stream 保留为「解析不到 subagent 会话」时的回退路径。 */

function shortTaskLabel(t) {
  // delegate 显示 label；bash 显示截断的 full_command（label 兜底）
  if (t.kind === "delegate") return t.label || "子代理任务";
  return truncate(t.full_command || t.label || "", 80);
}

/* ---- 任务行就地流式：bash 轮询 output 端点 + delegate 内嵌 SSE ---- */

/* bash 输出轮询（通用）：给定任务 key + 目标 pre 元素 + 状态回调，500ms
   GET /api/sessions/{sid}/tasks/{tid}/output 全量刷新输出区（textContent）。
   旧后端（端点 404）→ 静默停轮询并记为降级（重绘不再重启），保持静态尾部；
   网络失败 / 元素脱离 DOM 同样停轮询。句柄存 state.tasks.pollers
   （key=session_id:id）。卡片行与消息列表输出块共用同一实现：同一 key
   同时只有一处活跃（bash 卡片行不再就地展开，活跃的是消息块），
   后启动的会先停掉旧句柄再接管。onPhase 回调可选：
   onPhase("start"|"stop"|"degraded")，卡片行用它驱动状态小字显隐。 */
function startOutputPoller(key, t, pre, onPhase) {
  stopTaskPoller(key);
  if (onPhase) onPhase("start");
  let intervalId = null;
  // 只停自己的轮询：2s 重绘会先停旧轮询再启新轮询，旧 tick 的异步收尾
  // 不能误清新轮询（竞态：key 相同）。
  const stop = (degraded) => {
    if (state.tasks.pollers.get(key) !== intervalId) return;
    stopTaskPoller(key);
    if (degraded) state.tasks.degraded.add(key);
    if (onPhase) onPhase(degraded ? "degraded" : "stop");
  };
  const tick = async () => {
    if (!state.token) { stop(false); return; }
    try {
      const res = await api("/api/sessions/" + encodeURIComponent(t.session_id || "")
        + "/tasks/" + encodeURIComponent(t.id) + "/output");
      if (res.status === 401 || res.status === 403) {
        setBanner("⚠ 认证失败：请检查 Token。");
        stop(false); return;
      }
      if (res.status === 404 || !res.ok) {
        // 旧后端/端点不可用：停轮询并记为降级（重绘不再重启），输出区保持静态尾部
        stop(true); return;
      }
      const text = await res.text();
      if (!pre.isConnected) { stop(false); return; }
      const txt = String(text).trim() !== "" ? String(text) : "";
      pre.classList.toggle("empty", txt === "");
      pre.textContent = txt || "(无输出)";
      pre.scrollTop = pre.scrollHeight;
    } catch (e) {
      stop(false);   // 网络失败：停轮询，保留已有内容
    }
  };
  tick();   // 启动即拉一次
  intervalId = setInterval(tick, 500);
  state.tasks.pollers.set(key, intervalId);
}

function stopTaskPoller(key) {
  const id = state.tasks.pollers.get(key);
  if (id != null) { clearInterval(id); state.tasks.pollers.delete(key); }
}

/* ---- 消息列表内的 bash 任务输出块（.task-output-block） ----
   每个运行中的 bash 任务在 els.messages 末尾有一块：details 默认展开，
   summary = badge「bash」+ 任务 label + 状态/结束标记，pre 轮询刷新输出。
   块按任务 key（session_id:id）对应，任务结束后收起标记、保留最后输出，
   不删除（位置保持，新消息自然排在后面）。 */

function taskKey(t) {
  return (t.session_id || "") + ":" + (t.id != null ? t.id : "");
}

function findTaskOutputBlock(key) {
  if (!els.messages) return null;
  for (const b of els.messages.querySelectorAll(".task-output-block")) {
    if (b.getAttribute("data-task") === key) return b;
  }
  return null;
}

/* 确保消息列表末尾存在该任务的输出块；不存在则创建（默认展开）。
   已存在且用户手动折叠的块保持原状（不强制重开）。服务重启等导致
   task id 复用（旧块已标「✓ 已结束」）时复活为运行中并展开。 */
function ensureTaskOutputBlock(t) {
  const key = taskKey(t);
  let block = findTaskOutputBlock(key);
  if (!block) {
    block = el("details", "task-output-block");
    block.setAttribute("data-task", key);
    const sum = el("summary", "task-output-head");
    sum.appendChild(el("span", "kind-badge bash", "bash"));
    sum.appendChild(el("span", "task-output-label", shortTaskLabel(t)));
    if (t.session_id) sum.appendChild(el("span", "task-output-sid", "会话 " + shortId(t.session_id)));
    sum.appendChild(el("span", "task-output-state", "● 运行中"));
    const initOut = (t.output != null && String(t.output).trim() !== "") ? String(t.output) : "";
    const body = el("pre", "task-output-body" + (initOut ? "" : " empty"), initOut || "(等待输出…)");
    block.append(sum, body);
    els.messages.appendChild(block);
    scrollBottom(false);
    block.setAttribute("open", "");   // 新建：默认展开（用户要实时看编译进度）
    pruneMessages();                  // 新块是「● 运行中」：进行中，不折叠
  } else {
    const st = block.querySelector(".task-output-state");
    if (st && st.classList.contains("done")) {   // 旧 key 复用：复活为运行中
      st.classList.remove("done");
      st.textContent = "● 运行中";
      state.tasks.degraded.delete(key);
      block.setAttribute("open", "");
    }
  }
  return block;
}

/* 任务结束（从轮询列表消失）：块收起 + summary 标「✓ 已结束」，
   保留最后看到的输出文本；停轮询并清降级标记。 */
function finishTaskOutputBlock(block, key) {
  stopTaskPoller(key);
  state.tasks.degraded.delete(key);
  block.removeAttribute("open");
  const st = block.querySelector(".task-output-state");
  if (st) { st.textContent = "✓ 已结束"; st.classList.add("done"); }
}

/* bash 任务行点击：滚动到消息列表对应输出块 + 短暂高亮提示 */
function flashTaskOutputBlock(block) {
  if (typeof block.scrollIntoView === "function") block.scrollIntoView({ block: "nearest" });
  block.classList.add("flash");
  if (block._flashTimer) clearTimeout(block._flashTimer);
  block._flashTimer = setTimeout(() => block.classList.remove("flash"), 2000);
}

/* 对齐消息列表输出块与当前运行任务（pollTasks 每 2s + 历史/快照渲染后调用）：
   - 当前会话运行中的 bash 任务 → 确保块存在并启动/续上轮询（块拥有该 key 的
     轮询：始终重启，覆盖可能残留的卡片轮询）；降级（输出端点 404）→ 用
     /api/tasks 的 2000 字符尾部静态刷新，不重启轮询。
   - 已不在列表的任务 → 块收起、标「✓ 已结束」、停轮询（保留最后输出）。
   只操作当前会话消息区：其它会话的块在各自缓存 DOM 里，切回时由
   openSession restored 分支再次调用本函数重启轮询。 */
function reconcileTaskOutputBlocks(tasks) {
  if (state.view !== "chat" || !state.sessionId) return;
  const running = new Set();
  for (const t of tasks || []) {
    if (t.kind === "delegate") continue;
    if (t.session_id !== state.sessionId) continue;   // 只渲染当前会话的任务
    const key = taskKey(t);
    running.add(key);
    const block = ensureTaskOutputBlock(t);
    if (!block) continue;
    const pre = block.querySelector(".task-output-body");
    if (!pre) continue;
    if (state.tasks.degraded.has(key)) {
      // 旧后端无 output 端点：用任务列表尾部输出静态刷新（2s 粒度）
      const out = (t.output != null && String(t.output).trim() !== "") ? String(t.output) : "";
      pre.classList.toggle("empty", out === "");
      pre.textContent = out || "(无输出)";
    } else {
      startOutputPoller(key, t, pre, null);   // 块在 DOM → 轮询更新它
    }
  }
  for (const block of els.messages.querySelectorAll(".task-output-block")) {
    const key = block.getAttribute("data-task");
    if (!key || running.has(key)) continue;
    finishTaskOutputBlock(block, key);
  }
}

/* 停掉消息列表所有输出块的轮询（返回列表视图时调用；块留在缓存 DOM，
   切回时由 reconcileTaskOutputBlocks 重启）。 */
function stopAllTaskBlockPollers() {
  if (!els.messages) return;
  for (const block of els.messages.querySelectorAll(".task-output-block")) {
    stopTaskPoller(block.getAttribute("data-task"));
  }
}

/* delegate（subagent）内嵌 SSE：复用 /api/sessions/{sid}/events，只取
   assistant 相关事件文本追加到 .task-stream 滚动区。snapshot/resync
   （重连重放最近事件）→ 整体替换为其中的 assistant 文本，避免重复；
   live AssistantText → 替换；AssistantDelta → 追加。
   404（历史 subagent 会话已结束/不存在）→ 提示「任务已结束」。
   收起/任务消失/重绘 → abort 流。累积文本存 state.tasks.streamText，
   2s 轮询重绘重启流时恢复，不闪断。 */
function startTaskStream(t, key, streamEl, status) {
  stopTaskStream(key);
  const ctrl = new AbortController();
  state.tasks.streams.set(key, ctrl);
  if (status) status.hidden = false;
  streamEl.classList.remove("empty");
  streamEl.textContent = state.tasks.streamText.get(key) || "(等待流式输出…)";
  streamEl.scrollTop = streamEl.scrollHeight;
  (async () => {
    try {
      const res = await api("/api/sessions/" + encodeURIComponent(t.session_id || "") + "/events", {
        headers: { "Accept": "text/event-stream" },
        signal: ctrl.signal,
      });
      if (res.status === 401 || res.status === 403) {
        setBanner("⚠ 认证失败：请检查 Token。");
        if (status) status.hidden = true;
        return;
      }
      if (res.status === 404) {              // 历史 subagent：会话已结束
        setTaskStreamText(streamEl, key, "任务已结束");
        if (status) status.hidden = true;
        return;
      }
      if (!res.ok || !res.body) {
        setTaskStreamText(streamEl, key, "流式输出不可用（HTTP " + res.status + "）");
        if (status) status.hidden = true;
        return;
      }
      const reader = res.body.getReader();
      const decoder = new TextDecoder();
      let buf = "";
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        if (ctrl.signal.aborted) break;
        buf += decoder.decode(value, { stream: true }).replace(/\r\n/g, "\n");
        let idx;
        while ((idx = buf.indexOf("\n\n")) !== -1) {
          const block = buf.slice(0, idx);
          buf = buf.slice(idx + 2);
          handleTaskStreamBlock(block, streamEl, key);
        }
      }
      // 正常结束（后端关闭流）：停止消费，保留已显示内容
      if (status) status.hidden = true;
    } catch (e) {
      if (e && e.name === "AbortError") return;   // 主动收起/任务消失/重绘
      // 网络失败：保留已显示内容
    } finally {
      // 只清自己的条目：2s 重绘会先 abort 旧流再启新流（key 相同），
      // 旧流的 finally 不能误删新流的 AbortController
      if (state.tasks.streams.get(key) === ctrl) state.tasks.streams.delete(key);
    }
  })();
}

function stopTaskStream(key) {
  const ctrl = state.tasks.streams.get(key);
  if (ctrl) { ctrl.abort(); state.tasks.streams.delete(key); }
}

/* 整体替换流式区文本（snapshot 重放 / AssistantText / 404 提示） */
function setTaskStreamText(streamEl, key, text) {
  state.tasks.streamText.set(key, text);
  if (streamEl.isConnected) {
    streamEl.classList.toggle("empty", String(text).trim() === "");
    streamEl.textContent = text;
    streamEl.scrollTop = streamEl.scrollHeight;
  }
}

/* 追加流式文本（live AssistantDelta） */
function appendTaskStreamText(streamEl, key, text) {
  if (!text) return;
  const next = (state.tasks.streamText.get(key) || "") + text;
  state.tasks.streamText.set(key, next);
  if (streamEl.isConnected) {
    streamEl.classList.remove("empty");
    streamEl.textContent = next;
    streamEl.scrollTop = streamEl.scrollHeight;
  }
}

/* 解析单个 SSE 事件块（轻量：只处理 assistant 相关事件）。
   live 帧 event 名为 CamelCase（AssistantText/AssistantDelta），data 是
   {text|delta,...}；snapshot/resync 的 data 是 AgentEvent 数组
   （{type:"assistant_text"|"assistant_delta", data:"..."}）。 */
function handleTaskStreamBlock(block, streamEl, key) {
  let eventName = "message";
  const dataLines = [];
  for (const line of block.split("\n")) {
    if (line.startsWith(":")) continue;                 // 心跳/注释行
    if (line.startsWith("event:")) eventName = line.slice(6).trim();
    else if (line.startsWith("data:")) dataLines.push(line.slice(5).replace(/^ /, ""));
  }
  if (!dataLines.length) return;
  const data = dataLines.join("\n");

  if (eventName === "snapshot" || eventName === "resync") {
    // 重连重放：整体替换（assistant_text 置替、assistant_delta 追加），防重复
    try {
      const parsed = JSON.parse(data);
      const events = Array.isArray(parsed) ? parsed : (parsed.events || []);
      let acc = "";
      for (const ev of events) {
        if (!ev || typeof ev !== "object") continue;
        const txt = ev.data !== undefined ? ev.data : "";
        if (ev.type === "assistant_text") acc = String(txt);
        else if (ev.type === "assistant_delta") acc += String(txt);
      }
      if (acc) setTaskStreamText(streamEl, key, acc);
    } catch (e) { /* 坏数据：忽略 */ }
    return;
  }
  if (eventName === "AssistantText") {
    let txt = data;
    try { txt = pickText(JSON.parse(data), ["text", "content"]); } catch (e) { /* 原样 */ }
    if (txt) setTaskStreamText(streamEl, key, String(txt));
    return;
  }
  if (eventName === "AssistantDelta") {
    let txt = data;
    try { txt = pickText(JSON.parse(data), ["delta", "text", "content"]); } catch (e) { /* 原样 */ }
    appendTaskStreamText(streamEl, key, String(txt));
    return;
  }
}

/* 渲染任务列表到指定容器（composer 面板等；当前无侧边栏分组调用）。
   bash：行点击 → 切到该任务所属会话，视图就绪后滚动到消息列表里的
   .task-output-block 并高亮（对应输出在消息区可滚动查看）；已在该会话
   时直接 flash；找不到对应块（任务已结束 / 未渲染）时回退为就地展开
   .task-output（静态尾部）+ 500ms 轮询 output 端点（旧后端 404 → 停轮询，
   降级为静态尾部）。delegate：行点击 → openSession(该 subagent 的会话)，
   那边有完整消息/工具卡片/思考块渲染；解析不到 subagent 会话时回退为就地
   展开内嵌 SSE 流式区 .task-stream。.task-output / .task-stream 的渲染代码
   保留（prevExpanded 恢复逻辑依赖），只是点击分支改变。
   重绘前记录已展开的行并按任务 key（session_id:id）恢复——2s 轮询重绘
   不打断正在查看的输出/流；消失的任务在此停掉轮询/流并清理文本缓冲。
   只停「本容器已展开行」的轮询/流：消息列表输出块的轮询（同一 key）不
   受影响——bash 行不再就地展开，块轮询独立存活，由 reconcile 接管。 */
function renderTaskList(tasks, container) {
  const list = container;
  if (!list) return;
  // 记录「已展开」的行（输出区或流式区），重建后按展开状态重启；
  // 只停这些行的轮询/流（见函数头注释）。
  const prevExpanded = new Set();
  for (const row of list.querySelectorAll(".task-row")) {
    const key = row.getAttribute("data-task");
    if (!key) continue;
    const pre = row.querySelector(".task-output");
    const streamEl = row.querySelector(".task-stream");
    const expanded = (pre && !pre.hidden) || (streamEl && !streamEl.hidden);
    if (expanded) {
      prevExpanded.add(key);
      stopTaskPoller(key);
      stopTaskStream(key);
    }
  }
  // 消失的任务：清理已累积的流式文本缓冲与降级标记（轮询/流已在上方停掉）。
  // 放在空列表早退之前：任务全部结束时也要清理，防泄漏。
  const activeKeys = new Set();
  for (const t of tasks) {
    activeKeys.add((t.session_id || "") + ":" + (t.id != null ? t.id : ""));
  }
  for (const k of Array.from(state.tasks.streamText.keys())) {
    if (!activeKeys.has(k)) state.tasks.streamText.delete(k);
  }
  for (const k of Array.from(state.tasks.degraded.keys())) {
    if (!activeKeys.has(k)) state.tasks.degraded.delete(k);
  }
  list.innerHTML = "";
  if (!tasks.length) {
    list.appendChild(el("div", "tasks-empty", "暂无运行中的任务"));
    return;
  }
  for (const t of tasks) {
    const row = el("div", "task-row");
    const key = (t.session_id || "") + ":" + (t.id != null ? t.id : "");
    row.setAttribute("data-task", key);
    const isDelegate = t.kind === "delegate";
    row.title = isDelegate ? "点击切换到该子代理的会话" : "点击切换到该任务的会话并跳转输出块";
    const line = el("div", "task-line");
    const badge = el("span", "kind-badge " + (isDelegate ? "delegate" : "bash"),
      isDelegate ? "子代理" : "bash");
    line.appendChild(badge);
    line.appendChild(el("span", "task-label", shortTaskLabel(t)));
    if (t.role) line.appendChild(el("span", "task-meta trole", t.role));
    if (t.session_id) line.appendChild(el("span", "task-meta tsid", "会话 " + shortId(t.session_id)));
    const status = el("span", "task-stream-status", "● 流式输出中…");
    status.hidden = true;   // 仅轮询/流进行中显示（轻量视觉提示）
    line.appendChild(status);
    row.appendChild(line);

    let pre = null, streamEl = null;
    if (isDelegate) {
      streamEl = el("pre", "task-stream", "(等待流式输出…)");
      streamEl.hidden = true;
      row.appendChild(streamEl);
    } else {
      const out = (t.output != null && String(t.output).trim() !== "") ? String(t.output) : "";
      pre = el("pre", "task-output" + (out ? "" : " empty"), out || "(无输出)");
      pre.hidden = true;
      row.appendChild(pre);
    }
    // 取消按钮默认不显示，藏进展开的输出/流式区（防误触）：展开时才出现，
    // 点击需 confirm 确认后才真正取消。
    const cancel = el("button", "task-cancel task-cancel-inside", "取消");
    cancel.title = "取消任务 " + (t.id != null ? t.id : "");
    cancel.hidden = true;
    cancel.addEventListener("click", async (ev) => {
      ev.stopPropagation();
      if (!window.confirm("确认取消任务 " + shortTaskLabel(t) + "？")) return;
      await cancelTask(t);
    });
    row.appendChild(cancel);
    // 就地展开/收起（回退路径：找不到消息块 / 解析不到 subagent 会话时用）
    const toggleRow = () => {
      const showing = isDelegate ? !streamEl.hidden : !pre.hidden;
      if (showing) {                 // 当前展开 → 收起：停轮询/流
        if (isDelegate) {
          streamEl.hidden = true;
          stopTaskStream(key);
          state.tasks.streamText.delete(key);
        } else {
          pre.hidden = true;
          stopTaskPoller(key);
          state.tasks.degraded.delete(key);   // 收起即重置：重新展开时重新尝试轮询
        }
        status.hidden = true;
        cancel.hidden = true;
      } else {                       // 当前收起 → 展开：启动轮询/流
        if (isDelegate) {
          streamEl.hidden = false;
          startTaskStream(t, key, streamEl, status);
        } else {
          pre.hidden = false;
          if (!state.tasks.degraded.has(key)) {   // 已降级（404）：只显示静态尾部
            startOutputPoller(key, t, pre, (phase) => { status.hidden = phase !== "start"; });
          }
        }
        cancel.hidden = false;
      }
    };
    row.addEventListener("click", () => {
      if (isDelegate) {
        // 新行为：切到该 subagent 的会话（完整消息/工具卡片/思考块渲染）
        const subId = resolveSubagentSessionId(t);
        if (subId) { openSession(subId); return; }
        // 列表可能还没拉到这个 subagent（刚创建）：补拉一次再试，
        // 仍解析不到 → 回退就地展开
        refreshSessionsForSidebar().then(() => {
          const subId2 = resolveSubagentSessionId(t);
          if (subId2) openSession(subId2);
          else toggleRow();
        });
        return;
      }
      // bash：点击 → 总是切到该任务所属会话，视图就绪后滚动高亮输出块
      // （openSession 的 onReady 在 restored/fresh 两条路径的消息区就绪后触发）。
      // 已在目标会话 → 直接 flash；找不到输出块（任务已结束/未渲染）→
      // 回退就地展开 toggleRow。
      if (!t.session_id) { toggleRow(); return; }   // 无会话归属：就地展开
      if (state.view === "chat" && state.sessionId === t.session_id) {
        const block = findTaskOutputBlock(key);
        if (block) { flashTaskOutputBlock(block); return; }
        toggleRow();
        return;
      }
      openSession(t.session_id, () => {
        const block = findTaskOutputBlock(key);
        if (block) { flashTaskOutputBlock(block); return; }
        // 找不到输出块（任务不在该会话消息区 / 已结束）：回退就地展开。
        // 异步回调期间行可能已被 2s 轮询重绘（旧行脱离 DOM）：只在仍
        // 挂载时展开，避免展开到已离屏的旧行（轮询句柄泄漏）。
        if (row.isConnected) toggleRow();
      });
    });
    if (prevExpanded.has(key)) {     // 轮询重绘恢复展开态：重启轮询/流
      if (isDelegate) {
        streamEl.hidden = false;
        startTaskStream(t, key, streamEl, status);
      } else {
        pre.hidden = false;
        if (!state.tasks.degraded.has(key)) {     // 降级行不重启轮询
          startOutputPoller(key, t, pre, (phase) => { status.hidden = phase !== "start"; });
        }
      }
      cancel.hidden = false;
    }
    list.appendChild(row);
  }
}

async function cancelTask(t) {
  if (!t || t.id == null) return;
  if (!state.token) { setBanner("⚠ 请先输入 Token。", true); return; }
  if (state.tasks.cancelling.has(t.id)) return;   // 防重复点击
  state.tasks.cancelling.add(t.id);
  try {
    const res = await api("/api/sessions/" + encodeURIComponent(t.session_id || "")
      + "/tasks/" + encodeURIComponent(t.id), { method: "DELETE" });
    if (res.status === 401 || res.status === 403) { setBanner("⚠ 认证失败：请检查 Token。"); return; }
    if (res.status === 404) { setBanner("⚠ 任务不存在（可能已完成）。"); return; }
    if (res.status !== 204) throw new Error("HTTP " + res.status);
    // 成功：立即刷新两处显示（统一轮询）
    await pollTasks();
  } catch (e) {
    setBanner("⚠ 取消失败：" + e.message);
  } finally {
    state.tasks.cancelling.delete(t.id);
  }
}
/* 统一任务轮询定时器（2s 常驻）：composer 折叠条/面板 + 消息列表输出块
   共用一次 /api/tasks（替代原「徽标 3s + 面板 2s」双轮询） */
function startTasksPolling() {
  stopTasksPolling();
  state.tasks.timer = setInterval(pollTasks, 2000);
}
function stopTasksPolling() {
  if (state.tasks.timer) { clearInterval(state.tasks.timer); state.tasks.timer = null; }
}
