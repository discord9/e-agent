/* =============================================================================
 * sse.js — SSE：connectSSE/readSSEStream/handleSSEBlock/applyLiveEvent/
 * scheduleReconnect/stopSSE/setConn；并在文件尾部承载「事件绑定与启动」
 * 区块（软键盘适配、init()）。该区块必须在整个拼接的最后执行：此时所有
 * 文件的顶层声明（含 const/let）均已就绪。
 * 依赖 app.js + render.js + sessions.js + tasks.js。
 * =============================================================================*/

/* =====================================================================
 * SSE：fetch + ReadableStream（EventSource 无法带 header）
 * ===================================================================*/
function setConn(stateName, text) {
  els.connState.className = "conn-state " + stateName;
  els.connState.textContent = text;
}

/* 三重校验：发起打开时的上下文（同代次、同 workspace、同会话）是否仍成立。
   连接启动、每条流分块消费、重连调度与触发、以及 handleSSEBlock 每个事件块
   处理前都调用它——陈旧流（来自已被取代的会话/workspace/代次）的任意内容
   （snapshot/status/resync/live 事件）一律不得碰 UI。 */
function stillCurrent(id, wsId, epoch) {
  return epoch === sessionOpenEpoch && state.workspace.id === wsId && state.sessionId === id;
}

/* 会话已知状态：区分 SSE 404 的两种含义（历史无流 vs 真不存在）。
   /api/sessions/<id>/events 只服务 live 会话；历史/已结束会话保持 404
   （server 端设计：streaming needs a live runner）。但历史 transcript
   可读——404 不等于「会话不存在」。在 wsId 对应 workspace 的列表
   （state.workspaceLists[wsId]）与激活列表（state.lastList，单服务器模式
   的唯一数据源）中查找 id：
   - 找到且 active===false → "historical"：历史/已结束，无实时流属预期
   - 找到且 active!==false（true 或缺省=旧 server 视为活跃）→ "live"：
     应存活，404 才意味着会话真不存在
   - 两个列表都没有 → "unknown"：如任务面板直连刚结束的子会话（列表还
     没刷新到它）——保守处理：404 静默，不弹「不存在」 */
function sessionKnownState(id, wsId) {
  const lists = [];
  if (state.workspaceLists && Array.isArray(state.workspaceLists[wsId])) {
    lists.push(state.workspaceLists[wsId]);
  }
  if (Array.isArray(state.lastList)) lists.push(state.lastList);
  for (const list of lists) {
    const s = list.find((x) => x && x.id === id);
    if (s) return s.active === false ? "historical" : "live";
  }
  return "unknown";
}

function connectSSE(id, wsId, epoch) {
  // 起流前三重校验：陈旧 history 响应绝不能对刚激活的服务器/会话起 SSE。
  if (!stillCurrent(id, wsId, epoch)) return;
  stopSSE();
  state.sse.stopped = false;
  state.sse.ctrl = new AbortController();
  const ctrl = state.sse.ctrl;

  fetch(fullUrl("/api/sessions/" + encodeURIComponent(id) + "/events"), {
    headers: {
      "Authorization": "Bearer " + state.token,
      "Accept": "text/event-stream",
    },
    signal: ctrl.signal,
  }).then((res) => {
    if (res.status === 401 || res.status === 403) {
      setBanner("⚠ 认证失败：请检查 Token。");
      throw new Error("auth");
    }
    if (res.status === 404) {
      // 404 的两种含义，按会话已知状态区分（判定见 sessionKnownState）：
      // - 已知历史/已结束（active===false）或不在任何列表（任务面板直连
      //   刚结束的子会话）：SSE 端点只服务 live 会话，404 = 没有实时流，
      //   会话本身存在（history 刚加载成功，transcript 可读）。静默降级：
      //   不弹「不存在」、不重连（重连只会再次 404）；历史会话给轻量提示。
      // - 已知应存活（active!==false）却 404：会话真的不存在，保持报错。
      // 先校验上下文：陈旧请求的 404 不得弹提示、不得停新会话的流。
      if (!stillCurrent(id, wsId, epoch)) { try { ctrl.abort(); } catch (e) { /* 忽略 */ } return; }
      const known = sessionKnownState(id, wsId);
      if (known !== "live") {
        state.sse.stopped = true;                       // 无流可连：停，不重连
        if (known === "historical") setConn("ended", "会话已结束");
        throw new Error("silent-gone");
      }
      setBanner("⚠ 会话不存在或已被删除。");
      state.sse.stopped = true;                         // 404 = 无流：重连也 404
      throw new Error("gone");
    }
    if (!res.ok || !res.body) throw new Error("HTTP " + res.status);
    // 响应回来时上下文可能已被取代（新打开/切换）：不起流、不画连接状态
    if (!stillCurrent(id, wsId, epoch)) { try { ctrl.abort(); } catch (e) { /* 忽略 */ } return; }
    setConn("ok", "● 已连接");
    return readSSEStream(res.body.getReader(), id, wsId, epoch, ctrl);
  }).then(() => {
    // 正常结束（后端关闭流）→ 按断线处理
    throw new Error("stream end");
  }).catch((err) => {
    if (err && err.name === "AbortError") return;   // 主动停止
    if (state.sse.stopped) return;
    if (err && (err.message === "auth" || err.message === "gone" || err.message === "silent-gone")) {
      state.sse.stopped = true;   // 认证失败 / 会话不存在 / 历史无流：都不重连（404 重连也 404）
      return;
    }
    scheduleReconnect(id, wsId, epoch);   // 携带断线流的三元组：重连前必须仍是同一上下文
  });
}

/* 逐块读取 SSE：按空行切分事件块（兼容 \r\n 行尾）。
   每次 read 前后都做三重校验：停止/切换后迟到的流立即早退（abort 的是
   本流自己的 ctrl——绝不能动 state.sse.ctrl，那可能已是新流的控制器）。 */
async function readSSEStream(reader, id, wsId, epoch, ctrl) {
  const decoder = new TextDecoder();
  let buf = "";
  for (;;) {
    if (!stillCurrent(id, wsId, epoch)) { try { ctrl && ctrl.abort(); } catch (e) { /* 忽略 */ } return; }
    const { done, value } = await reader.read();
    if (done) break;
    if (!stillCurrent(id, wsId, epoch)) { try { ctrl && ctrl.abort(); } catch (e) { /* 忽略 */ } return; }
    buf += decoder.decode(value, { stream: true }).replace(/\r\n/g, "\n");
    let idx;
    while ((idx = buf.indexOf("\n\n")) !== -1) {
      const block = buf.slice(0, idx);
      buf = buf.slice(idx + 2);
      handleSSEBlock(block, id, wsId, epoch);
    }
  }
}

/* 解析单个 SSE 事件块：任何分支（snapshot/status/resync/live）动手改 UI 前
   必须通过三重校验——陈旧流的块整块丢弃，绝不画进当前会话/workspace。 */
function handleSSEBlock(block, id, wsId, epoch) {
  if (!stillCurrent(id, wsId, epoch)) return;
  let eventName = "message";
  const dataLines = [];
  for (const line of block.split("\n")) {
    if (line.startsWith(":")) continue;               // 心跳/注释行
    if (line.startsWith("event:")) eventName = line.slice(6).trim();
    else if (line.startsWith("data:")) dataLines.push(line.slice(5).replace(/^ /, ""));
  }
  if (!dataLines.length) return;
  const data = dataLines.join("\n");

  if (eventName === "snapshot") {
    // 已用 history 渲染过则跳过（避免重复）；恢复的会话（initSource="restored"，
    // 视图来自缓存）也跳过——缓存内容与 snapshot 等价，重放会造成重复；
    // history 加载失败时仍作为兜底
    if (state.initSource !== "history" && state.initSource !== "restored") {
      try {
        const parsed = JSON.parse(data);
        const entries = Array.isArray(parsed) ? parsed : (parsed.entries || []);
        renderHistory(entries);
        state.initSource = "snapshot";
      } catch (e) { /* 忽略坏数据 */ }
    }
    return;
  }
  if (eventName === "status") {
    try { applyStatus(JSON.parse(data).status); } catch (e) { /* 忽略 */ }
    return;
  }
  if (id !== state.sessionId || state.view !== "chat") return;  // 已切换会话
  if (eventName === "resync") {
    // Lag 追平：后端重发完整事件日志（AgentEvent 数组，{type,data} 形状）。
    // 与 snapshot 不同，无论初始渲染来源都强制整体替换 transcript。
    // 渲染到离屏容器，成功才一次性替换；失败回滚旧内容。避免「先清空再
    // 重放」在手机上（可上千条事件）造成消息区空白、像消息消失一样。
    const real = els.messages;
    const backup = real.innerHTML;
    const temp = real.cloneNode(false);   // 同 class/id，无子节点
    temp.innerHTML = "";
    els.messages = temp;
    state.acc = newAccumulator();
    // 排队提示是「当下」状态，重放的是过去事件：清空 queueBar 并跳过重放
    state.queue.length = 0;
    renderQueueBar();
    const NAME = {
      prompt_queued: "PromptQueued", prompt_consumed: "PromptConsumed",
      user_prompt: "UserPrompt", assistant_text: "AssistantText",
      assistant_delta: "AssistantDelta", reasoning_delta: "ReasoningDelta",
      tool_call: "ToolCall", tool_result: "ToolResult",
      notice: "Notice", error: "Error",
      background_completed: "BackgroundCompleted",
      background_completion_notice: "BackgroundCompletionNotice",
      usage: "Usage",
    };
    try {
      const parsed = JSON.parse(data);
      const events = Array.isArray(parsed) ? parsed : (parsed.events || []);
      for (const ev of events) {
        const name = (ev && NAME[ev.type]) || "Notice";
        // 已过去的排队事件：不重放（它们不该出现在 queueBar；已在上方清空）
        if (name === "PromptQueued" || name === "PromptConsumed") continue;
        const payload = (ev && ev.data !== undefined) ? ev.data : (ev || {});
        applyLiveEvent(name, payload);
      }
      real.innerHTML = temp.innerHTML;
      els.messages = real;
      state.initSource = "snapshot";
    } catch (e) {
      els.messages = real;
      real.innerHTML = backup;
      appendNotice("⚠ 会话同步失败，已保留原内容");
    }
    return;
  }
  if (state.initSource === null) return;   // 初始渲染未完成前的 live 事件丢弃

  let payload = null;
  try { payload = JSON.parse(data); } catch (e) { payload = data; }
  applyLiveEvent(eventName, payload);
}

/* live AgentEvent → 增量渲染 */
function applyLiveEvent(name, payload) {
  const acc = state.acc;
  switch (name) {
    case "UserPrompt":
      appendUserMsg(pickText(payload, ["text", "prompt", "content"]));
      break;
    case "AssistantText":
      setAssistantText(pickText(payload, ["text", "content"]), acc);
      break;
    case "AssistantDelta":
      appendAssistantDelta(pickText(payload, ["delta", "text", "content"]), acc);
      break;
    case "ReasoningDelta":
      appendReasoningDelta(pickText(payload, ["delta", "text", "reasoning"]), acc);
      break;
    case "ToolCall": {
      const p = (payload && typeof payload === "object") ? payload : {};
      let args = p.arguments;
      if (args && typeof args === "object") args = JSON.stringify(args);
      else args = pickText(p, ["arguments", "args", "params"]);
      appendToolCall(pickText(p, ["name"]), args, acc, p.call_id);
      break;
    }
    case "ToolResult": {
      const p = (payload && typeof payload === "object") ? payload : {};
      const isErr = p.is_error === true || p.error === true;
      let content = p.content;
      if (content && typeof content === "object") content = JSON.stringify(content);
      else content = pickText(p, ["content", "text", "result", "error"]);
      appendToolResult(isErr, content, acc, p.call_id);
      break;
    }
    case "Notice":
      appendNotice(pickText(payload, ["text", "message"]));
      break;
    case "Error":
      appendError(pickText(payload, ["error", "message", "text"]));
      break;
    case "BackgroundCompleted":
    case "BackgroundCompletionNotice": {
      const p = (payload && typeof payload === "object") ? payload : {};
      const label = p.label ? "（" + p.label + "）" : "";
      appendNoticeLong("⌛ 后台任务 #" + (p.id ?? "?") + " 完成" + label + "\n",
        pickText(p, ["output", "text", "content"]) || "");
      break;
    }
    case "Usage":
      applyUsage(payload);
      break;
    case "PromptQueued":
      // 排队提示进 queueBar（输入区上方固定条），不再 appendNotice 到 messages
      queuePromptQueued(pickText(payload, ["text", "prompt", "content"]));
      break;
    case "PromptConsumed":
      queuePromptConsumed();
      break;
    default:
      // 后端未知事件类型：不渲染、不崩，只留 console 警告（诊断辅助）
      console.warn("[SSE] 未知事件类型，已跳过：", name, payload);
  }
}

/* 断线重连：3 秒后重新加载 history + SSE。必须携带断线流的三元组
   (id, wsId, epoch)——调度时与触发时都校验「仍是同一上下文」：期间任何
   打开/切换/返回列表都会取代代次，陈旧流的重连绝不执行（否则会把已过期
   的会话重新加载/重画到新激活的上下文上）。 */
function scheduleReconnect(id, wsId, epoch) {
  if (state.sse.stopped || !stillCurrent(id, wsId, epoch)) return;
  setConn("retrying", "↻ 连接断开，3 秒后重连…");
  state.sse.retryTimer = setTimeout(() => {
    if (state.sse.stopped || state.view !== "chat" || !stillCurrent(id, wsId, epoch)) return;
    state.initSource = null;             // 允许 snapshot 兜底
    openWith(id, true, undefined, wsId, epoch);
  }, 3000);
}

function stopSSE() {
  state.sse.stopped = true;
  if (state.sse.ctrl) { try { state.sse.ctrl.abort(); } catch (e) { /* 忽略 */ } }
  if (state.sse.retryTimer) { clearTimeout(state.sse.retryTimer); state.sse.retryTimer = null; }
  state.sse.ctrl = null;
  setConn("", "");
}
/* =====================================================================
 * 事件绑定与启动
 * ===================================================================*/
els.newSessionBtn.addEventListener("click", createSession);
els.newPrompt.addEventListener("keydown", (e) => {
  if ((e.ctrlKey || e.metaKey) && e.key === "Enter") { e.preventDefault(); createSession(); }
});
els.searchInput.addEventListener("input", () => {
  state.searchQuery = els.searchInput.value.trim().toLowerCase();
  renderSessionList(state.lastList);   // 轮询仍会全量重绘，但搜索词留在 state 里，过滤持续生效
});
els.showArchiveBtn.addEventListener("click", () => {
  state.showArchived = !state.showArchived;
  renderSessionList();                 // 只重绘列表；侧边栏归档分组独立于该开关
});
els.backBtn.addEventListener("click", backToList);
// 「← 主会话」：从 subagent 会话快速返回其父会话（无父则不显示按钮）
els.backParentBtn.addEventListener("click", () => {
  const cur = (state.lastList || []).find((s) => s.id === state.sessionId);
  if (cur && cur.parent_session_id) openSession(cur.parent_session_id);
});
els.sidebarBtn.addEventListener("click", () => {
  if (state.sidebar.open) closeSidebar();
  else openSidebar();
});
els.sidebarCloseBtn.addEventListener("click", closeSidebar);
els.sidebarOverlay.addEventListener("click", closeSidebar);   // 点遮罩关闭
els.sidebarFilter.addEventListener("input", () => {
  state.sidebar.filter = els.sidebarFilter.value.trim().toLowerCase();
  state.sidebar.showAllWs = new Set();   // 清空筛选后回到默认条数限制（每 workspace 独立）
  renderSidebarTree(true);
});
/* composer 任务折叠条：点击展开 / 收起面板（有任务时才可见） */
if (els.tasksToggleBar) {
  els.tasksToggleBar.addEventListener("click", () => {
    state.tasks.composerOpen = !state.tasks.composerOpen;
    renderComposerTasks();
  });
}
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && state.sidebar.open) closeSidebar();
  if (e.key === "Escape" && tokenBoxOpen) setTokenBoxOpen(false);   // Esc 收起 token 输入框
});
els.sendBtn.addEventListener("click", sendPrompt);
els.cancelBtn.addEventListener("click", cancelTurn);
els.compactBtn.addEventListener("click", compactSession);
els.promptInput.addEventListener("keydown", (e) => {
  // fork 面板开着时优先处理：↑↓ 移动选中、Enter/Tab 选中、Esc 关闭
  // （fork 面板与 slash 菜单不会同时开——input 事件在 fork 面板开着时
  //  跳过 updateSlashMenu——但顺序上仍先判 forkMenu.open）。
  if (forkMenu.open) {
    if (e.key === "ArrowDown") { e.preventDefault(); moveForkMenu(1); return; }
    if (e.key === "ArrowUp") { e.preventDefault(); moveForkMenu(-1); return; }
    if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); selectForkItem(forkMenu.selected); return; }
    if (e.key === "Tab") { e.preventDefault(); selectForkItem(forkMenu.selected); return; }
    if (e.key === "Escape") { e.preventDefault(); closeForkMenu(); return; }
  }
  // 斜杠菜单开着时：↑↓ 移动选中、Esc 关闭、Tab 填入；Enter 落到下方分支
  // （菜单开着 = 选中当前项填入，菜单关着 = 正常发送）。
  if (slashMenu.open) {
    if (e.key === "ArrowDown") { e.preventDefault(); moveSlashMenu(1); return; }
    if (e.key === "ArrowUp") { e.preventDefault(); moveSlashMenu(-1); return; }
    if (e.key === "Escape") { e.preventDefault(); closeSlashMenu(); return; }
    if (e.key === "Tab") { e.preventDefault(); acceptSlashMenu(); return; }
  }
  if (e.key === "Enter" && !e.shiftKey) {
    e.preventDefault();
    if (slashMenu.open) acceptSlashMenu();
    else sendPrompt();
  }
});
els.promptInput.addEventListener("input", () => {
  autosizeInput();
  if (forkMenu.open) return;   // fork 面板开着：不弹 slash 菜单，避免两面板重叠
  updateSlashMenu();
});
els.promptInput.addEventListener("blur", () => { closeSlashMenu(); closeForkMenu(); });   // 失焦关闭菜单/面板
els.messages.addEventListener("scroll", (ev) => {
  // 只有用户主动滚动才处理；程序滚动（scrollTop 赋值）不碰 userScrolled。
  if (!ev.isTrusted) return;
  const m = els.messages;
  // 精确到底（不是 80px 模糊带）：用户拖到最底部才解锁自动跟随。
  // 只要没到底部，任何用户滚动都立即锁定——避免慢慢上滑时在底部
  // 模糊带内被反复拉回（用户报告的「慢拖弹回、快拖能滚」）。
  const atBottom = m.scrollHeight - m.scrollTop - m.clientHeight <= 4;
  userScrolled = !atBottom;
  // 离开底部时显示「回到底部」按钮；在底部则隐藏
  els.jumpBottomBtn.hidden = atBottom;
  // 滚到接近顶部时加载更早历史（分页；防重入 / 已全部加载 / 未开会话时跳过）
  if (state.loadingOlder || state.olderDone || !state.sessionId) return;
  if (m.scrollTop < 30) loadOlder();
});
/* 长文本「展开全文/收起」+ 占位「加载更早历史」：事件委托在消息容器上。
   委托原因：innerHTML 快照恢复（缓存会话 / resync 离屏替换）会重建按钮/
   链接元素，直接绑定的监听器不保留；容器本身不换，委托不受影响。
   older-load 在 summary 内：preventDefault 阻止 details 的默认开合。 */
els.messages.addEventListener("click", (ev) => {
  const t = ev.target;
  if (!t || !t.classList) return;
  if (t.classList.contains("expand-toggle")) {
    // 按钮在卡片/notice 底部的 footer（与正文分离）；_target 是它控制的
    // 正文 pre（卡片可能有多个 .expandable），回退到就近查找（旧布局）
    const c = t._target || t.closest(".expandable")
      || (t.closest(".tool-card") || t.closest(".notice") || {}).querySelector?.(".expandable");
    if (!c) return;
    const expanded = c.classList.toggle("expanded");
    const label = t.textContent.includes("（") ? t.textContent.slice(t.textContent.indexOf("（")) : "";
    t.textContent = expanded ? "收起" + label : "展开全文" + label;
    return;
  }
  if (t.classList.contains("older-load")) {
    ev.preventDefault();
    ev.stopPropagation();
    loadOlder();
  }
});
els.jumpBottomBtn.addEventListener("click", () => {
  userScrolled = false;   // 显式回底：覆盖「用户在看历史」的锁定
  scrollBottom(true);
  els.jumpBottomBtn.hidden = true;
});

/* =====================================================================
 * 软键盘适配：visualViewport 高度 → CSS 变量 --app-height
 * ===================================================================*/
/* 移动端软键盘弹出时 100dvh 不收缩（键盘是覆盖式：visualViewport 变小但
   布局视口高度不变）→ 底部 composer 被键盘盖住、顶栏/侧边栏被挤出可视区。
   标准解法：监听 visualViewport 的 resize（键盘弹出/收起触发），动态把
   visualViewport.height 写入 --app-height，布局用它替代 100dvh（style.css
   html/body 高度）。桌面 visualViewport.height === innerHeight，设的值等于
   原高度，无视觉变化。 */
let appHeightRaf = null;
function syncAppHeight() {
  if (appHeightRaf !== null) return;        // 节流：合并同帧内连续 resize
  appHeightRaf = requestAnimationFrame(() => {
    appHeightRaf = null;
    const vv = window.visualViewport;
    const h = (vv && vv.height) ? vv.height : window.innerHeight;
    document.documentElement.style.setProperty("--app-height", h + "px");
    // 兜底：布局收缩后 composer 应在可视区底部（body flex column 内）。
    // 若个别浏览器仍有偏移，把输入框滚进可视区；正常收缩时是 no-op
    // （block:nearest 只滚最近的可滚祖先，不打断用户滚动位置）。
    const inp = els.promptInput;
    if (inp && inp.offsetParent !== null) inp.scrollIntoView({ block: "nearest" });
  });
}

function init() {
  // 挂到 window：pet.html 是独立 <script>，顶层 const state 对它不可见；
  // 桌宠点击时从这里读当前 sessionId / token / 运行中任务。
  window.state = state;
  refreshBanner();
  // 软键盘适配：初始按当前 visualViewport 高度设置 --app-height；并绑定
  // visualViewport.resize（键盘弹出/收起触发，iOS Safari 关键路径）+
  // window.resize（无 visualViewport 的浏览器兜底）。
  syncAppHeight();
  const vv = window.visualViewport;
  if (vv && vv.addEventListener) vv.addEventListener("resize", syncAppHeight);
  window.addEventListener("resize", syncAppHeight);
  els.chatView.classList.add("hidden");
  els.topActions.hidden = true;
  // URL 深链：?session=<id>。只在列表拿到数据（pollSessions）且 token 就绪后
  // 打开一次；token 为空时先记录，用户填 token 触发 restartTransport 再处理。
  const dl = new URLSearchParams(location.search).get("session");
  if (dl) state.deepLink.pending = dl;
  startPolling();
  pollSessions();
  // 侧边栏跨刷新恢复：上次开着就继续开（切会话/返回列表都不关，仅手动关）
  try {
    if (localStorage.getItem("e-agent.sidebar.open") === "1") openSidebar();
  } catch (e) { /* 静默 */ }
  // 运行中任务：统一轮询（2s 常驻）同时更新侧边栏树分组 + composer
  // 折叠条/面板（无 token 时 fetchTasks 静默跳过；填 token 后下一轮生效）
  startTasksPolling();
  pollTasks();
}

init();
