
"use strict";

/* 历史分页：每页条数。长会话分块加载/渲染，避免一次渲染上千条 DOM
   导致移动/桌面浏览器滚动卡死。 */
const HISTORY_PAGE = 200;

/* 输入框默认 placeholder；applyStatus 在 Finished 之外的状态恢复它
   （openSession 每次 applyStatus("Idle") 都会重置输入区）。 */
const PROMPT_PLACEHOLDER = "输入消息：Enter 发送，Shift+Enter 换行…";

/* =====================================================================
 * 全局状态
 * ===================================================================*/
const state = {
  token: localStorage.getItem("eagent_token") || "",
  view: "list",              // "list" | "chat"
  sessionId: null,           // 当前打开的会话
  status: "Idle",
  initSource: null,          // "history" | "snapshot" | null —— 初始渲染来源
  pollTimer: null,
  sse: { ctrl: null, retryTimer: null, stopped: false },
  acc: null,                 // 增量渲染累积器（见 newAccumulator）
  nextBeforeSeq: null,       // 历史分页游标：下一段更早历史的 before_seq（loadHistory 响应里取；null=没有更多）
  loadingOlder: false,       // 是否正在加载更早历史（防重入）
  olderDone: false,          // 更早历史已全部加载（next_before_seq 为 null）
  searchQuery: "",           // 会话列表搜索词（已小写化）；轮询重绘后过滤依然生效
  lastList: [],              // 最近一次轮询拿到的完整列表，供搜索框重绘
  queue: [],                 // 排队提示（FIFO；最多显示 3 条 + "+N"）
  deepLink: { pending: null, handled: false },  // URL ?session= 深链：待打开 id + 一次性标志
  sessionStates: {},         // sessionId -> {html, scrollTop, nextBeforeSeq, olderDone, draft}：切走时保存，切回时恢复（不重新加载历史）
  sidebar: {                 // 会话侧边栏
    open: false,             // 是否打开
    expanded: new Set(),     // 已展开的主会话 id（会话树；重绘时保留）
    filter: "",              // 筛选关键词（已小写化）；空 = 默认显示
    showAll: false,          // 是否已展开全部主会话（超出 15 条限制时）
  },
  tasks: {                   // 运行中任务（渲染进侧边栏）
    badgeSeq: 0,             // 徽标轮询竞态序号：只应用最新一次响应
    panelSeq: 0,             // 面板轮询竞态序号
    badgeTimer: null,        // 徽标轮询（独立 3s，侧边栏关闭时也跑）
    panelTimer: null,        // 侧边栏打开时的任务列表刷新（2s）
    cancelling: new Set(),   // 正在取消的任务 id（防重复点击）
  },
  renameActive: false,       // 行内重命名进行中：列表页 1s 轮询重绘跳过，防编辑框被冲掉
};

/* 常用 DOM 引用 */
const $ = (id) => document.getElementById(id);
const els = {
  topActions: $("topActions"), backBtn: $("backBtn"), connState: $("connState"),
  banner: $("banner"), tokenInput: $("tokenInput"),
  listView: $("listView"), chatView: $("chatView"),
  newPrompt: $("newPrompt"), newSessionBtn: $("newSessionBtn"),
  sessionList: $("sessionList"), listMeta: $("listMeta"), listHint: $("listHint"),
  searchInput: $("searchInput"),
  chatSessionId: $("chatSessionId"), chatStatus: $("chatStatus"), usageInfo: $("usageInfo"),
  messages: $("messages"), promptInput: $("promptInput"), queueBar: $("queueBar"),
  composerMeta: $("composerMeta"),
  jumpBottomBtn: $("jumpBottomBtn"),
  sendBtn: $("sendBtn"), cancelBtn: $("cancelBtn"), compactBtn: $("compactBtn"),
  sidebarBtn: $("sidebarBtn"), sidebarOverlay: $("sidebarOverlay"),
  sidebar: $("sidebar"), sidebarCloseBtn: $("sidebarCloseBtn"),
  sidebarFilter: $("sidebarFilter"),
  sidebarTree: $("sidebarTree"), sidebarTasks: $("sidebarTasks"),
  sidebarTasksTitle: $("sidebarTasksTitle"),
};

/* =====================================================================
 * 小工具
 * ===================================================================*/
function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  }[c]));
}

function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text != null) e.textContent = text;
  return e;
}

function truncate(s, n) {
  s = String(s);
  return s.length > n ? s.slice(0, n) + "\n… (省略 " + (s.length - n) + " 字符)" : s;
}

function fmtTime(iso) {
  if (!iso) return "";
  const d = new Date(iso);
  if (isNaN(d.getTime())) return String(iso);
  return d.toLocaleString("zh-CN", { hour12: false });
}

function setBanner(msg, isWarn) {
  els.banner.textContent = msg;
  els.banner.className = "banner" + (isWarn ? " warn" : "");
  els.banner.hidden = !msg;
}

/* 从事件载荷中提取字符串正文：后端字段名可能不同，做防御性兼容 */
function pickText(payload, keys) {
  if (typeof payload === "string") return payload;
  if (!payload || typeof payload !== "object") return "";
  for (const k of keys) {
    if (typeof payload[k] === "string") return payload[k];
  }
  for (const v of Object.values(payload)) {
    if (typeof v === "string") return v;
  }
  return "";
}

/* =====================================================================
 * Markdown 渲染：marked + KaTeX（内嵌，单 HTML 自包含）。
 * marked 配置在启动时注册一次；renderMarkdown 只做摘公式 → 解析 →
 * 还原，避免每条消息重复 setOptions/use（开销虽小但纯属浪费）。
 * ===================================================================*/
marked.setOptions({ gfm: true, breaks: true });
marked.use({
  renderer: {
    link(href, title, text) {
      // 只允许 http/https/mailto/# 链接；javascript: 等协议拒绝
      if (!/^(https?:|mailto:|#|\/)/i.test(href || "")) {
        return text;
      }
      return '<a href="' + escapeHtml(href) + '" target="_blank" rel="noopener noreferrer">' + text + "</a>";
    },
  },
});

function renderMarkdown(text) {
  // GFM 全开，单换行 <br>（breaks）。LaTeX 用 $...$ / $$...$$
  // 先摘出，避免被 marked 的转义/斜体干扰；渲染完再还原为 KaTeX HTML。
  const math = [];
  let s = String(text == null ? "" : text).replace(/\r\n?/g, "\n");
  s = s
    .replace(/\$\$([\s\S]+?)\$\$/g, (m, body) => {
      math.push({ display: true, body });
      return "\u0000M" + (math.length - 1) + "\u0000";
    })
    .replace(/\$([^\n$]+?)\$/g, (m, body) => {
      math.push({ display: false, body });
      return "\u0000M" + (math.length - 1) + "\u0000";
    });
  // 不允许内嵌原始 HTML：把 < > 先转义再喂给 marked（表格/代码块不受影响）
  s = s.replace(/<(\/?)([a-zA-Z][a-zA-Z0-9-]*)([^>]*)>/g, (m, slash, tag, rest) => {
    // 白名单：保留代码块围栏和占位符（它们不含 <tag> 形态）
    return "&lt;" + slash + tag + rest + "&gt;";
  });
  let html;
  try {
    html = marked.parse(s);
  } catch (e) {
    return escapeHtml(s);   // 解析失败兜底：纯文本
  }
  // 还原 LaTeX（KaTeX 渲染失败则显示原文）
  html = html.replace(/\u0000M(\d+)\u0000/g, (m, i) => {
    const item = math[+i];
    if (!item) return "";
    try {
      return katex.renderToString(item.body, { displayMode: item.display, throwOnError: false });
    } catch (e) {
      return "<code>" + escapeHtml(item.body) + "</code>";
    }
  });
  return html;
}

/* =====================================================================
 * Token 与认证
 * ===================================================================*/
els.tokenInput.value = state.token;
els.tokenInput.addEventListener("input", () => {
  state.token = els.tokenInput.value.trim();
  if (state.token) localStorage.setItem("eagent_token", state.token);
  else localStorage.removeItem("eagent_token");
  refreshBanner();
  restartTransport();   // token 变化 → 重启轮询 / SSE
});

function refreshBanner() {
  if (!state.token) {
    setBanner("⚠ 未配置访问令牌：请在右上角输入 Token，所有请求需要 Authorization: Bearer <token>。", true);
  } else {
    setBanner("");
  }
}

/* 统一请求入口：自动附带 Authorization header */
async function api(path, opts = {}) {
  const headers = Object.assign({}, opts.headers || {});
  if (state.token) headers["Authorization"] = "Bearer " + state.token;
  if (opts.body && !headers["Content-Type"]) headers["Content-Type"] = "application/json";
  return fetch(path, Object.assign({}, opts, { headers }));
}

/* =====================================================================
 * 会话列表视图
 * ===================================================================*/
const STATUS_LABEL = {
  Idle: "空闲", Busy: "处理中", Compacting: "压缩中", Finished: "已完成",
};
function statusLabel(s) {
  if (s && s.startsWith("Failed")) return "失败";
  return STATUS_LABEL[s] || (s || "未知");
}
function statusChipClass(s) {
  if (s === "Busy") return "busy";
  if (s === "Compacting") return "compacting";
  if (s === "Finished") return "finished";
  if (s && s.startsWith("Finished")) return "finished";
  if (s && s.startsWith("Failed")) return "error";
  return "idle";
}

function shortId(id) {
  return id && id.length > 8 ? id.slice(0, 8) + "…" : (id || "");
}

async function pollSessions() {
  if (state.view !== "list" || !state.token) return;
  try {
    const res = await api("/api/sessions");
    if (res.status === 401 || res.status === 403) {
      setBanner("⚠ 认证失败：请检查 Token 是否正确。");
      return;
    }
    if (!res.ok) throw new Error("HTTP " + res.status);
    const list = await res.json();
    renderSessionList(Array.isArray(list) ? list : []);
    renderSidebarTree();                 // 侧边栏会话树随轮询刷新
    if (state.view === "chat") updateComposerMeta();   // model/role 可能随轮询更新（幂等）
    maybeHandleDeepLink(Array.isArray(list) ? list : []);
  } catch (e) {
    if (!navigator.onLine || e instanceof TypeError) {
      setBanner("⚠ 无法连接服务器（网络错误）。", true);
    }
  }
}

/* URL 深链：?session=<id>。token 就绪且列表拿到数据后自动打开一次。
   active 会话直接 openSession；inactive（历史）会话走 resumeSession
   （resume 成功后内部会 openSession）。一次性标志防止轮询重复触发。 */
function maybeHandleDeepLink(list) {
  if (state.deepLink.handled || !state.deepLink.pending) return;
  if (!state.token) return;                    // token 为空：等用户填写后再处理
  const target = state.deepLink.pending;
  const hit = (list || []).find((s) => s.id === target);
  if (!hit) return;                            // 列表里还没出现：等下一轮轮询
  state.deepLink.handled = true;
  state.deepLink.pending = null;
  if (hit.active === false) resumeSession(target);
  else openSession(target);
}

function renderSessionList(list) {
  // 行内重命名进行中：跳过本轮重绘（列表页轮询 1s 一次，会冲掉编辑框）；
  // 保存/取消后由 enterRename 清除标志并自行重绘
  if (state.renameActive) return;
  state.lastList = Array.isArray(list) ? list : [];
  // 排序：后端保证 pinned 置顶在前、组内按 last_active_at 降序；前端按数组
  // 顺序渲染、不自行重排（旧 server / mock 返回什么顺序就渲染什么顺序）。
  let rows = state.lastList;
  if (state.searchQuery) {
    const q = state.searchQuery;
    rows = rows.filter((s) => {
      // 对 id / title / model / parent_session_id 子串匹配，大小写不敏感
      const hay = [s.id, s.title, s.model, s.parent_session_id]
        .map((v) => String(v || "").toLowerCase());
      return hay.some((h) => h.includes(q));
    });
  }
  els.listMeta.textContent = "共 " + rows.length + " 个";
  els.sessionList.innerHTML = "";
  if (!rows.length) {
    els.listHint.textContent = state.searchQuery
      ? "没有匹配的会话（搜索词: " + state.searchQuery + "）。"
      : "暂无会话，在上方输入初始提示词创建一个。";
    return;
  }
  els.listHint.textContent = "";
  for (const s of rows) {
    // Historical (inactive) sessions come from the metadata table: grey
    // row, clicking resumes them instead of opening directly.
    const inactive = s.active === false;
    const row = el("div", "session-row" + (inactive ? " inactive" : "") +
      (s.pinned === true ? " pinned" : ""));
    row.title = s.id + (s.model ? " · " + s.model : "") +
      (s.parent_session_id ? " · 子会话 ← " + s.parent_session_id : "");

    const dot = el("span", "busy-dot" + (s.busy ? " busy" : ""));
    // 有标题优先显示：标题一行 + 完整 id 小字一行；无标题单行完整 id
    let sid;
    if (s.title) {
      const box = el("span", "sid has-title");
      const t = el("span", "sid-title", s.title);
      const i = el("span", "sid-id", s.id);
      box.append(t, i);
      sid = box;
    } else {
      sid = el("span", "sid", s.id);
    }
    const edit = el("button", "tree-edit", "✎");
    edit.type = "button";
    edit.title = "重命名";
    edit.addEventListener("click", (ev) => {
      ev.stopPropagation();                // 不触发打开会话
      enterRename(sid, s, () => { renderSessionList(state.lastList); });
    });
    const pin = el("button", "pin-btn" + (s.pinned === true ? " on" : ""), "📌");
    pin.type = "button";
    pin.title = "置顶/取消置顶";
    pin.addEventListener("click", (ev) => {
      ev.stopPropagation();                // 不触发打开会话
      togglePin(s, () => { renderSessionList(state.lastList); renderSidebarTree(true); });
    });
    const chip = el("span", "status-chip " + statusChipClass(s.status), statusLabel(s.status));
    const model = el("span", "smodel", s.model || "");
    const meta = el("span", "smeta",
      fmtTime(s.created_at) + " · " + (s.entry_count ?? "-") + " 条" +
      (s.parent_session_id ? " · ↳" + shortId(s.parent_session_id) : ""));
    const del = el("button", "del danger", "删除");
    del.title = "删除会话 " + s.id;
    del.addEventListener("click", async (ev) => {
      ev.stopPropagation();
      if (!confirm("确认删除会话 " + s.id + " ？")) return;
      try {
        const res = await api("/api/sessions/" + encodeURIComponent(s.id), { method: "DELETE" });
        if (res.status === 401 || res.status === 403) { setBanner("⚠ 认证失败：请检查 Token。"); return; }
        if (!res.ok && res.status !== 204) throw new Error("HTTP " + res.status);
        delete state.sessionStates[s.id];   // 删除会话：同步清掉视图缓存（切回不再恢复）
      } catch (e) {
        setBanner("⚠ 删除失败：" + e.message);
      }
    });
    row.addEventListener("click", () => {
      if (inactive) { resumeSession(s.id); return; }
      openSession(s.id);
    });
    row.append(dot, sid, edit, pin, chip, model, meta, del);
    els.sessionList.appendChild(row);
  }
}

/* 恢复（resume）一个历史会话：POST /api/sessions {id} 建回活跃会话后打开 */
async function resumeSession(id) {
  if (!state.token) { setBanner("⚠ 请先输入 Token。", true); return; }
  try {
    const res = await api("/api/sessions", { method: "POST", body: JSON.stringify({ id }) });
    if (res.status === 401 || res.status === 403) { setBanner("⚠ 认证失败：请检查 Token。"); return; }
    if (res.status !== 201) throw new Error("HTTP " + res.status);
    const s = await res.json();
    openSession(s.id);
  } catch (e) {
    setBanner("⚠ 恢复会话失败：" + e.message);
  }
}

async function createSession() {
  if (!state.token) { setBanner("⚠ 请先输入 Token。", true); return; }
  const text = els.newPrompt.value.trim();
  const body = {};
  if (text) body.initial_prompt = text;
  els.newSessionBtn.disabled = true;
  try {
    const res = await api("/api/sessions", { method: "POST", body: JSON.stringify(body) });
    if (res.status === 401 || res.status === 403) { setBanner("⚠ 认证失败：请检查 Token。"); return; }
    if (res.status !== 201) throw new Error("HTTP " + res.status);
    const s = await res.json();
    els.newPrompt.value = "";
    // 新会话不在 lastList 里（聊天视图轮询已停）：先把返回的 meta 塞进列表，
    // openSession 的 updateComposerMeta 才能显示 model/role
    state.lastList = state.lastList.filter((x) => x.id !== s.id);
    state.lastList.push(s);
    openSession(s.id);
  } catch (e) {
    setBanner("⚠ 创建会话失败：" + e.message);
  } finally {
    els.newSessionBtn.disabled = false;
  }
}

/* =====================================================================
 * 消息渲染
 * ===================================================================*/
function newAccumulator() {
  return {
    assistantEl: null,   // 当前助手消息容器（流式累积）
    assistantBody: null,
    assistantText: "",   // 已累积的文本（用于替换/查重）
    thinkingEl: null,    // 当前 thinking 折叠块
    thinkBody: null,
    toolStack: [],       // 未配对结果的工具卡片（live 事件按顺序配对）
    pendingByCall: new Map(), // call_id -> 卡片元素（history 渲染按 id 配对）
  };
}

function freezeAssistant(acc) {
  // 工具调用 / 新用户回合开始时，结束当前助手消息与思考区的累积，
  // 使下一回合的 delta 从新的消息块开始
  // 流式期间是纯文本（快）；冻结时用完整文本重算一次 markdown，
  // 让表格/代码块/公式在回合结束时正确渲染。
  if (acc && acc.assistantBody && acc.assistantText) {
    acc.assistantBody.innerHTML = renderMarkdown(acc.assistantText);
  }
  // 思考结束：转圈 → 勾号（表示该轮思考完成）
  if (acc && acc.thinkingEl) {
    const dot = acc.thinkingEl.querySelector(".think-dot");
    if (dot) {
      dot.classList.remove("active");
      dot.classList.add("done");
    }
  }
  acc.assistantEl = null;
  acc.assistantBody = null;
  acc.assistantText = "";
  acc.thinkingEl = null;
  acc.thinkBody = null;
}

/* 历史前置插入期间抑制自动滚动：loadOlder 自己负责保持滚动位置 */
let suppressScroll = false;
// 用户主动上滑离开底部后，暂停自动跟随（流式/工具结果不再强扯回底部）；
// 用户滚回底部或开始新回合（新 UserPrompt）时恢复跟随。
let userScrolled = false;

function scrollBottom(force) {
  const m = els.messages;
  if (suppressScroll) return;
  if (userScrolled) return;   // 用户主动离开底部在看历史：任何自动滚动都不打扰
  const near = m.scrollHeight - m.scrollTop - m.clientHeight < 80;
  if (force || near) {
    m.scrollTop = m.scrollHeight;
    // 程序滚到底后同步按钮状态（scroll 事件被 isTrusted 拦截时不会触发）
    if (els.jumpBottomBtn) els.jumpBottomBtn.hidden = true;
  }
}

function appendUserMsg(text) {
  freezeAssistant(state.acc);
  // 新回合开始：恢复自动跟随（用户发了新消息，理应看到后续输出）
  userScrolled = false;
  const msg = el("div", "msg msg-user");
  const who = el("span", "who", "you>");
  const body = el("div", "msg-body");
  body.innerHTML = renderMarkdown(text);
  msg.append(who, body);
  els.messages.appendChild(msg);
  scrollBottom(false);
}

function appendSystemMsg(text) {
  freezeAssistant(state.acc);
  const msg = el("div", "msg msg-system");
  const who = el("span", "who", "system>");
  const body = el("div", "msg-body");
  body.textContent = text;
  msg.append(who, body);
  els.messages.appendChild(msg);
}

/* 助手消息：有则累积，无则新建 */
function assistantBubble(acc, reason) {
  if (!acc.assistantEl) {
    const msg = el("div", "msg msg-assistant");
    const who = el("span", "who", "ai>");
    const body = el("div", "msg-body");
    msg.append(who, body);
    els.messages.appendChild(msg);
    acc.assistantEl = msg;
    acc.assistantBody = body;
    if (reason) scrollBottom(false);
  }
  return acc.assistantBody;
}

function appendAssistantDelta(text, acc) {
  const body = assistantBubble(acc, true);
  acc.assistantText += text;
  // 流式渲染：直接追加（markdown 不重算，保持简单、快）
  body.insertAdjacentText("beforeend", text);
  scrollBottom(false);
}

/* 非流式回合结束时整段落文本 */
function setAssistantText(text, acc) {
  const body = assistantBubble(acc, true);
  if (acc.assistantText) {
    // 已累积过 delta：整段覆盖（TUI 语义：同回合的最终完整文本）
    body.textContent = "";
    acc.assistantText = "";
  }
  body.innerHTML = renderMarkdown(text);
  acc.assistantText = text;
  scrollBottom(false);
}

/* thinking 折叠块：默认折叠（页面不再被大段思考内容撑爆）；
   折叠时 summary 左侧有转圈动画指示「思考进行中」。 */
function thinkingBlock(acc) {
  if (!acc.thinkingEl) {
    const det = el("details", "thinking");
    det.open = false;                       // 默认折叠
    const sum = el("summary", "", "");
    const dot = el("span", "think-dot");    // 转圈动画（活跃时转，停止时静止）
    const label = el("span", "think-label", "思考中…");
    sum.append(dot, label);
    const body = el("div", "think-body");
    det.append(sum, body);
    els.messages.appendChild(det);
    acc.thinkingEl = det;
    acc.thinkBody = body;
  }
  return acc.thinkBody;
}

function appendReasoningDelta(text, acc) {
  const body = thinkingBlock(acc);
  body.insertAdjacentText("beforeend", text);
  // 思考进行中：折叠栏的圆点转圈
  if (acc.thinkingEl) {
    const dot = acc.thinkingEl.querySelector(".think-dot");
    if (dot) dot.classList.add("active");
  }
  scrollBottom(false);
}

/* 工具调用卡片（live 事件，无 call_id，按顺序配对结果） */
function appendToolCall(name, args, acc, callId) {
  freezeAssistant(acc);
  const card = buildToolCard(name, args, "执行中…", "pending", null);
  acc.toolStack.push({ el: card, filled: false });
  if (callId) acc.pendingByCall.set(callId, card);
  els.messages.appendChild(card);
  scrollBottom(true);   // 工具调用卡片出现时强制跟随底部（工具结果常几秒后一次性到达）
}

function appendToolResult(isError, content, acc, callId) {
  let card = null;
  if (callId && acc.pendingByCall.has(callId)) {
    card = acc.pendingByCall.get(callId);
  }
  if (!card) {
    // 从后往前找最近一个未填充的卡片
    for (let i = acc.toolStack.length - 1; i >= 0; i--) {
      if (!acc.toolStack[i].filled) { card = acc.toolStack[i].el; acc.toolStack[i].filled = true; break; }
    }
  } else {
    for (const t of acc.toolStack) if (t.el === card) t.filled = true;
  }
  if (card) {
    card.querySelector(".tool-state").textContent = isError ? "失败" : "完成";
    const resEl = card.querySelector(".tool-result");
    resEl.className = "tool-result" + (isError ? " err" : "");
    resEl.textContent = content || (isError ? "(无错误信息)" : "(无输出)");
    card.removeAttribute("open");   // 结果到达：收起为标题行（默认折叠）
  } else {
    // 没有可配对的卡片：独立展示结果行
    const card2 = buildToolCard("工具结果", "", isError ? "失败" : "完成",
      isError ? "err" : "", content || "");
    els.messages.appendChild(card2);
  }
  scrollBottom(true);   // 工具结果填充后强制滚到底（buildToolCard 内设置的
                        // stateText/resultText 也随此调用被包含）
}

function buildToolCard(name, args, stateText, stateCls, resultText) {
  // 工具卡片：details 可折叠。pending（执行中/等待结果）时默认展开，
  // 结果到达后由 appendToolResult 收起（details.removeAttribute("open")）。
  const card = el("details", "tool-card");
  if (stateCls === "pending") card.setAttribute("open", "");
  const head = el("summary", "tool-head");
  const nm = el("span", "tool-name", name || "tool");
  const st = el("span", "tool-state", stateText);
  head.append(nm, st);

  let argsText = args != null ? String(args) : "";
  let pretty = argsText;
  try { pretty = JSON.stringify(JSON.parse(argsText), null, 2); } catch (e) { /* 保持原文 */ }
  const argsEl = el("pre", "tool-args", truncate(pretty, 600));

  const resEl = el("pre", "tool-result " + stateCls,
    resultText != null ? resultText : (stateCls === "pending" ? "等待结果…" : ""));
  card.append(head, argsEl, resEl);
  return card;
}

/* 提示行（Notice / 排队等） */
function appendNotice(text) {
  const n = el("div", "notice", text);
  els.messages.appendChild(n);
  scrollBottom(false);
}

/* =====================================================================
 * 排队提示条（queueBar）：固定在输入区上方，不随消息滚动。
 * PromptQueued 入队、PromptConsumed 出队（FIFO）；最多显示 3 条 + "+N"。
 * ===================================================================*/
function renderQueueBar() {
  const bar = els.queueBar;
  if (!state.queue.length) {
    bar.hidden = true;
    bar.textContent = "";
    return;
  }
  const shown = state.queue.slice(0, 3);
  const extra = state.queue.length - shown.length;
  let text = shown.map((t) => "⏳ 排队中: " + t).join("\n");
  if (extra > 0) text += "\n+ " + extra + " 条排队中";
  bar.textContent = text;
  bar.hidden = false;
}

function queuePromptQueued(text) {
  state.queue.push(text);
  renderQueueBar();
}

function queuePromptConsumed() {
  if (state.queue.length) state.queue.shift();   // 移除最旧一条（正在被处理的那条）
  renderQueueBar();
}

/* 错误行 */
function appendError(text) {
  const e = el("div", "msg-error", "错误: " + text);
  els.messages.appendChild(e);
  scrollBottom(false);
}

/* 压缩分界线 */
function appendCompaction(summary) {
  freezeAssistant(state.acc);
  els.messages.appendChild(el("div", "compaction", "—— 上下文已压缩 ——"));
  if (summary) {
    const n = el("div", "notice", "摘要: " + summary);
    els.messages.appendChild(n);
  }
  scrollBottom(false);
}

/* =====================================================================
 * History 全量渲染（SessionEntry 数组）
 * ===================================================================*/
function renderEntry(entry, acc, pendingCards) {
  switch (entry.type) {
    case "message": return renderMessage(entry.message, acc, pendingCards);
    case "compaction": return appendCompaction(entry.summary);
    case "notice": return appendNotice(entry.text);
    case "background_completion":
      return appendNotice("⌛ 后台任务 #" + (entry.id ?? "?") + " 完成"
        + (entry.label ? "（" + entry.label + "）" : "")
        + "\n" + truncate(entry.output || "", 300));
    case "forked_from":
      els.messages.appendChild(el("div", "forked",
        "分叉自会话 " + shortId(entry.source) + " @ 条目 #" + (entry.at ?? "?")));
      return scrollBottom(false);
    case "prompt_queued":
      // 历史记录里的排队提示（后端演进若引入）：显示为 notice，不落入默认分支
      return appendNotice("⏳ 提示已排队: "
        + (typeof entry.text === "string" ? entry.text : ""));
    case "prompt_consumed":
      return appendNotice("▶ 开始处理排队的提示");
    default:
      // 未知条目类型：尽力显示原始 JSON（后端演进兼容）
      return appendNotice("未知条目: " + truncate(JSON.stringify(entry), 300));
  }
}

function renderMessage(m, acc, pendingCards) {
  if (!m) return;
  if (m.User) {
    appendUserMsg(m.User.content || "");
    const nImg = (m.User.images || []).length;
    if (nImg > 0) appendNotice("📷 附带 " + nImg + " 张图片");
    return;
  }
  if (m.System) { appendSystemMsg(m.System.content || ""); return; }
  if (m.Assistant) {
    const a = m.Assistant;
    if (a.reasoning) {
      const det = el("details", "thinking");
      det.open = false;
      const sum = el("summary", "", "");
      const dot = el("span", "think-dot done");  // 历史：已完成的思考，直接勾号
      const label = el("span", "think-label", "思考");
      sum.append(dot, label);
      det.append(sum);
      det.append(el("div", "think-body", a.reasoning));
      els.messages.appendChild(det);
    }
    // 每个 history 条目是完整消息：结束上一个助手消息的累积，另起一块
    freezeAssistant(acc);
    if (a.content) {
      const body = assistantBubble(acc, true);
      body.innerHTML = renderMarkdown(a.content);
      acc.assistantText = a.content;
    } else if (!a.tool_calls || !a.tool_calls.length) {
      // 空内容助手消息：占位，避免空白
      const body = assistantBubble(acc, true);
      body.innerHTML = "<span class=\"dim\">（空回复）</span>";
    }
    for (const tc of a.tool_calls || []) {
      const card = buildToolCard(tc.name, tc.arguments, "等待结果…", "pending", null);
      els.messages.appendChild(card);
      pendingCards.set(tc.id, card);
    }
    scrollBottom(false);
    return;
  }
  if (m.Tool) {
    const t = m.Tool;
    let card = pendingCards.get(t.call_id);
    if (card) {
      pendingCards.delete(t.call_id);
      card.querySelector(".tool-state").textContent = t.is_error ? "失败" : "完成";
      const resEl = card.querySelector(".tool-result");
      resEl.className = "tool-result" + (t.is_error ? " err" : "");
      resEl.textContent = t.content || (t.is_error ? "(无错误信息)" : "(无输出)");
    } else {
      // 无对应 ToolCall（如历史截断后）：独立卡片
      const card2 = buildToolCard(t.name, "", t.is_error ? "失败" : "完成",
        t.is_error ? "err" : "", t.content || "");
      els.messages.appendChild(card2);
    }
    scrollBottom(false);
    return;
  }
  // 其他 message 形状
  appendNotice("消息: " + truncate(JSON.stringify(m), 300));
}

/* 渲染一批 SessionEntry。prepend=true 时把新条目插入容器开头（保留既有内容），
   用于滚动分页加载更早历史；prepend=false 时整体替换（初始 history 渲染）。 */
function renderEntries(entries, prepend) {
  const acc = newAccumulator();
  const pendingCards = new Map();
  const list = Array.isArray(entries) ? entries : [];
  const prevAcc = state.acc;
  let sentinel = null;
  // 无论 prepend 与否，渲染期间都禁止自动滚动：每条消息的 scrollBottom
  // 都会触发一次强制布局读（forced reflow），N 条近似 O(N²) 累计开销，
  // 是长会话初始渲染阻塞主线程（触摸拖不动）的主要放大器。渲染结束后
  // 由调用方（renderHistory 末尾 scrollBottom(true) / loadOlder 自己
  // 保持位置）负责最终滚动。
  suppressScroll = true;
  if (prepend) {
    // 末尾放一个哨兵注释节点标记「旧内容结束」；渲染后把哨兵之后的节点按序移到开头
    sentinel = document.createComment("older-boundary");
    els.messages.appendChild(sentinel);
    // history 条目（compaction / user 消息等）会 freezeAssistant(state.acc)；
    // 前置插入不能动底部当前流的累积器。整批渲染是同步的，事件不会穿插，可安全临时切换。
    state.acc = acc;
  } else {
    state.acc = acc;
    // 清空消息区（jumpBottomBtn 在 messages 外，不受影响）
    els.messages.innerHTML = "";
    els.jumpBottomBtn.hidden = true;
  }
  try {
    for (const e of list) renderEntry(e, acc, pendingCards);
  } finally {
    suppressScroll = false;
    if (prepend) state.acc = prevAcc;
  }
  if (prepend && sentinel) {
    const firstOld = els.messages.firstChild;   // 旧内容第一个节点（或哨兵自身）
    let n = sentinel.nextSibling;
    while (n) {
      const next = n.nextSibling;
      els.messages.insertBefore(n, firstOld);
      n = next;
    }
    sentinel.remove();
  }
}

function renderHistory(entries) {
  renderEntries(entries, false);
  scrollBottom(true);
}

/* =====================================================================
 * 会话状态（status 事件 / 列表 busy 字段）
 * ===================================================================*/
function applyStatus(status) {
  state.status = status || "Idle";
  els.chatStatus.textContent = statusLabel(state.status);
  els.chatStatus.className = "status-chip " + statusChipClass(state.status);
  const busy = state.status === "Busy" || state.status === "Compacting";
  els.cancelBtn.disabled = !busy;         // Busy/Compacting 时可取消
  els.compactBtn.disabled = busy;         // 空闲时才压缩
  // Finished：会话不再接受输入，禁用输入区（busy 的 subagent 仍可排队输入）
  const finished = !!state.status && state.status.startsWith("Finished");
  els.sendBtn.disabled = finished;
  els.promptInput.disabled = finished;
  if (finished) {
    const s = (state.lastList || []).find((x) => x.id === state.sessionId);
    els.promptInput.placeholder = s && s.parent_session_id
      ? "子任务已结束，无法继续发送"
      : "会话已结束";
  } else {
    els.promptInput.placeholder = PROMPT_PLACEHOLDER;  // 恢复默认（openSession 每次 applyStatus("Idle") 已重置）
  }
  // 回合结束（回到 Idle）：流式 delta 期间是纯文本（快），此刻用完整
  // 文本重算一次 markdown，让表格/代码块/公式正确渲染；同时停思考动画。
  if (!busy && state.acc) {
    if (state.acc.assistantBody && state.acc.assistantText) {
      state.acc.assistantBody.innerHTML = renderMarkdown(state.acc.assistantText);
    }
    if (state.acc.thinkingEl) {
      const dot = state.acc.thinkingEl.querySelector(".think-dot");
      if (dot) {
        dot.classList.remove("active");
        dot.classList.add("done");
      }
    }
  }
}

function applyUsage(usage) {
  if (!usage || typeof usage !== "object") return;
  const s = usage.session || {};
  const parts = [];
  let pct = null;
  // context_window 配置了才显示百分比（TUI 同语义：>=80% 标红提示接近压缩阈值）
  if (usage.context_input != null && usage.context_window) {
    pct = Math.round(usage.context_input / usage.context_window * 100);
    parts.push("上下文 " + usage.context_input + "/" + usage.context_window + " tok (" + pct + "%)");
  } else if (usage.context_input != null) {
    parts.push("上下文 " + usage.context_input + " tok");
  }
  if (s.input_tokens != null) parts.push("输入 " + s.input_tokens);
  if (s.output_tokens != null) parts.push("输出 " + s.output_tokens);
  if (parts.length) {
    els.usageInfo.textContent = "用量: " + parts.join(" · ");
    // >=80% 接近自动压缩阈值：标红提醒
    els.usageInfo.classList.toggle("usage-high", pct !== null && pct >= 80);
  }
}

/* =====================================================================
 * SSE：fetch + ReadableStream（EventSource 无法带 header）
 * ===================================================================*/
function setConn(stateName, text) {
  els.connState.className = "conn-state " + stateName;
  els.connState.textContent = text;
}

function connectSSE(id) {
  if (id !== state.sessionId) return;   // 已切换会话：不动当前会话的流
  stopSSE();
  state.sse.stopped = false;
  state.sse.ctrl = new AbortController();
  const ctrl = state.sse.ctrl;

  fetch("/api/sessions/" + encodeURIComponent(id) + "/events", {
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
      setBanner("⚠ 会话不存在或已被删除。");
      throw new Error("gone");
    }
    if (!res.ok || !res.body) throw new Error("HTTP " + res.status);
    setConn("ok", "● 已连接");
    return readSSEStream(res.body.getReader(), id);
  }).then(() => {
    // 正常结束（后端关闭流）→ 按断线处理
    throw new Error("stream end");
  }).catch((err) => {
    if (err && err.name === "AbortError") return;   // 主动停止
    if (state.sse.stopped) return;
    if (err && err.message === "auth") { state.sse.stopped = true; return; }
    scheduleReconnect(id);
  });
}

/* 逐块读取 SSE：按空行切分事件块（兼容 \r\n 行尾） */
async function readSSEStream(reader, id) {
  const decoder = new TextDecoder();
  let buf = "";
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    buf += decoder.decode(value, { stream: true }).replace(/\r\n/g, "\n");
    let idx;
    while ((idx = buf.indexOf("\n\n")) !== -1) {
      const block = buf.slice(0, idx);
      buf = buf.slice(idx + 2);
      handleSSEBlock(block, id);
    }
  }
}

/* 解析单个 SSE 事件块 */
function handleSSEBlock(block, id) {
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
      appendNotice("⌛ 后台任务 #" + (p.id ?? "?") + " 完成" + label + "\n"
        + truncate(pickText(p, ["output", "text", "content"]) || "", 300));
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
      // 后端新事件：未知则尽量显示，避免静默丢失
      appendNotice("事件 " + name + ": " + truncate(JSON.stringify(payload), 200));
  }
}

/* 断线重连：3 秒后重新加载 history + SSE */
function scheduleReconnect(id) {
  if (state.sse.stopped || id !== state.sessionId) return;
  setConn("retrying", "↻ 连接断开，3 秒后重连…");
  state.sse.retryTimer = setTimeout(() => {
    if (state.sse.stopped || state.view !== "chat" || id !== state.sessionId) return;
    state.initSource = null;             // 允许 snapshot 兜底
    openWith(id, true);
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
 * 对话视图流程
 * ===================================================================*/
async function loadHistory(id) {
  try {
    // 只取尾部 HISTORY_PAGE 条：长会话不一次全量渲染（Firefox 等浏览器
    // 对超大 DOM 滚动卡死）；滚动触顶时 loadOlder 增量加载更早。
    const res = await api("/api/sessions/" + encodeURIComponent(id) + "/history?limit=" + HISTORY_PAGE);
    if (res.status === 401 || res.status === 403) { setBanner("⚠ 认证失败：请检查 Token。"); return "auth"; }
    if (res.status === 404) { setBanner("⚠ 会话不存在或已被删除。"); return "gone"; }
    if (!res.ok) throw new Error("HTTP " + res.status);
    const data = await res.json();
    const entries = Array.isArray(data) ? data : (data.entries || []);
    state.nextBeforeSeq = (data.next_before_seq !== undefined ? data.next_before_seq : null);
    state.olderDone = (state.nextBeforeSeq === null);
    if (state.initSource !== "snapshot") {
      renderHistory(entries);
      state.initSource = "history";
    }
    return "ok";
  } catch (e) {
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
  state.loadingOlder = true;
  const prevHeight = els.messages.scrollHeight;   // 插入前高度
  try {
    const res = await api("/api/sessions/" + encodeURIComponent(state.sessionId)
      + "/history?before_seq=" + state.nextBeforeSeq + "&limit=" + HISTORY_PAGE);
    if (res.status === 401 || res.status === 403) { setBanner("⚠ 认证失败：请检查 Token。"); return; }
    if (!res.ok) return;                          // 静默失败，下次滚动重试
    const data = await res.json();
    const entries = Array.isArray(data) ? data : (data.entries || []);
    state.nextBeforeSeq = (data.next_before_seq !== undefined ? data.next_before_seq : null);
    if (state.nextBeforeSeq === null) state.olderDone = true;
    if (entries.length) {
      renderEntries(entries, true);               // 前置插入
      // 保持滚动位置：内容在顶部增高，scrollTop 相应下移
      els.messages.scrollTop += els.messages.scrollHeight - prevHeight;
    }
  } catch (e) {
    // 静默失败，下次滚动重试
  } finally {
    state.loadingOlder = false;
  }
}

/* history 加载（可选）+ 连接 SSE；auth/gone 时不再重试连接 */
function openWith(id, withHistory) {
  const step = withHistory ? loadHistory(id) : Promise.resolve("ok");
  step.then((r) => {
    if (state.sessionId !== id) return;   // 已切换会话：丢弃过期回调
    if (r === "auth" || r === "gone") return;
    connectSSE(id);
  });
}

/* 输入框左下角元信息：当前会话的 model · role（对齐 TUI 输入框左下角）。
   数据来自 state.lastList（/api/sessions 已含 model/role 字段）；幂等：
   内容没变就不动 DOM。两者都空 → hidden。 */
function updateComposerMeta() {
  const meta = els.composerMeta;
  if (!meta) return;
  const s = (state.lastList || []).find((x) => x.id === state.sessionId);
  const model = s && s.model ? String(s.model) : "";
  const role = s && s.role ? String(s.role) : "";
  if (!model && !role) {
    if (!meta.hidden || meta.textContent !== "") {   // 幂等：仅在变化时触碰 DOM
      meta.hidden = true;
      meta.textContent = "";
      meta.title = "";
    }
    return;
  }
  const text = role ? model + " · " + role : model;
  if (meta.hidden || meta.textContent !== text) {
    meta.hidden = false;
    meta.textContent = text;
    meta.title = s.model || "";   // 完整 model 名（可能被省略号截断）
  }
}

/* 切走前保存当前会话视图状态（消息 DOM、滚动位置、分页游标、输入草稿），
   切回时原样恢复、不重新加载历史。 */
function saveSessionState() {
  if (!state.sessionId || state.view !== "chat") return;
  state.sessionStates[state.sessionId] = {
    html: els.messages.innerHTML,
    scrollTop: els.messages.scrollTop,
    nextBeforeSeq: state.nextBeforeSeq,
    olderDone: state.olderDone,
    draft: els.promptInput.value,
  };
}

function openSession(id) {
  saveSessionState();          // 切走：保存当前会话（消息/滚动/分页/草稿）
  state.renameActive = false;  // 切换会话会销毁编辑框：清标志，恢复轮询重绘
  stopSSE();
  state.sessionId = id;
  state.view = "chat";
  // 任何手动打开都会取代/完成 URL 深链，避免返回列表后深链再次触发
  state.deepLink.handled = true;
  state.deepLink.pending = null;
  els.listView.classList.add("hidden");
  els.chatView.classList.remove("hidden");
  els.topActions.hidden = false;
  els.chatSessionId.textContent = "会话 " + id;
  updateComposerMeta();          // 显示当前会话 model · role（缓存/首开/恢复共用）
  els.usageInfo.textContent = "";
  applyStatus("Idle");
  refreshBanner();
  history.replaceState(null, "", "/?session=" + encodeURIComponent(id));
  const cached = state.sessionStates[id];
  if (cached) {
    // 切回缓存过的会话：恢复视图，不重新加载历史；SSE 重连但跳过 snapshot
    state.initSource = "restored";
    state.nextBeforeSeq = cached.nextBeforeSeq;
    state.loadingOlder = false;
    state.olderDone = cached.olderDone;
    state.acc = newAccumulator();
    els.messages.innerHTML = cached.html;
    els.messages.scrollTop = cached.scrollTop;
    els.promptInput.value = cached.draft || "";
    autosizeInput();
    const m = els.messages;
    const atBottom = m.scrollHeight - m.scrollTop - m.clientHeight <= 4;
    userScrolled = !atBottom;      // 恢复到非底部位置：不自动跟随滚动
    els.jumpBottomBtn.hidden = atBottom;
    openWith(id, false);           // 只重连 SSE，跳过历史加载与 snapshot
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
    openWith(id, true);
  }
  renderSidebarTree();             // 更新 .current 高亮
}

function backToList() {
  saveSessionState();          // 返回列表也保存视图状态：再次打开该会话时恢复
  state.renameActive = false;  // 同 openSession：视图切换即销毁编辑框
  stopSSE();
  state.sessionId = null;
  state.view = "list";
  state.acc = null;
  state.nextBeforeSeq = null;
  state.loadingOlder = false;
  state.olderDone = false;
  els.chatView.classList.add("hidden");
  els.listView.classList.remove("hidden");
  els.topActions.hidden = true;
  refreshBanner();
  history.replaceState(null, "", "/");
  pollSessions();
}

/* 发送 / 取消 / 压缩 */
async function sendPrompt() {
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
  if (raw === "/rename") {
    setBanner("用法：/rename <标题>（留空可清除）");
    return;   // 保留输入框，方便直接补参数
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
  if (state.view === "list") {
    stopPolling();
    startPolling();
    pollSessions();
  } else if (state.view === "chat" && state.sessionId) {
    // 重新走一遍打开流程（重新认证）
    const id = state.sessionId;
    stopSSE();
    state.initSource = null;
    openWith(id, true);
  }
}

function startPolling() {
  stopPolling();
  state.pollTimer = setInterval(pollSessions, 1000);
}
function stopPolling() {
  if (state.pollTimer) { clearInterval(state.pollTimer); state.pollTimer = null; }
}

/* =====================================================================
 * 会话侧边栏：运行中任务（复用原任务面板逻辑，渲染进 #sidebarTasks）
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
    return null;   // 网络/404：静默，保持徽标现状，不刷屏
  }
}

/* 任务数显示在侧边栏「运行中任务 (N)」标题上（原顶栏任务按钮徽标） */
function updateTasksTitle(count) {
  const n = Number(count) || 0;
  if (els.sidebarTasksTitle) {
    els.sidebarTasksTitle.textContent = "运行中任务" + (n > 0 ? " (" + n + ")" : "");
  }
}

/* 徽标轮询（独立 3s，侧边栏关闭时也运行）；序号防竞态 */
async function pollTasksBadge() {
  const seq = ++state.tasks.badgeSeq;
  const tasks = await fetchTasks();
  if (tasks === null || seq !== state.tasks.badgeSeq) return;  // 过期响应丢弃
  updateTasksTitle(tasks.length);
}

/* 侧边栏打开时的任务列表刷新（2s 一次） */
async function refreshSidebarTasks() {
  const seq = ++state.tasks.panelSeq;
  const tasks = await fetchTasks();
  if (tasks === null || seq !== state.tasks.panelSeq || !state.sidebar.open) return;
  renderTaskList(tasks, els.sidebarTasks);
}

function shortTaskLabel(t) {
  // delegate 显示 label；bash 显示截断的 full_command（label 兜底）
  if (t.kind === "delegate") return t.label || "子代理任务";
  return truncate(t.full_command || t.label || "", 80);
}

function renderTaskList(tasks, container) {
  const list = container || els.sidebarTasks;
  list.innerHTML = "";
  if (!tasks.length) {
    list.appendChild(el("div", "tasks-empty", "暂无运行中的任务"));
    return;
  }
  for (const t of tasks) {
    const row = el("div", "task-row");
    row.title = "点击展开 / 收起输出";
    const line = el("div", "task-line");
    const badge = el("span", "kind-badge " + (t.kind === "delegate" ? "delegate" : "bash"),
      t.kind === "delegate" ? "子代理" : "bash");
    line.appendChild(badge);
    line.appendChild(el("span", "task-label", shortTaskLabel(t)));
    if (t.role) line.appendChild(el("span", "task-meta trole", t.role));
    if (t.session_id) line.appendChild(el("span", "task-meta tsid", "会话 " + shortId(t.session_id)));
    const cancel = el("button", "task-cancel", "取消");
    cancel.title = "取消任务 " + (t.id != null ? t.id : "");
    cancel.addEventListener("click", async (ev) => {
      ev.stopPropagation();
      await cancelTask(t);
    });
    line.appendChild(cancel);
    row.appendChild(line);

    const out = (t.output != null && String(t.output).trim() !== "") ? String(t.output) : "";
    const pre = el("pre", "task-output" + (out ? "" : " empty"), out || "(无输出)");
    pre.hidden = true;
    row.appendChild(pre);
    row.addEventListener("click", () => { pre.hidden = !pre.hidden; });
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
    // 成功：立即刷新列表 + 徽标
    await refreshSidebarTasks();
    await pollTasksBadge();
  } catch (e) {
    setBanner("⚠ 取消失败：" + e.message);
  } finally {
    state.tasks.cancelling.delete(t.id);
  }
}

function openSidebar() {
  if (state.sidebar.open) return;
  state.sidebar.open = true;
  renderSidebarTree();                 // 树可能已随轮询刷新；打开时确保渲染
  els.sidebarOverlay.hidden = false;
  els.sidebar.hidden = false;
  // 双 rAF：先让浏览器以 -100% 位姿渲染一帧，再加 .open 触发左滑过渡
  requestAnimationFrame(() => requestAnimationFrame(() => els.sidebar.classList.add("open")));
  refreshSidebarTasks();
  stopTasksPanelPolling();
  state.tasks.panelTimer = setInterval(refreshSidebarTasks, 2000);
  // 聊天视图下会话轮询已停：补拉一次列表，树与 busy/current 状态保持新鲜
  if (state.view === "chat") refreshSessionsForSidebar();
}

function closeSidebar() {
  if (!state.sidebar.open) return;
  state.sidebar.open = false;
  els.sidebarOverlay.hidden = true;
  els.sidebar.classList.remove("open");
  stopTasksPanelPolling();
  window.setTimeout(() => {
    if (!state.sidebar.open) els.sidebar.hidden = true;
  }, 220);
}

/* 聊天视图下补拉会话列表（pollSessions 只在列表视图跑）；失败静默保留旧数据 */
async function refreshSessionsForSidebar() {
  if (!state.token) return;
  try {
    const res = await api("/api/sessions");
    if (!res.ok) return;
    const list = await res.json();
    if (!Array.isArray(list)) return;
    state.lastList = list;
    renderSidebarTree();
    updateComposerMeta();          // 聊天视图下列表刷新：同步 model/role（幂等）
  } catch (e) { /* 静默：保留旧列表 */ }
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
 * 数据来自 state.lastList（pollSessions）。列表未变化时跳过重绘，
 * 保留展开状态与滚动位置。
 * ===================================================================*/
const MAX_TREE_ROOTS = 15;
let lastTreeSig = "";

function sidebarTreeSig() {
  const list = state.lastList || [];
  // 签名含当前会话 id：打开/切换会话后重绘以更新 .current 高亮；
  // 含 label：subagent 任务标题变化时树要重绘；含 pinned：置顶态变化（轮询/本端）都要反映
  return state.sessionId + "|" + JSON.stringify(list.map((s) => [
    s.id, s.title || "", s.label || "", s.status, s.busy ? 1 : 0, s.active === false ? 0 : 1,
    s.entry_count ?? 0, s.parent_session_id || "", s.pinned === true ? 1 : 0,
  ]));
}

/* force=true 时无视签名强制重绘（筛选输入、展开全部按钮） */
function renderSidebarTree(force) {
  const tree = els.sidebarTree;
  if (!tree) return;
  const sig = sidebarTreeSig();
  if (!force && sig === lastTreeSig) return;   // 数据未变：保留展开/滚动状态
  lastTreeSig = sig;
  const prevScroll = tree.scrollTop;
  tree.innerHTML = "";
  const list = state.lastList || [];
  if (!list.length) {
    tree.appendChild(el("div", "tree-empty", "暂无会话"));
    return;
  }
  const childrenByParent = new Map();
  for (const s of list) {
    if (!s.parent_session_id) continue;
    if (!childrenByParent.has(s.parent_session_id)) childrenByParent.set(s.parent_session_id, []);
    childrenByParent.get(s.parent_session_id).push(s);
  }
  const rootIds = new Set(list.filter((s) => !s.parent_session_id).map((s) => s.id));
  const orphans = list.filter((s) => s.parent_session_id && !rootIds.has(s.parent_session_id));
  const filter = state.sidebar.filter;
  const roots = list.filter((s) => !s.parent_session_id);
  if (filter) {
    // 筛选：主会话匹配 title（无 title 回退 id），大小写不敏感；子会话随父显示
    const match = (s) => (s.title || s.id).toLowerCase().includes(filter);
    const matchedRoots = roots.filter(match);
    const matchedOrphans = orphans.filter(match);
    if (!matchedRoots.length && !matchedOrphans.length) {
      tree.appendChild(el("div", "tree-empty", "无匹配会话"));
    } else {
      for (const s of matchedRoots) tree.appendChild(buildTreeRoot(s, childrenByParent.get(s.id) || []));
      if (matchedOrphans.length) tree.appendChild(buildTreeGroup("未关联", matchedOrphans));
    }
  } else {
    let shown = roots;
    let moreBtn = null;
    if (!state.sidebar.showAll && roots.length > MAX_TREE_ROOTS) {
      shown = roots.slice(0, MAX_TREE_ROOTS);
      const more = roots.length - shown.length;
      moreBtn = el("button", "tree-more", "+" + more + " 个更早的会话");
      moreBtn.type = "button";
      moreBtn.title = "显示全部主会话";
      moreBtn.addEventListener("click", () => {
        state.sidebar.showAll = true;
        renderSidebarTree(true);
      });
    }
    for (const s of shown) tree.appendChild(buildTreeRoot(s, childrenByParent.get(s.id) || []));
    if (moreBtn) tree.appendChild(moreBtn);
    if (orphans.length) tree.appendChild(buildTreeGroup("未关联", orphans));
  }
  tree.scrollTop = prevScroll;
}

function buildTreeRoot(s, kids) {
  const node = el("div", "tree-node");
  const row = el("div", "tree-row" + (state.sessionId === s.id ? " current" : "") +
    (s.pinned === true ? " pinned" : ""));
  const hasKids = kids.length > 0;
  const toggle = el("button", "tree-toggle", hasKids ? "▸" : "");
  toggle.type = "button";
  toggle.disabled = !hasKids;      // 无子会话时留位（占 24px，不响应）
  if (hasKids) {
    toggle.title = "展开 / 收起子会话";
    toggle.addEventListener("click", (ev) => {
      ev.stopPropagation();          // 点击 ▸ 只展开/收起，不切换会话
      toggleSidebarNode(s.id, toggle, kids);
    });
  }
  const dot = el("span", "busy-dot" + (s.busy ? " busy" : ""));
  const title = s.title || shortId(s.id);
  const titleEl = el("span", "tree-id", title);
  titleEl.title = s.title || s.id;        // 完整 title（无 title 时回退完整 id）
  const edit = el("button", "tree-edit", "✎");
  edit.type = "button";
  edit.title = "重命名";
  edit.addEventListener("click", (ev) => {
    ev.stopPropagation();                  // 不触发切换会话
    enterRename(titleEl, s, () => { renderSidebarTree(true); });
  });
  const count = el("span", "tree-count", (s.entry_count ?? 0) + " 条");
  // 📌 置顶按钮（仅主会话根节点）：放行尾 count 后。subagent 子节点不加——
  // pin 是会话级操作，subagent 的置顶语义后续需要时再单独支持。
  const pin = el("button", "pin-btn" + (s.pinned === true ? " on" : ""), "📌");
  pin.type = "button";
  pin.title = "置顶/取消置顶";
  pin.addEventListener("click", (ev) => {
    ev.stopPropagation();                  // 不触发切换会话
    togglePin(s, () => { renderSidebarTree(true); renderSessionList(state.lastList); });
  });
  row.append(toggle, dot, titleEl, edit, count, pin);
  row.title = (s.title || s.id) + (s.model ? " · " + s.model : "") + (s.busy ? "（处理中）" : "");
  row.addEventListener("click", () => {
    if (s.active === false) { resumeSession(s.id); return; }   // 与列表页一致：历史会话先恢复
    openSession(s.id);
  });
  node.appendChild(row);
  if (hasKids) {
    const children = el("div", "tree-children");
    children.hidden = true;
    // 筛选时匹配的父节点直接展开显示全部子会话；否则按展开状态
    const showKids = !!state.sidebar.filter || state.sidebar.expanded.has(s.id);
    if (showKids) {
      children.hidden = false;
      toggle.classList.add("open");
      toggle.textContent = "▾";
      renderTreeChildren(children, kids);
    }
    node.appendChild(children);
  }
  return node;
}

function toggleSidebarNode(id, toggle, kids) {
  const children = toggle.closest(".tree-node").querySelector(".tree-children");
  if (!children) return;
  if (children.hidden) {
    children.hidden = false;
    toggle.classList.add("open");
    toggle.textContent = "▾";
    renderTreeChildren(children, kids);   // 展开时才渲染子节点（400+ 会话不拖慢树）
    state.sidebar.expanded.add(id);
  } else {
    children.hidden = true;
    toggle.classList.remove("open");
    toggle.textContent = "▸";
    state.sidebar.expanded.delete(id);
  }
}

/* 父节点 / 「未关联」组的子节点渲染：默认只显示活跃 subagent
   （active !== false，即活跃或 busy），非活跃收进折叠的
   「历史子会话 (N)」分组（buildHistGroup，默认收起、点击展开）。 */
function renderTreeChildren(container, kids) {
  container.innerHTML = "";
  const active = [], hist = [];
  for (const k of kids) (k.active === false ? hist : active).push(k);
  renderSubagentRows(container, active);
  if (hist.length) container.appendChild(buildHistGroup(hist));
}

/* 渲染 subagent 行（不做活跃/历史分组）；hist=true 时行灰显小字 */
function renderSubagentRows(container, kids, hist) {
  for (const k of kids) {
    const row = el("div", "tree-row tree-row-child" + (hist ? " tree-hist" : "") +
                   (state.sessionId === k.id ? " current" : ""));
    const dot = el("span", "busy-dot" + (k.busy ? " busy" : ""));
    // label 优先：subagent 的任务面板标题最友好；旧 server 无 label → 回退 title/id
    const title = k.label || k.title || shortId(k.id);
    const titleEl = el("span", "tree-id", title);
    titleEl.title = k.label || k.title || k.id;
    const edit = el("button", "tree-edit", "✎");
    edit.type = "button";
    edit.title = "重命名";
    edit.addEventListener("click", (ev) => {
      ev.stopPropagation();
      enterRename(titleEl, k, () => { renderSidebarTree(true); });
    });
    const badge = el("span", "child-badge", "子");
    row.append(dot, titleEl, edit, badge);
    // busy 的 subagent：title 提示可发送消息（点击行 openSession 是现有行为，保持不变）
    row.title = (k.label || k.title || k.id) + (k.busy ? "（处理中）· 可发送消息" : "");
    row.addEventListener("click", () => {
      if (k.active === false) { resumeSession(k.id); return; }
      openSession(k.id);
    });
    container.appendChild(row);
  }
}

/* 「历史子会话 (N)」折叠分组：非活跃 subagent 默认收起，点击展开；
   不持久化展开状态（每次重绘默认折叠，简单）。 */
function buildHistGroup(kids) {
  const node = el("div", "tree-node");
  const row = el("div", "tree-row tree-hist-row");
  const toggle = el("button", "tree-toggle", "▸");
  toggle.type = "button";
  toggle.title = "展开 / 收起";
  toggle.addEventListener("click", (ev) => toggleTreeGroup(ev, toggle));
  const idEl = el("span", "tree-id tree-group tree-hist-label", "历史子会话 (" + kids.length + ")");
  row.append(toggle, idEl);
  node.appendChild(row);
  const children = el("div", "tree-children");
  children.hidden = true;
  renderSubagentRows(children, kids, true);
  node.appendChild(children);
  return node;
}

/* 「未关联」分组：孤儿 subagent 的根节点，默认折叠 */
function buildTreeGroup(label, kids) {
  const node = el("div", "tree-node");
  const row = el("div", "tree-row");
  const toggle = el("button", "tree-toggle", "▸");
  toggle.type = "button";
  toggle.title = "展开 / 收起";
  toggle.addEventListener("click", (ev) => toggleTreeGroup(ev, toggle));
  const idEl = el("span", "tree-id tree-group", label);
  row.append(toggle, idEl);
  node.appendChild(row);
  const children = el("div", "tree-children");
  children.hidden = true;
  renderTreeChildren(children, kids);
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
    toggle.textContent = "▾";
  } else {
    children.hidden = true;
    toggle.classList.remove("open");
    toggle.textContent = "▸";
  }
}

/* =====================================================================
 * 会话重命名（✎ → PUT /api/sessions/{id}/title）
 * 侧边栏树节点与列表页行共用 enterRename：点击 ✎ 后标题文本原位换成
 * <input> + ✓/×，box 上的点击 stopPropagation 屏蔽行点击（打开会话）。
 * Enter 保存、Esc 取消；空输入保存 = 清除标题（树/列表回退显示 id）。
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
    // 树 DOM 可能持有轮询前旧数组里的对象引用（列表数据未变时 renderSidebarTree
    // 被签名跳过、不重绘，但 state.lastList 每次轮询都是新数组）：保存时按 id
    // 从 state.lastList 重新解析，确保写回的是当前渲染所用的对象
    const cur = (state.lastList || []).find((x) => x.id === s.id) || s;
    const ok = await saveTitle(cur, val);
    if (!ok) { input.disabled = false; input.focus(); return; }   // 失败：保留编辑框可重试
    state.renameActive = false;
    afterSave();                // 成功：调用方重绘（树 renderSidebarTree(true) / 列表 renderSessionList）
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
  state.renameActive = true;    // 编辑期间列表页轮询重绘跳过，避免编辑框被冲掉
}

/* PUT 保存标题；成功写回传入的会话对象（即 state.lastList 里的对象，
   空=清除 → null，树/列表回退显示 id）。返回是否成功（false 时调用方
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
   传入的会话对象（即 state.lastList 里的对象）并回调调用方重绘（列表行与树主
   节点共用）。旧服务器没有 pin 端点：404/405 统一提示「服务器不支持置顶」
   （与重命名一致）；pinned 字段缺失（undefined）视为未置顶。
   列表顺序信任后端（pinned 置顶在前、组内 last_active_at 降序），前端不重排。 */
async function togglePin(s, afterToggle) {
  if (!state.token) { setBanner("⚠ 请先输入 Token。", true); return; }
  const target = s.pinned !== true;   // 兼容旧 server 无 pinned 字段：undefined → 置顶
  try {
    const res = await api("/api/sessions/" + encodeURIComponent(s.id) + "/pin",
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
    // 轮询可能已换掉 state.lastList 里的对象引用（列表数据未变时树/列表不重绘，
    // 但数组每轮都是新的）：按 id 重新解析当前对象再写回，确保重绘反映新状态
    const cur = (state.lastList || []).find((x) => x.id === s.id) || s;
    cur.pinned = target;
    afterToggle();
  } catch (e) {
    setBanner("⚠ 置顶失败：" + e.message, true);
  }
}

function stopTasksPanelPolling() {
  if (state.tasks.panelTimer) { clearInterval(state.tasks.panelTimer); state.tasks.panelTimer = null; }
}

function startTasksBadgePolling() {
  stopTasksBadgePolling();
  state.tasks.badgeTimer = setInterval(pollTasksBadge, 3000);
}
function stopTasksBadgePolling() {
  if (state.tasks.badgeTimer) { clearInterval(state.tasks.badgeTimer); state.tasks.badgeTimer = null; }
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
els.backBtn.addEventListener("click", backToList);
els.backBtn.addEventListener("click", closeSidebar);   // 返回列表时收起侧边栏
els.sidebarBtn.addEventListener("click", () => {
  if (state.sidebar.open) closeSidebar();
  else openSidebar();
});
els.sidebarCloseBtn.addEventListener("click", closeSidebar);
els.sidebarOverlay.addEventListener("click", closeSidebar);   // 点遮罩关闭
els.sidebarFilter.addEventListener("input", () => {
  state.sidebar.filter = els.sidebarFilter.value.trim().toLowerCase();
  state.sidebar.showAll = false;   // 清空筛选后回到默认 15 条限制
  renderSidebarTree(true);
});
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && state.sidebar.open) closeSidebar();
});
els.sendBtn.addEventListener("click", sendPrompt);
els.cancelBtn.addEventListener("click", cancelTurn);
els.compactBtn.addEventListener("click", compactSession);
els.promptInput.addEventListener("keydown", (e) => {
  if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); sendPrompt(); }
});
els.promptInput.addEventListener("input", autosizeInput);
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
els.jumpBottomBtn.addEventListener("click", () => {
  userScrolled = false;   // 显式回底：覆盖「用户在看历史」的锁定
  scrollBottom(true);
  els.jumpBottomBtn.hidden = true;
});

function init() {
  refreshBanner();
  els.chatView.classList.add("hidden");
  els.topActions.hidden = true;
  // URL 深链：?session=<id>。只在列表拿到数据（pollSessions）且 token 就绪后
  // 打开一次；token 为空时先记录，用户填 token 触发 restartTransport 再处理。
  const dl = new URLSearchParams(location.search).get("session");
  if (dl) state.deepLink.pending = dl;
  startPolling();
  pollSessions();
  // 任务数（侧边栏标题）：独立轻量轮询，侧边栏关闭时也保持更新（无 token 时静默跳过）
  updateTasksTitle(0);
  startTasksBadgePolling();
  pollTasksBadge();
}

init();
