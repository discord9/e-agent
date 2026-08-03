/* =============================================================================
 * sessions.js — 会话字段校验与轮询（pollSessions）、对话视图流程
 * （loadHistory/loadOlder/openWith/openSession、
 * saveSessionState 缓存、发送/取消/压缩）、侧边栏会话树（renderSidebarTree
 * 及树节点/分组/重命名/置顶）、resolveSubagentSessionId。
 * 依赖 app.js + render.js；被 tasks.js（resolveSubagentSessionId）、
 * sse.js（handleSSEBlock 内的会话流程）调用。
 * =============================================================================*/

/* =====================================================================
 * API 响应字段校验（诊断辅助，非严格校验）
 * /api/sessions 字段全靠前端约定：加/漏字段容易出 bug（历史上出过
 * title 漏传、active 判定问题）。这里只做轻量类型检查：发现问题在
 * banner 黄条提示，不阻断渲染、不打断轮询。
 * ===================================================================*/
function validateSession(s) {
  const problems = [];
  if (!s || typeof s !== "object") return ["条目不是对象"];
  if (typeof s.id !== "string") problems.push("id 缺失或非字符串");
  if (typeof s.status !== "string") problems.push("status 缺失或非字符串");
  if (s.entry_count !== undefined && typeof s.entry_count !== "number") problems.push("entry_count 非数字");
  if (s.busy !== undefined && typeof s.busy !== "boolean") problems.push("busy 非布尔");
  if (s.active !== undefined && typeof s.active !== "boolean") problems.push("active 非布尔");
  if (s.parent_session_id !== undefined && s.parent_session_id !== null && typeof s.parent_session_id !== "string") problems.push("parent_session_id 非字符串");
  if (s.model !== undefined && s.model !== null && typeof s.model !== "string") problems.push("model 非字符串");
  if (s.role !== undefined && s.role !== null && typeof s.role !== "string") problems.push("role 非字符串");
  if (s.title !== undefined && s.title !== null && typeof s.title !== "string") problems.push("title 非字符串");
  if (s.label !== undefined && s.label !== null && typeof s.label !== "string") problems.push("label 非字符串");
  if (s.pinned !== undefined && s.pinned !== null && typeof s.pinned !== "boolean") problems.push("pinned 非布尔");
  if (s.archived !== undefined && s.archived !== null && typeof s.archived !== "boolean") problems.push("archived 非布尔");
  return problems;
}

function validateSessions(list) {
  const problems = [];
  if (!Array.isArray(list)) return ["列表不是数组"];
  for (const s of list.slice(0, 3)) {          // 最多报前 3 个会话，避免刷屏
    const who = (s && typeof s.id === "string") ? shortId(s.id) : "?";
    for (const p of validateSession(s)) problems.push(who + ": " + p);
  }
  return problems;
}

/* 校验问题 → banner 黄条。同一批问题只报一次（签名相同跳过）；
   数据恢复正常后只清除自己占用的 banner，不碰其它提示。 */
function applyValidation(problems) {
  if (problems.length) {
    const sig = problems.join("|");
    if (sig !== state.lastValidateSig) {
      state.lastValidateSig = sig;
      state.validateBannerUp = true;
      const summary = problems.slice(0, 3).join("；") +
        (problems.length > 3 ? "；等 " + problems.length + " 处" : "");
      setBanner("⚠ 服务器返回数据异常：" + summary, true);
    }
  } else if (state.validateBannerUp) {
    state.validateBannerUp = false;
    state.lastValidateSig = null;
    setBanner("");
  }
}

/* 轮询请求超时：任一 workspace 请求永久 pending 会卡住整轮（渲染/深链/
   校验/后续调度全部停摆）。每个请求挂 AbortController + 10s 超时，超时
   按失败处理（保留旧列表 stale，标记 workspaceErrors）。 */
const POLL_TIMEOUT_MS = 10000;

/* 深链 history 有界超时：恶劣网络下深链直接按 id probe history，同样挂
   10s 上限（复用 fetchWithTimeout）——只作用于深链路径（maybeHandleDeepLink
   命中分支 / attemptDeepLinkProbe），普通会话打开的网络语义不变。超时按
   失败处理 → 连 SSE 走 snapshot 兜底。 */
const DEEP_LINK_HISTORY_TIMEOUT_MS = POLL_TIMEOUT_MS;

async function fetchWithTimeout(ws, path, opts = {}) {
  const ctrl = new AbortController();
  const timer = setTimeout(() => ctrl.abort(), POLL_TIMEOUT_MS);
  try {
    return await apiFor(ws, path, Object.assign({}, opts, { signal: ctrl.signal }));
  } finally {
    clearTimeout(timer);   // 请求先完成：撤掉超时定时器（防迟到 abort）
  }
}

async function pollAllWorkspaces() {
  const wss = (state.workspaces || []).slice();
  await Promise.allSettled(wss.map((ws) => pollWorkspaceSessions(ws)));
  // 整轮完成后统一渲染一次：各 workspace 响应只更新缓存（workspaceLists），
  // 不再各自触发渲染——多 workspace 聚合下避免每响应全量重建树。
  afterPollRound();
}

/* 聚合轮询整轮收尾：深链、字段校验、当前会话元信息与侧边栏同步。
   侧边栏是唯一导航，因此即使抽屉关闭也持续刷新数据缓存；打开时强制
   重绘即可得到最新树。 */
function afterPollRound() {
  maybeHandleDeepLink();
  applyValidation(validateSessions(state.lastList || []));
  if (state.sessionId) {
    updateComposerMeta();               // model/role 可能随轮询更新（幂等）
    const cur = (state.lastList || []).find((s) => s.id === state.sessionId);
    if (cur) els.backParentBtn.hidden = !cur.parent_session_id;
  }
  if (!els.sidebar.hidden) renderSidebarTree();
}

/* 兼容别名：既有调用点（switchWorkspace/restartTransport/init）
   语义不变——现在等于聚合轮询全部 workspace；经 runPollRound 与定时轮询
   （setTimeout 链）共用同一个 in-flight 守卫，同一时刻只有一轮在途。 */
function pollSessions() {
  return runPollRound();
}

/* 拉取单个 workspace 的会话列表（只更新缓存，不渲染）：
   - 成功 → state.workspaceLists[ws.id] = list（并清错误标记）
   - 失败（HTTP/网络/格式/认证）→ 保留旧列表（stale），标记
     state.workspaceErrors[ws.id]（侧边栏显示「无法连接」分组头）
   激活 workspace 额外同步 state.lastList（既有单服务器路径的唯一数据源）；
   渲染/深链/校验 banner 由 pollAllWorkspaces 整轮完成后统一执行一次
   （afterPollRound）——各 workspace 响应不再各自全量重建树。 */
async function pollWorkspaceSessions(ws) {
  if (!workspaceToken(ws)) return;   // 全局 token 也未配置：跳过（不显示错误）
  let list = null;
  let err = null;
  try {
    const res = await fetchWithTimeout(ws, "/api/sessions");
    if (res.status === 401 || res.status === 403) {
      err = "auth";
      if (ws === state.workspace) setBanner("⚠ 认证失败：请检查 Token。");
    } else if (!res.ok) {
      err = "http" + res.status;
    } else {
      let parsed = null;
      try {
        parsed = await res.json();
      } catch (e) {
        // 旧 server 返回 HTML 错误页等非 JSON：不崩，提示后跳过本轮；
        // 占住校验 banner 位（恢复后由 applyValidation 自动清除），
        // 并重置签名，恢复后的问题批次会重新上报
        err = "format";
        if (ws === state.workspace) {
          state.validateBannerUp = true;
          state.lastValidateSig = null;
          setBanner("⚠ 服务器返回异常格式（非 JSON，可能为旧版服务器）。", true);
        }
      }
      if (!err) {
        if (Array.isArray(parsed)) {
          list = parsed;
        } else {
          // 合法 JSON 但不是数组（如 {}）：按格式错误处理——保留旧缓存
          // （stale）+ 错误标记；只有激活 workspace 提示 banner（背景
          // workspace 的格式错误只标记自己的分组，不弹全局 banner）。
          err = "format";
          if (ws === state.workspace) {
            state.validateBannerUp = true;
            state.lastValidateSig = null;
            setBanner("⚠ 服务器返回异常格式（非数组 JSON）。", true);
          }
        }
      }
    }
  } catch (e) {
    // 超时（AbortError）按失败处理：保留旧列表 stale、标记错误，不弹 banner
    err = (e && e.name === "AbortError") ? "timeout" : "network";
    if (ws === state.workspace && (!navigator.onLine || e instanceof TypeError)) {
      setBanner("⚠ 无法连接服务器（网络错误）。", true);
    }
  }
  // 在途请求守卫：请求发出后 workspace 被删除（removeWorkspace）→ 直接丢弃，
  // 绝不写回已删 workspace 的缓存/错误标记，也不重绘（聚合视图已随删除重绘，
  // 写回会让被删服务器"复活"在侧边栏里；review 发现 2）。
  if (!state.workspaces.includes(ws)) return;
  if (list) {
    state.workspaceLists[ws.id] = list;
    state.workspaceErrors[ws.id] = null;
  } else {
    if (state.workspaceLists[ws.id] === undefined) state.workspaceLists[ws.id] = [];
    state.workspaceErrors[ws.id] = err;   // 保留旧列表（stale），标记错误
  }
  if (ws === state.workspace) {
    // 激活 workspace 的列表缓存 → lastList（既有单服务器路径的唯一数据源）；
    // 渲染/深链/校验统一由 pollAllWorkspaces 整轮完成后执行（afterPollRound）
    state.lastList = state.workspaceLists[ws.id];
  }
}
/* URL 深链：?session=<id>。token 就绪后立即尝试（init / restartTransport /
   afterPollRound 三个入口共用，状态机防重复）：
   - fresh 列表命中（成功轮询过的列表里有该 id）→ 既有分支：active 直接
     openSession；inactive（历史）走 resumeSession（resume 成功后内部会
     openSession）
   - fresh 列表成功但没有 id / 列表失败 / 超时 / 只有 stale 缓存 → 直接
     对该 workspace probe history（列表缺失不是权威不存在：历史会话可能
     还没进列表；深链只支持当前激活 workspace，不跨服务器遍历）
   - attempt 状态机（probing + attemptEpoch）防止 init、token restart、
     poll completion 重复发起；openSession/resumeSession 自身声明新 epoch，
     与跨服务器打开竞态（过期深链不打开）。 */
function maybeHandleDeepLink() {
  if (state.deepLink.handled || !state.deepLink.pending) return;
  if (!state.token) return;                    // token 为空：等用户填写后再处理
  const target = state.deepLink.pending;
  const ws = state.workspace;
  if (!ws) return;
  const hit = (state.lastList || []).find((s) => s.id === target);
  // 只有成功轮询过的列表（workspaceErrors === null）才算 fresh 命中；
  // stale/失败/过期缓存里的命中不可信 → 落到 probe 路径（列表缺失不是
  // 权威不存在）。
  if (hit && state.workspaceErrors[ws.id] === null) {
    state.deepLink.handled = true;
    state.deepLink.pending = null;
    if (hit.active === false) {
      // fresh inactive：resume 恢复后再拉 history——恢复也是深链 attempt 的
      // 一部分：声明代次 + 标记，让恢复后的 history 带深链有界超时（防
      // resume 后拉 history 永久 pending 卡死），SSE 404 恢复语义同
      // probe 路径（标记在恢复成功/放弃时由 openWith/sse.js 清除）。
      const claimed = ++sessionOpenEpoch;
      state.deepLink.probing = true;
      state.deepLink.attemptEpoch = claimed;
      resumeSession(ws.id, target, claimed, DEEP_LINK_HISTORY_TIMEOUT_MS);
    } else {
      openSession(target, undefined, undefined, DEEP_LINK_HISTORY_TIMEOUT_MS);
    }
    return;
  }
  if (state.deepLink.probing) return;          // 在途 attempt：不重复发起
  attemptDeepLinkProbe();
}

/* 深链直接请求主路径：不等列表，立即对激活 workspace 打开会话。
   - history 加有界超时（fetchWithTimeout），只作用于深链路径
   - "auth"/"gone" 停流（openWith 既有语义）；"fail"（网络/超时）连 SSE
     走 snapshot 兜底
   - 带 attempt 标记（probing + attemptEpoch）进入 SSE 阶段：/events 404
     且 history 已成功、分类 historical/unknown → resumeSession 恢复
     （sse.js connectSSE 404 分支处理）；history 与 SSE 都失败 → 持久
     提示 + 自动重连重试
   openSession 会消费 pending/handled——attempt 标记独立存活到 SSE 阶段，
   由 sse.js 在 404 分类/连接成功时清除。 */
function attemptDeepLinkProbe() {
  const target = state.deepLink.pending;
  const ws = state.workspace;
  const claimed = ++sessionOpenEpoch;          // 入口声明一次代次（openSession 共享）
  state.deepLink.probing = true;               // attempt 标记：防重复发起 + SSE 404 恢复判定
  state.deepLink.attemptEpoch = claimed;
  openSession(target, undefined, claimed, DEEP_LINK_HISTORY_TIMEOUT_MS);
}

/* workspace 的会话列表解析：
   - 激活 workspace → state.lastList（既有单服务器路径的唯一数据源，
     pollWorkspaceSessions 每次轮询同步它）
   - 背景 workspace → state.workspaceLists[ws.id]（各自轮询维护的缓存）
   单服务器模式（旧行为）下 state.workspaceLists 为空，激活列表 = lastList，
   所有既有测试/代码路径不变。 */
function workspaceListFor(ws) {
  if (ws === state.workspace) return state.lastList || [];
  const l = state.workspaceLists[ws.id];
  return Array.isArray(l) ? l : [];
}

/* ws-chip 按 workspace 分色：以 state.workspaces 数组下标对固定色板取模
   （色板 6 色，超 6 个 workspace 循环复用）。同一 workspace 的所有 chip
   （组头/置顶行）同色，跨 workspace 一眼可辨。色类样式见
   style.css 的 .ws-chip-0..5（Solarized 色 tint）。 */
const WS_CHIP_PALETTE = 6;
function wsChipClass(ws) {
  const i = state.workspaces.indexOf(ws);
  return i < 0 ? "" : "ws-chip-" + (i % WS_CHIP_PALETTE);
}

/* 恢复（resume）一个历史会话：POST /api/sessions {id} 建回活跃会话后打开。
   wsId 是发起恢复的服务器（调用方在发起时捕获，不能是 POST await 期间的
   实时激活服务器）；POST 成功后必须重新校验 epoch 与 wsId 才打开——否则
   POST 期间用户切到其它服务器/打开别的会话，过期恢复会在错误服务器上
   打开（旧 bug：resumeSession 无条件 openSession）。 */
async function resumeSession(wsId, id, epoch, timeoutMs) {
  const ws = state.workspaces.find((w) => w.id === wsId) || state.workspace;
  if (!workspaceToken(ws)) { setBanner("⚠ 请先输入 Token。", true); return; }
  // 恢复也是「打开」：入口（点击/深链）声明一次代次；嵌套调用（resumeSessionIn
  // → resumeSession → openSession）共享调用方传入的同一代次，绝不二次递增。
  const claimed = (epoch === undefined) ? ++sessionOpenEpoch : epoch;
  try {
    const res = await apiFor(ws, "/api/sessions", { method: "POST", body: JSON.stringify({ id }) });
    if (res.status === 401 || res.status === 403) { setBanner("⚠ 认证失败：请检查 Token。"); return; }
    if (res.status !== 201) throw new Error("HTTP " + res.status);
    const s = await res.json();
    if (claimed !== sessionOpenEpoch) return;    // 期间有更新的打开/切换：过期恢复丢弃
    if (state.workspace.id !== wsId) return;     // 发起恢复的服务器已不是激活的：不在这里打开
    // timeoutMs（深链恢复路径传入）透传给恢复后打开的 history：深链的
    // 有界超时覆盖整个恢复生命周期；普通恢复不传 → 无超时，语义不变。
    openSession(s.id, undefined, claimed, timeoutMs);
  } catch (e) {
    setBanner("⚠ 恢复会话失败：" + e.message);
  }
}

/* 跨 workspace 打开会话：目标 wsId 非激活 → 先 await switchWorkspace(wsId)
   （切换完成后 next tick 才继续），再 openSession(id)。竞态防护：
   - 入口点击声明一次 action epoch，嵌套的 switchWorkspace/openSession 共享
     同一代次（不再各自递增）；任何直接切换/新的打开都会取代代次，使旧
     的在途打开失效（最新一次操作获胜）；
   - state.workspace.id 二次校验：切换期间目标又被顶掉（用户直接切了
     workspace）也丢弃。
   同 workspace 直接 openSession（同步，无切换开销）。 */
async function openSessionIn(wsId, id, onReady, epoch) {
  // 入口点击：整个「（可能切 workspace）+ 打开」是一次动作，声明一次代次，
  // 嵌套的 switchWorkspace/openSession 共享它——不再各自递增。
  const claimed = (epoch === undefined) ? ++sessionOpenEpoch : epoch;
  if (wsId !== state.workspace.id) {
    await switchWorkspace(wsId, claimed);
  }
  if (state.workspace.id !== wsId) return;      // 目标已被顶掉：丢弃
  openSession(id, onReady, claimed);
}

/* 跨 workspace 恢复历史会话：先切到目标服务器，再 POST /api/sessions {id}
   （请求打到目标服务器）建回活跃会话后打开。wsId 透传给 resumeSession 做
   POST 完成后的二次校验（期间切走则不在错误服务器打开）。 */
async function resumeSessionIn(wsId, id, epoch) {
  // 入口点击：整个「（可能切 workspace）+ 恢复」是一次动作，声明一次代次，
  // 嵌套的 switchWorkspace/resumeSession/openSession 共享它。
  const claimed = (epoch === undefined) ? ++sessionOpenEpoch : epoch;
  if (wsId !== state.workspace.id) {
    await switchWorkspace(wsId, claimed);
  }
  if (state.workspace.id !== wsId) return;      // 目标已被顶掉：丢弃
  await resumeSession(wsId, id, claimed);
}

/* 在指定 workspace 新建会话（侧边栏组头「+」按钮；区别于 createSession 的
   激活 workspace + initial_prompt 路径）：无 initial_prompt，请求打到目标
   workspace（apiFor(ws) 而非 api()）；成功后把新会话 meta 塞进该 workspace
   的列表缓存（与激活列表缓存处理一致；背景 workspace 的
   缓存 = workspaceLists[ws.id]，switchWorkspace 会把它接为 lastList，composer
   meta 立即可用），再走跨 workspace 打开模式（openSessionIn：非激活先切）。 */
async function createSessionIn(ws) {
  if (!workspaceToken(ws)) { setBanner("⚠ 请先输入 Token。", true); return; }
  // POST 前只捕获当前代次、绝不递增（review：旧实现 ++sessionOpenEpoch 声明
  // action 代次，会让当前正在进行的 history/SSE 的 epoch 校验失败 → 创建挂起/
  // 失败时当前流被误杀）。成功后才由打开动作 openSessionIn 声明新代次（它入口
  // 自行 ++/claim，与 resumeSessionIn/openSessionIn「成功才声明」语义一致）；
  // 迟到响应仍由 captured !== sessionOpenEpoch 丢弃，绝不把用户强切回发起创建
  // 时的 workspace（review：迟到响应覆盖用户导航）。
  const captured = sessionOpenEpoch;
  try {
    const res = await apiFor(ws, "/api/sessions", { method: "POST", body: JSON.stringify({}) });
    // 响应到达先校验发起上下文（review：迟到失败污染新视图——POST 挂起期间
    // 用户已导航（打开/切换）或 workspace 被删，迟到的 401/500/格式
    // 错误必须整体丢弃，不刷 banner、不改新视图的任何状态）。
    if (captured !== sessionOpenEpoch) return;    // 期间有更新导航：过期创建不打开
    if (!state.workspaces.includes(ws)) return;  // 目标 workspace 已被删除
    if (res.status === 401 || res.status === 403) { setBanner("⚠ 认证失败：请检查 Token。"); return; }
    if (res.status !== 201) throw new Error("HTTP " + res.status);
    const s = await res.json();
    if (captured !== sessionOpenEpoch) return;    // 解析期间又有更新导航：过期创建不打开
    if (!state.workspaces.includes(ws)) return;  // 目标 workspace 已被删除
    const wl = workspaceListFor(ws);
    if (Array.isArray(wl)) {
      const i = wl.findIndex((x) => x.id === s.id);
      if (i >= 0) wl.splice(i, 1);
      wl.push(s);
    }
    await openSessionIn(ws.id, s.id);   // 成功路径：入口声明新 epoch（++/claim）
  } catch (e) {
    // catch 刷 banner 前同样守卫发起上下文：导航发生在请求/解析期间的迟到
    // 失败不覆盖新视图的提示（review：迟到失败污染新视图）。
    if (captured !== sessionOpenEpoch) return;
    if (!state.workspaces.includes(ws)) return;
    setBanner("⚠ 创建会话失败：" + e.message);
  }
}

/* =====================================================================
 * 对话视图流程
 * ===================================================================*/
/* 深链 attempt 标记匹配当前打开代次 → 返回深链 history 有界超时；否则
   undefined（普通打开/重连不得获得超时，网络语义不变）。标记在深链
   attempt 全生命周期存活（probe / fresh-hit resume / SSE 404 恢复 /
   scheduleReconnect 重试），直到成功（SSE 200）或放弃（auth/gone/
   switchWorkspace/404 恢复完成）。 */
function deepLinkTimeoutFor(epoch) {
  return (state.deepLink.probing && state.deepLink.attemptEpoch === epoch)
    ? DEEP_LINK_HISTORY_TIMEOUT_MS : undefined;
}

async function loadHistory(id, wsId, epoch, timeoutMs) {
  const ws = state.workspaces.find((w) => w.id === wsId) || state.workspace;
  try {
    // 只取尾部 HISTORY_PAGE 条：长会话不一次全量渲染（Firefox 等浏览器
    // 对超大 DOM 滚动卡死）；滚动触顶时 loadOlder 增量加载更早。
    // 深链超时：显式 timeoutMs 优先（probe / resumeSession 恢复路径传入），
    // 否则按深链 attempt 标记兜底（scheduleReconnect 重试路径：标记与
    // 代次仍匹配 → 重试的 history 同样有界，不会永久 pending 卡死）；
    // 普通打开/重连无标记 → 无超时，网络语义不变。
    const effTimeout = (timeoutMs !== undefined) ? timeoutMs : deepLinkTimeoutFor(epoch);
    const url = "/api/sessions/" + encodeURIComponent(id) + "/history?limit=" + HISTORY_PAGE;
    const res = effTimeout ? await fetchWithTimeout(ws, url) : await apiFor(ws, url);
    // 竞态防护：响应回来时打开/切换 workspace 已发生 → 丢弃，不碰 DOM/state
    // （陈旧响应绝不渲染到新激活的服务器/会话，也不起 SSE）。
    if (epoch !== sessionOpenEpoch || state.workspace.id !== wsId || state.sessionId !== id) return "stale";
    if (res.status === 401 || res.status === 403) { setBanner("⚠ 认证失败：请检查 Token。"); return "auth"; }
    if (res.status === 404) { setBanner("⚠ 会话不存在或已被删除。"); return "gone"; }
    if (!res.ok) throw new Error("HTTP " + res.status);
    const data = await res.json();
    if (epoch !== sessionOpenEpoch || state.workspace.id !== wsId || state.sessionId !== id) return "stale";
    const entries = Array.isArray(data) ? data : (data.entries || []);
    state.nextBeforeSeq = (data.next_before_seq !== undefined ? data.next_before_seq : null);
    state.olderDone = (state.nextBeforeSeq === null);
    if (state.initSource !== "snapshot") {
      if (state.initSource === "restored") {
        // 缓存的视图可能过期（切走期间会话有新消息）：用最新尾部替换，
        // 而不是追加（追加会与缓存内容重复）。保留滚动位置（距底部偏移）
        // 与进行中的增量块（thinking/assistant/tool 卡片，未落盘只活在
        // 内存/SSE 增量里，history 里没有它们）。
        const offset = els.messages.scrollHeight - els.messages.scrollTop - els.messages.clientHeight;
        const inflight = [];
        if (state.acc) {
          if (state.acc.assistantEl) inflight.push(state.acc.assistantEl);
          if (state.acc.thinkingEl) inflight.push(state.acc.thinkingEl);
          if (state.acc.toolStack) for (const t of state.acc.toolStack) inflight.push(t.el);
        }
        state.acc.toolStack = [];   // 防重：替换后 reattachInFlight 重新收集
        renderHistory(entries);     // 清空 + 渲染最新尾部
        for (const el of inflight) {
          if (el && !el.isConnected) els.messages.appendChild(el);
        }
        reattachInFlight(state.acc);   // 重新绑定进行中块，增量续写不中断
        if (offset > 4) {
          els.messages.scrollTop = els.messages.scrollHeight - offset - els.messages.clientHeight;
          userScrolled = true;
          els.jumpBottomBtn.hidden = false;
        }
      } else {
        renderHistory(entries);
      }
      state.initSource = "history";
    }
    return "ok";
  } catch (e) {
    if (epoch !== sessionOpenEpoch || state.workspace.id !== wsId || state.sessionId !== id) return "stale";
    // 网络问题：交给 SSE snapshot 兜底；两者都失败则提示
    if (!state.acc || !els.messages.children.length) {
      setBanner("⚠ 加载历史失败：" + e.message + "（等待 SSE 快照…）", true);
    }
    return "fail";
  }
}

/* 滚动到顶部时加载更早历史（分页：一次一个 compaction 段） */
async function loadOlder() {
  if (state.loadingOlder || state.olderDone || !state.sessionId || state.nextBeforeSeq === null) return;
  const wsId = state.workspace.id;   // 发起时的 workspace：响应回来校验用
  const epoch = sessionOpenEpoch;
  const sid = state.sessionId;
  state.loadingOlder = true;
  const prevHeight = els.messages.scrollHeight;   // 插入前高度
  try {
    const res = await api("/api/sessions/" + encodeURIComponent(sid)
      + "/history?before_seq=" + state.nextBeforeSeq + "&limit=" + HISTORY_PAGE);
    if (epoch !== sessionOpenEpoch || state.workspace.id !== wsId || state.sessionId !== sid) return;
    if (res.status === 401 || res.status === 403) { setBanner("⚠ 认证失败：请检查 Token。"); return; }
    if (!res.ok) return;                          // 静默失败，下次滚动重试
    const data = await res.json();
    if (epoch !== sessionOpenEpoch || state.workspace.id !== wsId || state.sessionId !== sid) return;
    const entries = Array.isArray(data) ? data : (data.entries || []);
    state.nextBeforeSeq = (data.next_before_seq !== undefined ? data.next_before_seq : null);
    if (state.nextBeforeSeq === null) state.olderDone = true;
    if (entries.length) {
      renderEntries(entries, true);               // 前置插入
      // 占位折叠块保持在最顶部：更早条目插到它前面后，把它移回最前，
      // 保证后续 prune 折叠的仍是「最早」的块（占位内展开顺序不乱）。
      const ph = [...els.messages.children].find((c) => c.classList && c.classList.contains("older-collapse"));
      if (ph) els.messages.insertBefore(ph, els.messages.firstChild);
      // 保持滚动位置：内容在顶部增高，scrollTop 相应下移
      els.messages.scrollTop += els.messages.scrollHeight - prevHeight;
    }
  } catch (e) {
    // 静默失败，下次滚动重试
  } finally {
    // 陈旧响应不碰 state：新会话/新加载可能已接管 loadingOlder
    if (epoch === sessionOpenEpoch && state.workspace.id === wsId && state.sessionId === sid) {
      state.loadingOlder = false;
    }
  }
}

/* history 加载（可选）+ 连接 SSE；auth/gone 时不再重试连接。
   onReady：视图就绪后的回调（历史渲染完 / 缓存恢复完，消息区可操作时
   触发一次）。
   wsId/epoch：打开时捕获的发起上下文——响应回来时任何一项不匹配
   （新的打开/切换 workspace）→ 丢弃过期回调，不起 SSE。 */
function openWith(id, withHistory, onReady, wsId, epoch, timeoutMs) {
  const step = withHistory ? loadHistory(id, wsId, epoch, timeoutMs) : Promise.resolve("ok");
  step.then((r) => {
    if (epoch !== sessionOpenEpoch) return;       // 更新的打开/切换 workspace 已发生
    if (state.workspace.id !== wsId) return;      // 已被切到其它服务器
    if (state.sessionId !== id) return;           // 已切换会话：丢弃过期回调
    if (r === "auth" || r === "gone") {
      // 深链 attempt 终止（无流可恢复）：清标记，防止残留标记污染后续
      // 非深链会话的 SSE 404 分类（标记与 epoch 绑定，此处显式清更稳）
      state.deepLink.probing = false;
      state.deepLink.attemptEpoch = -1;
      return;
    }
    connectSSE(id, wsId, epoch);
    if (onReady) onReady();
  });
}

/* 输入框左下角元信息：当前会话的 model · role（对齐 TUI 输入框左下角）。
   数据来自 state.lastList（/api/sessions 已含 model/role 字段）；幂等：
   内容没变就不动 DOM。两者都空 → hidden。 */
/* 顶部会话标识：有标题 → 标题（可点击重命名）+ id 小字；无标题 → 完整 id。
   数据来自 state.lastList；标题点击复用 enterRename（同侧边栏编辑）。 */
function renderChatSessionId(id) {
  const el0 = els.chatSessionId;
  if (!el0) return;
  const s = (state.lastList || []).find((x) => x.id === id);
  const title = s && s.title ? String(s.title) : "";
  el0.innerHTML = "";
  const t = el("span", "chat-sid-title", title || id);
  t.title = "点击重命名";
  t.addEventListener("click", (ev) => {
    ev.stopPropagation();
    enterRename(t, s || { id, title: null }, () => { renderChatSessionId(id); });
  });
  el0.append(t);
  if (title) {
    const idline = el("span", "chat-sid-id", id);
    el0.append(idline);
  }
}

function updateComposerMeta() {
  const meta = els.composerMeta;
  if (!meta) return;
  const s = (state.lastList || []).find((x) => x.id === state.sessionId);
  const model = s && s.model ? String(s.model) : "";
  let role = s && s.role ? String(s.role) : "";
  // 前缀保险：无 parent 的 sub-/btw- 前缀会话不是主会话，脏数据里
  // 可能带着 role="main"（孤儿 subagent）——不显示误导性的 "· main"。
  if (role === "main" && s && !s.parent_session_id && /^(sub|btw)-/i.test(String(s.id || ""))) {
    role = "";
  }
  if (!model && !role) {
    if (!meta.hidden || meta.textContent !== "") {   // 幂等：仅在变化时触碰 DOM
      meta.hidden = true;
      meta.textContent = "";
      meta.title = "";
    }
    return;
  }
  const text = role ? model + " · " + role : model;
  // 会话状态：非 Idle 时在 model·role 尾部追加状态（Busy→处理中、
  // Compacting→压缩中、Failed→失败、Finished→已完成）；Idle 静默。
  // 状态是独立 span（带色 class），不动 meta 自身 class。
  const st = s && s.status ? String(s.status) : "";
  const showSt = st !== "" && st !== "Idle";
  const stCls = !showSt ? "" : (st.startsWith("Failed") ? "error"
    : st === "Compacting" ? "compacting" : "busy");
  const full = showSt ? text + " · " + statusLabel(st) : text;
  const curSt = meta.querySelector(".composer-status");
  const curCls = curSt ? curSt.className : "";
  const wantCls = "composer-status" + (stCls ? " " + stCls : "");
  if (meta.hidden || meta.textContent !== full || curCls !== wantCls) {
    meta.hidden = false;
    meta.title = s.model || "";   // 完整 model 名（可能被省略号截断）
    meta.textContent = "";        // 清空后重建（文本节点 + 可选状态 span）
    meta.append(showSt ? text + " · " : text);   // DOM append 接受字符串（自动转文本节点）
    if (showSt) meta.append(el("span", wantCls, statusLabel(st)));
  }
}

/* 切走前保存当前会话视图状态（消息 DOM、滚动位置、分页游标、输入草稿），
   切回时原样恢复、不重新加载历史。 */
function saveSessionState() {
  if (!state.sessionId) return;
  state.sessionStates[state.sessionId] = {
    html: els.messages.innerHTML,
    scrollTop: els.messages.scrollTop,
    nextBeforeSeq: state.nextBeforeSeq,
    olderDone: state.olderDone,
    draft: els.promptInput.value,
  };
}

function openSession(id, onReady, epoch, timeoutMs) {
  // 打开即声明新代次（入口调用）；嵌套调用（openSessionIn/resumeSession 等
  // 已声明过）传入共享的代次，不再递增——一次用户动作只有一个 action epoch。
  const claimed = (epoch === undefined) ? ++sessionOpenEpoch : epoch;
  const wsId = state.workspace.id;    // 发起时的工作区（history/SSE 校验用）
  stopTaskRows();              // 会话行级 output interval / delegate SSE 不跨导航存活
  closeForkMenu();
  saveSessionState();          // 切走：保存当前会话（消息/滚动/分页/草稿）
  state.renameActive = false;  // 切换会话会销毁编辑框：清标志，恢复轮询重绘
  stopSSE();
  state.sessionId = id;
  // 侧边栏是唯一导航，轮询常驻以保持会话树/busy 状态新鲜。
  // 任何手动打开都会取代/完成 URL 深链，避免后续导航再次触发
  state.deepLink.handled = true;
  state.deepLink.pending = null;
  els.chatView.classList.remove("hidden", "no-session");
  els.topActions.hidden = false;
  // subagent 会话：显示「← 主会话」快速返回父会话（主会话/无父则隐藏）。
  // 任务面板直连跳转时 lastList 可能还没包含该 subagent（轮询未刷新）：
  // cur 不存在则不动按钮（保持现状），等 refreshSessionsForSidebar 拉到
  // 列表后由它判定，避免把返回按钮误藏。
  const cur = (state.lastList || []).find((s) => s.id === id);
  if (cur) els.backParentBtn.hidden = !cur.parent_session_id;
  renderChatSessionId(id);
  updateComposerMeta();          // 显示当前会话 model · role（缓存/首开/恢复共用）
  els.usageInfo.textContent = "";
  applyStatus("Idle");
  refreshBanner();
  history.replaceState(null, "", "/?session=" + encodeURIComponent(id));
  const cached = state.sessionStates[id];
  if (cached) {
    // 切回缓存过的会话：先恢复视图（立即响应），再拉最新尾部补全——
    // 切走期间会话可能继续产生消息（缓存已过期），必须用最新 history
    // 替换渲染，否则那些消息（snapshot 也被跳过）永远不会显示。
    state.initSource = "restored";
    state.nextBeforeSeq = cached.nextBeforeSeq;
    state.loadingOlder = false;
    state.olderDone = cached.olderDone;
    state.acc = newAccumulator();
    els.messages.innerHTML = cached.html;
    reattachInFlight(state.acc);   // 重新绑定缓存里「进行中」的思考/助手/工具卡片，
                                   // 使增量续写而不是新建（防止重复出现多个块）
    pruneMessages();               // 缓存快照也可能超上限（功能上线前的旧快照）：维持有界
    els.messages.scrollTop = cached.scrollTop;
    els.promptInput.value = cached.draft || "";
    autosizeInput();
    const m = els.messages;
    const atBottom = m.scrollHeight - m.scrollTop - m.clientHeight <= 4;
    userScrolled = !atBottom;      // 恢复到非底部位置：不自动跟随滚动
    els.jumpBottomBtn.hidden = atBottom;
    openWith(id, true, onReady, wsId, claimed, timeoutMs);   // 拉最新尾部替换过期缓存；onReady 在替换渲染完成后触发
  } else {
    // 首次打开：走既有流程（加载历史 + SSE）
    state.initSource = null;
    state.nextBeforeSeq = null;
    state.loadingOlder = false;
    state.olderDone = false;
    state.acc = newAccumulator();
    // 清空消息区（jumpBottomBtn 在 messages 外，不受影响）
    els.messages.innerHTML = "";
    els.jumpBottomBtn.hidden = true;
    els.promptInput.value = "";    // 输入框草稿跟随会话：新会话从空开始
    autosizeInput();
    userScrolled = false;          // 打开新会话：恢复自动跟随
    openWith(id, true, onReady, wsId, claimed, timeoutMs);   // onReady 在 loadHistory 渲染完成后触发
  }
  if (!els.sidebar.hidden) renderSidebarTree();   // 更新 .current 高亮（侧边栏可见时）
}

/* 发送 / 取消 / 压缩 */

/* =====================================================================
 * 斜杠命令自动补全菜单（Slack/Discord 风格）
 * 输入框内输入 / 且是命令起始（开头或前面是空白）→ 弹出候选列表；
 * 继续输入按前缀过滤；↑↓ 移动选中、Enter/Tab 填入、Esc/失焦/发送关闭。
 * 填入后走现有 sendPrompt 的斜杠命令处理逻辑（用户补参数回车执行）。
 * ===================================================================*/
const SLASH_COMMANDS = [
  { name: "/compact", desc: "压缩上下文（释放 token）", args: "" },
  { name: "/model", desc: "切换当前会话模型", args: "<profile>" },
  { name: "/rename", desc: "重命名当前会话", args: "<标题>" },
  { name: "/btw", desc: "fork 旁路 subagent 继续探讨", args: "<问题>" },
  { name: "/fork", desc: "从历史消息 fork 出新会话", args: "" },
  { name: "/undo", desc: "撤销最近的文件操作", args: "" },
  { name: "/help", desc: "显示所有命令及用法", args: "" },
];

const slashMenu = {
  open: false,     // 菜单是否显示
  items: [],       // 当前过滤后的候选（与渲染行一一对应）
  selected: 0,     // 当前选中索引
  mode: "command", // "command" = 命令候选；"profile" = /model 参数补全（profile 候选）
};

/* /model profile 候选缓存：GET /api/models 懒加载一次（Web 端 /model 自动补全）。
   null = 未加载；[] = 已加载（含加载失败退化为空）。 */
let modelProfileCache = null;
let modelProfilePromise = null;

function loadModelProfiles() {
  if (modelProfilePromise) return modelProfilePromise;
  modelProfilePromise = (async () => {
    try {
      const res = await api("/api/models");
      if (!res.ok) throw new Error("HTTP " + res.status);
      const data = await res.json();
      modelProfileCache = Array.isArray(data) ? data : [];
    } catch (e) {
      modelProfileCache = [];   // 加载失败：退化为空列表（菜单不弹，不影响输入）
    }
    return modelProfileCache;
  })();
  return modelProfilePromise;
}

/* 光标前是 "/model <参数>"（/model 后至少一个空格）→ 返回已输入的前缀
   （可为空串）；否则返回 null（保持命令菜单逻辑）。"/model"（无空格）返回
   null → 命令菜单（/model 命令项）；"/model " 与 "/model c" 都走 profile 补全。 */
function slashModelArg() {
  const v = els.promptInput.value;
  const caret = (els.promptInput.selectionStart == null)
    ? v.length : els.promptInput.selectionStart;
  const before = v.slice(0, caret);
  const m = /^\/model[ \t]+(\S*)$/.exec(before);
  return m ? m[1] : null;
}

/* /model 参数补全：菜单显示 profile 候选（按已输入前缀过滤）。
   候选未加载时先关菜单，加载完成后再按当前输入重开。 */
function updateModelMenu(filter) {
  if (modelProfileCache === null) {
    closeSlashMenu();
    loadModelProfiles().then(() => {
      if (slashModelArg() !== null) updateSlashMenu();
    });
    return;
  }
  const items = modelProfileCache.filter((p) => p.startsWith(filter));
  if (!items.length) { closeSlashMenu(); return; }   // 无匹配：关闭
  slashMenu.mode = "profile";
  slashMenu.items = items;
  if (slashMenu.selected >= items.length) slashMenu.selected = 0;
  slashMenu.open = true;
  renderSlashMenu();
}

/* 光标前的「当前词」：以 / 开头且前面是输入框开头或空白 → 返回该词（如 "/com"）；
   否则返回 null（不是命令起始，菜单不应出现）。 */
function currentSlashWord() {
  const v = els.promptInput.value;
  const caret = (els.promptInput.selectionStart == null)
    ? v.length : els.promptInput.selectionStart;
  const before = v.slice(0, caret);
  const m = /(^|\s)(\/[^\s]*)$/.exec(before);
  return m ? m[2] : null;
}

/* input 事件驱动：更新菜单（显示/过滤/关闭）。输入、删除都走这里。
   "/model <参数>"（含 "/model " 尾随空格——currentSlashWord 匹配不到）优先
   走 profile 补全；其余 / 命令走命令候选。 */
function updateSlashMenu() {
  const modelArg = slashModelArg();
  if (modelArg !== null) { updateModelMenu(modelArg); return; }
  const word = currentSlashWord();
  if (word === null) { closeSlashMenu(); return; }
  const items = SLASH_COMMANDS.filter((c) => c.name.startsWith(word));
  if (!items.length) { closeSlashMenu(); return; }   // 无匹配：关闭
  slashMenu.mode = "command";
  slashMenu.items = items;
  if (slashMenu.selected >= items.length) slashMenu.selected = 0;
  slashMenu.open = true;
  renderSlashMenu();
}

function renderSlashMenu() {
  const menu = els.slashMenu;
  if (!menu) return;
  menu.innerHTML = "";
  slashMenu.items.forEach((c, i) => {
    const row = el("div", "slash-item" + (i === slashMenu.selected ? " selected" : ""));
    if (slashMenu.mode === "profile") {
      row.append(el("span", "slash-name", c));   // profile 候选：只显示 profile 名
    } else {
      row.append(el("span", "slash-name", c.name), el("span", "slash-desc", c.desc));
      if (c.args) row.append(el("span", "slash-args", c.args));
    }
    // mousedown 阻止默认：菜单项点击不抢走输入框焦点（否则 blur 先关菜单，click 落空）
    row.addEventListener("mousedown", (e) => { if (e.preventDefault) e.preventDefault(); });
    row.addEventListener("click", () => selectSlashItem(i));
    menu.appendChild(row);
  });
  menu.hidden = false;
}

function closeSlashMenu() {
  if (!slashMenu.open) return;
  slashMenu.open = false;
  slashMenu.items = [];
  slashMenu.selected = 0;
  slashMenu.mode = "command";
  if (els.slashMenu) els.slashMenu.hidden = true;
}

/* ↑↓ 移动选中（循环） */
function moveSlashMenu(delta) {
  if (!slashMenu.open || !slashMenu.items.length) return;
  const n = slashMenu.items.length;
  slashMenu.selected = (slashMenu.selected + delta + n) % n;
  renderSlashMenu();
}

/* 选中当前项（Enter/Tab/点击）→ 填入输入框。
   带参数命令填入 "/cmd <占位>" 并选中占位符（输入即覆盖）；无参数命令
   只填 "/cmd"，光标在末尾，用户回车执行（与现有 sendPrompt 衔接）。 */
function acceptSlashMenu() {
  selectSlashItem(slashMenu.selected);
}

function selectSlashItem(i) {
  const c = slashMenu.items[i];
  if (!c) return;
  const inp = els.promptInput;
  if (slashMenu.mode === "profile") {
    // /model 参数补全：填入完整 "/model <profile>" 并直接发送（一步到位，
    // 与命令模式 Enter 执行一致；走 sendPrompt 的 /model 分支 POST）。
    inp.value = "/model " + c;
    inp.selectionStart = inp.selectionEnd = inp.value.length;
    closeSlashMenu();
    autosizeInput();
    sendPrompt();
    return;
  }
  inp.value = c.args ? c.name + " " + c.args : c.name;
  if (c.args) {
    const at = (c.name + " ").length;   // 光标落在参数占位起始处
    inp.selectionStart = at;
    inp.selectionEnd = inp.value.length;   // 选中占位符：直接输入即覆盖
  } else {
    inp.selectionStart = inp.selectionEnd = inp.value.length;
  }
  closeSlashMenu();
  autosizeInput();
  inp.focus();
}

/* =====================================================================
 * fork 面板：/fork 命令弹出，列出当前会话的历史 turn 边界
 * （GET /api/sessions/{id}/fork-candidates，只列边界消息），↑↓ 选择、
 * Enter/Tab 选中、Esc/失焦关闭；选中 → POST /api/sessions/{id}/fork
 * body {"at": N} 建新会话并打开。面板与 slash 菜单同风格（absolute
 * 定位在 composer 内、选中高亮、mousedown preventDefault 保焦点）。
 * ===================================================================*/
const forkMenu = {
  open: false,     // 面板是否显示
  items: [],       // 候选 [{at, seq, preview}, ...]（与渲染行一一对应）
  selected: 0,     // 当前选中索引
  loading: false,  // 是否正在拉取候选（渲染「加载中…」）
  requestSeq: 0,    // 打开/关闭代次：同一会话重复打开时也丢弃更早候选响应
};

function forkContextCurrent(sid, wsId, epoch) {
  return epoch === sessionOpenEpoch && state.workspace.id === wsId && state.sessionId === sid;
}

async function openForkMenu() {
  if (!state.sessionId) return;
  const sid = state.sessionId;
  const wsId = state.workspace.id;
  const captured = sessionOpenEpoch;
  const requestSeq = ++forkMenu.requestSeq;
  forkMenu.open = true;
  forkMenu.loading = true;
  forkMenu.items = [];
  forkMenu.selected = 0;
  renderForkMenu();
  try {
    const res = await api("/api/sessions/" + encodeURIComponent(sid) + "/fork-candidates");
    if (!forkContextCurrent(sid, wsId, captured) || requestSeq !== forkMenu.requestSeq || !forkMenu.open) return;
    if (res.status === 401 || res.status === 403) { setBanner("⚠ 认证失败：请检查 Token。", true); closeForkMenu(); return; }
    if (res.status === 404 || res.status === 405) { setBanner("⚠ 服务器不支持 fork（需新版后端）", true); closeForkMenu(); return; }
    if (!res.ok) throw new Error("HTTP " + res.status);
    const data = await res.json();
    if (!forkContextCurrent(sid, wsId, captured) || requestSeq !== forkMenu.requestSeq || !forkMenu.open) return;
    forkMenu.items = Array.isArray(data) ? data : [];
    forkMenu.loading = false;
    renderForkMenu();
  } catch (e) {
    if (!forkContextCurrent(sid, wsId, captured) || requestSeq !== forkMenu.requestSeq || !forkMenu.open) return;
    forkMenu.loading = false;
    setBanner("⚠ 加载 fork 候选失败：" + e.message, true);
    closeForkMenu();
  }
}

function closeForkMenu() {
  ++forkMenu.requestSeq;   // 让已发出的 candidates 响应失效（含 blur/Esc/导航关闭）
  if (!forkMenu.open) return;
  forkMenu.open = false;
  forkMenu.items = [];
  forkMenu.selected = 0;
  forkMenu.loading = false;
  if (els.forkMenu) els.forkMenu.hidden = true;
}

function renderForkMenu() {
  const menu = els.forkMenu;
  if (!menu) return;
  menu.innerHTML = "";
  if (forkMenu.loading) {
    menu.appendChild(el("div", "fork-loading", "加载中…"));
    menu.hidden = false;
    return;
  }
  if (!forkMenu.items.length) {
    menu.appendChild(el("div", "fork-empty", "没有可 fork 的边界消息"));
    menu.hidden = false;
    return;
  }
  forkMenu.items.forEach((c, i) => {
    const row = el("div", "fork-item" + (i === forkMenu.selected ? " selected" : ""));
    row.append(
      el("span", "fork-at", c.at != null ? String(c.at) : ""),
      el("span", "fork-preview", truncate(c.preview || "", 60)),
    );
    // mousedown 阻止默认：行点击不抢走输入框焦点（否则 blur 先关面板，click 落空）
    row.addEventListener("mousedown", (e) => { if (e.preventDefault) e.preventDefault(); });
    row.addEventListener("click", () => selectForkItem(i));
    menu.appendChild(row);
  });
  menu.hidden = false;
}

/* ↑↓ 移动选中（循环） */
function moveForkMenu(delta) {
  if (!forkMenu.open || !forkMenu.items.length) return;
  const n = forkMenu.items.length;
  forkMenu.selected = (forkMenu.selected + delta + n) % n;
  renderForkMenu();
}

/* 选中当前项（Enter/Tab/点击）→ POST /fork 建新会话 */
async function selectForkItem(i) {
  const item = forkMenu.items[i];
  if (!item) return;
  const sid = state.sessionId;
  const wsId = state.workspace.id;
  const captured = sessionOpenEpoch;
  const savedItems = forkMenu.items;       // 409 冲突时重开面板保留候选
  const savedSelected = forkMenu.selected;
  closeForkMenu();
  const requestSeq = forkMenu.requestSeq;   // 同一会话重新打开/重选也会取代旧 POST
  try {
    const res = await api("/api/sessions/" + encodeURIComponent(sid) + "/fork",
      { method: "POST", body: JSON.stringify({ at: item.at }) });
    if (!forkContextCurrent(sid, wsId, captured) || requestSeq !== forkMenu.requestSeq) return;
    if (res.status === 401 || res.status === 403) { setBanner("⚠ 认证失败：请检查 Token。", true); return; }
    if (res.status === 409) {
      // 后端拒绝（非边界/越界）：提示原因并重开面板，保留候选供重选
      let msg = "HTTP 409";
      try {
        const d = await res.json();
        if (!forkContextCurrent(sid, wsId, captured) || requestSeq !== forkMenu.requestSeq) return;
        if (d && typeof d.error === "string") msg = d.error;
        else if (d && typeof d.message === "string") msg = d.message;
      } catch (e) { /* 非 JSON 响应：用通用文本 */ }
      setBanner("⚠ " + msg, true);
      forkMenu.items = savedItems;
      forkMenu.selected = savedSelected;
      forkMenu.open = true;
      renderForkMenu();
      return;
    }
    if (!res.ok) throw new Error("HTTP " + res.status);
    const data = await res.json();
    if (!forkContextCurrent(sid, wsId, captured) || requestSeq !== forkMenu.requestSeq) return;
    const id = data && data.id;
    if (!id) throw new Error("响应缺少 id");
    // 成功：清空输入框，打开新会话并刷新侧边栏列表；banner 放最后——
    // openSession 内部会 refreshBanner()（token 就绪时清空横幅），先设会被抹掉
    els.promptInput.value = "";
    autosizeInput();
    openSession(id);
    refreshSessionsForSidebar();
    setBanner("已从历史 fork 出新会话：" + id);
  } catch (e) {
    if (!forkContextCurrent(sid, wsId, captured) || requestSeq !== forkMenu.requestSeq) return;
    setBanner("⚠ fork 失败：" + e.message, true);
  }
}

async function sendPrompt() {
  closeSlashMenu();                    // 发送（按钮/回车）时关闭菜单
  const raw = els.promptInput.value;   // 命令解析用原始输入（/rename 空标题=清除需区分尾部空格）
  const text = raw.trim();
  if (!text || !state.sessionId) return;
  // 斜杠命令：只拦截已知命令（与 TUI 语义一致），未知 /xxx 当普通消息发给模型。
  // 命中命令一律不 POST /prompt。
  if (text === "/compact") {
    // fire-and-forget：compactSession 自带 confirm、提交提示与失败 banner，无需 await
    compactSession();
    els.promptInput.value = "";
    autosizeInput();
    return;
  }
  if (raw === "/help") {
    // 命令列表较长，走 scrollback Notice（pre-wrap 多行）而非顶部 banner
    appendNotice([
      "/compact - 压缩上下文",
      "/rename <标题> - 重命名会话",
      "/btw <问题> - fork 旁路 subagent",
      "/fork - 从历史消息 fork",
      "/undo - 撤销文件操作",
    ].join("\n"));
    els.promptInput.value = "";
    autosizeInput();
    return;
  }
  if (raw === "/rename") {
    setBanner("用法：/rename <标题>（留空可清除）");
    return;   // 保留输入框，方便直接补参数
  }
  if (raw === "/model") {
    setBanner("用法：/model <profile>（如 /model chatgpt/sol）；切换后当前会话继续用新模型");
    return;   // 保留输入框，方便直接补参数
  }
  if (raw.startsWith("/model ")) {
    const profile = raw.slice("/model ".length).trim();
    if (!profile) {
      setBanner("用法：/model <profile>（如 /model chatgpt/sol）");
      return;
    }
    try {
      const res = await api("/api/sessions/" + encodeURIComponent(state.sessionId) + "/model",
        { method: "POST", body: JSON.stringify({ profile }) });
      if (res.status === 401 || res.status === 403) { setBanner("⚠ 认证失败：请检查 Token。", true); return; }
      if (res.status === 404 || res.status === 405) { setBanner("⚠ 服务器不支持 /model", true); return; }
      if (!res.ok) {
        // 400：profile 不存在/解析失败（server 返回纯文本错误）
        const text = await res.text().catch(() => "");
        setBanner("⚠ " + (text || ("HTTP " + res.status)), true);
        return;   // 保留输入框，方便改 profile
      }
      const data = await res.json();
      const model = data && data.model ? data.model : profile;
      // 成功：清空输入框，刷新列表让侧边栏/输入框元信息显示新模型
      els.promptInput.value = "";
      autosizeInput();
      setBanner("已切换到 " + model);
      refreshSessionsForSidebar();
    } catch (e) {
      setBanner("⚠ 切换模型失败：" + e.message, true);
    }
    return;
  }
  if (raw.startsWith("/rename ")) {
    const title = raw.slice("/rename ".length).trim();
    const cur = (state.lastList || []).find((x) => x.id === state.sessionId);
    if (!cur) {
      setBanner("⚠ 未找到当前会话，无法重命名。", true);
      return;   // 保留输入框
    }
    if (await saveTitle(cur, title)) {
      // 成功：清空输入框；空 title 即清除标题（saveTitle 内部落 null）
      els.promptInput.value = "";
      autosizeInput();
    }
    // 失败：saveTitle 内部已提示，保留输入框可重试
    return;
  }
  if (raw === "/btw") {
    setBanner("用法：/btw <问题>：从当前会话 fork 一个旁路 subagent 继续探讨");
    return;   // 保留输入框，方便直接补参数
  }
  if (raw.startsWith("/btw ")) {
    const question = raw.slice("/btw ".length).trim();
    if (!question) {
      setBanner("用法：/btw <问题>：从当前会话 fork 一个旁路 subagent 继续探讨");
      return;   // 保留输入框
    }
    try {
      const res = await api("/api/sessions/" + encodeURIComponent(state.sessionId) + "/btw",
        { method: "POST", body: JSON.stringify({ prompt: question }) });
      if (res.status === 401 || res.status === 403) { setBanner("⚠ 认证失败：请检查 Token。", true); return; }
      // 旧 server 无 /btw 端点：404/405 → 明确提示，其余按通用错误处理
      if (res.status === 404 || res.status === 405) { setBanner("⚠ 服务器不支持 /btw", true); return; }
      if (!res.ok) throw new Error("HTTP " + res.status);
      const data = await res.json();
      const id = data && data.id;
      if (!id) throw new Error("响应缺少 id");
      // 成功：清空输入框，拉最新列表让新 subagent 出现在侧边栏树里
      els.promptInput.value = "";
      autosizeInput();
      setBanner("已创建 btw subagent：" + id + "（侧边栏可切换）");
      refreshSessionsForSidebar();
    } catch (e) {
      setBanner("⚠ 创建 btw subagent 失败：" + e.message, true);
    }
    return;
  }
  if (raw === "/fork") {
    openForkMenu();
    return;   // 保留输入框；面板选中成功后才清空（selectForkItem）
  }
  if (!state.token) { setBanner("⚠ 请先输入 Token。", true); return; }
  // 防御：Finished 会话的按钮/输入框已被 applyStatus 禁用，这里再挡一道，
  // 避免陈旧状态或直接调用时对 finished 会话发出 409
  if (state.status && state.status.startsWith("Finished")) {
    setBanner("⚠ 会话已结束，无法发送消息。", true);
    return;
  }
  els.promptInput.value = "";
  autosizeInput();
  try {
    const res = await api("/api/sessions/" + encodeURIComponent(state.sessionId) + "/prompt",
      { method: "POST", body: JSON.stringify({ text }) });
    if (res.status === 401 || res.status === 403) { setBanner("⚠ 认证失败：请检查 Token。"); return; }
    if (res.status !== 202) throw new Error("HTTP " + res.status);
  } catch (e) {
    setBanner("⚠ 发送失败：" + e.message);
  }
}

async function cancelTurn() {
  if (!state.sessionId) return;
  try {
    const res = await api("/api/sessions/" + encodeURIComponent(state.sessionId) + "/cancel",
      { method: "POST" });
    if (!res.ok && res.status !== 202) throw new Error("HTTP " + res.status);
  } catch (e) {
    setBanner("⚠ 取消失败：" + e.message);
  }
}

async function compactSession() {
  if (!state.sessionId) return;
  if (!confirm("确认压缩该会话的上下文？")) return;
  try {
    const res = await api("/api/sessions/" + encodeURIComponent(state.sessionId) + "/compact",
      { method: "POST" });
    if (!res.ok && res.status !== 202) throw new Error("HTTP " + res.status);
    appendNotice("⏳ 压缩请求已提交…");
  } catch (e) {
    setBanner("⚠ 压缩失败：" + e.message);
  }
}

/* 输入框自动增高 */
function autosizeInput() {
  const t = els.promptInput;
  t.style.height = "auto";
  t.style.height = Math.min(t.scrollHeight, 160) + "px";
}

/* token 变化后重启传输（轮询 / SSE） */
function restartTransport() {
  // 每次 token 重连声明独立代次：即使 workspace/session 未变，上一 token 的
  // history/SSE 迟到响应也会因 captured !== sessionOpenEpoch 被丢弃。
  const claimed = ++sessionOpenEpoch;
  closeForkMenu();
  stopPolling();
  startPolling();
  pollSessions();
  if (state.sessionId) {
    // 重新走一遍打开流程（重新认证）
    const id = state.sessionId;
    stopSSE();
    state.initSource = null;
    openWith(id, true, undefined, state.workspace.id, claimed);
  }
  maybeHandleDeepLink();   // token 从空变有效：立即重试 pending 深链（不再等 poll round）
}

/* 聚合轮询：setTimeout 链（2s）驱动所有 workspace——一轮（并行拉取全部 +
   Promise.allSettled）完成才调度下一轮，天然防重入：慢响应不叠加、不并发
   轰炸。stopPolling 或条件不满足（聊天视图 + 侧边栏关闭）时不再续调度。
   立即轮询（pollSessions：restartTransport/switchWorkspace/
   refreshSessionsForSidebar/init 的即时刷新）与定时轮询共用同一个 in-flight
   守卫（runPollRound 串行链）：在途轮询未完成时，新请求不并发叠加——把
   「新鲜一轮」排队到在途轮询之后，多个并发请求合并为同一轮。 */
const POLL_INTERVAL_MS = 2000;

/* 侧边栏是唯一会话导航：无论抽屉是否展开都保持聚合轮询，让 busy、
   新会话和归档状态在下次打开侧边栏时已是最新。 */
function shouldPollSessions() { return true; }

/* 单一 in-flight 守卫（promise 串行链）：同一时刻只有一轮轮询在途。
   无在途轮询 → 立即启动一轮；有在途轮询 → 把「新鲜一轮」排队到其后
   （调用方要的是当下数据，不能共享一个可能已过时的在途轮询），多个并发
   请求合并为同一轮（同代排队复用）。排队 intent 携带 generation：在途
   结束后启动前校验 gen 仍有效——stopPolling（gen 递增）后旧 intent 作废
   被丢弃（不启动）；换代后的即时刷新（pollSessions）用新 gen 替换旧
   intent（仍保持全局 single-flight，不并发叠加）。 */
let pollRoundInFlight = null;   // 当前在途轮询的 Promise
let pollRoundQueued = null;     // 已排队的「新鲜一轮」Promise（在途结束后立即跑）
let pollRoundQueuedGen = -1;    // 排队 intent 的 generation（换代后替换而非复用）

function runPollRound() {
  if (pollRoundInFlight) {
    const gen = state.pollGen;
    if (!pollRoundQueued || pollRoundQueuedGen !== gen) {
      pollRoundQueuedGen = gen;
      pollRoundQueued = pollRoundInFlight.then(
        () => startPollRound(gen),
        () => startPollRound(gen)
      );
    }
    return pollRoundQueued;
  }
  return startPollRound(state.pollGen);
}

function startPollRound(gen) {
  pollRoundQueued = null;
  pollRoundQueuedGen = -1;
  if (gen !== state.pollGen) return;   // 过期 intent（stopPolling/换代后）→ 丢弃
  pollRoundInFlight = (async () => {
    try {
      await pollAllWorkspaces();
    } catch (e) {
      // 一轮内的渲染/校验异常（afterPollRound 抛错等）不外泄：轮询链继续，
      // 不产生未处理 rejection（断链防护在 pollRound 的 finally 兜底）
    } finally {
      pollRoundInFlight = null;
    }
  })();
  return pollRoundInFlight;
}

function startPolling() {
  stopPolling();
  state.pollTimer = setTimeout(pollRound, POLL_INTERVAL_MS);
}
function stopPolling() {
  state.pollGen++;   // 使在途轮询的 finally 续调度失效（stop 后不再续）
  if (state.pollTimer) { clearTimeout(state.pollTimer); state.pollTimer = null; }
}
async function pollRound() {
  const gen = state.pollGen;
  state.pollTimer = null;
  try {
    await runPollRound();
  } finally {
    // 无论成功/异常都续调度下一轮——防断链；期间 stopPolling（gen 变化）
    // 或条件不满足（聊天视图关侧边栏）→ 不续调度
    if (gen === state.pollGen && state.pollTimer === null && shouldPollSessions()) {
      state.pollTimer = setTimeout(pollRound, POLL_INTERVAL_MS);
    }
  }
}
/* delegate 任务 → 对应 subagent 会话 id。
   /api/tasks 的 delegate 条目 session_id 是父会话（任务注册在父的
   registry）；新版后端直接在条目上带 subagent_session_id，优先用它跳转，
   不再依赖 label 匹配（任务完成时 Greptime running_tasks 行被清除，
   /api/sessions 的 label 变 null，label 匹配永远失效）。
   旧后端回退：subagent 自己的会话出现在 /api/sessions，以
   parent_session_id === 父 id 且 label === 任务 label 关联。列表未加载
   或该 delegate 已结束（label 行被消费）→ null。 */
function resolveSubagentSessionId(t) {
  // 新版后端：任务条目直接带 subagent 会话 id，无需 label 匹配
  if (t && t.subagent_session_id) return t.subagent_session_id;
  // 旧后端回退：label 匹配（保留现状逻辑）
  if (!t || !t.session_id || !t.label) return null;
  const list = state.lastList || [];
  const hit = list.find((s) => s.parent_session_id === t.session_id && s.label === t.label);
  return hit ? hit.id : null;
}

/* 侧边栏开关状态跨刷新持久化（localStorage；隐私模式禁用时静默失败，不影响功能） */
function persistSidebarOpen() {
  try { localStorage.setItem("e-agent.sidebar.open", state.sidebar.open ? "1" : "0"); }
  catch (e) { /* 静默 */ }
}

function openSidebar() {
  if (state.sidebar.open) return;
  state.sidebar.open = true;
  persistSidebarOpen();                // 跨刷新保持打开状态
  renderSidebarTree(true);             // force：hidden 期间可能错过了轮询更新
  els.sidebarOverlay.hidden = false;
  els.sidebar.hidden = false;
  // 双 rAF：先让浏览器以收起位姿渲染一帧，再加 .open 触发过渡
  // （桌面：width 0→280px；手机：translateX(-100%)→0）
  requestAnimationFrame(() => requestAnimationFrame(() => els.sidebar.classList.add("open")));
  pollTasks();   // 打开时立即刷新树内任务分组（统一轮询常驻，这里只求即时性）
  refreshSessionsForSidebar();
}

function closeSidebar() {
  if (!state.sidebar.open) return;
  state.sidebar.open = false;
  persistSidebarOpen();                // 跨刷新保持关闭状态
  els.sidebarOverlay.hidden = true;
  els.sidebar.classList.remove("open");
  window.setTimeout(() => {
    if (!state.sidebar.open) els.sidebar.hidden = true;
  }, 220);
}

/* 聊天视图下补拉会话列表（聚合：一次拉全所有 workspace，侧边栏分组刷新；
   背景服务器失败只标记、不打断激活服务器）。失败静默保留旧数据。整轮
   完成后 afterPollRound 统一做聊天视图的同步（composer meta / 返回按钮）。
   经 runPollRound 与定时轮询共用 in-flight 守卫：在途轮询未完成时共享
   同一轮，不并发叠加。 */
async function refreshSessionsForSidebar() {
  if (!state.token) return;
  await runPollRound();
}
/* =====================================================================
 * 会话侧边栏：会话树
 * 主会话（parent_session_id 空）为根，默认折叠；subagent 挂父节点下
 * （缩进 + 「子」徽章），展开才渲染子节点；孤儿 subagent 归入「未关联」。
 * 每个父节点（及「未关联」组）默认只显示活跃 subagent（active !== false，
 * 即活跃或 busy；旧 server 无 active 字段 → 视为活跃）：非活跃 subagent
 * 收进折叠的「历史子会话 (N)」分组（点击展开，不持久化展开状态）。
 * subagent 标题优先用 label（running_tasks 任务面板标题），回退 title/id。
 * 默认只渲染最近 15 个主会话（列表按 last_active_at 降序），超出显示
 * 「+N 个更早的会话」按钮点击显示全部；子会话/孤儿组不计数。
 * 筛选时（state.sidebar.filter 非空）：主会话按 title/id 匹配才显示，
 * 匹配的显示其所有子会话；孤儿按自身 title/id 匹配。
 * 数据来自各 workspace 的列表（激活 = state.lastList，背景 =
 * workspaceLists[ws.id]，由 pollWorkspaceSessions 聚合轮询维护）。
 * 列表未变化时跳过重绘，保留展开状态与滚动位置。
 * ===================================================================*/
const MAX_TREE_ROOTS = 8;   // 每个 workspace 分组内默认只显示最近 8 个主会话（少滑即见）
const PIN_ORDER_KEY = "eagent.pinOrder";
let lastTreeSig = "";

/* 置顶分组是跨 workspace 聚合的，sid 不能单独作键。存储只保留最小的
   {wsId, sid} 有序对；localStorage 损坏/被禁用时静默回退到服务器列表顺序。 */
function readPinOrder() {
  try {
    const raw = JSON.parse(localStorage.getItem(PIN_ORDER_KEY) || "[]");
    if (!Array.isArray(raw)) return [];
    const seen = new Set();
    return raw.filter((x) => {
      if (!x || typeof x.wsId !== "string" || typeof x.sid !== "string") return false;
      const key = x.wsId + "\n" + x.sid;
      if (seen.has(key)) return false;
      seen.add(key);
      return true;
    }).map((x) => ({ wsId: x.wsId, sid: x.sid }));
  } catch (e) { return []; }
}

function writePinOrder(order) {
  try { localStorage.setItem(PIN_ORDER_KEY, JSON.stringify(order)); }
  catch (e) { /* 隐私模式/存储已满：本次拖拽仍可正常重绘 */ }
}

function pinOrderKey(wsId, sid) { return wsId + "\n" + sid; }

/* 已记录项按存储顺序在前；新置顶（存储中没有）保持现有聚合顺序并沉底。 */
function sortPinnedSessions(items) {
  const rank = new Map(readPinOrder().map((x, i) => [pinOrderKey(x.wsId, x.sid), i]));
  return items.map((x, i) => ({ x, i, rank: rank.get(pinOrderKey(x.ws.id, x.s.id)) }))
    .sort((a, b) => {
      const ar = a.rank, br = b.rank;
      if (ar === undefined && br === undefined) return a.i - b.i;
      if (ar === undefined) return 1;
      if (br === undefined) return -1;
      return ar - br;
    }).map((x) => x.x);
}

function removePinOrder(wsId, sid) {
  const old = readPinOrder();
  const next = old.filter((x) => !(x.wsId === wsId && (sid === undefined || x.sid === sid)));
  if (next.length !== old.length) writePinOrder(next);
}

function allPinnedSessions() {
  const out = [];
  for (const ws of state.workspaces) {
    for (const s of workspaceListFor(ws)) {
      // 归档 = 从活跃入口隐藏：pinned+archived 只出现在「归档」分组。
      if (s.pinned === true && s.archived !== true && isMainSession(s)) out.push({ ws, s });
    }
  }
  return sortPinnedSessions(out);
}

/* from 移到 target 的上/下方，并把当前全部置顶项写成完整顺序。作为纯排序
   入口也供 harness 直接调用，DOM 事件只负责算出 from/target。 */
function movePinnedSession(fromWsId, fromSid, targetWsId, targetSid, before) {
  const items = allPinnedSessions();
  const fromKey = pinOrderKey(fromWsId, fromSid);
  const targetKey = pinOrderKey(targetWsId, targetSid);
  const from = items.find((x) => pinOrderKey(x.ws.id, x.s.id) === fromKey);
  if (!from || fromKey === targetKey) return false;
  const rest = items.filter((x) => pinOrderKey(x.ws.id, x.s.id) !== fromKey);
  const targetAt = rest.findIndex((x) => pinOrderKey(x.ws.id, x.s.id) === targetKey);
  if (targetAt < 0) return false;
  rest.splice(targetAt + (before ? 0 : 1), 0, from);
  writePinOrder(rest.map((x) => ({ wsId: x.ws.id, sid: x.s.id })));
  renderSidebarTree(true);       // renderSidebarTree 自己保留 scrollTop / 展开状态
  return true;
}

/* Pointer Events 同时覆盖鼠标、触控和笔。触屏先长按 280ms；长按前移动
   超过阈值就按原方向滚动树。相比 HTML5 DnD，这条路径可在移动端工作，
   并且不会把行内 toggle/pin/archive 按钮的普通点击误判为拖拽。 */
function enablePinnedPointerDrag(row, node, wsId, sid) {
  let press = null;
  let dragging = false;
  let timer = null;
  let drop = null;
  let suppressClick = false;
  const clearDrop = () => {
    if (drop) drop.node.classList.remove("pin-drop-before", "pin-drop-after");
    drop = null;
  };
  const begin = (ev) => {
    dragging = true;
    node.classList.add("pin-dragging");
    row.setAttribute("aria-grabbed", "true");
    if (row.setPointerCapture) {
      try { row.setPointerCapture(ev.pointerId); } catch (e) { /* pointer may already be gone */ }
    }
  };
  row.classList.add("pin-draggable");
  row.tabIndex = 0;
  row.setAttribute("aria-grabbed", "false");
  row.setAttribute("aria-roledescription", "可排序的置顶会话");
  row.setAttribute("aria-keyshortcuts", "Alt+ArrowUp Alt+ArrowDown");
  row.title += " · 拖拽调整置顶顺序 · Alt+↑/↓ 键盘排序";
  // 键盘等价路径：焦点在行上时 Alt+↑/↓ 与前/后一项互换。
  row.addEventListener("keydown", (ev) => {
    if (!ev.altKey || (ev.key !== "ArrowUp" && ev.key !== "ArrowDown")) return;
    const body = node.parentNode;
    if (!body) return;
    const nodes = [...body.children];
    const at = nodes.indexOf(node);
    const nextAt = at + (ev.key === "ArrowUp" ? -1 : 1);
    if (nextAt < 0 || nextAt >= nodes.length) return;
    const target = nodes[nextAt];
    ev.preventDefault();
    ev.stopPropagation();
    movePinnedSession(wsId, sid, target.getAttribute("data-pin-ws"),
      target.getAttribute("data-pin-sid"), ev.key === "ArrowUp");
  });
  // capture 阶段挡掉拖拽/手动滚动后的合成 click，尤其避免从行内按钮起滑
  // 后误触取消置顶或归档。正常短按不受影响。
  row.addEventListener("click", (ev) => {
    if (!suppressClick) return;
    suppressClick = false;
    ev.preventDefault();
    ev.stopPropagation();
    if (ev.stopImmediatePropagation) ev.stopImmediatePropagation();
  }, true);
  row.addEventListener("pointerdown", (ev) => {
    if (ev.button !== undefined && ev.button !== 0) return;
    const onButton = !!(ev.target && ev.target.closest && ev.target.closest("button"));
    // 鼠标点按钮保持原交互；触屏按钮仍记录 pointer，以便手指从按钮起滑时
    // 手动滚动（row 的 touch-action:none 是可靠接管纵向拖拽所必需）。
    if (onButton && ev.pointerType !== "touch") return;
    press = { x: ev.clientX, y: ev.clientY, lastY: ev.clientY, id: ev.pointerId,
      touch: ev.pointerType === "touch", onButton, scrolling: false };
    if (press.touch && row.setPointerCapture) {
      try { row.setPointerCapture(ev.pointerId); } catch (e) { /* ignore */ }
    }
    if (press.touch && !onButton) timer = window.setTimeout(() => { if (press && !press.scrolling) begin(ev); }, 280);
  });
  row.addEventListener("pointermove", (ev) => {
    if (!press || ev.pointerId !== press.id) return;
    const distance = Math.hypot(ev.clientX - press.x, ev.clientY - press.y);
    if (!dragging) {
      if (press.touch) {
        if (distance > 8 || press.scrolling) {
          window.clearTimeout(timer); timer = null;
          press.scrolling = true;
          const tree = els.sidebarTree;
          if (tree) tree.scrollTop += press.lastY - ev.clientY;
          press.lastY = ev.clientY;
          ev.preventDefault();
        }
        return;
      }
      if (distance < 5) return;
      begin(ev);
    }
    ev.preventDefault();
    // 指针被 capture 后可继续拖到滚动容器边缘；轻量逐事件滚动，便于置顶项
    // 多于一屏时跨越可视区，不启用持续动画循环。
    const tree = els.sidebarTree;
    if (tree && tree.getBoundingClientRect) {
      const treeRect = tree.getBoundingClientRect();
      if (ev.clientY < treeRect.top + 28) tree.scrollTop -= 14;
      else if (ev.clientY > treeRect.bottom - 28) tree.scrollTop += 14;
    }
    clearDrop();
    const body = node.parentNode;
    for (const candidate of body.children) {
      if (candidate === node) continue;
      const targetRow = candidate.querySelector(".tree-row");
      if (!targetRow || !targetRow.getBoundingClientRect) continue;
      const rect = targetRow.getBoundingClientRect();
      if (ev.clientY < rect.top || ev.clientY > rect.bottom) continue;
      const before = ev.clientY < rect.top + rect.height / 2;
      candidate.classList.add(before ? "pin-drop-before" : "pin-drop-after");
      drop = { node: candidate, before,
        wsId: candidate.getAttribute("data-pin-ws"), sid: candidate.getAttribute("data-pin-sid") };
      break;
    }
  });
  const finish = (ev) => {
    if (!press || (ev.pointerId !== undefined && ev.pointerId !== press.id)) return;
    window.clearTimeout(timer); timer = null;
    const chosen = ev.type === "pointercancel" ? null : drop;
    const wasScrolling = press.scrolling;
    clearDrop();
    node.classList.remove("pin-dragging");
    row.setAttribute("aria-grabbed", "false");
    press = null;
    if (!dragging && !wasScrolling) return;
    suppressClick = true;
    window.setTimeout(() => { suppressClick = false; }, 0);
    if (!dragging) return;
    dragging = false;
    // pointerup 后浏览器还会合成 click；行 click 自身也有样式标记兜底。
    row.classList.add("pin-drag-click-block");
    window.setTimeout(() => row.classList.remove("pin-drag-click-block"), 0);
    if (chosen) movePinnedSession(wsId, sid, chosen.wsId, chosen.sid, chosen.before);
  };
  row.addEventListener("pointerup", finish);
  row.addEventListener("pointercancel", finish);
}

/* 侧边栏筛选匹配：置顶分组与 workspace 内普通根共用同一规则——title
   与完整 id 都参与匹配；子串匹配、大小写不敏感。filter 为空 → 全部匹配。 */
function treeSessionMatches(s, filter) {
  if (!filter) return true;
  const q = String(filter).toLowerCase();
  return [s.title, s.id].some((value) => String(value || "").toLowerCase().includes(q));
}

/* 聚合树签名：所有 workspace 的列表（激活 = lastList，背景 = 各自缓存）+
   激活标记 + 错误标记 + 当前会话 id + 树行渲染字段（含 model——树行
   tooltip 渲染 model，缺了它 model 变化不会触发重绘）。任一变化 → 重绘
   （保留展开/滚动状态）。 */
function sidebarTreeSig() {
  const parts = [];
  for (const ws of state.workspaces) {
    const list = workspaceListFor(ws);
    parts.push(ws.id + ":" + (ws === state.workspace ? 1 : 0) + ":"
      + (state.workspaceErrors[ws.id] || "") + ":"
      + (state.sidebar.showAllWs.has(ws.id) ? 1 : 0) + ":"
      + JSON.stringify(list.map((s) => [
        s.id, s.title || "", s.label || "", s.status, s.busy ? 1 : 0, s.active === false ? 0 : 1,
        s.entry_count ?? null, s.parent_session_id || "", s.pinned === true ? 1 : 0,
        s.archived === true ? 1 : 0, s.model || "",
      ])));
  }
  return state.sessionId + "|" + JSON.stringify(readPinOrder()) + "|" + parts.join("|");
}

/* force=true 时无视签名强制重绘（筛选输入、展开全部按钮、切换 workspace）
   聚合结构：每个 workspace 一个分组（.tree-ws-section），组头 = 服务器名 +
   徽章（点击切到该服务器；错误时附加 muted「无法连接」）；
   组内复用既有树逻辑（renderTreeForList：roots/orphans/MAX_TREE_ROOTS/
   「未关联」组/历史子会话组）。孤儿按各自 workspace 的列表判定，绝不跨
   服务器匹配 parent。 */
function renderSidebarTree(force) {
  const tree = els.sidebarTree;
  if (!tree) return;
  const sig = sidebarTreeSig();
  if (state.renameActive && !force) return;  // 保留正在编辑的标题输入框
  if (!force && sig === lastTreeSig) return;   // 数据未变：保留展开/滚动状态
  lastTreeSig = sig;
  const prevScroll = tree.scrollTop;
  tree.innerHTML = "";
  // ---- 置顶分组：所有 workspace 的 pinned 会话集中到最顶上 ----
  // （跨 workspace 聚合；点击自动切到所属 server 并打开。workspace 内
  //  不再重复渲染 pinned——buildTreeRoot 的 .pinned 样式标记仍保留。）
  //  筛选非空时与普通根同一 title/id 匹配规则：仅匹配的置顶根显示、随父
  //  展示子会话；不匹配的隐藏（workspace 内剔除逻辑不变——剔除后置顶分组
  //  是它们唯一出现位，不匹配则整体不显示）。
  const filter = state.sidebar.filter;
  // 先按完整置顶集合排序再筛选；这样筛选不会改变隐藏项的相对位置。
  // （allPinnedSessions 已排除 archived：归档会话只出现在「归档」分组。）
  const pinned = allPinnedSessions().filter(({ s }) => treeSessionMatches(s, filter));
  if (pinned.length) {
    const pinnedSec = el("div", "tree-ws-section pinned");
    const pinnedBody = el("div", "tree-ws-body");
    for (const { ws, s } of pinned) {
      // 按所属 workspace 构建子会话映射（pinned 根的子会话跟随置顶分组）
      const wsList = workspaceListFor(ws);
      const kidsByParent = new Map();
      for (const x of wsList) {
        // 已归档子会话只出现在 workspace 的「归档」分组，不能同时跟随
        // pinned 根进入置顶聚合分组。
        if (!x.parent_session_id || x.archived === true) continue;
        if (!kidsByParent.has(x.parent_session_id)) kidsByParent.set(x.parent_session_id, []);
        kidsByParent.get(x.parent_session_id).push(x);
      }
      const node = buildTreeRoot(s, kidsByParent.get(s.id) || [], ws.id);
      // 置顶聚合行：workspace 小字跟在标题文字后面（标题行内，下一行是
      // 完整 session id）——有标题跟标题，无标题跟 id；复用 ws-chip-N 分色。
      const marker = el("span", "ws-pin-label " + wsChipClass(ws), ws.name || ws.url);
      node.setAttribute("data-pin-ws", ws.id);
      node.setAttribute("data-pin-sid", s.id);
      const row = node.querySelector(".tree-row");
      // 筛选态不开放拖拽：只看见子集时重排会让插入位置含糊。
      if (row && !filter) enablePinnedPointerDrag(row, node, ws.id, s.id);
      const titleEl = row && row.querySelector(".tree-id");
      if (titleEl) {
        const titleLine = titleEl.querySelector(".tree-title") || titleEl;
        titleLine.appendChild(marker);
      }
      pinnedBody.appendChild(node);
    }
    pinnedSec.appendChild(pinnedBody);
    tree.appendChild(pinnedSec);
  }
  for (const ws of state.workspaces) {
    const list = workspaceListFor(ws);
    const err = state.workspaceErrors[ws.id] || null;
    const sec = el("div", "tree-ws-section" + (ws === state.workspace ? " active" : ""));
    // ---- 组头：服务器名 + 徽章；点击切换 workspace（switchWorkspace 切换服务器） ----
    const header = el("div", "tree-ws-header");
    header.title = ws.url || ws.name;          // hover 显示服务器地址
    const chip = el("span", "ws-chip " + wsChipClass(ws), ws.name);
    header.appendChild(chip);
    if (err) {
      header.appendChild(el("span", "ws-err", "无法连接"));
      header.title += "（" + err + "）";
    }
    if (ws === state.workspace) {
      header.appendChild(el("span", "ws-cur", "当前"));
      sec.classList.add("active");
    }
    // 组头「+」：在该 workspace 新建会话（无 initial_prompt；请求打到该
    // workspace 而非激活的——聚合模式下「在某台服务器上新建」必须打给那台）。
    // 在途防重：请求期间按钮禁用 + 忽略重复点击（pending 标志挂在 state，
    // 不挂按钮元素——树随轮询重绘，元素会被替换）。
    const addBtn = el("button", "ws-add", "+");
    addBtn.title = "在 " + (ws.name || ws.url) + " 新建会话";
    addBtn.disabled = state.wsCreatePending.has(ws.id);
    addBtn.addEventListener("click", (ev) => {
      ev.stopPropagation();                 // 不触发组头切换
      if (state.wsCreatePending.has(ws.id)) return;   // 在途：忽略重复点击
      state.wsCreatePending.add(ws.id);
      addBtn.disabled = true;
      createSessionIn(ws).finally(() => {
        state.wsCreatePending.delete(ws.id);
        addBtn.disabled = false;   // 期间未重绘时恢复按钮（重绘后由 disabled 属性重新绑定）
      });
    });
    header.appendChild(addBtn);
    if (state.workspaces.length > 1) {
      // 组头删除按钮：出错/无法连接的服务器也能在侧边栏直接移除
      //（顶部 × 只删当前激活的，切不过去就删不掉——这是它的逃生口）。
      const delBtn = el("button", "ws-del", "×");
      delBtn.title = "删除服务器 " + (ws.name || ws.url);
      delBtn.addEventListener("click", (ev) => {
        ev.stopPropagation();                 // 不触发组头切换
        if (!confirm("确认删除服务器 " + (ws.name || ws.url) + " ？")) return;
        removeWorkspace(ws);
      });
      header.appendChild(delBtn);
    }
    header.addEventListener("click", () => {
      if (ws !== state.workspace) switchWorkspace(ws.id);   // 已激活则无操作
    });
    sec.appendChild(header);
    // ---- 组体：既有树逻辑（per-workspace） ----
    const body = el("div", "tree-ws-body");
    if (!list.length) {
      body.appendChild(el("div", "tree-empty",
        err ? "无法连接，等待重试…" : "暂无会话"));
    } else {
      renderTreeForList(body, list, ws.id);
    }
    sec.appendChild(body);
    tree.appendChild(sec);
  }
  tree.scrollTop = prevScroll;
}

/* 主会话判定：无 parent 且 id 不以 sub-/btw- 开头。历史脏数据里存在
   parent 丢失的孤儿 subagent（sub-20260729-* 等），无 parent 会被误判
   为主会话 → 前缀保险：这类会话归入「未关联」组，不占主会话位。
   置顶分组与 workspace 内剔除共用同一判定：pinned 子会话留在其父节点
   下，不会既进置顶分组（当根渲染）又留在 workspace 内（重复）。 */
function isMainSession(s) {
  return !s.parent_session_id && !/^(sub|btw)-/i.test(String(s.id || ""));
}

/* 单个 workspace 的树渲染（原 renderSidebarTree 主体，抽出来按 ws 复用）：
   roots/orphans/MAX_TREE_ROOTS/hist-groups 全部按本列表判定；wsId 用于
   expanded 键限定（wsId:sid）与 .current 高亮（只高亮激活 workspace 内
   当前会话）。 */
function renderTreeForList(container, list, wsId) {
  if (!list.length) {
    container.appendChild(el("div", "tree-empty", "暂无会话"));
    return;
  }
  // 服务端把 archived 沉底排序（server.rs 排序：pinned → unarchived →
  // archived），若直接按数组顺序取窗口，归档腾出的位会被更早的会话顶上
  // （「归档后侧边栏不变短反而冒出别的会话」）。这里按最近活跃重排（含
  // 归档），让归档会话保留在原时间槽位；窗口取槽位时归档的不顶替。
  const sorted = [...list].sort((a, b) =>
    String(b.last_active_at || b.created_at || "").localeCompare(String(a.last_active_at || a.created_at || "")));
  const childrenByParent = new Map();
  for (const s of sorted) {
    if (!s.parent_session_id) continue;
    if (!childrenByParent.has(s.parent_session_id)) childrenByParent.set(s.parent_session_id, []);
    childrenByParent.get(s.parent_session_id).push(s);
  }
  // 归档分组：归档会话（archived === true）连同其子会话一起收进折叠的
  // 「归档」分组，不参与主会话/未关联/MAX_TREE_ROOTS 的常规渲染。
  const archivedIds = new Set(sorted.filter((s) => s.archived === true).map((s) => s.id));
  const inArchive = (s) => s.archived === true
    || (s.parent_session_id && archivedIds.has(s.parent_session_id));
  const archivedSessions = sorted.filter(inArchive);
  const rest = sorted.filter((s) => !inArchive(s));
  // pinned 根节点已在置顶分组渲染，这里从 workspace 内剔除（其子会话
  // 跟随剔除——置顶分组里 buildTreeRoot 会带出它们的子会话）。
  const pinnedRootIds = new Set(rest.filter((s) => s.pinned === true && isMainSession(s)).map((s) => s.id));
  const inPinnedSubtree = (s) => pinnedRootIds.has(s.id)
    || (s.parent_session_id && pinnedRootIds.has(s.parent_session_id));
  const listForWorkspace = rest.filter((s) => !inPinnedSubtree(s));
  const rootIds = new Set(listForWorkspace.filter(isMainSession).map((s) => s.id));
  const orphans = listForWorkspace.filter((s) => s.parent_session_id
    ? !rootIds.has(s.parent_session_id)
    : !isMainSession(s));
  const filter = state.sidebar.filter;
  const roots = listForWorkspace.filter(isMainSession);
  // 普通主根的子会话排除已归档的（它们收进「归档」分组；否则会同时出现在
  // 父节点子树和归档分组里，重复渲染）。
  const kidsFor = (s) => (childrenByParent.get(s.id) || []).filter((k) => !inArchive(k));
  if (filter) {
    // 筛选：主会话匹配 title（无 title 回退 id），大小写不敏感；子会话随父显示
    const match = (s) => treeSessionMatches(s, filter);
    const matchedRoots = roots.filter(match);
    const matchedOrphans = orphans.filter(match);
    const matchedArchived = archivedSessions.filter(match);
    if (!matchedRoots.length && !matchedOrphans.length && !matchedArchived.length) {
      container.appendChild(el("div", "tree-empty", "无匹配会话"));
    } else {
      for (const s of matchedRoots) container.appendChild(buildTreeRoot(s, kidsFor(s), wsId));
      if (matchedOrphans.length) container.appendChild(buildTreeGroup("未关联", matchedOrphans, wsId));
      if (matchedArchived.length) container.appendChild(buildArchiveGroup(matchedArchived, wsId, childrenByParent));
    }
  } else {
    let shown = roots;
    let moreBtn = null;
    const totalRootSlots = sorted.filter((s) => isMainSession(s) && !inPinnedSubtree(s)).length;
    if (!state.sidebar.showAllWs.has(wsId) && totalRootSlots > MAX_TREE_ROOTS) {
      // 槽位法窗口：取最近 MAX_TREE_ROOTS 个主会话槽位（含归档，sorted
      // 已按时间重排），归档会话的槽位空着、不顶替——归档后侧边栏真实
      // 变短，不会冒出更早的会话。
      const slots = sorted.filter((s) => isMainSession(s) && !inPinnedSubtree(s))
        .slice(0, MAX_TREE_ROOTS);
      shown = slots.filter((s) => !inArchive(s));
      const more = roots.length - shown.length;
      moreBtn = el("button", "tree-more", "+" + more + " 个更早的会话");
      moreBtn.type = "button";
      moreBtn.title = "显示全部主会话";
      moreBtn.addEventListener("click", () => {
        state.sidebar.showAllWs.add(wsId);   // 只展开本 workspace 分组
        renderSidebarTree(true);
      });
    }
    for (const s of shown) container.appendChild(buildTreeRoot(s, kidsFor(s), wsId));
    if (moreBtn) container.appendChild(moreBtn);
    if (orphans.length) container.appendChild(buildTreeGroup("未关联", orphans, wsId));
    if (archivedSessions.length) container.appendChild(buildArchiveGroup(archivedSessions, wsId, childrenByParent));
  }
}

/* 删除会话：侧边栏是唯一导航，所有会话树行都提供该操作。
   按 workspace 定位缓存，避免跨服务器同名 session id 误删。 */
function treeDeleteButton(s, wsId) {
  const del = el("button", "tree-del danger", "×");
  del.type = "button";
  del.title = "删除会话 " + s.id;
  del.setAttribute("aria-label", del.title);
  del.addEventListener("click", async (ev) => {
    ev.stopPropagation();
    if (!confirm("确认删除会话 " + s.id + " ？")) return;
    const ws = state.workspaces.find((w) => w.id === wsId);
    if (!ws) return;
    del.disabled = true;
    try {
      const res = await apiFor(ws, "/api/sessions/" + encodeURIComponent(s.id), { method: "DELETE" });
      if (res.status === 401 || res.status === 403) {
        setBanner("⚠ 认证失败：请检查 Token。");
        return;
      }
      if (!res.ok && res.status !== 204) throw new Error("HTTP " + res.status);
      const list = workspaceListFor(ws);
      const at = list.findIndex((x) => x.id === s.id);
      if (at >= 0) list.splice(at, 1);
      if (ws === state.workspace) {
        // 删除当前会话后不制造另一种导航态：留在聊天空状态，并打开侧边栏。
        if (state.sessionId === s.id) clearCurrentSession();
        // clearCurrentSession 会先保存离开时视图；删除成功后再清，避免复活缓存。
        delete state.sessionStates[s.id];
      }
      removePinOrder(ws.id, s.id);
      renderSidebarTree(true);
    } catch (e) {
      setBanner("⚠ 删除失败：" + e.message);
    } finally {
      if (del.isConnected) del.disabled = false;
    }
  });
  return del;
}

function clearCurrentSession() {
  ++sessionOpenEpoch;
  saveSessionState();
  stopTaskRows();
  closeForkMenu();
  stopSSE();
  state.sessionId = null;
  state.acc = null;
  state.nextBeforeSeq = null;
  state.loadingOlder = false;
  state.olderDone = false;
  els.chatView.classList.add("no-session");
  els.messages.innerHTML = "";
  els.promptInput.value = "";
  els.backParentBtn.hidden = true;
  els.chatSessionId.textContent = "";
  els.usageInfo.textContent = "";
  history.replaceState(null, "", "/");
  applyStatus("Idle");
  if (!state.sidebar.open) openSidebar();
}

function buildTreeRoot(s, kids, wsId) {
  const node = el("div", "tree-node");
  const row = el("div", "tree-row"
    + (state.workspace.id === wsId && state.sessionId === s.id ? " current" : "")
    + (s.pinned === true ? " pinned" : "")
    + (s.archived === true ? " archived" : ""));
  const hasKids = kids.length > 0;
  const toggle = el("button", "tree-toggle");
  toggle.type = "button";
  toggle.disabled = !hasKids;      // 无子会话时留位（占 24px，不响应）
  if (hasKids) {
    toggle.title = "展开 / 收起子会话";
    toggle.addEventListener("click", (ev) => {
      ev.stopPropagation();          // 点击箭头只展开/收起，不切换会话
      toggleSidebarNode(wsId + ":" + s.id, toggle, kids, wsId);
    });
  }
  // 活跃子会话聚合只看直接子会话（subagent 通常一层，孙会话不计）：
  // 后端 running_tasks 分不清 Idle/Busy——live subagent（label 在）只有
  // active=true、busy 恒为 false（apply_subagent_label 只设 active）。
  // 所以用 active===true 统计"存活/运行中的子 agent 数"（包括等待输入的
  // Idle subagent——它们确实在工作流里活着）。数字优先于父节点的点。
  const busyKidCount = kids.filter((k) => k.active === true).length;
  const kidsBusy = busyKidCount > 0;
  const dot = busyKidCount > 0
    ? el("span", "busy-dot busy-count", String(busyKidCount))
    : el("span", "busy-dot" + (s.busy ? " busy" : ""));
  dot.setAttribute("aria-label", busyKidCount > 0
    ? busyKidCount + " 个子任务处理中"
    : (s.busy ? "会话处理中" : "会话空闲"));
  // 有标题：两行（title 行 + 完整 id 行）；无标题：一行完整 id。
  // 单行 ellipsis 截断的长 id 无法区分会话，双行让 title/id 都可见。
  const hasTitle = !!s.title;
  const titleEl = el("span", "tree-id" + (hasTitle ? " has-title" : ""),
    hasTitle ? "" : s.id);
  titleEl.title = s.title || s.id;        // hover 显示完整 title/id
  if (hasTitle) {
    titleEl.append(
      el("span", "tree-title", s.title),
      el("span", "tree-idline", s.id),
    );
  }
  const count = el("span", "tree-count", (s.entry_count ?? 0) + " 条");
  // 📌 置顶按钮（仅主会话根节点）：放行尾 count 后。subagent 子节点不加——
  // pin 是会话级操作，subagent 的置顶语义后续需要时再单独支持。
  const pin = el("button", "pin-btn" + (s.pinned === true ? " on" : ""));
  pin.innerHTML = pinSvg();   // SVG 图钉：状态色跟随 currentColor
  pin.type = "button";
  pin.title = s.pinned === true ? "取消置顶" : "置顶";
  pin.setAttribute("aria-label", pin.title);
  pin.setAttribute("aria-pressed", String(s.pinned === true));
  pin.addEventListener("click", (ev) => {
    ev.stopPropagation();                  // 不触发切换会话
    const ws = state.workspaces.find((w) => w.id === wsId);
    togglePin(s, () => renderSidebarTree(true), ws);
  });
  // 🗄 归档按钮（仅主会话根节点，与 pin 同规则）：归档会话收进「归档」
  // 分组；在分组里点按钮 = 恢复。
  const archive = el("button", "archive-btn" + (s.archived === true ? " on" : ""));
  archive.innerHTML = archiveSvg();   // SVG 归档盒：状态色跟随 currentColor
  archive.type = "button";
  archive.title = s.archived === true ? "恢复（取消归档）" : "归档";
  archive.setAttribute("aria-label", archive.title);
  archive.setAttribute("aria-pressed", String(s.archived === true));
  archive.addEventListener("click", (ev) => {
    ev.stopPropagation();                  // 不触发切换会话
    const ws = state.workspaces.find((w) => w.id === wsId);
    toggleArchived(s, () => renderSidebarTree(true), ws);
  });
  row.append(toggle, dot, titleEl, count, pin, archive, treeDeleteButton(s, wsId));
  row.title = (s.title || s.id) + (s.model ? " · " + s.model : "")
    + (s.busy ? "（处理中）" : "")
    + (kidsBusy ? "（子任务处理中）" : "");
  row.addEventListener("click", (ev) => {
    if (row.classList.contains("pin-drag-click-block")) {
      if (ev) { ev.preventDefault(); ev.stopPropagation(); }
      return;
    }
    if (s.active === false) { resumeSessionIn(wsId, s.id); return; }   // 历史会话先恢复
    openSessionIn(wsId, s.id);
  });
  node.appendChild(row);
  if (hasKids) {
    const children = el("div", "tree-children");
    children.hidden = true;
    // 筛选时匹配的父节点直接展开显示全部子会话；否则按展开状态
    // （expanded 键 workspace 限定：wsId:sid，跨服务器不撞名）
    const showKids = !!state.sidebar.filter || state.sidebar.expanded.has(wsId + ":" + s.id);
    if (showKids) {
      children.hidden = false;
      toggle.classList.add("open");
      renderTreeChildren(children, kids, wsId);
    }
    node.appendChild(children);
  }
  return node;
}

function toggleSidebarNode(key, toggle, kids, wsId) {
  const children = toggle.closest(".tree-node").querySelector(".tree-children");
  if (!children) return;
  if (children.hidden) {
    children.hidden = false;
    toggle.classList.add("open");
    renderTreeChildren(children, kids, wsId);   // 展开时才渲染子节点（400+ 会话不拖慢树）
    state.sidebar.expanded.add(key);
  } else {
    children.hidden = true;
    toggle.classList.remove("open");
    state.sidebar.expanded.delete(key);
  }
}

/* 父节点 / 「未关联」组的子节点渲染：默认只显示正在运行的 subagent；
   idle（无论 active 与否）都收进折叠的「历史子会话 (N)」分组
   （buildHistGroup，默认收起、点击展开）。 */
function renderTreeChildren(container, kids, wsId) {
  container.innerHTML = "";
  const busy = [], hist = [];
  for (const k of kids) (k.busy ? busy : hist).push(k);
  renderSubagentRows(container, busy, false, wsId);
  if (hist.length) container.appendChild(buildHistGroup(hist, wsId));
}

/* 渲染 subagent 行（不做活跃/历史分组）；hist=true 时行灰显小字 */
function renderSubagentRows(container, kids, hist, wsId) {
  for (const k of kids) {
    const row = el("div", "tree-row tree-row-child" + (hist ? " tree-hist" : "") +
      (state.workspace.id === wsId && state.sessionId === k.id ? " current" : ""));
    const dot = el("span", "busy-dot" + (k.busy ? " busy" : ""));
    // label 优先：subagent 的任务面板标题最友好；旧 server 无 label → 回退 title/id
    // 有 label/title：两行（label/title 行 + 完整 id 行）；无则一行完整 id。
    const hasTitle = !!(k.label || k.title);
    const titleEl = el("span", "tree-id" + (hasTitle ? " has-title" : ""),
      hasTitle ? "" : k.id);
    titleEl.title = k.label || k.title || k.id;
    if (hasTitle) {
      titleEl.append(
        el("span", "tree-title", k.label || k.title),
        el("span", "tree-idline", k.id),
      );
    }
    const badge = el("span", "child-badge", "子");
    row.append(dot, titleEl, badge, treeDeleteButton(k, wsId));
    // busy 的 subagent：title 提示可发送消息（点击行 openSession 是现有行为，保持不变）
    row.title = (k.label || k.title || k.id) + (k.busy ? "（处理中）· 可发送消息" : "");
    row.addEventListener("click", () => {
      if (k.active === false) { resumeSessionIn(wsId, k.id); return; }
      openSessionIn(wsId, k.id);
    });
    container.appendChild(row);
  }
}

/* 「历史子会话 (N)」折叠分组：非活跃 subagent 默认收起，点击展开；
   不持久化展开状态（每次重绘默认折叠，简单）。 */
function buildHistGroup(kids, wsId) {
  const node = el("div", "tree-node");
  const row = el("div", "tree-row tree-hist-row");
  const toggle = el("button", "tree-toggle");
  toggle.type = "button";
  toggle.title = "展开 / 收起";
  toggle.addEventListener("click", (ev) => toggleTreeGroup(ev, toggle));
  const idEl = el("span", "tree-id tree-group tree-hist-label", "历史子会话 (" + kids.length + ")");
  row.append(toggle, idEl);
  node.appendChild(row);
  const children = el("div", "tree-children");
  children.hidden = true;
  renderSubagentRows(children, kids, true, wsId);
  node.appendChild(children);
  return node;
}

/* 「未关联」分组：孤儿 subagent 的根节点，默认折叠 */
function buildTreeGroup(label, kids, wsId) {
  const node = el("div", "tree-node");
  const row = el("div", "tree-row");
  const toggle = el("button", "tree-toggle");
  toggle.type = "button";
  toggle.title = "展开 / 收起";
  toggle.addEventListener("click", (ev) => toggleTreeGroup(ev, toggle));
  const idEl = el("span", "tree-id tree-group", label);
  row.append(toggle, idEl);
  node.appendChild(row);
  const children = el("div", "tree-children");
  children.hidden = true;   // 「未关联」分组默认折叠，点击展开（不持久化）
  renderTreeChildren(children, kids, wsId);
  node.appendChild(children);
  return node;
}

/* 「归档 (N)」折叠分组：归档会话（主会话 + 其子会话 + 归档孤儿）默认
   收起，点击展开；分组内主会话行保留 buildTreeRoot（含 pin/归档按钮，
   可直接恢复），孤儿子会话用 renderSubagentRows 渲染。点击分组内会话
   行可正常打开（与普通树行一致）。不持久化展开状态（每次重绘默认折叠）。 */
function buildArchiveGroup(archivedSessions, wsId, childrenByParent) {
  const archivedIds = new Set(archivedSessions.map((s) => s.id));
  const isMain = (s) => !s.parent_session_id && !/^(sub|btw)-/i.test(String(s.id || ""));
  // 根：无 parent 或 parent 不在归档集合内（归档孤儿）。主会话根用
  // buildTreeRoot 渲染（子会话随父显示在根的子树里）；非主根用 subagent 行。
  const roots = archivedSessions.filter((s) => !s.parent_session_id || !archivedIds.has(s.parent_session_id));
  const mainRootIds = new Set(roots.filter(isMain).map((s) => s.id));
  // 其余：parent 在归档集合内且父不是 buildTreeRoot 渲染的主根——否则会
  // 与根节点子树里的子会话重复渲染。
  const subRows = archivedSessions.filter((s) => s.parent_session_id
    && archivedIds.has(s.parent_session_id)
    && !mainRootIds.has(s.parent_session_id));
  const node = el("div", "tree-node");
  const row = el("div", "tree-row tree-archive-row");
  const toggle = el("button", "tree-toggle");
  toggle.type = "button";
  toggle.title = "展开 / 收起";
  toggle.addEventListener("click", (ev) => toggleTreeGroup(ev, toggle));
  const idEl = el("span", "tree-id tree-group tree-archive-label",
    "归档 (" + archivedSessions.length + ")");
  row.append(toggle, idEl);
  node.appendChild(row);
  const children = el("div", "tree-children");
  children.hidden = true;
  for (const s of roots) {
    if (!isMain(s)) { renderSubagentRows(children, [s], true, wsId); continue; }
    children.appendChild(buildTreeRoot(s, childrenByParent.get(s.id) || [], wsId));
  }
  if (subRows.length) renderSubagentRows(children, subRows, true, wsId);
  node.appendChild(children);
  return node;
}

/* 分组折叠切换（未关联 / 历史子会话）：默认收起，点击展开，不持久化 */
function toggleTreeGroup(ev, toggle) {
  ev.stopPropagation();
  const children = toggle.closest(".tree-node").querySelector(".tree-children");
  if (children.hidden) {
    children.hidden = false;
    toggle.classList.add("open");
  } else {
    children.hidden = true;
    toggle.classList.remove("open");
  }
}
/* =====================================================================
 * 会话重命名（✎ → PUT /api/sessions/{id}/title）
 * 侧边栏树节点与聊天标题共用 enterRename：点击标题后文本原位换成
 * <input> + ✓/×，box 上的点击 stopPropagation 屏蔽行点击（打开会话）。
 * Enter 保存、Esc 取消；空输入保存 = 清除标题（树/聊天标题回退显示 id）。
 * 旧服务器没有该端点：404/405/409 统一提示「服务器不支持重命名」。
 * 失败提示走顶部 banner（setBanner，与应用其余错误一致）而非行内红字：
 * 实现简单、不打断编辑流程——编辑框保留，可改完重试。
 * ===================================================================*/
function enterRename(titleEl, s, afterSave) {
  const box = el("span", "rename-box");
  const input = document.createElement("input");
  input.type = "text";
  input.className = "rename-input";
  input.value = s.title || "";
  input.spellcheck = false;
  input.title = "Enter 保存，Esc 取消；留空保存 = 清除标题";
  const save = el("button", "rename-save", "✓");
  save.type = "button";
  save.title = "保存";
  const cancel = el("button", "rename-cancel", "×");
  cancel.type = "button";
  cancel.title = "取消";
  box.append(input, save, cancel);
  titleEl.replaceWith(box);

  // 编辑态内的任何点击都不冒泡到行（行点击 = 打开会话）
  box.addEventListener("click", (ev) => ev.stopPropagation());

  const leave = () => {
    state.renameActive = false;
    box.replaceWith(titleEl);   // 取消：恢复原标题元素（若已随重绘 detached，安全无操作）
  };

  const doSave = async () => {
    const val = input.value.trim();
    input.disabled = true;      // 防重复提交
    // 树 DOM 可能持有轮询前旧数组里的对象引用（会话数据未变时 renderSidebarTree
    // 被签名跳过、不重绘，但 state.lastList 每次轮询都是新数组）：保存时按 id
    // 从 state.lastList 重新解析，确保写回的是当前渲染所用的对象
    const cur = (state.lastList || []).find((x) => x.id === s.id) || s;
    const ok = await saveTitle(cur, val);
    if (!ok) { input.disabled = false; input.focus(); return; }   // 失败：保留编辑框可重试
    state.renameActive = false;
    afterSave();                // 成功：调用方重绘树或聊天标题
  };

  save.addEventListener("click", () => doSave());
  cancel.addEventListener("click", () => leave());
  input.addEventListener("keydown", (ev) => {
    ev.stopPropagation();       // 不冒泡：Esc 不触发关侧边栏等全局处理
    if (ev.key === "Enter") { ev.preventDefault(); doSave(); }
    else if (ev.key === "Escape") { ev.preventDefault(); leave(); }
  });
  input.focus();
  input.select();
  state.renameActive = true;    // 编辑期间轮询树重绘跳过，避免编辑框被冲掉
}

/* PUT 保存标题；成功写回传入的会话对象（即 state.lastList 里的对象，
   空=清除 → null，树/聊天标题回退显示 id）。返回是否成功（false 时调用方
   保留编辑框）。 */
async function saveTitle(s, newTitle) {
  try {
    const res = await api("/api/sessions/" + encodeURIComponent(s.id) + "/title",
      { method: "PUT", body: JSON.stringify({ title: newTitle }) });
    if (res.status === 401 || res.status === 403) {
      setBanner("⚠ 认证失败：请检查 Token。");
      return false;
    }
    if (!res.ok) {
      // 旧服务器没有 title 端点：axum 对未知路径回 404、对已知路径错方法回 405；
      // 409 归入同一类，统一提示不支持重命名
      if (res.status === 404 || res.status === 405 || res.status === 409) {
        setBanner("⚠ 服务器不支持重命名。", true);
      } else {
        setBanner("⚠ 重命名失败：HTTP " + res.status, true);
      }
      return false;
    }
    s.title = newTitle || null;
    return true;
  } catch (e) {
    setBanner("⚠ 重命名失败：" + e.message, true);
    return false;
  }
}

/* PUT 切换置顶（📌 → PUT /api/sessions/{id}/pin {"pinned": bool}）；成功写回
   目标 workspace 会话对象（workspaceListFor）并回调重绘树。旧服务器没有 pin 端点：404/405 统一提示「服务器不支持置顶」
   （与重命名一致）；pinned 字段缺失（undefined）视为未置顶。
   会话顺序信任后端（pinned 置顶在前、组内 last_active_at 降序），前端不重排。
   ws 可选（跨服务器树节点传所属 workspace；缺省 = 激活 workspace，
   既有调用不变）：请求打到目标服务器，避免背景服务器的置顶打到激活服务器。 */
async function togglePin(s, afterToggle, ws) {
  const targetWs = ws || state.workspace;
  if (!workspaceToken(targetWs)) { setBanner("⚠ 请先输入 Token。", true); return; }
  const target = s.pinned !== true;   // 兼容旧 server 无 pinned 字段：undefined → 置顶
  try {
    const res = await apiFor(targetWs, "/api/sessions/" + encodeURIComponent(s.id) + "/pin",
      { method: "PUT", body: JSON.stringify({ pinned: target }) });
    if (res.status === 401 || res.status === 403) {
      setBanner("⚠ 认证失败：请检查 Token。");
      return;
    }
    if (!res.ok) {
      if (res.status === 404 || res.status === 405) {
        setBanner("⚠ 服务器不支持置顶。", true);
      } else {
        setBanner("⚠ 置顶失败：HTTP " + res.status, true);
      }
      return;
    }
    // 轮询可能已换掉缓存里的对象引用（会话数据未变时树不重绘，
    // 但数组每轮都是新的）：按 id 重新解析当前对象再写回，确保重绘反映新状态
    const cur = workspaceListFor(targetWs).find((x) => x.id === s.id) || s;
    cur.pinned = target;
    // 取消后移除；重新置顶也先当作新项，按当前聚合顺序沉到已排序项之后。
    removePinOrder(targetWs.id, s.id);
    afterToggle();
  } catch (e) {
    setBanner("⚠ 置顶失败：" + e.message, true);
  }
}

/* PUT 切换归档（🗄 → PUT /api/sessions/{id}/archive {"archived": bool}）；
   成功写回目标 workspace 会话对象（workspaceListFor）并回调调用方重绘
   （树主节点使用）。旧服务器没有 archive 端点：404/405 统一提示
   「服务器不支持归档」（与置顶/重命名一致）；archived 字段缺失
   （undefined）视为未归档。会话顺序信任后端（未归档在前、归档最后），
   前端不重排。
   ws 可选（跨服务器树节点传所属 workspace；缺省 = 激活 workspace，
   既有调用不变）：请求打到目标服务器。 */
async function toggleArchived(s, afterToggle, ws) {
  const targetWs = ws || state.workspace;
  if (!workspaceToken(targetWs)) { setBanner("⚠ 请先输入 Token。", true); return; }
  const target = s.archived !== true;   // 兼容旧 server 无 archived 字段：undefined → 归档
  try {
    const res = await apiFor(targetWs, "/api/sessions/" + encodeURIComponent(s.id) + "/archive",
      { method: "PUT", body: JSON.stringify({ archived: target }) });
    if (res.status === 401 || res.status === 403) {
      setBanner("⚠ 认证失败：请检查 Token。");
      return;
    }
    if (!res.ok) {
      if (res.status === 404 || res.status === 405) {
        setBanner("⚠ 服务器不支持归档。", true);
      } else {
        setBanner("⚠ 归档失败：HTTP " + res.status, true);
      }
      return;
    }
    // 轮询可能已换掉缓存里的对象引用（会话数据未变时树不重绘，
    // 但数组每轮都是新的）：按 id 重新解析当前对象再写回，确保重绘反映新状态
    const cur = workspaceListFor(targetWs).find((x) => x.id === s.id) || s;
    cur.archived = target;
    if (target) removePinOrder(targetWs.id, s.id);
    afterToggle();
  } catch (e) {
    setBanner("⚠ 归档失败：" + e.message, true);
  }
}
