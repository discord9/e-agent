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
  queue: [],                 // 排队提示（FIFO；最多显示 3 条 + "+N"）
  deepLink: { pending: null, handled: false },  // URL ?session= 深链：待打开 id + 一次性标志
  sessionStates: {},         // sessionId -> {html, scrollTop, nextBeforeSeq, olderDone, draft}：切走时保存，切回时恢复（不重新加载历史）
  sidebar: {                 // 会话侧边栏
    open: false,             // 是否打开
    expanded: new Set(),     // 已展开的主会话 id（会话树；重绘时保留）
    filter: "",              // 筛选关键词（已小写化）；空 = 默认显示
    showAll: false,          // 是否已展开全部主会话（超出 15 条限制时）
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
  if (state.token) localStorage.setItem("eagent_token", state.token);
  else localStorage.removeItem("eagent_token");
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
