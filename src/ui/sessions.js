/* =============================================================================
 * sessions.js — 会话：列表/字段校验/轮询（pollSessions/renderSessionList）、
 * 对话视图流程（loadHistory/loadOlder/openWith/openSession/backToList、
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

async function pollSessions() {
  if (state.view !== "list" || !state.token) return;
  try {
    const res = await api("/api/sessions");
    if (res.status === 401 || res.status === 403) {
      setBanner("⚠ 认证失败：请检查 Token 是否正确。");
      return;
    }
    if (!res.ok) throw new Error("HTTP " + res.status);
    let list;
    try {
      list = await res.json();
    } catch (e) {
      // 旧 server 返回 HTML 错误页等非 JSON：不崩，提示后跳过本轮；
      // 占住校验 banner 位（恢复后由 applyValidation 自动清除），
      // 并重置签名，恢复后的问题批次会重新上报
      state.validateBannerUp = true;
      state.lastValidateSig = null;
      setBanner("⚠ 服务器返回异常格式（非 JSON，可能为旧版服务器）。", true);
      return;
    }
    renderSessionList(Array.isArray(list) ? list : []);
    renderSidebarTree();                 // 侧边栏会话树随轮询刷新
    if (state.view === "chat") updateComposerMeta();   // model/role 可能随轮询更新（幂等）
    maybeHandleDeepLink(Array.isArray(list) ? list : []);
    applyValidation(validateSessions(list));   // 字段校验：只提示，不阻断渲染
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
    state.loadingOlder = false;
  }
}

/* history 加载（可选）+ 连接 SSE；auth/gone 时不再重试连接。
   onReady：视图就绪后的回调（历史渲染完 / 缓存恢复完，消息区可操作时
   触发一次）。 */
function openWith(id, withHistory, onReady) {
  const step = withHistory ? loadHistory(id) : Promise.resolve("ok");
  step.then((r) => {
    if (state.sessionId !== id) return;   // 已切换会话：丢弃过期回调
    if (r === "auth" || r === "gone") return;
    connectSSE(id);
    if (onReady) onReady();
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

function openSession(id, onReady) {
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
  // subagent 会话：显示「← 主会话」快速返回父会话（主会话/无父则隐藏）
  const cur = (state.lastList || []).find((s) => s.id === id);
  els.backParentBtn.hidden = !(cur && cur.parent_session_id);
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
    openWith(id, false, onReady);  // 只重连 SSE，跳过历史加载与 snapshot；
                                   // onReady 在恢复完成（微任务）后触发
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
    openWith(id, true, onReady);   // onReady 在 loadHistory 渲染完成后触发
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
  els.backParentBtn.hidden = true;
  refreshBanner();
  history.replaceState(null, "", "/");
  pollSessions();
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
};

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

/* input 事件驱动：更新菜单（显示/过滤/关闭）。输入、删除都走这里。 */
function updateSlashMenu() {
  const word = currentSlashWord();
  if (word === null) { closeSlashMenu(); return; }
  const items = SLASH_COMMANDS.filter((c) => c.name.startsWith(word));
  if (!items.length) { closeSlashMenu(); return; }   // 无匹配：关闭
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
    row.append(el("span", "slash-name", c.name), el("span", "slash-desc", c.desc));
    if (c.args) row.append(el("span", "slash-args", c.args));
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
};

async function openForkMenu() {
  if (!state.sessionId) return;
  forkMenu.open = true;
  forkMenu.loading = true;
  forkMenu.items = [];
  forkMenu.selected = 0;
  renderForkMenu();
  try {
    const res = await api("/api/sessions/" + encodeURIComponent(state.sessionId) + "/fork-candidates");
    if (res.status === 401 || res.status === 403) { setBanner("⚠ 认证失败：请检查 Token。", true); closeForkMenu(); return; }
    if (res.status === 404 || res.status === 405) { setBanner("⚠ 服务器不支持 fork（需新版后端）", true); closeForkMenu(); return; }
    if (!res.ok) throw new Error("HTTP " + res.status);
    const data = await res.json();
    forkMenu.items = Array.isArray(data) ? data : [];
    forkMenu.loading = false;
    renderForkMenu();
  } catch (e) {
    forkMenu.loading = false;
    setBanner("⚠ 加载 fork 候选失败：" + e.message, true);
    closeForkMenu();
  }
}

function closeForkMenu() {
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
  const savedItems = forkMenu.items;       // 409 冲突时重开面板保留候选
  const savedSelected = forkMenu.selected;
  closeForkMenu();
  try {
    const res = await api("/api/sessions/" + encodeURIComponent(sid) + "/fork",
      { method: "POST", body: JSON.stringify({ at: item.at }) });
    if (res.status === 401 || res.status === 403) { setBanner("⚠ 认证失败：请检查 Token。", true); return; }
    if (res.status === 409) {
      // 后端拒绝（非边界/越界）：提示原因并重开面板，保留候选供重选
      let msg = "HTTP 409";
      try {
        const d = await res.json();
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
  renderSidebarTree();                 // 树可能已随轮询刷新；打开时确保渲染
  els.sidebarOverlay.hidden = false;
  els.sidebar.hidden = false;
  // 双 rAF：先让浏览器以收起位姿渲染一帧，再加 .open 触发过渡
  // （桌面：width 0→280px；手机：translateX(-100%)→0）
  requestAnimationFrame(() => requestAnimationFrame(() => els.sidebar.classList.add("open")));
  pollTasks();   // 打开时立即刷新树内任务分组（统一轮询常驻，这里只求即时性）
  // 聊天视图下会话轮询已停：补拉一次列表，树与 busy/current 状态保持新鲜
  if (state.view === "chat") refreshSessionsForSidebar();
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
const MAX_TREE_ROOTS = 8;   // 默认只显示最近 8 个主会话（少滑即见）
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
  const toggle = el("button", "tree-toggle");
  toggle.type = "button";
  toggle.disabled = !hasKids;      // 无子会话时留位（占 24px，不响应）
  if (hasKids) {
    toggle.title = "展开 / 收起子会话";
    toggle.addEventListener("click", (ev) => {
      ev.stopPropagation();          // 点击箭头只展开/收起，不切换会话
      toggleSidebarNode(s.id, toggle, kids);
    });
  }
  const dot = el("span", "busy-dot" + (s.busy ? " busy" : ""));
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
    renderTreeChildren(children, kids);   // 展开时才渲染子节点（400+ 会话不拖慢树）
    state.sidebar.expanded.add(id);
  } else {
    children.hidden = true;
    toggle.classList.remove("open");
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
  const toggle = el("button", "tree-toggle");
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
  const toggle = el("button", "tree-toggle");
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
  } else {
    children.hidden = true;
    toggle.classList.remove("open");
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
