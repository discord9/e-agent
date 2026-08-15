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

/* 404 分类后处理（复用）：historical/unknown/live 三态的收尾行为。404 一律
   已停（state.sse.stopped=true，不重连）；调用方保证当前上下文仍是
   stillCurrent(id, wsId, epoch)——缓存直判路径同步调用，live 刷新重分类
   路径在 await 刷新后再校验一次。上下文已变（用户切走/开了别的会话）→
   什么都不做：不弹 banner、不动连接状态。
   - "historical"（active===false）：历史/已结束会话无实时流属预期 →
     静默 + 「会话已结束」轻提示（conn-state ended）
   - "unknown"（不在任何列表）：任务面板直连刚结束的子会话 → 完全静默
   - "live"（应存活却 404）：会话真不存在 → 弹「不存在」banner
   三种状态都不重连（stopped 已由 404 处理置位）。 */
function handleSse404Classified(known, id, wsId, epoch) {
  if (!stillCurrent(id, wsId, epoch)) return;
  if (known === "live") {
    setBanner("⚠ 会话不存在或已被删除。");
  } else if (known === "historical") {
    setConn("ended", "会话已结束");
  }
  // unknown：完全静默（无 banner、无连接状态提示）
}

/* 缓存判定为 live 的 404：刷新对应 workspace 的会话列表后再分类（防竞态）。
   场景：任务面板点击 subagent → openSession（列表刷新异步触发）→ subagent
   已在服务端结束而浏览器上一轮列表仍是 active:true → /events 404 先于刷新
   完成。刷新后：
   - active===false → 历史已结束：静默 + 轻提示（同 historical 路径）
   - 仍查不到 → unknown：静默
   - 仍 active → 真 live：才弹「不存在」banner
   刷新失败（HTTP/网络）→ pollWorkspaceSessions 保留旧列表（stale），分类
   按旧缓存走（live → banner），不吞真错误。
   await 完成后经 handleSse404Classified 的 stillCurrent 守卫：期间用户
   切走/开了别的会话 → 不弹、不动状态。 */
async function handleLive404Refresh(id, wsId, epoch) {
  const ws = (state.workspaces || []).find((w) => w.id === wsId);
  if (ws) await pollWorkspaceSessions(ws);
  handleSse404Classified(sessionKnownState(id, wsId), id, wsId, epoch);
}

function connectSSE(id, wsId, epoch) {
  // 起流前三重校验：陈旧 history 响应绝不能对刚激活的服务器/会话起 SSE。
  if (!stillCurrent(id, wsId, epoch)) return;
  stopSSE();
  state.sse.stopped = false;
  // 每次会话连接重置“（压缩前）”用量标注：标注是 per-session 状态，旧的
  // 压缩标注绝不能串到新会话（新会话的 snapshot/live 事件会重新推导）。
  state.usagePreCompaction = false;
  state.compactionUsagePending = false;
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
      // - 缓存判定为 live（active!==false）却 404：可能是会话真不存在，
      //   也可能是缓存过期——任务面板点击 subagent 时 openSession 先于
      //   列表刷新（异步触发），subagent 已在服务端结束（live 注册被清理）
      //   而浏览器上一轮列表仍是 active:true，/events 404 先于刷新完成。
      //   处理：先停重连（404 一律停），live 时刷新对应 workspace 的会话
      //   列表后重分类——刷新后 active===false → 静默 + 「会话已结束」轻
      //   提示（同 historical 路径）；仍查不到 → 静默；仍 active → 才弹
      //   原「不存在」banner。
      // 先校验上下文：陈旧请求的 404 不得弹提示、不得停新会话的流。
      if (!stillCurrent(id, wsId, epoch)) { try { ctrl.abort(); } catch (e) { /* 忽略 */ } return; }
      const known = sessionKnownState(id, wsId);
      // 深链 attempt（?session=<id> 直接 probe history 打开）专属恢复：
      // - history 已成功（transcript 可读）+ 分类 historical/unknown
      //   （列表知其为历史，或列表还没有它——列表缺失不是权威不存在）
      //   → 会话存在但无 live runner：resumeSession 建回活跃会话，
      //     成功后复用现有 openSession（重连 SSE）。恢复只做一次
      //     （resume 前清标记，防止恢复后重建的流再 404 造成循环）。
      // - history 失败（网络/超时）→ history 与 SSE 都失败：持久提示 +
      //   不置 stopped → 走 scheduleReconnect 自动重试，网络恢复后
      //   history 成功 → 再次 404 → 走上面的恢复分支。
      // - history 成功但分类 live → 清标记，落到既有 live 刷新重分类
      //   （真 live 才报「不存在」）。
      // 任务面板/普通路径（无 attempt 标记）保持既有三态分类不变。
      if (state.deepLink.probing && state.deepLink.attemptEpoch === epoch) {
        const historyOk = (state.initSource === "history" || state.initSource === "restored");
        if (historyOk && (known === "historical" || known === "unknown")) {
          state.deepLink.probing = false;
          state.deepLink.attemptEpoch = -1;
          // 恢复后的 openSession → history 同样带深链有界超时（显式透传；
          // 标记已清防恢复循环，不能靠标记兜底）
          resumeSession(wsId, id, epoch, DEEP_LINK_HISTORY_TIMEOUT_MS);
          throw new Error("silent-gone");
        }
        if (!historyOk) {
          setBanner("⚠ 无法读取会话历史，且该会话暂无实时流；将自动重试。", true);
          throw new Error("deep-link-retry");
        }
        state.deepLink.probing = false;
        state.deepLink.attemptEpoch = -1;
      }
      state.sse.stopped = true;                         // 404 = 无流：一律停，不重连
      if (known === "live") {
        // 缓存可能过期（任务面板直连刚结束的 subagent）：异步刷新列表后
        // 在 stillCurrent 守卫下重分类；期间用户切走/开了别的会话 → 不弹、
        // 不动状态（守卫在 handleSse404Classified 内）。
        handleLive404Refresh(id, wsId, epoch);
      } else {
        // 历史/未知：缓存判定已足够，同步分类即可（无需刷新）
        handleSse404Classified(known, id, wsId, epoch);
      }
      throw new Error("silent-gone");
    }
    if (!res.ok || !res.body) throw new Error("HTTP " + res.status);
    // 响应回来时上下文可能已被取代（新打开/切换）：不起流、不画连接状态
    if (!stillCurrent(id, wsId, epoch)) { try { ctrl.abort(); } catch (e) { /* 忽略 */ } return; }
    setConn("ok", "● 已连接");
    // 深链 attempt 已完成（live 流建立）：清标记
    state.deepLink.probing = false;
    state.deepLink.attemptEpoch = -1;
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

/* 从初始 snapshot 事件数组恢复 current usage：取最后一个 Usage 事件（最近
   一次正常模型请求；compaction 不刷新它）交给 applyUsage——与 live 路径共用
   state.lastUsage + renderUsageLine，不引入第二套状态。同时按事件顺序推导
   “（压缩前）”标注：压缩成功 Notice（"compacted: …"）之后的 Usage 是压缩
   自身发出的旧基线（runner 的 compact_operation 先 emit 投影再 apply_usage），
   标注保留；其后的普通轮 Usage 才清除标注。 */
function restoreUsageFromSnapshot(events) {
  if (!Array.isArray(events)) return;
  let pending = false;        // 压缩成功 Notice 之后、压缩自身旧基线 Usage 未消费
  let preCompaction = false;  // 最近一次 Usage 是否为压缩前基线（默认：普通轮的）
  let lastUsage;
  for (const ev of events) {
    if (!ev) continue;
    if (ev.type === "notice") {
      const text = ev.data && (ev.data.text || ev.data.message);
      if (typeof text === "string" && text.startsWith("compacted: ")) {
        preCompaction = true;   // 若其后无 Usage，显示的旧值同样属于压缩前
        pending = true;
      }
    } else if (ev.type === "usage" && ev.data !== undefined) {
      if (pending) {
        pending = false;        // 压缩自身的旧基线 Usage：标注保留
      } else {
        preCompaction = false;  // 普通轮 fresh Usage：清除标注
      }
      lastUsage = ev.data;
    }
  }
  if (lastUsage !== undefined) {
    state.usagePreCompaction = preCompaction;
    state.compactionUsagePending = false;
    applyUsage(lastUsage);
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
    let entries = null;
    try {
      const parsed = JSON.parse(data);
      entries = Array.isArray(parsed) ? parsed : (parsed.entries || []);
    } catch (e) { /* 忽略坏数据 */ }
    if (entries) {
      // 初始 snapshot 含 Usage 事件时立即恢复 current usage（最近一次正常
      // 模型请求的 context_input/context_window），不等下一次模型调用；
      // 复用 applyUsage 路径（state.lastUsage + renderUsageLine）。
      restoreUsageFromSnapshot(entries);
      // 已用 history 渲染过则跳过（避免重复）；恢复的会话（initSource="restored"，
      // 视图来自缓存）也跳过——缓存内容与 snapshot 等价，重放会造成重复；
      // history 加载失败时仍作为兜底
      if (state.initSource !== "history" && state.initSource !== "restored") {
        renderHistory(entries);
        state.initSource = "snapshot";
      }
      // GoalBar：snapshot 里最新的 goal_updated（set 或 clear 墓碑）折叠
      // 出来刷新 GoalBar——history 失败走 snapshot 兜底时 GET /goal 可能
      // 还没返回，这里保证 GoalBar 不依赖那次 GET；幂等，无 stale 风险
      //（handleSSEBlock 顶部已校验三元组）。
      let snapshotGoal = undefined;   // undefined = snapshot 无 goal_updated
      for (const ev of entries) {
        if (ev && ev.type === "goal_updated" && ev.data && "goal" in ev.data) {
          snapshotGoal = ev.data.goal;   // null = cleared
        }
      }
      if (snapshotGoal !== undefined) renderGoalBar(snapshotGoal);
    }
    return;
  }
  if (eventName === "status") {
    try { applyStatus(JSON.parse(data).status); } catch (e) { /* 忽略 */ }
    return;
  }
  if (id !== state.sessionId) return;  // 已切换会话
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
      goal_updated: "GoalUpdated",   // set 与 clear（goal:null）都刷新 GoalBar
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
      // innerHTML 会在 real 下创建全新的节点；重放时 acc 绑定的是离屏 temp
      // 的旧节点，不能让后续 delta 继续写入孤儿 DOM。
      state.acc = newAccumulator();
      state.initSource = "snapshot";
    } catch (e) {
      els.messages = real;
      real.innerHTML = backup;
      // 回滚同样通过 innerHTML 重建节点，不能保留离屏重放期间的引用。
      state.acc = newAccumulator();
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
    case "Notice": {
      const text = pickText(payload, ["text", "message"]);
      appendNotice(text);
      // 压缩成功投影（"compacted: …"）之后紧跟着一条携带压缩前基线的 Usage
      // （runner 的 compact_operation 先 emit 投影 Notice，再 apply_usage 旧值）。
      // 置“（压缩前）”标注并挂起下一次 Usage 的清除动作——那条正是压缩自己
      // 发出的旧基线，不是普通轮的新值。
      if (typeof text === "string" && text.startsWith("compacted: ")) {
        state.usagePreCompaction = true;
        state.compactionUsagePending = true;
      }
      break;
    }
    case "Error":
      appendError(pickText(payload, ["error", "message", "text"]));
      break;
    case "BackgroundCompleted":
    case "BackgroundCompletionNotice": {
      const p = (payload && typeof payload === "object") ? payload : {};
      // delegate 后台完成（output 以 "subagent session: " 开头）走专门渲染，
      // 其余（bash 等）保持 appendNoticeLong 路径
      appendBackgroundCompletion(p.id ?? "?", p.label,
        pickText(p, ["output", "text", "content"]) || "");
      break;
    }
    case "Usage":
      if (state.compactionUsagePending) {
        // 压缩成功路径发出的旧基线 Usage：消费挂起标记，但不清除“（压缩前）”
        // 标注——这一条展示的正是压缩前的旧值。
        state.compactionUsagePending = false;
      } else {
        // 普通模型轮的 fresh Usage：清除压缩前标注。
        state.usagePreCompaction = false;
      }
      applyUsage(payload);
      break;
    case "PromptQueued":
      // 排队提示进 queueBar（输入区上方固定条），不再 appendNotice 到 messages
      queuePromptQueued(pickText(payload, ["text", "prompt", "content"]));
      break;
    case "PromptConsumed":
      queuePromptConsumed();
      break;
    case "GoalUpdated": {
      // goal 快照/墓碑：刷新 GoalBar + 一行 notice（与历史 goal_updated
      // 渲染一致；不当作普通用户消息）。
      const p = (payload && typeof payload === "object") ? payload : {};
      const goal = p.goal || null;
      renderGoalBar(goal);
      appendNotice(goal
        ? "goal [" + (goal.status || "?") + "] " + (goal.objective || "")
        : "goal cleared");
      break;
    }
    default:
      // 后端未知事件类型：不渲染、不崩，只留 console 警告（诊断辅助）
      console.warn("[SSE] 未知事件类型，已跳过：", name, payload);
  }
}

/* 断线重连：3 秒后重新加载 history + SSE。必须携带断线流的三元组
   (id, wsId, epoch)——调度时与触发时都校验「仍是同一上下文」：期间任何
   打开会话/切换 workspace 都会取代代次，陈旧流的重连绝不执行（否则会把已过期
   的会话重新加载/重画到新激活的上下文上）。 */
function scheduleReconnect(id, wsId, epoch) {
  if (state.sse.stopped || !stillCurrent(id, wsId, epoch)) return;
  setConn("retrying", "↻ 连接断开，3 秒后重连…");
  state.sse.retryTimer = setTimeout(() => {
    if (state.sse.stopped || !stillCurrent(id, wsId, epoch)) return;
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
  // 草稿实时同步进会话缓存（per-ws 键）：openSession 之外的路径
  // （scheduleReconnect/restartTransport 的 openWith→loadHistory）不经过
  // saveSessionState，这里兜底保证缓存里的 draft 与输入框一致。条目只在
  // 切走时由 saveSessionState 创建；当前会话尚无条目（从未切走过）时跳过
  // ——输入框本身就是草稿源，无需复制（也不制造残缺缓存条目）。
  if (state.sessionId) {
    const st = state.sessionStates[state.workspace.id + ":" + state.sessionId];
    if (st) st.draft = els.promptInput.value;
  }
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
  if (t.classList.contains("copy-toggle")) {
    // 「复制全文」：_srcText 存未截断原文（live 元素）；innerHTML 快照往返后
    // expando 丢失 → 回退到 _target / closest(.expandable) 里的 .expand-full
    // （全文常驻 DOM 且随快照序列化，与展开按钮同一兜底机制）。
    // 短文本不生成 copy-toggle，能走到这里的一定是超阈值长文本。
    const c = t._target || t.closest(".expandable")
      || (t.closest(".tool-card") || t.closest(".notice") || {}).querySelector?.(".expandable");
    const text = t._srcText != null ? t._srcText
      : (c ? (c.querySelector(".expand-full") || {}).textContent : "");
    if (typeof text === "string" && text !== "") copyTextToClipboard(text, t);
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
  els.chatView.classList.remove("hidden");
  els.chatView.classList.toggle("no-session", !state.sessionId);
  els.topActions.hidden = false;
  // URL 深链：?session=<id>。token 就绪后立即尝试（与轮询并行），不再等
  // 列表轮询整轮——列表缺失/失败/超时/过期时直接按 id probe history 打开；
  // token 为空时先记录，用户填 token 触发 restartTransport 再处理。
  const dl = new URLSearchParams(location.search).get("session");
  if (dl) state.deepLink.pending = dl;
  startPolling();
  pollSessions();
  maybeHandleDeepLink();
  // 侧边栏跨刷新恢复：上次开着就继续开（切会话不关，仅手动关）
  try {
    if (localStorage.getItem("e-agent.sidebar.open") === "1") openSidebar();
  } catch (e) { /* 静默 */ }
  // 运行中任务：统一轮询（2s 常驻）同时更新侧边栏树分组 + composer
  // 折叠条/面板（无 token 时 fetchTasks 静默跳过；填 token 后下一轮生效）
  startTasksPolling();
  pollTasks();
  // 页面隐藏（切标签页/最小化）时暂停两条 2s 轮询（会话 + 任务）：后台
  // 标签页无需拉取，避免 5 workspace × 2 条轮询持续打服务器；重新可见时
  // 立即补一轮再重启定时器（序列与 init/switchWorkspace 的启动一致）。
  // 只挂一次：init 是唯一启动入口。
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) {
      stopPolling();
      stopTasksPolling();
    } else {
      startPolling();
      pollSessions();
      startTasksPolling();
      pollTasks();
    }
  });
}

init();
