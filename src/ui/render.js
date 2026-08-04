/* =============================================================================
 * render.js — 消息渲染：marked 配置 + renderMarkdown、长文本预览展开
 * （maybeTruncateEl）、消息/思考/工具卡片/notice/错误/压缩线渲染、增量
 * 累积（newAccumulator/freezeAssistant/reattachInFlight）、历史全量渲染
 * （renderEntry/renderMessage/renderEntries）、消息上限 pruneMessages、
 * 状态/用量展示（applyStatus/applyUsage）。
 * 依赖 app.js（state/els/escapeHtml/el/truncate/shortId/LONG_TEXT_THRESHOLD
 * 等）；被 sessions.js/tasks.js/sse.js 调用。
 * =============================================================================*/

/* 长内容「预览 + 展开全文」辅助（Solarized Light 一致，文本可选中复制）：
   内容 ≤ threshold 直出（不增加任何交互成本）；超过则渲染截断预览 +
   「展开全文」按钮（箭头由 CSS ::after 绘制），完整文本以隐藏 span 常驻 DOM（display:none），
   点击按钮原地切换（展开显示全文 / 收起回预览，按钮文案随之切换）。
   完整文本常驻是为了 innerHTML 快照往返（会话缓存恢复、resync 离屏
   替换）后仍能展开——按钮监听走消息容器事件委托，快照重建后依然可点；
   代价是长内容在 DOM 里多一份文本。调用方传 pre（工具参数/结果、notice
   长文本等），保持 pre 语义：文本可选中复制。 */
function maybeTruncateEl(container, text, threshold, footerEl, label) {
  const s = String(text == null ? "" : text);
  const n = (threshold > 0) ? threshold : LONG_TEXT_THRESHOLD;
  if (s.length <= n) { container.textContent = s; return container; }
  container.textContent = "";
  container.classList.add("expandable");
  const preview = el("span", "expand-preview", s.slice(0, n) + "\n… ");
  const btn = el("button", "expand-toggle", "展开全文" + (label ? "（" + label + "）" : ""));
  btn.type = "button";
  // 记住按钮控制的正文 pre：卡片里可能有多个 .expandable（args/result），
  // 事件委托用这个引用精确定位，而不是就近找第一个
  btn._target = container;
  const full = el("span", "expand-full", s);
  container.append(preview, full);
  // 按钮在正文 pre 框内末尾（同框，控件样式与正文区分）；footerEl 保留
  // 为兼容参数（当前无调用传它）
  if (footerEl) footerEl.appendChild(btn);
  else container.appendChild(btn);
  return container;
}
/* =====================================================================
 * 文件工具差异化渲染：edit_file → -/+ 两段 diff、read_file → 行号+内容
 * 视图、write_file → 全新增单侧 diff。解析逻辑平移自 TUI
 * （tui/keys.rs parse_edit_arguments / parse_edited_line；渲染镜像
 * tui/state.rs push_diff_side 的 30 行/侧截断 + "… (N more lines)"）。
 * 行号列灰底右对齐、删除红/新增绿，样式在 style.css .tool-card 区。
 * ===================================================================*/
const DIFF_LINE_LIMIT = 30;      // 镜像 TUI push_diff_side 的每侧截断行数
const DELEGATE_COLLAPSE_THRESHOLD = 200;  // delegate 完成答案超过该字符数时默认折叠

/* "file edited (line N)" → N（TUI parse_edited_line 的 JS 版）；解析失败返回
   null（调用方按「不编号」处理，镜像 TUI 的 start.unwrap_or(0) + 空 label） */
function parseEditedLine(content) {
  const m = /^file edited \(line (\d+)\)$/.exec(String(content == null ? "" : content).trim());
  return m ? parseInt(m[1], 10) : null;
}

/* edit_file arguments（原始 JSON 字符串）→ {path, old, new}（TUI
   parse_edit_arguments 的 JS 版）；非 edit_file / 解析失败返回 null */
function parseEditArguments(args) {
  let v;
  try { v = JSON.parse(String(args)); } catch (e) { return null; }
  if (!v || typeof v !== "object") return null;
  const { path, old: oldText, new: newText } = v;
  if (typeof path !== "string" || typeof oldText !== "string" || typeof newText !== "string") return null;
  return { path, old: oldText, new: newText };
}

/* buildToolCard 时解析文件工具参数缓存到卡片 expando；innerHTML 快照往返
   （缓存恢复 / resync 离屏替换）后 expando 丢失，优先从 data-tool-args
   （原始参数，属性随快照序列化）重解析；再回退旧 .tool-args 的完整 JSON
   （展开态取 .expand-full 全文，直出取 textContent——兼容旧版本缓存快照，
   其 .tool-args 仍是 pretty JSON；新紧凑渲染不是 JSON，解析失败返回 null） */
function cardArgs(card) {
  if (card._toolArgs) return card._toolArgs;
  const raw = card.getAttribute("data-tool-args");
  if (raw != null) {
    try {
      const v = JSON.parse(raw);
      if (v && typeof v === "object") return v;
    } catch (e) { /* 落到旧 .tool-args 解析 */ }
  }
  const pre = card.querySelector(".tool-args");
  if (!pre) return null;
  const full = pre.querySelector(".expand-full");
  const text = full ? full.textContent : pre.textContent;
  try { return JSON.parse(text); } catch (e) { return null; }
}

/* 同上：expando 丢失后从 .tool-name 取工具名 */
function cardToolName(card) {
  if (card._toolName) return card._toolName;
  const nm = card.querySelector(".tool-name");
  return nm ? nm.textContent : "";
}

/* 单行 diff 行：[符号][行号][内容]。sign 空串 → 无符号列（read_file 视图）；
   lineNo null → 无行号列（页脚行 / TUI 解析失败路径）。sign 决定行的
   diff-add / diff-del 底色类（+ 绿 / - 红） */
function diffRow(sign, lineNo, text, extraCls) {
  const kind = sign === "+" ? " diff-add" : (sign === "−" || sign === "-" ? " diff-del" : "");
  const row = el("div", "diff-row" + kind + (extraCls ? " " + extraCls : ""));
  if (sign) row.appendChild(el("span", "diff-sign", sign + " "));
  if (lineNo != null) row.appendChild(el("span", "diff-ln", String(lineNo)));
  const tx = el("span", "diff-text");
  tx.textContent = text;   // 原样保留（行内空白/尾随空格），CSS pre-wrap 呈现
  row.appendChild(tx);
  return row;
}

/* 单侧 diff 行序列（镜像 TUI push_diff_side）：最多 30 行，超出的行数用
   "… (N more lines)" 标记（带同侧符号）。startLine null → 行不编号 */
function diffSideRows(text, sign, startLine) {
  const rows = [];
  const s = String(text == null ? "" : text);
  if (s === "") return rows;   // 空 old/new/content：不生成空红/绿 diff 行（纯新增/纯删除）
  const lines = s.split("\n");
  // Rust str::lines() 丢弃结尾单个空段：文本以 \n 结尾时不产生空 diff 行
  if (lines.length > 1 && lines[lines.length - 1] === "") lines.pop();
  const shown = Math.min(lines.length, DIFF_LINE_LIMIT);
  for (let i = 0; i < shown; i++) {
    rows.push(diffRow(sign, startLine != null ? startLine + i : null, lines[i]));
  }
  const remaining = lines.length - shown;
  if (remaining > 0) rows.push(el("div", "diff-more", sign + "… (" + remaining + " more lines)"));
  return rows;
}

/* edit_file 结果："file edited (line N)" → 行号 N 起，- 红 / + 绿 两段 diff
   （旧内容在上、新内容在下，镜像 TUI push_tool_result 的顺序） */
function renderEditDiff(resEl, args, content) {
  const line = parseEditedLine(content);
  resEl.classList.add("tool-diff");
  resEl.appendChild(el("div", "diff-head",
    "file edited" + (line != null ? " (line " + line + ")" : "")));
  for (const r of diffSideRows(args.old, "−", line)) resEl.appendChild(r);
  for (const r of diffSideRows(args.new, "+", line)) resEl.appendChild(r);
}

/* write_file 结果："file written" 确认行 + 全新增（+ 绿）单侧 diff。
   新文件内容即全部行，行号从 1 起；覆盖写时旧内容前端不可得，纯新增
   视图即是诚实呈现（后端旧内容 marker 属后续项，不做） */
function renderWriteDiff(resEl, args, content) {
  resEl.classList.add("tool-diff");
  resEl.appendChild(el("div", "diff-head", content || "file written"));
  for (const r of diffSideRows(args.content, "+", 1)) resEl.appendChild(r);
}

/* read_file 结果 → 行号 + 内容逐行视图：行号 = args.offset（默认 1）+ 行下标；
   页脚 "[showing lines X-Y of Z; ...]" 行原样显示但不编号；纯状态输出
   （[empty file] / [offset ... past end ...]）不做行号，走普通截断。
   超过 DIFF_LINE_LIMIT 行时预览 + "… (N more lines)" + 展开全文（复用
   expand-toggle 事件委托；全文常驻 .diff-full，快照往返后仍可展开） */
const READ_STATUS_RE = /^\[(empty file|offset \d+ is past end of file \(\d+ lines\))\]$/;
const READ_FOOTER_RE = /^\[showing lines \d+-\d+ of \d+[^\]]*\]$/;
function renderFileView(resEl, args, content) {
  const s = String(content == null ? "" : content);
  if (s === "") {   // 空内容与后端 "[empty file]" 语义一致：状态输出，不生成空行号行
    maybeTruncateEl(resEl, "[empty file]", LONG_TEXT_THRESHOLD, null);
    return;
  }
  if (READ_STATUS_RE.test(s.trim())) {          // 状态输出不是文件行：不编号
    maybeTruncateEl(resEl, s, LONG_TEXT_THRESHOLD, null);
    return;
  }
  const offset = Math.max(1, parseInt(args && args.offset, 10) || 1);
  const lines = s.split("\n");
  if (lines.length > 1 && lines[lines.length - 1] === "") lines.pop();  // 镜像 Rust lines()
  const rows = [];
  for (let i = 0; i < lines.length; i++) {
    const isFooter = READ_FOOTER_RE.test(lines[i].trim());
    rows.push(diffRow("", isFooter ? null : offset + i, lines[i], isFooter ? "diff-footer" : null));
  }
  resEl.classList.add("tool-diff");
  if (rows.length <= DIFF_LINE_LIMIT) {
    for (const r of rows) resEl.appendChild(r);
    return;
  }
  resEl.classList.add("expandable");
  for (const r of rows.slice(0, DIFF_LINE_LIMIT)) resEl.appendChild(r);
  resEl.appendChild(el("div", "diff-more", "… (" + (rows.length - DIFF_LINE_LIMIT) + " more lines)"));
  // 展开按钮紧跟预览区（截断标记之后、全文之前）：不依赖滚动到内容底部即可发现
  const btn = el("button", "expand-toggle", "展开全文（内容）");
  btn.type = "button";
  btn._target = resEl;      // 事件委托精确定位（与 maybeTruncateEl 同机制）
  resEl.appendChild(btn);
  const full = el("div", "diff-full");
  for (const r of rows.slice(DIFF_LINE_LIMIT)) full.appendChild(r);
  resEl.appendChild(full);
}

/* 文件工具结果渲染入口：edit_file → diff、read_file → 行号视图、write_file →
   单侧新增；参数/结果格式不符时退回普通文本截断。isError 由调用方排除。
   渲染前清空结果容器：pending 期间的 "等待结果…" 占位文本（maybeTruncateEl
   写入的 textContent）与可能残留的展开态不能混进结构化 diff 内容。 */
function renderFileToolResult(resEl, name, args, content) {
  resEl.textContent = "";
  resEl.classList.remove("expandable", "expanded");
  if (name === "edit_file" && args && typeof args.old === "string" && typeof args.new === "string") {
    renderEditDiff(resEl, args, content);
    return;
  }
  if (name === "read_file" && args) {
    renderFileView(resEl, args, content);
    return;
  }
  if (name === "write_file" && args && typeof args.content === "string") {
    renderWriteDiff(resEl, args, content);
    return;
  }
  maybeTruncateEl(resEl, content || "(无输出)", LONG_TEXT_THRESHOLD, null);
}
/* =====================================================================
 * 统一工具结果渲染入口（renderToolResult）+ 统一工具参数渲染
 * （renderToolArgs）：live（appendToolResult）与 history（renderMessage
 * 的 m.Tool 分支）共用同一套按工具分派的渲染，保证两种路径行为一致。
 * 设计约束：不做 renderer 框架，只有普通函数；所有用户内容走 textContent
 * （防 XSS），格式不匹配时安全回退原文（不丢信息）；不改后端工具协议。
 * ===================================================================*/
/* 普通文本结果（含错误）：短直出，长可展开。err 底色由调用方加 .err */
function renderPlainResult(resEl, content, emptyText) {
  resEl.textContent = "";
  resEl.classList.remove("expandable", "expanded");
  maybeTruncateEl(resEl, content || emptyText, LONG_TEXT_THRESHOLD, null);
}

/* web_search 结果：直接 markdown 化（renderMarkdown 自带转义 + 安全链接白名单，
   不做 Exa 结果结构解析）。容器在 buildToolCard 已是 div（.tool-markdown）。 */
function renderSearchResult(resEl, content) {
  resEl.classList.add("tool-markdown");
  resEl.innerHTML = renderMarkdown(String(content == null ? "" : content));
}

/* delegate 结果：
   - 后台启动回执 "started background task N: label\nsubagent session: sub-…" →
     「后台运行 任务 #N label」+ 「子代理会话 sub-…」
   - 同步完成 "subagent session: sub-…\n<答案>" → 「子代理会话 sub-…」+ 答案 markdown
   - 不匹配回退原文（错误正文由 isError 分支走 renderPlainResult，保持完整可复制） */
function renderDelegateResult(resEl, content) {
  const s = String(content == null ? "" : content);
  const bg = /^started background task (\d+): ([^\n]*)\nsubagent session: ([^\n]+)$/.exec(s);
  if (bg) {
    resEl.classList.add("delegate-result");
    const wrap = el("div", "delegate-result-wrap");
    const line = el("div", "delegate-bg-line");
    line.appendChild(el("span", "delegate-chip", "后台运行"));
    line.appendChild(el("span", "delegate-label",
      "任务 #" + bg[1] + (bg[2] ? " " + bg[2] : "")));
    wrap.appendChild(line);
    wrap.appendChild(el("div", "delegate-session", "子代理会话 " + bg[3]));
    resEl.appendChild(wrap);
    return;
  }
  const done = /^subagent session: ([^\n]+)\n?([\s\S]*)$/.exec(s);
  if (done) {
    resEl.classList.add("delegate-result");
    const wrap = el("div", "delegate-result-wrap");
    wrap.appendChild(el("div", "delegate-session", "子代理会话 " + done[1]));
    if (done[2]) {
      const md = el("div", "tool-markdown");
      md.innerHTML = renderMarkdown(done[2]);
      wrap.appendChild(md);
    }
    resEl.appendChild(wrap);
    return;
  }
  maybeTruncateEl(resEl, s || "(无输出)", LONG_TEXT_THRESHOLD, null);
}

/* get_background_tasks 结果：
   - "No background tasks running." → 空状态「当前没有后台任务」
   - 否则首行标题 + 每行 "#N: desc" 拆成 id/描述行
   - 不匹配回退原文 */
const BG_EMPTY_RE = /^No background tasks running\.$/;
const BG_LINE_RE = /^#(\d+):\s*(.*)$/;
function renderBackgroundTaskList(resEl, content) {
  const s = String(content == null ? "" : content);
  if (s.trim() === "") {   // 空内容：格式不符，回退原文占位
    maybeTruncateEl(resEl, s || "(无输出)", LONG_TEXT_THRESHOLD, null);
    return;
  }
  if (BG_EMPTY_RE.test(s.trim())) {
    resEl.appendChild(el("div", "task-empty", "当前没有后台任务"));
    return;
  }
  const lines = s.split("\n");
  let ok = lines.length >= 1;
  for (let i = 1; i < lines.length; i++) {
    if (!BG_LINE_RE.test(lines[i])) { ok = false; break; }
  }
  if (!ok) { maybeTruncateEl(resEl, s || "(无输出)", LONG_TEXT_THRESHOLD, null); return; }
  resEl.classList.add("task-snapshot");
  const wrap = el("div", "task-snapshot-wrap");
  wrap.appendChild(el("div", "task-snapshot-head", lines[0]));
  for (let i = 1; i < lines.length; i++) {
    const m = BG_LINE_RE.exec(lines[i]);
    if (!m) continue;   // 空行等：跳过不渲染（结构校验已通过）
    const row = el("div", "task-snapshot-row");
    row.appendChild(el("span", "task-snapshot-id", "#" + m[1]));
    row.appendChild(el("span", "task-snapshot-desc", m[2]));
    wrap.appendChild(row);
  }
  resEl.appendChild(wrap);
}

/* read_image 结果：后端已剥离 __EA_IMAGE__ marker，这里只有文本摘要；
   保持现有行为（纯文本，长摘要可展开） */
function renderImageReceipt(resEl, content) {
  renderPlainResult(resEl, String(content == null ? "" : content), "(无输出)");
}

/* 统一结果入口：isError → 纯文本（保持完整可复制）；否则按工具名分派。
   args 只对文件工具传入（diff 渲染需要）；其余工具结果渲染只依赖 content。 */
function renderToolResult(resEl, name, args, content, isError) {
  resEl.textContent = "";
  resEl.classList.remove("expandable", "expanded");
  if (isError) { renderPlainResult(resEl, content, "(无错误信息)"); return; }
  switch (name) {
    case "read_file":
    case "write_file":
    case "edit_file":
      renderFileToolResult(resEl, name, args, content);
      return;
    case "web_search":
      renderSearchResult(resEl, content);
      return;
    case "delegate":
      renderDelegateResult(resEl, content);
      return;
    case "get_background_tasks":
      renderBackgroundTaskList(resEl, content);
      return;
    case "read_image":
      renderImageReceipt(resEl, content);
      return;
    default:
      renderPlainResult(resEl, content, "(无输出)");
  }
}
/* =====================================================================
 * 统一工具参数渲染（renderToolArgs）：返回三种值——
 *   El（结构化紧凑渲染，buildToolCard append 进 .tool-args）
 *   string（原文回退 / MCP 未知工具 pretty JSON，buildToolCard 走
 *           maybeTruncateEl 保持长参数可展开）
 *   null（无参数工具：隐藏参数区，如 get_background_tasks）
 * 所有用户内容一律 textContent，不拼未转义 HTML。
 * ===================================================================*/
/* 文件工具 read_file：📄 path + 可选 chips（offset/limit） */
function renderReadFileArgs(parsed, rawText) {
  if (!parsed || typeof parsed !== "object" || typeof parsed.path !== "string") return rawText;
  const row = el("div", "tool-args-compact");
  row.appendChild(el("span", "tool-primary", "📄 " + parsed.path));
  if (parsed.offset != null) row.appendChild(el("span", "tool-chip", "offset " + parsed.offset));
  if (parsed.limit != null) row.appendChild(el("span", "tool-chip", "limit " + parsed.limit));
  return row;
}

/* write_file：✎ path + N 行 + N 字符（完整 content 不显示在参数区，结果 diff 已展示） */
function renderWriteFileArgs(parsed, rawText) {
  if (!parsed || typeof parsed !== "object" || typeof parsed.path !== "string"
      || typeof parsed.content !== "string") return rawText;
  const row = el("div", "tool-args-compact");
  row.appendChild(el("span", "tool-primary", "✎ " + parsed.path));
  row.appendChild(el("span", "tool-chip", parsed.content.split("\n").length + " 行"));
  row.appendChild(el("span", "tool-chip", parsed.content.length + " 字符"));
  return row;
}

/* edit_file：✎ path + 旧 N 行 → 新 M 行（old/new 正文不显示在参数区） */
function renderEditFileArgs(parsed, rawText) {
  if (!parsed || typeof parsed !== "object" || typeof parsed.path !== "string"
      || typeof parsed.old !== "string" || typeof parsed.new !== "string") return rawText;
  const row = el("div", "tool-args-compact");
  row.appendChild(el("span", "tool-primary", "✎ " + parsed.path));
  row.appendChild(el("span", "tool-chip", "旧 " + parsed.old.split("\n").length + " 行"));
  row.appendChild(el("span", "tool-arrow", "→"));
  row.appendChild(el("span", "tool-chip", "新 " + parsed.new.split("\n").length + " 行"));
  return row;
}

/* web_search：🔎 query */
function renderSearchArgs(parsed, rawText) {
  if (!parsed || typeof parsed !== "object" || typeof parsed.query !== "string") return rawText;
  const row = el("div", "tool-args-compact");
  row.appendChild(el("span", "tool-query", "🔎 " + parsed.query));
  return row;
}

/* delegate：role chip + background chip + label（有才显示）+ workspace 行 +
   resume 行（有才显示，随 workspace 位于卡片头部）+ task 正文（pre，长任务
   可展开）。background 未提供时按 true（后端默认，delegate.rs）。全 textContent。 */
function renderDelegateArgs(parsed, rawText) {
  if (!parsed || typeof parsed !== "object" || typeof parsed.task !== "string") return rawText;
  const wrap = el("div", "delegate-args");
  const meta = el("div", "delegate-meta");
  if (parsed.role != null && parsed.role !== "") {
    meta.appendChild(el("span", "delegate-chip", "role: " + parsed.role));
  }
  const bg = parsed.background != null ? !!parsed.background : true;
  meta.appendChild(el("span", "delegate-chip" + (bg ? " delegate-bg" : ""),
    bg ? "后台运行" : "前台同步"));
  if (parsed.label != null && parsed.label !== "") {
    meta.appendChild(el("span", "delegate-label", String(parsed.label)));
  }
  wrap.appendChild(meta);
  const loc = el("div", "delegate-loc");
  loc.appendChild(el("div", "delegate-ws",
    "工作区: " + (parsed.workspace != null ? parsed.workspace : "")));
  if (parsed.resume != null && parsed.resume !== "") {
    loc.appendChild(el("div", "delegate-resume", "续接会话: " + parsed.resume));
  }
  wrap.appendChild(loc);
  wrap.appendChild(maybeTruncateEl(el("pre", "task-snapshot-body"), parsed.task,
    LONG_TEXT_THRESHOLD, null));
  return wrap;
}

/* get_background_tasks：无参数 → 隐藏参数区（返回 null）；异常形态回退原文 */
function renderBackgroundTaskArgs(parsed, rawText) {
  const empty = String(rawText).trim() === ""
    || (parsed && typeof parsed === "object" && Object.keys(parsed).length === 0);
  return empty ? null : rawText;
}

/* cancel_background_task：取消后台任务 #N */
function renderCancelBackgroundTaskArgs(parsed, rawText) {
  if (!parsed || typeof parsed !== "object" || parsed.id == null) return rawText;
  const row = el("div", "tool-args-compact");
  row.appendChild(el("span", "tool-primary", "取消后台任务 #" + parsed.id));
  return row;
}

/* read_image：🖼 path */
function renderReadImageArgs(parsed, rawText) {
  if (!parsed || typeof parsed !== "object" || typeof parsed.path !== "string") return rawText;
  const row = el("div", "tool-args-compact");
  row.appendChild(el("span", "tool-primary", "🖼 " + parsed.path));
  return row;
}

/* bash / pwsh（Windows 工具名）：$ command + background:true 时追加
   [background] 徽标；缺 command 字段回退原文 */
function renderBashArgs(parsed, rawText) {
  if (!parsed || typeof parsed !== "object" || typeof parsed.command !== "string") return rawText;
  const row = el("div", "tool-args-compact");
  row.appendChild(el("span", "tool-primary", "$ " + parsed.command));
  if (parsed.background === true) {
    row.appendChild(el("span", "tool-chip tool-bg", "[background]"));
  }
  return row;
}

/* 统一参数入口：按工具名分派；MCP/未知工具保持 pretty JSON（向后兼容，
   旧快照的 .tool-args 也依赖此形态回退解析） */
function renderToolArgs(name, parsed, rawText) {
  switch (name) {
    case "read_file": return renderReadFileArgs(parsed, rawText);
    case "write_file": return renderWriteFileArgs(parsed, rawText);
    case "edit_file": return renderEditFileArgs(parsed, rawText);
    case "web_search": return renderSearchArgs(parsed, rawText);
    case "delegate": return renderDelegateArgs(parsed, rawText);
    case "get_background_tasks": return renderBackgroundTaskArgs(parsed, rawText);
    case "cancel_background_task": return renderCancelBackgroundTaskArgs(parsed, rawText);
    case "read_image": return renderReadImageArgs(parsed, rawText);
    case "bash":
    case "pwsh":
      return renderBashArgs(parsed, rawText);
    default:
      try { return JSON.stringify(JSON.parse(rawText), null, 2); }
      catch (e) { return rawText; }
  }
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
 * 消息渲染
 * ===================================================================*/
function newAccumulator() {
  return {
    assistantEl: null,   // 当前助手消息容器（流式累积）
    assistantBody: null,
    assistantText: "",   // 已累积的文本（用于替换/查重）
    thinkingEl: null,    // 当前 thinking 折叠块
    thinkBody: null,
    thinkDot: null,      // 当前 thinking 的圆点元素（缓存：delta 追加不再 querySelector）
    toolStack: [],       // 未配对结果的工具卡片（live 事件按顺序配对）
    pendingByCall: new Map(), // call_id -> 卡片元素（history 渲染按 id 配对）
  };
}

function freezeAssistant(acc) {
  // 工具调用 / 新用户回合开始时，结束当前助手消息与思考区的累积，
  // 使下一回合的 delta 从新的消息块开始
  // 流式期间是纯文本（快）；冻结时用完整文本重算一次 markdown，
  // 让表格/代码块/公式在回合结束时正确渲染。
  if (!acc) return;   // 累积器未就绪（切工作区等）时直接返回，尾段赋值不能解引用 null
  if (acc && acc.assistantBody && acc.assistantText) {
    acc.assistantBody.innerHTML = renderMarkdown(acc.assistantText);
  }
  // 思考结束：转圈 → 勾号（表示该轮思考完成）
  if (acc && acc.thinkingEl) {
    const dot = acc.thinkDot || acc.thinkingEl.querySelector(".think-dot");
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

/* SSE delta 滚动合并（perf 修复）：delta 追加只标记「需要滚到底」，rAF 回调
   里每帧最多执行一次 scrollBottom——帧内多个 delta（思考+正文+工具事件）
   合并为一次 scrollHeight/scrollTop 布局读写，消除每 delta 的 forced reflow。
   force 路径（工具卡片/结果等一次性事件）仍即时执行，保持现有语义；
   scrollBottom 内部的 userScrolled/suppressScroll 锁定逻辑完全不变。
   无 rAF 环境（gjs harness / 极老浏览器）退化为 setTimeout(0) 模拟；
   两者都不可用则立即执行（保底不丢滚动）。 */
let scrollRafPending = false;
function scheduleScrollBottom() {
  if (scrollRafPending) return;      // 帧内已挂起：合并，不重复调度
  scrollRafPending = true;
  const run = () => {
    scrollRafPending = false;
    scrollBottom(false);
  };
  try {
    if (typeof requestAnimationFrame === "function") { requestAnimationFrame(run); return; }
  } catch (e) { /* 落到 setTimeout 兜底 */ }
  try {
    if (typeof setTimeout === "function") { setTimeout(run, 0); return; }
  } catch (e) { /* 立即执行保底 */ }
  run();
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
  pruneMessages();
}

function appendSystemMsg(text) {
  freezeAssistant(state.acc);
  const msg = el("div", "msg msg-system");
  const who = el("span", "who", "system>");
  const body = el("div", "msg-body");
  body.textContent = text;
  msg.append(who, body);
  els.messages.appendChild(msg);
  pruneMessages();
}

/* 助手消息：有则累积，无则新建 */
function assistantBubble(acc, reason) {
  if (!acc.assistantEl) {
    const msg = el("div", "msg msg-assistant");
    const who = el("span", "who", "ai>");
    const body = el("div", "msg-body");
    msg.append(who, body);
    els.messages.appendChild(msg);
    acc.assistantEl = msg;      // 先绑定再 prune：进行中的助手块绝不折叠
    acc.assistantBody = body;
    if (reason) scrollBottom(false);
    pruneMessages();
  }
  return acc.assistantBody;
}

function appendAssistantDelta(text, acc) {
  const body = assistantBubble(acc, true);
  acc.assistantText += text;
  // 流式渲染：直接追加（markdown 不重算，保持简单、快）
  body.insertAdjacentText("beforeend", text);
  scheduleScrollBottom();   // rAF 批处理：帧内多个 delta 合并为一次滚动
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
    acc.thinkingEl = det;                   // 先绑定再 prune：进行中的思考块不折叠
    acc.thinkBody = body;
    acc.thinkDot = dot;                     // 缓存圆点：后续 delta 不再 querySelector
    pruneMessages();
  }
  return acc.thinkBody;
}

function appendReasoningDelta(text, acc) {
  const body = thinkingBlock(acc);
  body.insertAdjacentText("beforeend", text);
  // 思考进行中：折叠栏的圆点转圈（dot 引用在 thinkingBlock 创建时缓存）
  if (acc.thinkingEl) {
    const dot = acc.thinkDot || acc.thinkingEl.querySelector(".think-dot");
    if (dot) dot.classList.add("active");
  }
  scheduleScrollBottom();   // rAF 批处理：帧内多个 delta 合并为一次滚动
}

/* 工具调用卡片（live 事件，无 call_id，按顺序配对结果） */
function appendToolCall(name, args, acc, callId) {
  freezeAssistant(acc);
  const card = buildToolCard(name, args, "执行中…", "pending", null);
  acc.toolStack.push({ el: card, filled: false });
  if (callId) acc.pendingByCall.set(callId, card);
  els.messages.appendChild(card);
  scrollBottom(true);   // 工具调用卡片出现时强制跟随底部（工具结果常几秒后一次性到达）
  pruneMessages();      // 卡片是「执行中…」：进行中，不折叠
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
    resEl.classList.remove("pending");
    resEl.classList.toggle("err", isError);
    // 统一结果渲染（与 history 的 renderMessage 同一条 renderToolResult 路径）。
    // 文件工具（edit/read/write）成功 → 差异化渲染（diff / 行号视图）；
    // 结果要可见，卡片保持展开。其余工具维持原行为：文本 + 收起。
    const name = cardToolName(card);
    const tArgs = (name === "edit_file" || name === "read_file" || name === "write_file")
      ? cardArgs(card) : null;
    const isFileTool = tArgs !== null;
    renderToolResult(resEl, name, tArgs, content, isError);
    if (isFileTool && !isError) {
      card.setAttribute("open", "");
    } else {
      card.removeAttribute("open");   // 结果到达：收起为标题行（默认折叠）
    }
  } else {
    // 没有可配对的卡片：独立展示结果行
    const card2 = buildToolCard("工具结果", "", isError ? "失败" : "完成",
      isError ? "err" : "", content || "");
    els.messages.appendChild(card2);
    pruneMessages();
  }
  scrollBottom(true);   // 工具结果填充后强制滚到底（buildToolCard 内设置的
                        // stateText/resultText 也随此调用被包含）
}

function buildToolCard(name, args, stateText, stateCls, resultText) {
  // 工具卡片：details 可折叠。pending（执行中/等待结果）时默认展开，
  // 结果到达后由 appendToolResult 收起（details.removeAttribute("open")）。
  // 文件工具（edit/read/write）例外：结果到达后保持展开（diff/行号要可见）。
  const card = el("details", "tool-card");
  if (stateCls === "pending") card.setAttribute("open", "");
  const head = el("summary", "tool-head");
  const nm = el("span", "tool-name", name || "tool");
  const st = el("span", "tool-state", stateText);
  head.append(nm, st);

  const argsText = args != null ? String(args) : "";
  let parsed = null;
  try { parsed = JSON.parse(argsText); } catch (e) { /* 原文回退 */ }
  // 结构化结果（diff 行 / markdown / 任务列表）用 div；其余保持 pre 语义。
  // read_image 结果仍是文本摘要，维持 pre。
  const structuredResult = name === "edit_file" || name === "read_file"
    || name === "write_file" || name === "web_search" || name === "delegate"
    || name === "get_background_tasks";
  const argsEl = el("div", "tool-args");
  const resEl = el(structuredResult ? "div" : "pre", "tool-result " + stateCls);
  card.append(head, argsEl, resEl);
  // 统一参数渲染：null → 隐藏参数区（无参数工具）；string → 原文/pretty JSON
  // （maybeTruncateEl 保持长参数可展开）；El → 结构化紧凑渲染直接放入
  const argsNode = renderToolArgs(name, parsed, argsText);
  if (argsNode === null) {
    argsEl.hidden = true;
  } else if (typeof argsNode === "string") {
    maybeTruncateEl(argsEl, argsNode, LONG_TEXT_THRESHOLD, null, "参数");
  } else {
    argsEl.appendChild(argsNode);
  }
  maybeTruncateEl(resEl,
    resultText != null ? resultText : (stateCls === "pending" ? "等待结果…" : ""),
    LONG_TEXT_THRESHOLD, null, "结果");
  // 原始参数存 data-tool-args（属性随 innerHTML 快照序列化）：快照往返
  // （缓存恢复 / resync 离屏替换）后 expando（_toolArgs）丢失，cardArgs
  // 优先从该属性重解析——文件工具的 .tool-args 已是紧凑渲染（不再含 JSON），
  // 没有它文件 diff 在快照恢复后会失效。
  card.setAttribute("data-tool-args", argsText);
  // 文件工具：解析参数供结果到达后的差异化渲染（diff / 行号视图）。
  // expando 在 innerHTML 快照往返后丢失，cardArgs/cardToolName 会从 DOM 重解析。
  card._toolName = name;
  card._toolArgs = parseToolArgs(name, argsText);
  return card;
}

/* buildToolCard 用的参数预解析：edit_file 严格校验 {path, old, new}；
   read_file / write_file 只要对象即可（字段在渲染时按需取用） */
function parseToolArgs(name, argsText) {
  if (name === "edit_file") return parseEditArguments(argsText);
  if (name === "read_file" || name === "write_file") {
    try {
      const v = JSON.parse(String(argsText));
      return v && typeof v === "object" ? v : null;
    } catch (e) { return null; }
  }
  return null;
}

/* 切回缓存会话（openSession restored 分支）时，把缓存 DOM 里「进行中」的
   元素重新绑定到新的累积器上：后续 ReasoningDelta / AssistantDelta /
   ToolResult 续写进旧块，而不是另起一块（否则同一个思考/助手/工具卡片会
   重复出现多个）。历史/已完成（dot.done / 已 markdown 化）的元素绝不绑定。 */
function reattachInFlight(acc) {
  // 1. thinking：最后一个 details.thinking，且其 .think-dot 没有 .done（进行中）才绑定
  const thinks = [...els.messages.querySelectorAll("details.thinking")];
  const t = thinks[thinks.length - 1];
  if (t && !t.querySelector(".think-dot.done")) {
    acc.thinkingEl = t;
    acc.thinkBody = t.querySelector(".think-body");
    acc.thinkDot = t.querySelector(".think-dot");   // 缓存圆点：增量续写不再查找
  }
  // 2. assistant：最后一个 .msg-assistant，且其 .msg-body 是纯文本（流式期间
  //    未 markdown 化，无元素子节点）→ 进行中，绑定并把 textContent 取回
  const as = [...els.messages.querySelectorAll(".msg-assistant")];
  const a = as[as.length - 1];
  if (a) {
    const body = a.querySelector(".msg-body");
    if (body && !body.querySelector("*")) {   // 无子元素 = 纯文本 = 流式中
      acc.assistantEl = a;
      acc.assistantBody = body;
      acc.assistantText = body.textContent;
    }
  }
  // 3. 工具卡片：所有 .tool-state 文本为 "执行中…" 的 details.tool-card →
  //    push 进 acc.toolStack（filled:false），供 appendToolResult 的 fallback 配对
  for (const c of [...els.messages.querySelectorAll("details.tool-card")]) {
    const st = c.querySelector(".tool-state");
    if (st && st.textContent === "执行中…") acc.toolStack.push({ el: c, filled: false });
  }
}

/* 提示行（Notice / 排队等） */
function appendNotice(text) {
  // 与 TUI 对 Notice 的「结束流式 lane」语义一致：通知穿插在流式回复中时
  // 结束当前助手/思考累积，使后续 delta 从新气泡开始（否则回合 N+1 的
  // 正文会续写进通知条上方的旧气泡，顺序反转）。
  freezeAssistant(state.acc);
  const n = el("div", "notice", text);
  els.messages.appendChild(n);
  scrollBottom(false);
  pruneMessages();
}

/* notice 变体：前缀 + 可能很长的正文（后台任务输出 / 未知条目 JSON）。
   正文走 maybeTruncateEl：短直出，长则预览 + 展开全文。 */
function appendNoticeLong(prefix, text) {
  freezeAssistant(state.acc);
  const n = el("div", "notice");
  if (prefix) n.append(prefix);
  const pre = maybeTruncateEl(el("pre", "notice-output"), text, LONG_TEXT_THRESHOLD, null);
  n.append(pre);
  els.messages.appendChild(n);
  scrollBottom(false);
  pruneMessages();
  return n;
}

/* 后台任务最终完成（live BackgroundCompleted 事件与 history background_completion
   条目共用）：output 以 "subagent session: " 开头 → delegate 完成（子代理会话 +
   markdown 答案）；否则走现有 bash 路径（appendNoticeLong）。 */
function appendBackgroundCompletion(id, label, output) {
  const s = String(output == null ? "" : output);
  const m = /^subagent session: ([^\n]+)\n?([\s\S]*)$/.exec(s);
  if (!m) {
    return appendNoticeLong("⌛ 后台任务 #" + (id ?? "?") + " 完成"
      + (label ? "（" + label + "）" : "") + "\n", s);
  }
  freezeAssistant(state.acc);
  const n = el("div", "notice delegate-complete");
  const line = el("div", "delegate-bg-line");
  line.appendChild(el("span", "delegate-chip", "后台运行"));
  line.appendChild(el("span", "delegate-label",
    "任务 #" + (id ?? "?") + (label ? " " + label : "")));
  n.appendChild(line);
  n.appendChild(el("div", "delegate-session", "子代理会话 " + m[1]));
  if (m[2]) {
    const md = el("div", "tool-markdown");
    md.innerHTML = renderMarkdown(m[2]);
    // delegate 答案：长答案（>200 字符）默认折叠——包进 details.delegate-collapse
    // （不 open），summary 显示字符数，点击展开；短答案直接展开显示，避免
    // 「已完成」这类短回复也要点开
    if (m[2].length <= DELEGATE_COLLAPSE_THRESHOLD) {
      n.appendChild(md);
    } else {
      const det = el("details", "delegate-collapse");
      det.open = false;                       // 默认折叠
      const sum = el("summary", "", "查看完整答案（" + m[2].length + " 字符）");
      det.append(sum, md);
      n.appendChild(det);
    }
  }
  els.messages.appendChild(n);
  scrollBottom(false);
  pruneMessages();
  return n;
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
  bar.innerHTML = "";
  const SHOWN = 3;
  const items = state.queue;
  const extra = items.length - SHOWN;
  const expanded = state.queueExpanded;
  const visible = expanded ? items : items.slice(0, SHOWN);
  for (const t of visible) {
    bar.appendChild(el("div", "queue-item", "⏳ 排队中: " + t));
  }
  if (extra > 0) {
    // 超过 3 条时给出展开/折叠按钮：展开看全部，再点收起
    const toggle = el("button", "queue-toggle",
      expanded ? "收起排队列表" : "+ " + extra + " 条排队中");
    toggle.type = "button";
    toggle.addEventListener("click", () => {
      state.queueExpanded = !state.queueExpanded;
      renderQueueBar();
    });
    bar.appendChild(toggle);
  }
  bar.hidden = false;
}

function queuePromptQueued(text) {
  state.queue.push(text);
  renderQueueBar();
}

function queuePromptConsumed() {
  if (state.queue.length) state.queue.shift();   // 移除最旧一条（正在被处理的那条）
  // 消费完（队列空）→ 清理该会话的快照，避免残留空快照/过期项
  if (!state.queue.length && state.sessionId) {
    delete state.queues[state.workspace.id + ":" + state.sessionId];
  }
  renderQueueBar();
}

/* 错误行 */
function appendError(text) {
  const e = el("div", "msg-error", "错误: " + text);
  els.messages.appendChild(e);
  scrollBottom(false);
  pruneMessages();
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
  pruneMessages();
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
      return appendBackgroundCompletion(entry.id ?? "?", entry.label, entry.output || "");
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
      return appendNoticeLong("未知条目: ", JSON.stringify(entry));
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
      resEl.classList.remove("pending");
      resEl.classList.toggle("err", t.is_error);
      const name = cardToolName(card);
      const tArgs = (name === "edit_file" || name === "read_file" || name === "write_file")
        ? cardArgs(card) : null;
      // 与 live 路径同一 renderToolResult：特殊结果（web_search / delegate /
      // get_background_tasks / 文件 diff）在 history 重放时渲染一致
      renderToolResult(resEl, name, tArgs, t.content, t.is_error);
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
  appendNoticeLong("消息: ", JSON.stringify(m));
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
  // 初始/整体渲染（非前置插入）：超限时折叠最早的已完成块。前置插入
  // （loadOlder）不 prune——刚加载的更早历史不立即折叠，等下一个底部新块。
  if (!prepend) pruneMessages();
}

/* =====================================================================
 * 消息列表上限（MAX_MESSAGE_BLOCKS）：新增块后 pruneMessages 维护上限。
 * 超过上限时把最早的一批「已完成」块（用户/助手/系统消息、思考块、
 * 工具卡片、notice/错误/压缩线/分叉行）移进顶部占位
 * details.older-collapse：折叠 = 移入占位容器，展开 = 原位显示。
 * 移动不销毁元素——expand 按钮（事件委托）、innerHTML 快照（会话缓存/
 * 恢复）都不受影响，快照反而更小（折叠进 details 后内容仍是完整的）。
 * 进行中（流式）的块绝不折叠：流式助手（state.acc.assistantEl）、流式
 * 思考（acc.thinkingEl）、执行中的工具卡片（.tool-state == "执行中…"）。
 * 只约束渲染块数：不删数据、不动后端（历史仍可滚动分页加载）。
 * 取舍：占位折叠的是块而不是删除，被折叠块的全部信息仍在 DOM 里；
 * 代价是长会话 DOM 总量不下降（上限约束的是「直接子块数」）。
 * ===================================================================*/
function isInflightBlock(k) {
  if (state.acc && (state.acc.assistantEl === k || state.acc.thinkingEl === k)) return true;
  if (k.classList.contains("tool-card")) {
    const st = k.querySelector(".tool-state");
    return st && st.textContent === "执行中…";
  }
  return false;
}

function isFoldableBlock(k) {
  return k.classList.contains("msg") || k.classList.contains("notice")
    || k.classList.contains("msg-error") || k.classList.contains("compaction")
    || k.classList.contains("forked") || k.classList.contains("thinking")
    || k.classList.contains("tool-card");
}

function pruneMessages() {
  if (suppressScroll) return;   // 批量渲染期间不折叠（loadOlder 前置插入 / 初始渲染）
  const m = els.messages;
  const kids = [...m.children];
  if (kids.length <= MAX_MESSAGE_BLOCKS) return;
  let holder = null;
  for (const k of kids) {
    if (k.classList && k.classList.contains("older-collapse")) { holder = k; break; }
  }
  let body = holder ? holder.querySelector(".older-body") : null;
  if (holder && !body) holder = null;   // 结构异常时按无占位处理
  let folded = 0;
  for (const k of kids) {
    if (k === holder || !k.classList) continue;
    if (isInflightBlock(k)) break;      // 进行中绝不折叠：从最早开始，遇到即停
    if (!isFoldableBlock(k)) continue;  // 注释等非块节点跳过
    if (!holder) {
      holder = el("details", "older-collapse");
      const sum = el("summary", "older-head");
      sum.appendChild(el("span", "older-label", "⬆ 更早的消息"));
      sum.appendChild(el("span", "older-load", "加载更早历史"));
      const b = el("div", "older-body");
      holder.append(sum, b);
      body = b;
      m.insertBefore(holder, k);        // 占位放在第一批被折叠块的位置（顺序不乱）
    }
    body.appendChild(k);                // 移入（不销毁：轮询/缓存/展开绑定仍在）
    folded++;
    if (kids.length - folded <= MAX_MESSAGE_BLOCKS) break;
  }
  if (holder && folded > 0) {
    const n = body.children.length;
    const lbl = holder.querySelector(".older-label");
    if (lbl) lbl.textContent = "⬆ 更早的 " + n + " 条消息";
    // 「加载更早历史」：后端还有更早历史时可见（点击走 loadOlder）
    const link = holder.querySelector(".older-load");
    if (link) link.hidden = !(state.nextBeforeSeq !== null && !state.olderDone);
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
  const busy = isRunningStatus(state.status);
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
      const dot = state.acc.thinkDot || state.acc.thinkingEl.querySelector(".think-dot");
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
