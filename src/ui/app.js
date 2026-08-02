/* =============================================================================
 * app.js — 核心：全局状态（state）、常量、DOM 引用（els）、通用工具函数、
 * 认证（api/token）。共享的状态/常量/工具必须在本文件最先声明。
 *
 * 加载顺序（server.rs 与 test_harness.py 按同一清单拼接进同一个 <script>）：
 *   app.js → render.js → sessions.js → tasks.js → sse.js
 * 同一 script 内顶层 function 声明全部提升（hoisting），跨文件互调无需
 * export/import；顶层 const/let 按拼接顺序初始化，因此 state/els/常量
 * 必须位于最前。事件绑定与启动（init）在最后一个文件 sse.js 尾部执行，
 * 保证所有定义就绪后再绑定/启动。
 * =============================================================================*/


"use strict";

/* 历史分页：每页条数。长会话分块加载/渲染，避免一次渲染上千条 DOM
   导致移动/桌面浏览器滚动卡死。 */
const HISTORY_PAGE = 200;

/* 长文本「预览 + 展开全文」阈值：超过则默认折叠为预览 + 展开按钮。
   工具参数原 600 也统一为此值（预览 + 一键展开的成本相同，统一更可预期）。 */
const LONG_TEXT_THRESHOLD = 300;

/* 消息列表上限：els.messages 直接子块超过该数时，把最早的一批「已完成」
   块折叠进顶部占位 details（.older-collapse）。只约束渲染块数，不删数据、
   不动后端（历史仍可滚动分页加载）。 */
const MAX_MESSAGE_BLOCKS = 300;

/* 输入框默认 placeholder；applyStatus 在 Finished 之外的状态恢复它
   （openSession 每次 applyStatus("Idle") 都会重置输入区）。 */
const PROMPT_PLACEHOLDER = "输入消息：Enter 发送，Shift+Enter 换行…";

/* =====================================================================
 * 全局状态
 * ===================================================================*/
const state = {
  token: localStorage.getItem("eagent_token") || "",
  workspaces: [],            // [{id,name,url,token}] 多服务器实例（initWorkspaces 填充）
  workspace: null,           // 当前激活的 workspace 对象（state.token 是其派生字段）
  view: "list",              // "list" | "chat"
  sessionId: null,           // 当前打开的会话
  status: "Idle",
  initSource: null,          // "history" | "snapshot" | null —— 初始渲染来源
  pollTimer: null,
  lastValidateSig: null,     // 上一轮校验问题签名（相同则不再刷 banner）
  validateBannerUp: false,   // 当前 banner 是否由校验提示占用（恢复数据后只清自己的）
  sse: { ctrl: null, retryTimer: null, stopped: false },
  acc: null,                 // 增量渲染累积器（见 newAccumulator）
  nextBeforeSeq: null,       // 历史分页游标：下一段更早历史的 before_seq（loadHistory 响应里取；null=没有更多）
  loadingOlder: false,       // 是否正在加载更早历史（防重入）
  olderDone: false,          // 更早历史已全部加载（next_before_seq 为 null）
  searchQuery: "",           // 会话列表搜索词（已小写化）；轮询重绘后过滤依然生效
  lastList: [],              // 最近一次轮询拿到的完整列表，供搜索框重绘
                             //（聚合模式下 = 激活 workspace 的列表；所有既有单服务器路径不变）
  workspaceLists: {},        // workspaceId -> session[]：每台服务器各自的 /api/sessions 缓存
                             //（聚合侧边栏/聚合列表视图的数据源；由各自 pollWorkspaceSessions 刷新）
  workspaceErrors: {},       // workspaceId -> error string|null：轮询失败标记（侧边栏显示「无法连接」）
  queue: [],                 // 排队提示（FIFO；最多显示 3 条 + "+N"）
  queueExpanded: false,      // 排队条是否展开显示全部（默认收起）
  deepLink: { pending: null, handled: false },  // URL ?session= 深链：待打开 id + 一次性标志
  sessionStates: {},         // sessionId -> {html, scrollTop, nextBeforeSeq, olderDone, draft}：切走时保存，切回时恢复（不重新加载历史）
  sidebar: {                 // 会话侧边栏
    open: false,             // 是否打开
    expanded: new Set(),     // 已展开的主会话 id（会话树；重绘时保留）
    filter: "",              // 筛选关键词（已小写化）；空 = 默认显示
    showAllWs: new Set(),   // 每 workspace 独立：已展开全部主会话的 wsId 集合（超出 15 条限制时）
  },
  tasks: {                   // 运行中任务（composer 折叠条/面板 + 消息列表输出块）
    seq: 0,                  // 统一轮询竞态序号：只应用最新一次响应
    timer: null,             // 统一轮询定时器（2s 常驻；替代原徽标/面板双轮询）
    list: [],                // 最近一次 /api/tasks 结果（两处渲染共用）
    cancelling: new Set(),   // 正在取消的任务 id（防重复点击）
    composerOpen: false,     // composer 任务面板展开状态（默认收起）
    pollers: new Map(),      // 展开中 bash 行的 output 轮询句柄（key=session_id:id → interval id）
    streams: new Map(),      // 展开中 delegate 行的 SSE AbortController（key=session_id:id）
    streamText: new Map(),   // 展开中 delegate 行的已累积流式文本（key → string，重绘恢复用）
    degraded: new Set(),     // bash 行 output 端点 404/不可用 → 降级静态尾部（key；重绘不再重启轮询）
  },
  renameActive: false,       // 行内重命名进行中：列表页 1s 轮询重绘跳过，防编辑框被冲掉
};

/* 常用 DOM 引用 */
const $ = (id) => document.getElementById(id);
const els = {
  topActions: $("topActions"), backBtn: $("backBtn"), backParentBtn: $("backParentBtn"), connState: $("connState"),
  banner: $("banner"), bannerText: $("bannerText"), bannerClose: $("bannerClose"),
  tokenInput: $("tokenInput"), tokenToggle: $("tokenToggle"),
  listView: $("listView"), chatView: $("chatView"),
  newPrompt: $("newPrompt"), newSessionBtn: $("newSessionBtn"),
  sessionList: $("sessionList"), listMeta: $("listMeta"), listHint: $("listHint"),
  searchInput: $("searchInput"),
  chatSessionId: $("chatSessionId"), chatStatus: $("chatStatus"), usageInfo: $("usageInfo"),
  messages: $("messages"), promptInput: $("promptInput"), queueBar: $("queueBar"),
  slashMenu: $("slashMenu"), forkMenu: $("forkMenu"),
  composerMeta: $("composerMeta"),
  jumpBottomBtn: $("jumpBottomBtn"),
  sendBtn: $("sendBtn"), cancelBtn: $("cancelBtn"), compactBtn: $("compactBtn"),
  sidebarBtn: $("sidebarBtn"), sidebarOverlay: $("sidebarOverlay"),
  sidebar: $("sidebar"), sidebarCloseBtn: $("sidebarCloseBtn"),
  sidebarFilter: $("sidebarFilter"),
  sidebarTree: $("sidebarTree"),
  tasksToggleBar: $("tasksToggleBar"), composerTasks: $("composerTasks"),
  workspaceSelect: $("workspaceSelect"), workspaceAddBtn: $("workspaceAddBtn"),
  workspaceRemoveBtn: $("workspaceRemoveBtn"), workspaceEditor: $("workspaceEditor"),
  wsNameInput: $("wsNameInput"), wsUrlInput: $("wsUrlInput"), wsTokenInput: $("wsTokenInput"),
  wsSaveBtn: $("wsSaveBtn"), wsCancelBtn: $("wsCancelBtn"),
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

/* 图钉 SVG（fill 继承 currentColor——emoji 📌 不吃 color，状态色
   （灰⇄金）必须用 SVG 才能切换）。返回 innerHTML 字符串。 */
function pinSvg() {
  return '<svg class="pin-icon" viewBox="0 0 24 24" width="16" height="16" ' +
    'aria-hidden="true" focusable="false" fill="currentColor">' +
    '<path d="M8 2h8a1 1 0 0 1 .7 1.7l-1.2 1.2v4.35c0 .8.32 1.56.88 2.12l1.54 1.54A1.22 1.22 0 0 1 17.06 15H12.8L12 22.25 11.2 15H6.94a1.22 1.22 0 0 1-.86-2.09l1.54-1.54a3 3 0 0 0 .88-2.12V4.9L7.3 3.7A1 1 0 0 1 8 2Z"/>' +
    '</svg>';
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

let bannerTimer = null;         // 非 warn 提示自动消失计时器（新 setBanner 时先清旧计时器）

function setBanner(msg, isWarn) {
  if (bannerTimer) { clearTimeout(bannerTimer); bannerTimer = null; }
  els.bannerText.textContent = msg;
  els.banner.className = "banner" + (isWarn ? " warn" : "");
  els.banner.hidden = !msg;
  if (msg && !isWarn) {
    bannerTimer = setTimeout(() => { bannerTimer = null; setBanner(""); }, 5000);
  }
}
els.bannerClose.addEventListener("click", () => setBanner(""));

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
 * 多工作区（多服务器实例）
 * 每个 workspace = {id, name, url, token}：url 是另一台 e-agent 实例的
 * 根地址（去尾部斜杠；空 = 同源相对路径），token 是该实例的访问令牌。
 * 持久化：localStorage "eagent.workspaces"（JSON 数组）+
 * "eagent.activeWorkspace"（激活 id）。state.workspace 是激活对象；
 * state.token 是派生字段，始终跟随激活 workspace 的 token（既有代码
 * 全部读 state.token，保持兼容）。
 * ===================================================================*/
function normalizeWorkspaceUrl(url) {
  return String(url || "").trim().replace(/\/+$/, "");
}

function saveWorkspaces() {
  try {
    localStorage.setItem("eagent.workspaces", JSON.stringify(state.workspaces));
    localStorage.setItem("eagent.activeWorkspace", state.workspace.id);
  } catch (e) { /* localStorage 不可用（隐私模式等）：静默，功能退化为本次会话内 */ }
}

/* 启动时加载；从未保存过 → 建默认条目 {id:"default", name:"默认", url:"",
   token:state.token}，既有单服务器行为完全不变。 */
function initWorkspaces() {
  let list = null;
  try {
    const raw = localStorage.getItem("eagent.workspaces");
    if (raw) {
      const parsed = JSON.parse(raw);
      if (Array.isArray(parsed) && parsed.length) list = parsed;
    }
  } catch (e) { list = null; }
  if (!list) list = [{ id: "default", name: "默认", url: "", token: state.token || "" }];
  for (const ws of list) {
    if (!ws.id) ws.id = "ws-" + Math.random().toString(36).slice(2, 10);
    if (!ws.name) ws.name = ws.url || "默认";
    ws.url = normalizeWorkspaceUrl(ws.url);
    if (!ws.token) ws.token = "";
  }
  state.workspaces = list;
  state.workspaceLists = {};     // 重新加载：丢弃旧缓存（轮询会重新填充）
  state.workspaceErrors = {};
  let active = null;
  try {
    const activeId = localStorage.getItem("eagent.activeWorkspace");
    active = list.find((w) => w.id === activeId) || null;
  } catch (e) { active = null; }
  if (!active) active = list[0];
  state.workspace = active;
  state.token = active.token || "";   // state.token 跟随激活 workspace
  saveWorkspaces();
  renderWorkspaceSelect();
}

/* 激活 workspace 的 base url（已去尾部斜杠；空 = 同源相对路径） */
function workspaceBaseUrl() {
  const ws = state.workspace;
  return (ws && ws.url) ? ws.url : "";
}

/* 相对 API 路径 → 完整 URL：base（无尾斜杠）+ 以 "/" 开头的 path。
   空 base → 原样返回（同源相对请求，既有行为不变）。 */
function fullUrl(path) {
  const base = workspaceBaseUrl();
  if (!base) return path;
  return base + (path.startsWith("/") ? path : "/" + path);
}

function renderWorkspaceSelect() {
  if (!els.workspaceSelect) return;
  els.workspaceSelect.innerHTML = "";
  for (const ws of state.workspaces) {
    const opt = document.createElement("option");
    opt.value = ws.id;
    opt.textContent = ws.name;
    if (ws === state.workspace) opt.selected = true;
    els.workspaceSelect.appendChild(opt);
  }
  if (els.workspaceRemoveBtn) {
    els.workspaceRemoveBtn.disabled = state.workspaces.length <= 1;   // 唯一一个时禁止删除
  }
}

/* 跨 workspace 打开/恢复的竞态防护：每次 openSessionIn/resumeSessionIn 递增
   一个 token，切换完成后只认自己那次（更新的打开/切换会使旧 token 失效），
   防止「快速点击两个不同服务器的会话」时过期续开覆盖新打开。 */
let sessionOpenEpoch = 0;

/* 切换工作区：清空当前工作区的会话/聊天/任务状态，视图回到列表，重跑启动
   加载序列。async：聚合模式下 cross-server 打开先 await 它再 openSession
   （切换完成的 next tick 才开目标会话）。聚合侧边栏/列表里其它 workspace
   的缓存列表（state.workspaceLists）保留，切换后立即渲染、不等轮询。 */
async function switchWorkspace(id, epoch) {
  const ws = state.workspaces.find((w) => w.id === id);
  if (!ws || ws === state.workspace) return;
  // 直接切换（入口：下拉/侧边栏/增删服务器）声明新代次，使一切在途打开/
  // 恢复/历史加载失效；openSessionIn/resumeSessionIn 嵌套调用时传入共享的
  // 代次（claimed），不在此递增——一次动作只有一个 action epoch。
  const claimed = (epoch === undefined) ? ++sessionOpenEpoch : epoch;
  stopPolling();
  stopTasksPolling();
  stopSSE();
  // ---- 清空当前工作区的会话/聊天状态（其它 workspace 的聚合缓存保留） ----
  state.sessionId = null;
  state.view = "list";
  state.sessionStates = {};
  state.lastList = (state.workspaceLists[ws.id] !== undefined) ? state.workspaceLists[ws.id] : [];
  state.queue.length = 0;
  state.queueExpanded = false;
  state.acc = null;
  state.nextBeforeSeq = null;
  state.loadingOlder = false;
  state.olderDone = false;
  state.searchQuery = "";
  state.initSource = null;           // 旧工作区的历史/快照来源标志不跨工作区保留
  state.deepLink.pending = null;
  state.deepLink.handled = true;
  state.tasks.list = [];
  state.tasks.cancelling = new Set();
  state.tasks.pollers = new Map();
  state.tasks.streams = new Map();
  state.tasks.streamText = new Map();
  state.tasks.degraded = new Set();
  state.tasks.composerOpen = false;
  state.sidebar.expanded = new Set();
  state.sidebar.filter = "";
  state.sidebar.showAllWs = new Set();
  // ---- 切换激活 workspace；state.token 跟随其 token ----
  state.workspace = ws;
  state.token = ws.token || "";
  saveWorkspaces();
  if (state.token) localStorage.setItem("eagent_token", state.token);
  else localStorage.removeItem("eagent_token");
  // ---- 视图重置到列表页（清空 DOM） ----
  els.chatView.classList.add("hidden");
  els.listView.classList.remove("hidden");
  els.topActions.hidden = true;
  els.backParentBtn.hidden = true;
  els.messages.innerHTML = "";
  els.promptInput.value = "";
  els.promptInput.placeholder = PROMPT_PLACEHOLDER;
  els.sessionList.innerHTML = "";
  els.sidebarTree.innerHTML = "";
  els.searchInput.value = "";
  els.sidebarFilter.value = "";
  els.queueBar.hidden = true;
  els.tasksToggleBar.hidden = true;
  els.composerTasks.hidden = true;
  els.composerTasks.innerHTML = "";
  els.usageInfo.textContent = "";
  els.composerMeta.hidden = true;
  els.composerMeta.textContent = "";
  els.tokenInput.value = state.token;
  applyStatus("Idle");               // 重置状态条/按钮/placeholder
  closeWorkspaceEditor();
  renderWorkspaceSelect();
  refreshBanner();
  updateTokenToggle();
  history.replaceState(null, "", "/");   // 丢弃可能指向旧工作区会话的 ?session= 深链
  // ---- 聚合视图立即重绘（用各 workspace 缓存列表，不等轮询） ----
  renderSessionList();
  renderSidebarTree(true);
  // ---- 重跑启动加载序列（与 init() 的加载部分一致） ----
  startPolling();
  pollAllWorkspaces();
  startTasksPolling();
  pollTasks();
}

function openWorkspaceEditor() {
  els.wsNameInput.value = "";
  els.wsUrlInput.value = "";
  els.wsTokenInput.value = "";
  els.workspaceEditor.hidden = false;
  els.wsNameInput.focus();
}
function closeWorkspaceEditor() {
  if (els.workspaceEditor) els.workspaceEditor.hidden = true;
}

/* 「+」面板保存：新增一个 workspace 并立即激活（url 留空 = 同源） */
function saveWorkspaceFromEditor() {
  const ws = {
    id: "ws-" + Math.random().toString(36).slice(2, 10),
    name: els.wsNameInput.value.trim() || "服务器",
    url: normalizeWorkspaceUrl(els.wsUrlInput.value),
    token: els.wsTokenInput.value.trim(),
  };
  state.workspaces.push(ws);
  saveWorkspaces();
  closeWorkspaceEditor();
  switchWorkspace(ws.id);
}

/* 「×」删除当前 workspace（仅剩一个时按钮禁用，switchWorkspace 里再挡一道） */
function removeActiveWorkspace() {
  if (state.workspaces.length <= 1) return;
  const removed = state.workspace;
  const idx = state.workspaces.indexOf(removed);
  state.workspaces.splice(idx, 1);
  delete state.workspaceLists[removed.id];    // 清理聚合缓存：被删服务器不再显示
  delete state.workspaceErrors[removed.id];
  const next = state.workspaces[Math.max(0, idx - 1)] || state.workspaces[0];
  saveWorkspaces();
  switchWorkspace(next.id);
}

els.workspaceSelect.addEventListener("change", () => switchWorkspace(els.workspaceSelect.value));
els.workspaceAddBtn.addEventListener("click", openWorkspaceEditor);
els.workspaceRemoveBtn.addEventListener("click", removeActiveWorkspace);
els.wsSaveBtn.addEventListener("click", saveWorkspaceFromEditor);
els.wsCancelBtn.addEventListener("click", closeWorkspaceEditor);

/* 启动即加载（须在下方 token 区块读取 state.token 之前执行：
   els.tokenInput.value 要跟随激活 workspace 的 token） */
initWorkspaces();

/* =====================================================================
 * Token 与认证
 * ===================================================================*/
let tokenBoxOpen = false;          // token 输入框展开状态（默认折叠，只显示按钮）
let tokenBlurTimer = null;         // 失焦延迟收起句柄（防止「点进去正要输入就收起」）

/* 折叠按钮文案/高亮跟随 token 设置状态 */
function updateTokenToggle() {
  if (!els.tokenToggle) return;
  const set = !!state.token;
  els.tokenToggle.textContent = set ? "🔑 已设置" : "🔑 Token";
  els.tokenToggle.classList.toggle("set", set);
  els.tokenToggle.title = set ? "已设置 Token（点击展开 / 收起）" : "点击展开 Token 输入";
}

function setTokenBoxOpen(open) {
  tokenBoxOpen = open;
  els.tokenInput.hidden = !open;
  if (tokenBlurTimer) { clearTimeout(tokenBlurTimer); tokenBlurTimer = null; }
}

els.tokenInput.value = state.token;
els.tokenInput.addEventListener("input", () => {
  state.token = els.tokenInput.value.trim();
  // 同步进当前 workspace 并持久化：token 是 workspace 的字段之一
  if (state.workspace) state.workspace.token = state.token;
  if (state.token) localStorage.setItem("eagent_token", state.token);
  else localStorage.removeItem("eagent_token");
  saveWorkspaces();
  refreshBanner();
  restartTransport();   // token 变化 → 重启轮询 / SSE
  updateTokenToggle();  // 按钮文案跟随设置状态
});
els.tokenToggle.addEventListener("click", () => {
  if (tokenBoxOpen) { setTokenBoxOpen(false); els.tokenInput.blur(); }
  else { setTokenBoxOpen(true); els.tokenInput.focus(); }
});
/* 失焦延迟收起：blur 后短暂停留，期间重新聚焦（点进输入框）则取消收起 */
els.tokenInput.addEventListener("focus", () => {
  if (tokenBlurTimer) { clearTimeout(tokenBlurTimer); tokenBlurTimer = null; }
});
els.tokenInput.addEventListener("blur", () => {
  tokenBlurTimer = setTimeout(() => setTokenBoxOpen(false), 150);
});
setTokenBoxOpen(false);   // 默认折叠：只留按钮，不挤顶栏
updateTokenToggle();

function refreshBanner() {
  if (!state.token) {
    setBanner("⚠ 未配置访问令牌：请在右上角输入 Token，所有请求需要 Authorization: Bearer <token>。", true);
  } else {
    setBanner("");
  }
}

/* workspace 的生效 token：优先 workspace 自身 token；激活 workspace 未单独
   配置时回退全局 state.token（历史行为：token 输入框直接写 state.token，
   再同步进 workspace.token）。聚合模式下各服务器 token 彼此独立。 */
function workspaceToken(ws) {
  if (ws && ws.token) return ws.token;
  if (!ws || ws === state.workspace) return state.token || "";
  return "";
}

/* 指定 workspace 的请求入口：base url 与 token 各自独立
   （空 base = 同源相对请求；空 token = 不带 Authorization header） */
async function apiFor(ws, path, opts = {}) {
  const headers = Object.assign({}, opts.headers || {});
  const token = workspaceToken(ws);
  if (token) headers["Authorization"] = "Bearer " + token;
  if (opts.body && !headers["Content-Type"]) headers["Content-Type"] = "application/json";
  const base = (ws && ws.url) ? ws.url : "";
  const url = base ? base + (path.startsWith("/") ? path : "/" + path) : path;
  return fetch(url, Object.assign({}, opts, { headers }));
}

/* 统一请求入口（激活 workspace）：自动附带 Authorization header；路径自动
   前缀激活 workspace 的 base url（空 base = 同源相对请求，既有行为不变） */
async function api(path, opts = {}) {
  return apiFor(state.workspace, path, opts);
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
