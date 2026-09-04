/* =============================================================================
 * usage-dashboard.js — strict client/view adapter for GET /api/usage/dashboard.
 * Loaded through /*__USAGE_DASHBOARD_JS__*\/ after the existing app bundle.
 * =============================================================================*/

const usageDashboardState = {
  open: false,
  request: 0,
  charts: [],
  grid: null,
  resizeObserver: null,
};

function usageContractError(path, expected) {
  throw new Error("用量接口格式错误：" + path + " 应为 " + expected);
}

function usageObject(value, path) {
  if (!value || typeof value !== "object" || Array.isArray(value)) usageContractError(path, "对象");
  return value;
}

function usageArray(value, path) {
  if (!Array.isArray(value)) usageContractError(path, "数组");
  return value;
}

function usageStringArray(value, path) {
  return usageArray(value, path).map((item, index) => usageString(item, path + "[" + index + "]", false));
}

function usageString(value, path, allowEmpty) {
  if (typeof value !== "string" || (!allowEmpty && !value)) usageContractError(path, "字符串");
  return value;
}

function usageBool(value, path) {
  if (typeof value !== "boolean") usageContractError(path, "布尔值");
  return value;
}

function usageMetric(value, path, nullable) {
  if (nullable && value === null) return null;
  if (typeof value !== "number" || !Number.isFinite(value) || !Number.isSafeInteger(value) || value < 0) {
    usageContractError(path, nullable ? "非负整数或 null" : "非负整数");
  }
  return value;
}

function usageMetrics(raw, path) {
  raw = usageObject(raw, path);
  return {
    callCount: usageMetric(raw.call_count, path + ".call_count", false),
    input: usageMetric(raw.input_tokens, path + ".input_tokens", false),
    output: usageMetric(raw.output_tokens, path + ".output_tokens", false),
    total: usageMetric(raw.total_tokens, path + ".total_tokens", false),
    cacheHit: usageMetric(raw.cache_hit_tokens, path + ".cache_hit_tokens", true),
    cacheMiss: usageMetric(raw.cache_miss_tokens, path + ".cache_miss_tokens", true),
    reasoning: usageMetric(raw.reasoning_tokens, path + ".reasoning_tokens", true),
    cacheHitReportedCalls: usageMetric(raw.cache_hit_reported_calls, path + ".cache_hit_reported_calls", false),
    cacheMissReportedCalls: usageMetric(raw.cache_miss_reported_calls, path + ".cache_miss_reported_calls", false),
    reasoningReportedCalls: usageMetric(raw.reasoning_reported_calls, path + ".reasoning_reported_calls", false),
  };
}

function usageDimensionRows(value, path, key) {
  return usageArray(value, path).map((raw, index) => {
    const itemPath = path + "[" + index + "]";
    raw = usageObject(raw, itemPath);
    return { key: usageString(raw[key], itemPath + "." + key, false), ...usageMetrics(raw, itemPath) };
  });
}

function normalizeUsagePayload(raw) {
  raw = usageObject(raw, "response");
  const top = usageObject(raw.top_sessions, "top_sessions");
  const data = {
    generatedAt: usageString(raw.generated_at, "generated_at", false),
    window: usageObject(raw.window, "window"),
    scope: usageString(raw.scope, "scope", false),
    totals: usageMetrics(raw.totals, "totals"),
    trend: usageArray(raw.trend, "trend").map((point, index) => {
      const path = "trend[" + index + "]";
      point = usageObject(point, path);
      return { at: usageString(point.bucket_start, path + ".bucket_start", false), ...usageMetrics(point, path) };
    }),
    byModel: usageDimensionRows(raw.by_model, "by_model", "model"),
    byRole: usageDimensionRows(raw.by_role, "by_role", "role"),
    byKind: usageDimensionRows(raw.by_kind, "by_kind", "kind"),
    topLimit: usageMetric(top.limit, "top_sessions.limit", false),
    topHasMore: usageBool(top.has_more, "top_sessions.has_more"),
    rows: usageArray(top.rows, "top_sessions.rows").map((row, index) => {
      const path = "top_sessions.rows[" + index + "]";
      row = usageObject(row, path);
      return {
        id: usageString(row.session_id, path + ".session_id", false),
        title: row.title === null ? "未命名会话" : usageString(row.title, path + ".title", true),
        models: usageStringArray(row.models, path + ".models"),
        role: usageString(row.role, path + ".role", false),
        roleLabel: row.role === "unknown" ? "未知角色" : row.role,
        kinds: usageStringArray(row.kinds, path + ".kinds"),
        ...usageMetrics(row, path),
      };
    }),
  };
  if (Number.isNaN(Date.parse(data.generatedAt))) usageContractError("generated_at", "RFC3339 时间");
  for (const [index, point] of data.trend.entries()) {
    if (Number.isNaN(Date.parse(point.at))) usageContractError("trend[" + index + "].bucket_start", "RFC3339 时间");
  }
  return data;
}

function formatUsageNumber(value) {
  return value === null ? "不可用" : new Intl.NumberFormat("zh-CN", { maximumFractionDigits: 0 }).format(value);
}

function formatUsageCompact(value) {
  return value === null ? "—" : new Intl.NumberFormat("zh-CN", { notation: "compact", maximumFractionDigits: 1 }).format(value);
}

function usageCoverage(reported, calls) {
  return calls ? reported + " / " + calls + " 次调用报告（" + (reported / calls * 100).toFixed(0) + "%）" : "暂无调用";
}

function usageLocalDateTime(date) {
  const pad = (value) => String(value).padStart(2, "0");
  return date.getFullYear() + "-" + pad(date.getMonth() + 1) + "-" + pad(date.getDate())
    + "T" + pad(date.getHours()) + ":" + pad(date.getMinutes());
}

function setDefaultUsageDates() {
  if (els.usageFrom.value || els.usageTo.value) return;
  const to = new Date();
  const from = new Date(to.getTime() - 7 * 24 * 60 * 60 * 1000);
  els.usageFrom.value = usageLocalDateTime(from);
  els.usageTo.value = usageLocalDateTime(to);
}

function usageQuery() {
  const pairs = [
    ["from", els.usageFrom.value ? new Date(els.usageFrom.value).toISOString() : ""],
    ["to", els.usageTo.value ? new Date(els.usageTo.value).toISOString() : ""],
    ["root_session_id", els.usageRootSession.value],
    ["model", els.usageModel.value],
    ["role", els.usageRole.value],
    ["kind", els.usageKind.value],
    ["bucket", els.usageBucket.value],
    ["top_n", "100"],
  ];
  return pairs.filter((pair) => pair[1]).map((pair) => encodeURIComponent(pair[0]) + "=" + encodeURIComponent(pair[1])).join("&");
}

/* Public usage endpoint: retain the active workspace base URL, but deliberately
   send no credential header and no query token. */
function usageDashboardGet(path) {
  const ws = state.workspace;
  const base = ws && ws.url ? ws.url : "";
  const url = base ? base + (path.startsWith("/") ? path : "/" + path) : path;
  return fetch(url, { method: "GET", cache: "no-store" });
}

function setUsageState(kind, title, detail) {
  els.usageStatus.innerHTML = "";
  if (!kind) return;
  const card = el("div", "usage-state-card " + kind);
  if (kind === "loading") card.append(el("span", "usage-spinner"));
  const copy = el("div");
  copy.append(el("strong", "", title));
  if (detail) copy.append(el("p", "", detail));
  card.append(copy);
  els.usageStatus.append(card);
}

function destroyUsageVisuals() {
  for (const chart of usageDashboardState.charts) chart.destroy();
  usageDashboardState.charts = [];
  if (usageDashboardState.grid) { usageDashboardState.grid.destroy(); usageDashboardState.grid = null; }
  if (usageDashboardState.resizeObserver) { usageDashboardState.resizeObserver.disconnect(); usageDashboardState.resizeObserver = null; }
  for (const id of ["usageTrendChart", "usageModelChart", "usageCompositionChart", "usageTable"]) {
    const node = $(id);
    if (node) node.innerHTML = "";
  }
}

function fillUsageSelect(select, values, allLabel, labelForValue) {
  const current = select.value;
  select.innerHTML = "";
  const all = el("option", "", allLabel); all.value = ""; select.append(all);
  for (const value of [...new Set(values)].sort()) {
    const option = el("option", "", labelForValue ? labelForValue(value) : value); option.value = value; select.append(option);
  }
  if (values.includes(current)) select.value = current;
}

function updateUsageFilterOptions(data) {
  const localSessions = Array.isArray(state.lastList) ? state.lastList : [];
  const currentRoot = els.usageRootSession.value;
  els.usageRootSession.innerHTML = "";
  const all = el("option", "", "全部会话"); all.value = ""; els.usageRootSession.append(all);
  for (const session of localSessions.filter((item) => !item.parent_session_id)) {
    const option = el("option", "", session.title || shortId(session.id)); option.value = session.id; els.usageRootSession.append(option);
  }
  if (localSessions.some((item) => item.id === currentRoot)) els.usageRootSession.value = currentRoot;
  fillUsageSelect(els.usageModel, data.byModel.map((row) => row.key), "全部模型");
  fillUsageSelect(els.usageRole, data.byRole.map((row) => row.key), "全部角色", (value) => value === "unknown" ? "未知角色" : value);
  fillUsageSelect(els.usageKind, data.byKind.map((row) => row.key), "全部类型");
}

function renderUsageKpis(data) {
  const t = data.totals;
  const average = t.callCount ? t.total / t.callCount : 0;
  const cacheValuesAvailable = t.cacheHit !== null && t.cacheMiss !== null;
  const cacheRatio = cacheValuesAvailable && t.cacheHit + t.cacheMiss > 0
    ? (t.cacheHit / (t.cacheHit + t.cacheMiss) * 100).toFixed(1) + "%" : "不可用";
  const cacheHint = "命中 " + formatUsageNumber(t.cacheHit) + " · 未命中 " + formatUsageNumber(t.cacheMiss)
    + " · 命中 " + usageCoverage(t.cacheHitReportedCalls, t.callCount)
    + " · 未命中 " + usageCoverage(t.cacheMissReportedCalls, t.callCount);
  const cards = [
    ["模型调用", formatUsageNumber(t.callCount), "完整筛选范围精确值", "var(--blue)"],
    ["总 Token", formatUsageNumber(t.total), "后端权威总计；不重复加入推理或缓存子集", "var(--green)"],
    ["输入 Token", formatUsageNumber(t.input), "完整筛选范围", "var(--cyan)"],
    ["输出 Token", formatUsageNumber(t.output), "完整筛选范围", "var(--violet)"],
    ["推理 Token", formatUsageNumber(t.reasoning), t.reasoning === null ? "供应商未提供" : usageCoverage(t.reasoningReportedCalls, t.callCount), "var(--magenta)"],
    ["缓存命中", formatUsageNumber(t.cacheHit), "命中覆盖 " + usageCoverage(t.cacheHitReportedCalls, t.callCount), "var(--yellow)"],
    ["缓存命中率", cacheRatio, cacheHint, "var(--orange)"],
    ["平均 Token / 调用", formatUsageNumber(average), "总 Token ÷ 模型调用", "var(--blue)"],
  ];
  els.usageKpis.innerHTML = "";
  for (const [label, value, hint, accent] of cards) {
    const card = el("article", "usage-kpi");
    if (card.style && card.style.setProperty) card.style.setProperty("--kpi-accent", accent);
    else card.style["--kpi-accent"] = accent;
    card.append(el("span", "usage-kpi-label", label), el("strong", "usage-kpi-value", value), el("span", "usage-kpi-hint", hint));
    els.usageKpis.append(card);
  }
}

function usageChartSize(node) { return { width: Math.max(280, node.clientWidth || 560), height: node.clientHeight || 260 }; }

function usageBaseChartOptions(node, series, axes) {
  const size = usageChartSize(node);
  return {
    width: size.width, height: size.height, cursor: { drag: { x: true, y: false } }, scales: { x: { time: true } }, series,
    axes: axes || [
      { stroke: "#586e75", grid: { stroke: "rgba(147,161,161,.25)" }, ticks: { stroke: "#93a1a1" } },
      { stroke: "#586e75", grid: { stroke: "rgba(147,161,161,.22)" }, ticks: { stroke: "#93a1a1" }, values: (u, vals) => vals.map(formatUsageCompact) },
    ],
    legend: { show: true },
  };
}

function renderUsageCharts(data) {
  const UPlot = globalThis.uPlot;
  if (typeof UPlot !== "function") throw new Error("图表组件未加载");
  const trendNode = $("usageTrendChart");
  const trend = data.trend.slice().sort((a, b) => Date.parse(a.at) - Date.parse(b.at));
  usageDashboardState.charts.push(new UPlot(usageBaseChartOptions(trendNode, [
    {}, { label: "输入", stroke: "#2aa198", width: 2, fill: "rgba(42,161,152,.08)" },
    { label: "输出", stroke: "#6c71c4", width: 2 }, { label: "推理（已报告）", stroke: "#d33682", width: 2 },
    { label: "缓存命中（已报告）", stroke: "#b58900", width: 2 },
  ]), [trend.map((p) => Date.parse(p.at) / 1000), trend.map((p) => p.input), trend.map((p) => p.output),
    trend.map((p) => p.reasoning), trend.map((p) => p.cacheHit)], trendNode));

  const modelNode = $("usageModelChart");
  const models = data.byModel.slice().sort((a, b) => b.total - a.total).slice(0, 8);
  const modelOpts = usageBaseChartOptions(modelNode, [{}, {
    label: "总 Token", stroke: "#268bd2", fill: "rgba(38,139,210,.72)", paths: UPlot.paths.bars({ size: [0.62, 80] }), points: { show: false },
  }], [
    { stroke: "#586e75", grid: { show: false }, ticks: { stroke: "#93a1a1" }, values: (u, vals) => vals.map((v) => models[Math.round(v)] ? models[Math.round(v)].key : "") },
    { stroke: "#586e75", grid: { stroke: "rgba(147,161,161,.22)" }, ticks: { stroke: "#93a1a1" }, values: (u, vals) => vals.map(formatUsageCompact) },
  ]);
  modelOpts.scales.x.time = false; modelOpts.cursor.drag.x = false;
  usageDashboardState.charts.push(new UPlot(modelOpts, [models.map((_, i) => i), models.map((item) => item.total)], modelNode));

  const compositionNode = $("usageCompositionChart");
  const composition = [["输入", data.totals.input], ["输出", data.totals.output], ["推理（已报告）", data.totals.reasoning],
    ["缓存命中（已报告）", data.totals.cacheHit], ["缓存未命中（已报告）", data.totals.cacheMiss]];
  const available = composition.filter((item) => item[1] !== null);
  const compositionOpts = usageBaseChartOptions(compositionNode, [{}, {
    label: "Token", stroke: "#859900", fill: "rgba(133,153,0,.72)", paths: UPlot.paths.bars({ size: [0.62, 80] }), points: { show: false },
  }], [
    { stroke: "#586e75", grid: { show: false }, ticks: { stroke: "#93a1a1" }, values: (u, vals) => vals.map((v) => available[Math.round(v)] ? available[Math.round(v)][0] : "") },
    { stroke: "#586e75", grid: { stroke: "rgba(147,161,161,.22)" }, ticks: { stroke: "#93a1a1" }, values: (u, vals) => vals.map(formatUsageCompact) },
  ]);
  compositionOpts.scales.x.time = false; compositionOpts.cursor.drag.x = false;
  usageDashboardState.charts.push(new UPlot(compositionOpts, [available.map((_, i) => i), available.map((item) => item[1])], compositionNode));

  if (typeof ResizeObserver === "function") {
    usageDashboardState.resizeObserver = new ResizeObserver(() => {
      for (const chart of usageDashboardState.charts) {
        const parent = chart.root.parentNode;
        if (parent && parent.clientWidth) chart.setSize(usageChartSize(parent));
      }
    });
    for (const id of ["usageTrendChart", "usageModelChart", "usageCompositionChart"]) usageDashboardState.resizeObserver.observe($(id));
  }
}

function renderUsageTable(data) {
  if (!globalThis.gridjs || typeof globalThis.gridjs.Grid !== "function") throw new Error("表格组件未加载");
  const html = globalThis.gridjs.html;
  const rows = data.rows.slice().sort((a, b) => b.total - a.total);
  usageDashboardState.grid = new globalThis.gridjs.Grid({
    columns: [
      { name: "会话 / 子代理", formatter: (cell, row) => html('<span class="usage-table-title" title="' + escapeHtml(cell) + '">' + escapeHtml(cell) + '</span><span class="usage-table-id">' + escapeHtml(row.cells[1].data) + '</span>') },
      { name: "会话 ID", hidden: true }, { name: "模型" },
      { name: "角色", formatter: (cell) => html('<span class="usage-role-chip">' + escapeHtml(cell) + '</span>') },
      { name: "类型" }, { name: "调用", sort: true, formatter: formatUsageNumber },
      { name: "总 Token", sort: true, formatter: formatUsageNumber }, { name: "输入", sort: true, formatter: formatUsageNumber },
      { name: "输出", sort: true, formatter: formatUsageNumber }, { name: "推理", sort: true, formatter: formatUsageNumber },
      { name: "缓存命中", sort: true, formatter: formatUsageNumber }, { name: "缓存未命中", sort: true, formatter: formatUsageNumber },
    ],
    data: rows.map((row) => [row.title, row.id, row.models.join(", "), row.roleLabel, row.kinds.join(", "), row.callCount, row.total, row.input, row.output, row.reasoning, row.cacheHit, row.cacheMiss]),
    sort: true, search: { enabled: true, placeholder: "筛选会话、模型、角色或 ID…" },
    pagination: rows.length > 10 ? { limit: 10, summary: true } : false,
    language: { search: { placeholder: "筛选会话、模型、角色或 ID…" }, pagination: { previous: "上一页", next: "下一页", showing: "显示", results: () => "条", of: "共", to: "至" }, noRecordsFound: "当前范围内没有会话用量" },
  }).render(els.usageTable);
  els.usageTableNote.textContent = data.topHasMore
    ? "Top " + data.topLimit + "（按总 Token）；上方 KPI、趋势和分组均为完整筛选范围"
    : "全部 " + rows.length + " 个结果（按总 Token）；上方 KPI、趋势和分组均为完整筛选范围";
}

async function loadUsageDashboard() {
  if (!usageDashboardState.open) return;
  const request = ++usageDashboardState.request;
  destroyUsageVisuals();
  els.usageContent.hidden = true;
  els.usageContent.setAttribute("aria-busy", "true");
  setUsageState("loading", "正在汇总用量…", "这不会影响当前对话或运行中的任务。");
  try {
    const res = await usageDashboardGet("/api/usage/dashboard?" + usageQuery());
    if (request !== usageDashboardState.request || !usageDashboardState.open) return;
    if (res.status === 503) {
      let body = null;
      try { body = await res.json(); } catch (error) { /* malformed error handled below */ }
      if (body && body.backend === "jsonl" && typeof body.error === "string") {
        setUsageState("warning", "JSONL 汇总不可用", body.error);
        return;
      }
      throw new Error("服务器返回 HTTP 503，且错误响应不符合 JSONL 契约");
    }
    if (!res.ok) throw new Error("服务器返回 HTTP " + res.status);
    const data = normalizeUsagePayload(await res.json());
    if (request !== usageDashboardState.request || !usageDashboardState.open) return;
    updateUsageFilterOptions(data);
    if (!data.totals.callCount && !data.totals.total && !data.rows.length && !data.trend.length) {
      setUsageState("empty", "没有匹配的用量", "尝试扩大时间范围，或重置模型、角色、类型和根会话筛选。");
      return;
    }
    setUsageState("", "", "");
    els.usageContent.hidden = false;
    renderUsageKpis(data); renderUsageCharts(data); renderUsageTable(data);
    els.usageUpdatedAt.textContent = "更新于 " + new Date(data.generatedAt).toLocaleString("zh-CN", { hour12: false });
  } catch (error) {
    if (request !== usageDashboardState.request || !usageDashboardState.open) return;
    setUsageState("error", "无法加载用量数据", error && error.message ? error.message : "请检查网络连接后重试。");
  } finally {
    if (request === usageDashboardState.request) els.usageContent.setAttribute("aria-busy", "false");
  }
}

function openUsageDashboard() {
  if (!globalThis.uPlot || !globalThis.gridjs) { setBanner("⚠ 用量可视化组件未能加载，请刷新页面后重试。", true); return; }
  usageDashboardState.open = true; setDefaultUsageDates(); closeSidebar();
  els.chatView.classList.add("hidden"); els.usageDashboard.hidden = false;
  els.usageDashboardBtn.setAttribute("aria-pressed", "true"); document.title = "用量仪表盘 · e-agent";
  loadUsageDashboard(); els.usageTitle.focus?.();
}

function closeUsageDashboard(restoreFocus = true) {
  usageDashboardState.open = false; usageDashboardState.request += 1; destroyUsageVisuals();
  els.usageDashboard.hidden = true; els.chatView.classList.remove("hidden");
  els.usageDashboardBtn.setAttribute("aria-pressed", "false"); document.title = "e-agent · Web UI";
  if (restoreFocus) els.usageDashboardBtn.focus();
}

function resetUsageFilters() {
  for (const select of [els.usageRootSession, els.usageModel, els.usageRole, els.usageKind]) select.value = "";
  els.usageBucket.value = "day"; els.usageFrom.value = ""; els.usageTo.value = ""; setDefaultUsageDates(); loadUsageDashboard();
}

els.usageDashboardBtn.addEventListener("click", () => usageDashboardState.open ? closeUsageDashboard() : openUsageDashboard());
els.usageCloseBtn.addEventListener("click", closeUsageDashboard);
els.usageRefreshBtn.addEventListener("click", loadUsageDashboard);
els.usageResetBtn.addEventListener("click", resetUsageFilters);
els.usageFilters.addEventListener("submit", (event) => { event.preventDefault(); loadUsageDashboard(); });
document.addEventListener("keydown", (event) => { if (event.key === "Escape" && usageDashboardState.open) closeUsageDashboard(); });
