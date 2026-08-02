/* =============================================================================
 * tasks.js — 运行中任务：/api/tasks 统一轮询（fetchTasks/pollTasks）、
 * composer 任务面板（renderComposerTasks/renderTaskList，卡片内就地展开：
 * bash 流式轮询 .task-output / delegate 内嵌 SSE 流式 .task-stream）、
 * 任务取消（cancelTask）。
 * 依赖 app.js + render.js + sessions.js（resolveSubagentSessionId）。
 * =============================================================================*/

/* =====================================================================
 * 运行中任务（统一轮询）
 *   1. 聊天视图 composer 上方：#tasksToggleBar 折叠条（计数即徽标，
 *      有任务时高亮）+ #composerTasks 面板（默认收起，state.tasks.composerOpen）。
 *      面板内每行一张任务卡片：bash 点击就地展开/收起 .task-output 并
 *      500ms 流式轮询 output 端点；delegate 点击切到 subagent 会话
 *      （解析不到时回退就地展开 .task-stream 内嵌 SSE）。
 * 统一轮询 pollTasks()（2s 常驻）：一次 GET /api/tasks 更新面板。
 * ===================================================================*/
/* GET /api/tasks → 任务数组；失败/无 token 返回 null（调用方决定如何处理）。
   注意：任务面板保持「激活 workspace」单服务器作用域——聚合多服务器任务
   超出本轮范围（任务行已带 [workspace:] 参数标签可区分），侧边栏聚合的是
   会话而非任务。 */
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

/* 统一任务轮询（防重入：seq 竞态序号只应用最新一次响应）。 */
async function pollTasks() {
  const seq = ++state.tasks.seq;
  const tasks = await fetchTasks();
  if (tasks === null || seq !== state.tasks.seq) return;  // 过期响应丢弃
  state.tasks.list = tasks;
  renderComposerTasks();
}

/* composer 上方折叠条 + 面板：计数即徽标；有任务时高亮，无任务整条隐藏。
   面板内容仅在展开时渲染（收起时只更新计数/箭头/高亮）。 */
function renderComposerTasks() {
  const bar = els.tasksToggleBar;
  if (!bar) return;
  const panel = els.composerTasks;
  const n = (state.tasks.list || []).length;
  bar.hidden = n === 0;
  bar.classList.toggle("active", n > 0);
  // 任务清空时整个组件（折叠条+面板）完全消失：强制收起面板，避免
  // 出现「暂无运行中任务」的空态壳；同时清理所有卡片轮询/流，并清空
  // 面板内容（面板隐藏后不再走 renderTaskList 的重绘清理路径）
  if (n === 0) {
    state.tasks.composerOpen = false;
    for (const k of [...state.tasks.pollers.keys()]) stopTaskPoller(k);
    for (const k of [...state.tasks.streams.keys()]) stopTaskStream(k);
    if (panel) panel.innerHTML = "";
  }
  bar.classList.toggle("open", state.tasks.composerOpen);
  const label = bar.querySelector(".tasks-toggle-label");
  if (label) label.textContent = "运行中任务 (" + n + ")";
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
   （key=session_id:id），同一 key 同时只有一处活跃，后启动的会先停掉旧
   句柄再接管。onPhase 回调可选：onPhase("start"|"stop"|"degraded")，
   卡片用它驱动状态小字显隐。 */
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
   bash：行点击 → 就地展开/收起 .task-output（500ms 轮询 output 端点
   流式更新；旧后端 404 → 停轮询，降级为静态尾部）。delegate：行点击 →
   openSession(该 subagent 的会话)，那边有完整消息/工具卡片/思考块渲染；
   解析不到 subagent 会话时回退为就地展开内嵌 SSE 流式区 .task-stream。
   重绘前记录已展开的行并按任务 key（session_id:id）恢复——2s 轮询重绘
   不打断正在查看的输出/流；消失的任务在此停掉轮询/流并清理文本缓冲。 */
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
    return;   // 组件已由 renderComposerTasks 在 n===0 时整体隐藏，不再渲染空态
  }
  for (const t of tasks) {
    const row = el("div", "task-row");
    const key = (t.session_id || "") + ":" + (t.id != null ? t.id : "");
    row.setAttribute("data-task", key);
    const isDelegate = t.kind === "delegate";
    row.title = isDelegate ? "点击切换到该子代理的会话" : "点击展开/收起输出（流式更新）";
    const line = el("div", "task-line");
    // 非 delegate 任务的 kind 是实际 shell 名（bash / pwsh）：徽章显示真实值
    const shellKind = isDelegate ? "delegate" : (t.kind || "shell");
    const badge = el("span", "kind-badge " + (isDelegate ? "delegate" : shellKind),
      isDelegate ? (t.role || "子代理") : shellKind);
    line.appendChild(badge);
    line.appendChild(el("span", "task-label", shortTaskLabel(t)));
    if (t.role) line.appendChild(el("span", "task-meta trole", t.role));
    // 参数标签（与 TUI 面板一致）：background / workspace / resume。resume
    // 表示该子代理是延续会话（delegate resume: "<id>"）。仅在有标签时渲染。
    const tags = [];
    if (t.background === true) tags.push(el("span", "task-tag", "background"));
    if (t.workspace && String(t.workspace).trim() !== "") {
      const tag = el("span", "task-tag", "workspace: " + truncate(t.workspace, 40));
      tag.title = String(t.workspace);   // 截断后完整路径放 title
      tags.push(tag);
    }
    if (t.resume && String(t.resume).trim() !== "") {
      const tag = el("span", "task-tag", "resume: " + t.resume);
      tag.title = String(t.resume);
      tags.push(tag);
    }
    if (tags.length) {
      const tagBox = el("span", "task-tags");
      for (const tag of tags) tagBox.appendChild(tag);
      line.appendChild(tagBox);
    }
    // 会话 id 完整显示：delegate 任务显示 subagent 自己的会话（点击跳转的目标），
    // 其余任务显示父会话。不截断——省略的 id 无法区分会话。
    const sidShown = isDelegate ? (t.subagent_session_id || t.session_id) : t.session_id;
    if (sidShown) line.appendChild(el("span", "task-meta tsid", "会话 " + sidShown));
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
    // 就地展开/收起（bash 点击主路径；delegate 解析不到 subagent 会话时的回退）
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
        if (subId) {
          openSession(subId);
          // 直连跳转时 lastList 可能还没包含该 subagent（轮询未刷新），
          // 补拉一次让 openSession/refreshSessionsForSidebar 能按
          // parent_session_id 显示「← 主会话」返回按钮。
          refreshSessionsForSidebar();
          return;
        }
        // 列表可能还没拉到这个 subagent（刚创建）：补拉一次再试，
        // 仍解析不到 → 回退就地展开
        refreshSessionsForSidebar().then(() => {
          const subId2 = resolveSubagentSessionId(t);
          if (subId2) openSession(subId2);
          else toggleRow();
        });
        return;
      }
      // bash：点击 → 就地展开/收起卡片内 .task-output + 启动 500ms 流式
      // 轮询（startOutputPoller 实时刷新输出；旧后端 404 自动降级为静态
      // 尾部）。命令输出放卡片里，流式保持。
      toggleRow();
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
    // 成功：立即刷新面板（统一轮询）
    await pollTasks();
  } catch (e) {
    setBanner("⚠ 取消失败：" + e.message);
  } finally {
    state.tasks.cancelling.delete(t.id);
  }
}
/* 统一任务轮询定时器（2s 常驻）：composer 折叠条/面板共用一次 /api/tasks */
function startTasksPolling() {
  stopTasksPolling();
  state.tasks.timer = setInterval(pollTasks, 2000);
}
function stopTasksPolling() {
  if (state.tasks.timer) { clearInterval(state.tasks.timer); state.tasks.timer = null; }
}
