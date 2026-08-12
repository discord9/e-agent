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
   ws 指定目标 workspace（apiFor 按 ws 的 base/token 发请求）：多 workspace
   聚合轮询时每个 workspace 各自拉取，单 workspace / 未配置时即激活服务器。
   注意：/api/tasks 的行数是 wrapper 任务存活指标（backend wrapper 进程/
   后台任务是否还在），与 orbit 环绕红点的 subagent Busy 语义不同——二者
   各自独立表达，不做换算（oracle#95）。
   注意：聚合只发生在前端数据层（state.tasks.list 合并 + _ws 标记），
   composer 面板仍保持「激活 workspace」作用域（renderComposerTasks 按
   _ws 过滤），侧边栏环绕点消费全量列表。 */
async function fetchTasks(ws) {
  if (!workspaceToken(ws)) return null;
  try {
    // fetchWithTimeout（sessions.js）：10s 上限 + AbortController。裸 apiFor
    // 无超时，任一 workspace 的 /api/tasks 永久 pending 会拖住 pollTasks 整轮
    // Promise.all——其它健康 workspace 的响应也进不了缓存，面板「加载不出来」。
    // 超时 → AbortError → 下方 catch 返回 null → 该 ws 保留旧缓存（stale），
    // 下轮 2s 轮询自动恢复。
    const res = await fetchWithTimeout(ws, "/api/tasks");
    if (res.status === 401 || res.status === 403) {
      // 认证失败只对激活 workspace 弹 banner（背景 workspace 静默，不刷屏）
      if (ws === state.workspace) setBanner("⚠ 认证失败：请检查 Token。");
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
   多 workspace 聚合：遍历 state.workspaces，每个 workspace 经 apiFor 各自
   拉 /api/tasks（与 pollAllWorkspaces 同款语义），合并成一份全量列表存
   state.tasks.list——侧边栏环绕点按 session_id 匹配父会话，需要全量。
   每个任务打 _ws 标记（所属 workspace id）供面板/侧边栏按 workspace 过滤；
   单 workspace / 未配置时行为与旧版一致（只拉激活 workspace 一份）。
   某 workspace 拉取失败 → 保留其旧缓存（stale，与单服务器语义一致），
   不参与本轮合并。 */
async function pollTasks() {
  const seq = ++state.tasks.seq;
  const wss = (state.workspaces || []).slice();
  if (!wss.length && state.workspace) wss.push(state.workspace);   // 兜底
  const results = await Promise.all(wss.map(async (ws) => ({ ws, tasks: await fetchTasks(ws) })));
  if (seq !== state.tasks.seq) return;   // 过期响应丢弃
  const all = [];
  for (const { ws, tasks } of results) {
    if (tasks === null) {
      // 拉取失败：保留该 workspace 的旧缓存（stale），避免任务闪烁消失
      const old = state.tasks.byWorkspace[ws.id];
      if (old) all.push(...old);
      continue;
    }
    const tagged = tasks.map((t) => Object.assign({}, t, { _ws: ws.id }));
    state.tasks.byWorkspace[ws.id] = tagged;
    all.push(...tagged);
  }
  state.tasks.list = all;
  renderComposerTasks();
  renderSidebarTree();   // 任务数据恢复后主动触发侧边栏重绘（dot 数据源变化，
                         // 不再依赖 sessionId 变化碰巧打破 sidebarTreeSig 去重）
}

/* 任务元数据签名（整列表/单行两级去重用）：决定任务列表/卡片是否需要重建。
   排除 t.output——输出变化走 renderTaskList 的保留行就地更新
   （updateRetainedTaskRow 只动 <pre>），不构成重建理由；无活跃 poller 的
   bash 行 output 由 tasksRenderSig 纳入渲染签名触发就地更新。 */
function taskKeySig(t) {
  return JSON.stringify([
    t.session_id || "", t.id != null ? t.id : "", t.kind || "", t.label || "",
    t.role || "", t.full_command || "", t.subagent_session_id || "",
    t.background === true ? 1 : 0, t.workspace || "", t.resume || "",
    t.owner_session || "",
  ]);
}

/* 单个任务 key（session_id:id，与行 data-task 一致） */
function taskKey(t) {
  return (t.session_id || "") + ":" + (t.id != null ? t.id : "");
}

function tasksListSig(list) {
  return JSON.stringify((list || []).map((t) => taskKeySig(t)));
}

let lastTasksSig = "";

/* 渲染签名：数据签名 + 无活跃 output poller 的 bash 行静态 output。
   有活跃 poller 的行文本由 500ms output 轮询实时刷新，计入签名只会每轮
   重建卡片（排除）；但「无 poller 的 bash 行」的文本 = /api/tasks 尾部
   output 快照——折叠行（展开才启动轮询）、降级行（旧后端 404 不再重启）、
   网络失败已停轮询的展开行——output 变化必须触发 renderTaskList 的保留行
   就地更新（updateRetainedTaskRow 只动 <pre>，不重建、不打断轮询/流）。 */
function tasksRenderSig(list) {
  const base = tasksListSig(list);
  const staticOut = [];
  for (const t of list || []) {
    const key = taskKey(t);
    if (t.kind !== "delegate" && !state.tasks.pollers.has(key)) {
      staticOut.push([key, String(t.output != null ? t.output : "")]);
    }
  }
  return base + "|s" + JSON.stringify(staticOut);
}

/* 已渲染签名：只在 renderTaskList 实际完成后更新（与 lastTasksSig 分开——
   收起期间数据变化时，重开必须按「数据 ≠ 已渲染 DOM」重建，不能跳过）。 */
let lastTasksRenderedSig = "";

/* composer 上方折叠条 + 面板：计数即徽标；有任务时高亮，无任务整条隐藏。
   面板只显示激活 workspace 的任务：state.tasks.list 是多 workspace 聚合的
   全量列表（每任务带 _ws 标记），这里按 state.workspace.id 过滤——其他
   workspace 的任务只供侧边栏环绕点消费，不混入本 workspace 的面板/徽标。
   面板内容仅在展开时渲染（收起时只更新计数/箭头/高亮，不触碰 DOM）。
   签名去重：数据未变且面板已渲染 → 跳过 renderTaskList，保留已展开的卡片
   DOM 与进行中的 output 轮询/SSE 流（2s 轮询不再每轮销毁重建）。 */
function renderComposerTasks() {
  const bar = els.tasksToggleBar;
  if (!bar) return;
  const panel = els.composerTasks;
  const wsId = state.workspace ? state.workspace.id : null;
  // 过滤到激活 workspace；无 _ws 的旧数据（switchWorkspace 清空等）视为当前
  const list = (state.tasks.list || []).filter((t) => !t._ws || t._ws === wsId);
  const n = list.length;
  const sig = tasksRenderSig(list);
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
    lastTasksRenderedSig = sig;   // DOM 已清空：与空列表一致
  }
  bar.classList.toggle("open", state.tasks.composerOpen);
  const label = bar.querySelector(".tasks-toggle-label");
  if (label) label.textContent = "运行中任务 (" + n + ")";
  if (panel) {
    panel.hidden = !state.tasks.composerOpen;
    updateJumpBottomPosition();   // 面板显隐后移动「回到底部」按钮，避免盖住面板
    if (state.tasks.composerOpen) {
      // 数据签名与已渲染签名分开：收起期间数据变化 → 重开时 sig ≠ 已渲染
      // 签名 → 重建（不显示旧行/旧闭包）；数据未变 → 跳过（DOM 仍最新）。
      // !rendered 兜底 DOM 被外部清空（switchWorkspace 等）但签名未变的场景。
      const rendered = panel.querySelectorAll(".task-row").length > 0;
      if (sig !== lastTasksRenderedSig || sig !== lastTasksSig || !rendered) {
        lastTasksSig = sig;
        lastTasksRenderedSig = sig;
        renderTaskList(list, panel);
      }
    } else {
      lastTasksSig = sig;   // 收起：只记录 data 签名，不更新已渲染签名
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

/* 滚动守卫：自动贴底只在用户未主动上滚时生效（与主聊天区 userScrolled 守卫
   同款语义，见 sse.js messages scroll 监听 / render.js scrollBottom）。
   程序滚动（scrollTop 赋值）派生的 scroll 事件 isTrusted=false 不处理；
   用户滚回底部（4px 容差）时复位，恢复自动跟随。状态存元素私有属性
   _userScrolled，随元素生命周期走（行重建即重置）。 */
function attachScrollGuard(el) {
  el.addEventListener("scroll", (ev) => {
    if (!ev.isTrusted) return;
    const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight <= 4;
    el._userScrolled = !atBottom;
  });
}

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
  pre._userScrolled = false;   // 新一轮轮询从底部跟随开始；用户上滚后才锁定
  let intervalId = null;
  // 只停自己的轮询：2s 重绘会先停旧轮询再启新轮询，旧 tick 的异步收尾
  // 不能误清新轮询（竞态：key 相同）。
  const stop = (degraded) => {
    if (state.tasks.pollers.get(key) !== intervalId) return;
    stopTaskPoller(key);
    if (degraded) state.tasks.degraded.add(key);
    if (onPhase) onPhase(degraded ? "degraded" : "stop");
  };
  let tickInFlight = false;   // 防重入：上一轮未完成则跳过本轮（慢响应不叠加）
  const tick = async () => {
    // clearInterval 无法撤回已排入事件队列的 tick；导航清理后即使旧回调
    // 恰好开始执行，也必须在发请求前确认自己仍是该行的当前 poller。
    if (state.tasks.pollers.get(key) !== intervalId) return;
    if (!state.token) { stop(false); return; }
    if (tickInFlight) return;
    tickInFlight = true;
    try {
      const res = await api("/api/sessions/" + encodeURIComponent(t.session_id || "")
        + "/tasks/" + encodeURIComponent(t.id) + "/output");
      if (state.tasks.pollers.get(key) !== intervalId) return;
      if (res.status === 401 || res.status === 403) {
        setBanner("⚠ 认证失败：请检查 Token。");
        stop(false); return;
      }
      if (res.status === 404 || !res.ok) {
        // 旧后端/端点不可用：停轮询并记为降级（重绘不再重启），输出区保持静态尾部
        stop(true); return;
      }
      const text = await res.text();
      if (state.tasks.pollers.get(key) !== intervalId) return;
      if (!pre.isConnected) { stop(false); return; }
      const txt = String(text).trim() !== "" ? String(text) : "";
      pre.classList.toggle("empty", txt === "");
      // 增量追加（保住选区）：_lastText 缓存上次已渲染的完整文本；txt 以它为
      // 前缀 → 只 append 差值文本节点，不整段 textContent 重写（整段重写会
      // 塌缩用户选区，无法复制）。输出被清空/重置/改写（非前缀）→ 整体替换
      // 并复位缓存。
      if (txt === "") {
        if (pre._lastText !== "" || pre.textContent !== "(无输出)") {
          pre.textContent = "(无输出)";   // 任务重置/清空：回到空占位
        }
        pre._lastText = "";
      } else if (txt.startsWith(pre._lastText || "")) {
        if (!pre._lastText && pre.textContent !== "") pre.textContent = "";   // 清掉占位/旧内容
        const chunk = txt.slice((pre._lastText || "").length);
        if (chunk) pre.appendChild(document.createTextNode(chunk));
        pre._lastText = txt;
      } else {
        pre.textContent = txt;   // 输出被改写（非前缀）：整体替换
        pre._lastText = txt;
      }
      if (!pre._userScrolled) pre.scrollTop = pre.scrollHeight;
    } catch (e) {
      stop(false);   // 网络失败：停轮询，保留已有内容
    } finally {
      tickInFlight = false;
    }
  };
  intervalId = setInterval(tick, 500);
  state.tasks.pollers.set(key, intervalId);
  tick();   // 先登记所有权再立即拉一次，tick 顶部可统一做陈旧回调校验
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
  streamEl._lastText = state.tasks.streamText.get(key) || "";   // 增量基准：DOM 与缓存对齐
  streamEl._userScrolled = false;   // 新流从底部跟随开始（元素新建/重建时本就无状态）
  if (!streamEl._userScrolled) streamEl.scrollTop = streamEl.scrollHeight;
  (async () => {
    try {
      const res = await api("/api/sessions/" + encodeURIComponent(t.session_id || "") + "/events", {
        headers: { "Accept": "text/event-stream" },
        signal: ctrl.signal,
      });
      // workspace/会话导航会 abort 并从 map 移除旧流；迟到响应不得再写
      // banner、状态或旧行文本（AbortController 在响应已落地时未必能阻止续体）。
      if (ctrl.signal.aborted || state.tasks.streams.get(key) !== ctrl) return;
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
        if (ctrl.signal.aborted || state.tasks.streams.get(key) !== ctrl) break;
        if (done) break;
        buf += decoder.decode(value, { stream: true }).replace(/\r\n/g, "\n");
        let idx;
        while ((idx = buf.indexOf("\n\n")) !== -1) {
          const block = buf.slice(0, idx);
          buf = buf.slice(idx + 2);
          handleTaskStreamBlock(block, streamEl, key);
        }
      }
      // 正常结束（后端关闭流）：停止消费，保留已显示内容。导航清理后的
      // 旧流不再触碰已脱离 DOM 的状态节点。
      if (!ctrl.signal.aborted && state.tasks.streams.get(key) === ctrl && status) status.hidden = true;
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

/* workspace / 会话导航的统一行级资源清理。除了主任务 2s 轮询，展开行还
   各自持有 output interval 或 delegate SSE；导航时全部停止并清掉旧 DOM，
   防止旧 workspace/session 在后台继续请求。下次展开由最新 tasks.list 重建。 */
function stopTaskRows() {
  for (const key of [...state.tasks.pollers.keys()]) stopTaskPoller(key);
  for (const key of [...state.tasks.streams.keys()]) stopTaskStream(key);
  state.tasks.composerOpen = false;
  lastTasksSig = "";
  lastTasksRenderedSig = "";
  if (els.composerTasks) {
    els.composerTasks.hidden = true;
    els.composerTasks.innerHTML = "";
  }
  updateJumpBottomPosition();   // 面板已强制收起：「回到底部」按钮回位
}

/* 「回到底部」按钮随 composer 任务区（折叠条 + 面板）浮动：任务区会盖住
   absolute 钉在聊天区右下（bottom:110px）的按钮，用户点「回到底部」会误触
   折叠条或任务行的「结束/取消」。布局上折叠条在面板之上、且面板打开时两者
   同时可见，所以避让必须算整体高度：折叠条可见时计入其高度，面板打开时再
   叠加面板高度（+ 8px 间隙）；任务清空（整个组件消失）时清掉内联 bottom
   回默认 110px。不用固定 calc(30vh+…)：面板高度随任务数变化，固定上限值
   在任务少时按钮悬空过高。 */
function updateJumpBottomPosition() {
  const btn = els.jumpBottomBtn;
  if (!btn) return;
  const bar = els.tasksToggleBar;
  const panel = els.composerTasks;
  // 面板打开时才避让（折叠条 + 面板整体之上）；面板关闭时折叠条单独
  // 可见但位于 bottom:110px 之上方、本就不挡按钮 → 回默认。避免「折叠条
  // 可见但面板收起」时按钮悬在 148px 的过度避让。
  const h = (state.tasks.composerOpen && panel && !panel.hidden)
    ? ((bar && !bar.hidden ? (bar.offsetHeight || 0) : 0) + (panel.offsetHeight || 0)) : 0;
  if (h > 0) btn.style.bottom = (110 + h + 8) + "px";
  else btn.style.bottom = "";   // 回默认 110px（style.css .jump-bottom）
}

/* 整体替换流式区文本（snapshot 重放 / AssistantText / 404 提示）。
   保留全量设置语义（初始/重置），但文本未变时跳过 DOM 写（保住选区），
   并维护 _lastText 供 appendTaskStreamText 增量续写。 */
function setTaskStreamText(streamEl, key, text) {
  state.tasks.streamText.set(key, text);
  if (!streamEl.isConnected) return;
  const s = String(text);
  if (streamEl._lastText === s) return;   // 文本未变：跳过 DOM 写（保住选区）
  streamEl.classList.toggle("empty", s.trim() === "");
  streamEl.textContent = s;
  streamEl._lastText = s;
  if (!streamEl._userScrolled) streamEl.scrollTop = streamEl.scrollHeight;
}

/* 追加流式文本（live AssistantDelta）：只 append 新 delta 的文本节点，
   不整段重写（整段重写会塌缩用户选区，无法复制）。streamText 缓存总是
   最新完整文本；DOM 按 _lastText 增量对齐，失步（行重建/重置）时整体替换。 */
function appendTaskStreamText(streamEl, key, text) {
  if (!text) return;
  const next = (state.tasks.streamText.get(key) || "") + text;
  state.tasks.streamText.set(key, next);
  if (!streamEl.isConnected) return;
  streamEl.classList.remove("empty");
  if (streamEl._lastText === next) return;   // 已同步
  if (next.startsWith(streamEl._lastText || "")) {
    if (!streamEl._lastText && streamEl.textContent !== "") {
      streamEl.textContent = "";   // 清掉占位/旧内容，再追加首个真实 chunk
    }
    const chunk = next.slice((streamEl._lastText || "").length);
    if (chunk) streamEl.appendChild(document.createTextNode(chunk));
    streamEl._lastText = next;
  } else {
    streamEl.textContent = next;   // 缓存与 DOM 失步（重建/重置）：整体替换
    streamEl._lastText = next;
  }
  if (!streamEl._userScrolled) streamEl.scrollTop = streamEl.scrollHeight;
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
   keyed 就地更新：元数据未变的行原样保留（展开态/轮询/流不打断），只重建
   变化/新增的行（按 prevExpanded 恢复展开）、移除消失的行；行顺序跟随
   tasks 数组。整列表未变时由 renderComposerTasks 的签名提前跳过。 */
function renderTaskList(tasks, container) {
  const list = container;
  if (!list) return;
  const rows = [...list.querySelectorAll(".task-row")];
  const byKey = new Map();
  for (const row of rows) {
    const key = row.getAttribute("data-task");
    if (key) byKey.set(key, row);
  }
  // 记录「已展开」的行（输出区或流式区），供变化/新增行重建后恢复展开；
  // 未变的行保持原样，它们的轮询/流继续运行、不被停掉。
  const prevExpanded = new Set();
  for (const [key, row] of byKey) {
    const pre = row.querySelector(".task-output");
    const streamEl = row.querySelector(".task-stream");
    if ((pre && !pre.hidden) || (streamEl && !streamEl.hidden)) prevExpanded.add(key);
  }
  // 消失的任务：清理已累积的流式文本缓冲与降级标记（轮询/流在下方移除
  // 循环里停掉）。放在空列表早退之前：任务全部结束时也要清理，防泄漏。
  const activeKeys = new Set();
  for (const t of tasks) activeKeys.add(taskKey(t));
  for (const k of Array.from(state.tasks.streamText.keys())) {
    if (!activeKeys.has(k)) state.tasks.streamText.delete(k);
  }
  for (const k of Array.from(state.tasks.degraded.keys())) {
    if (!activeKeys.has(k)) state.tasks.degraded.delete(k);
  }
  if (!tasks.length) {
    for (const [key, row] of byKey) { stopTaskPoller(key); stopTaskStream(key); row.remove(); }
    return;   // 组件已由 renderComposerTasks 在 n===0 时整体隐藏，不再渲染空态
  }
  let prev = null;   // 前一个已处理的行（插入锚点，保持 tasks 数组顺序）
  for (const t of tasks) {
    const key = taskKey(t);
    const sig = taskKeySig(t);
    let row = byKey.get(key) || null;
    if (row) byKey.delete(key);
    if (row && row.getAttribute("data-key-sig") === sig) {
      // 元数据未变：保留原行（展开态/轮询/流原样），但按当前锚点移动——
      // DOM 行序必须跟随 tasks 数组顺序（复用节点 move，不重建）
      const anchor = prev ? prev.nextSibling : list.firstChild;
      if (row !== anchor) {
        if (anchor) list.insertBefore(row, anchor);
        else list.appendChild(row);
      }
      updateRetainedTaskRow(row, t, key);   // 静态输出就地更新（不重建）
      prev = row;
      continue;
    }
    if (row) {      // 元数据变了：停旧轮询/流后重建该行
      stopTaskPoller(key);
      stopTaskStream(key);
      row.remove();
    }
    const nrow = buildTaskRow(t, key, prevExpanded.has(key));
    if (prev && prev.nextSibling) list.insertBefore(nrow, prev.nextSibling);
    else list.appendChild(nrow);
    prev = nrow;
  }
  // 消失的任务：移除行并停其轮询/流
  for (const [key, row] of byKey) {
    stopTaskPoller(key);
    stopTaskStream(key);
    row.remove();
  }
}

/* 保留行的就地更新（元数据未变时重建会打断展开态/轮询/流，这里只补静态
   输出）：对「无活跃 output poller」的 bash 行——折叠行、降级行（旧后端
   output 端点 404）、网络失败已停轮询的展开行——用新 t.output 更新
   .task-output 文本（仅就地 <pre>，不重建行）；有活跃轮询的行文本由
   500ms output 轮询实时刷新，跳过以免闪断。delegate 行走 SSE 流，无静态
   输出。 */
function updateRetainedTaskRow(row, t, key) {
  if (t.kind === "delegate") return;
  const pre = row.querySelector(".task-output");
  if (!pre) return;
  if (state.tasks.pollers.has(key)) return;
  const out = (t.output != null && String(t.output).trim() !== "") ? String(t.output) : "";
  pre.classList.toggle("empty", out === "");
  pre.textContent = out || "(无输出)";
  pre._lastText = out;   // 同步增量缓存：重新展开时 tick 以此为前缀基准，避免重复追加
}

/* 单个任务卡片行（keyed 更新用）：data-task = key、data-key-sig = 元数据
   签名。restoreExpanded=true 时按展开态启动 500ms output 轮询 / delegate
   SSE 流（与旧 renderTaskList 的「重绘恢复展开态」语义一致）。 */
function buildTaskRow(t, key, restoreExpanded) {
  // 当前会话发起的任务（bash 的 session_id / delegate 的父 session_id 等于
  // 正在查看的会话）→ 行加 current 标记：左侧 cyan accent bar + 「本会话」
  // 标签，任务面板里一眼可辨哪些属于当前会话。
  const isCurrentSession = t.session_id != null && t.session_id === state.sessionId;
  const row = el("div", "task-row" + (isCurrentSession ? " task-row-current" : ""));
  row.setAttribute("data-task", key);
  row.setAttribute("data-key-sig", taskKeySig(t));
    const isDelegate = t.kind === "delegate";
    row.title = isDelegate ? "点击切换到该子代理的会话" : "点击展开/收起输出（流式更新）";
    const line = el("div", "task-line");
    // 非 delegate 任务的 kind 是实际 shell 名（bash / pwsh）：徽章显示真实值
    const shellKind = isDelegate ? "delegate" : (t.kind || "shell");
    const badge = el("span", "kind-badge " + (isDelegate ? "delegate" : shellKind),
      isDelegate ? (t.role || "子代理") : shellKind);
    line.appendChild(badge);
    // 任务序号（后端 TaskMeta.id）：有 id 才显示，否则安静省略
    if (t.id != null) line.appendChild(el("span", "task-meta tid", "#" + t.id));
    line.appendChild(el("span", "task-label", shortTaskLabel(t)));
    if (t.role) line.appendChild(el("span", "task-meta trole", t.role));
    // 参数标签（与 TUI 面板一致）：background / workspace / resume。resume
    // 表示该子代理是延续会话（delegate resume: "<id>"）。仅在有标签时渲染。
    const tags = [];
    if (isCurrentSession) {
      const tag = el("span", "task-tag task-tag-current", "本会话");
      tag.title = "由当前查看的会话发起";
      tags.push(tag);
    }
    if (t.background === true) tags.push(el("span", "task-tag", "background"));
    if (t.workspace && String(t.workspace).trim() !== "") {
      const tag = el("span", "task-tag", "workspace: " + t.workspace);
      tag.title = String(t.workspace);   // 完整路径，悬停 title 保留（无害）
      tags.push(tag);
    }
    if (t.resume && String(t.resume).trim() !== "") {
      // 只显示 "resume" 文字：session id 不占标签空间（完整 id 在 hover
      // title 里，需要时仍可查看）。
      const tag = el("span", "task-tag", "resume");
      tag.title = String(t.resume);
      tags.push(tag);
    }
    if (tags.length) {
      const tagBox = el("span", "task-tags");
      for (const tag of tags) tagBox.appendChild(tag);
      line.appendChild(tagBox);
    }
    // 会话标识：delegate 任务显示 subagent 自己的会话 id（点击跳转的目标；
    // 它的 title 就是 task-label，已在上面显示，不再重复）。bash 任务显示
    // 发起者——owner_session（subagent 的 bash = subagent 会话 id）有值时查
    // 发起者的 title/label（subagent 的 title 可能为空，label 优先），
    // None/主会话任务回退 session_id（查主会话 title，无则「会话 <id>」）。
    const ownerId = t.owner_session || t.session_id;
    const sidShown = isDelegate ? (t.subagent_session_id || t.session_id) : ownerId;
    if (sidShown) {
      let sidLabel = "会话 " + sidShown;
      let sidTitle = "";
      if (!isDelegate) {
        const wsId = state.workspace ? state.workspace.id : null;
        const plist = (wsId && state.workspaceLists[wsId] !== undefined)
          ? state.workspaceLists[wsId] : state.lastList;
        const owner = (plist || []).find((s) => s && s.id === ownerId) || null;
        const ownerTitle = owner
          ? (owner.title != null && String(owner.title).trim() !== ""
              ? String(owner.title)
              : (owner.label != null ? String(owner.label) : ""))
          : "";
        if (ownerTitle.trim() !== "") {
          sidLabel = truncate(ownerTitle, 40);
          sidTitle = ownerId + ": " + ownerTitle;   // 悬停完整 id + title
        }
      }
      const tag = el("span", "task-meta tsid", sidLabel);
      if (sidTitle) tag.title = sidTitle;
      line.appendChild(tag);
    }
    // 父会话标签：delegate 任务显示其父会话 = t.session_id（发起它的会话）。
    // 先查激活 workspace 的 session 列表（state.workspaceLists，回退
    // state.lastList）拿父会话 title：有 title 显示「父: <title>」（截断到
    // ~40 字符，悬停 title 放完整标题 + 会话 id），无 title / 查不到 →
    // 回退「父: <session_id>」。非 delegate 任务 session_id 即父，
    // 且「会话 <id>」已显示，不重复。查不到 / 无父 / 与子会话 id 相同 →
    // 安静降级不显示。
    if (isDelegate && t.session_id && t.session_id !== sidShown) {
      const wsId = state.workspace ? state.workspace.id : null;
      const plist = (wsId && state.workspaceLists[wsId] !== undefined)
        ? state.workspaceLists[wsId] : state.lastList;
      const parent = (plist || []).find((s) => s && s.id === t.session_id) || null;
      const parentTitle = parent && parent.title != null ? String(parent.title) : "";
      if (parentTitle.trim() !== "") {
        const tag = el("span", "task-meta tparent", "父: " + truncate(parentTitle, 40));
        tag.title = t.session_id + ": " + parentTitle;   // 悬停显示完整标题 + 会话 id
        line.appendChild(tag);
      } else {
        line.appendChild(el("span", "task-meta tparent", "父: " + t.session_id));
      }
    }
    const status = el("span", "task-stream-status", "● 流式输出中…");
    status.hidden = true;   // 仅轮询/流进行中显示（轻量视觉提示）
    line.appendChild(status);
    // 行尾常显取消按钮（bash/delegate/btw 统一 .task-cancel-inline，状态点
    // 之后、margin-left:auto 推到行尾）：不用展开就能取消。delegate 的点击
    // 语义是「跳转到子代理会话」、btw 的「结束对话」= cancel task（cancel
    // 取消 WaitForInput runner，DelegateCleanup 清理注册），本就没有可展开
    // 的取消入口；bash 虽然行点击是展开输出区，但展开区按钮（原
    // task-cancel-inside）已移除——统一成行尾一个取消入口，避免一行两个
    // 取消按钮。muted 小字，hover 才变红，避免误触；stopPropagation 保证
    // 不触发行点击（bash 展开 / delegate 跳转会话）。
    const endBtn = el("button", "task-cancel-inline", isDelegate ? "结束" : "取消");
    endBtn.title = isDelegate
      ? "结束该子代理对话（取消任务 " + (t.id != null ? t.id : "") + "）"
      : "取消任务 " + (t.id != null ? t.id : "");
    endBtn.addEventListener("click", async (ev) => {
      ev.stopPropagation();            // 不触发行点击（跳转会话/展开输出）
      if (isDelegate) {
        if (!window.confirm("确认结束子代理对话 " + shortTaskLabel(t) + "？")) return;
      } else if (!window.confirm("确认取消任务 " + shortTaskLabel(t) + "？")) return;
      await cancelTask(t);             // 复用统一取消路径（DELETE /tasks/{id}）
    });
    line.appendChild(endBtn);
    row.appendChild(line);

    let pre = null, streamEl = null, cmdEl = null;
    if (isDelegate) {
      streamEl = el("pre", "task-stream", "(等待流式输出…)");
      streamEl.hidden = true;
      attachScrollGuard(streamEl);
      row.appendChild(streamEl);
    } else {
      // 展开区顶部命令行：bash 任务有 full_command 时显示完整命令原文
      // （不截断；el() 走 textContent，<>& 等字符安全渲染），置于
      // .task-output 之前，与输出区同开同关。full_command 缺失/空白 →
      // 安静省略不渲染该元素。
      const fullCmd = t.full_command != null ? String(t.full_command) : "";
      if (fullCmd.trim() !== "") {
        cmdEl = el("pre", "task-command", "命令: " + fullCmd);
        cmdEl.hidden = true;
        row.appendChild(cmdEl);
      }
      const out = (t.output != null && String(t.output).trim() !== "") ? String(t.output) : "";
      pre = el("pre", "task-output" + (out ? "" : " empty"), out || "(无输出)");
      pre._lastText = out;   // 初始静态快照即增量基准（首轮 tick 以它为前缀判断）
      pre.hidden = true;
      attachScrollGuard(pre);
      row.appendChild(pre);
    }
    // 取消入口已统一为行尾常显按钮（见上 .task-cancel-inline），原展开区
    // 隐藏取消按钮（task-cancel-inside）已移除，避免一行两个取消按钮。
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
          if (cmdEl) cmdEl.hidden = true;   // 命令行与输出区同关
          stopTaskPoller(key);
          state.tasks.degraded.delete(key);   // 收起即重置：重新展开时重新尝试轮询
        }
        status.hidden = true;
      } else {                       // 当前收起 → 展开：启动轮询/流
        if (isDelegate) {
          streamEl.hidden = false;
          startTaskStream(t, key, streamEl, status);
        } else {
          pre.hidden = false;
          if (cmdEl) cmdEl.hidden = false;   // 命令行与输出区同开
          if (!state.tasks.degraded.has(key)) {   // 已降级（404）：只显示静态尾部
            startOutputPoller(key, t, pre, (phase) => { status.hidden = phase !== "start"; });
          }
        }
      }
      updateJumpBottomPosition();   // 行展开/收起改变面板高度：同步移动按钮
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
    if (restoreExpanded) {     // 重建恢复展开态：重启轮询/流（与旧重绘语义一致）
      if (isDelegate) {
        streamEl.hidden = false;
        startTaskStream(t, key, streamEl, status);
      } else {
        pre.hidden = false;
        if (cmdEl) cmdEl.hidden = false;   // 重建恢复展开态：命令行同步展开
        if (!state.tasks.degraded.has(key)) {     // 降级行不重启轮询
          startOutputPoller(key, t, pre, (phase) => { status.hidden = phase !== "start"; });
        }
      }
    }
    return row;
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
/* 统一任务轮询定时器（2s 常驻）：composer 折叠条/面板共用一次 /api/tasks。
   与 pollSessions 的 2s 链错峰：首轮延迟 1000ms 再进入 2s 周期，避免两条
   轮询同帧各自 Promise.all(5) 打出 10 个并发请求（相位偏移 1000ms）。 */
const TASKS_POLL_OFFSET_MS = 1000;

function startTasksPolling() {
  stopTasksPolling();
  state.tasks.timer = setTimeout(() => {
    state.tasks.timer = setInterval(pollTasks, 2000);
  }, TASKS_POLL_OFFSET_MS);
}
function stopTasksPolling() {
  if (state.tasks.timer) { clearInterval(state.tasks.timer); state.tasks.timer = null; }
}
