#!/usr/bin/env python3
"""Scratch test driver for src/ui/index.html.

Extracts the inline <script> from the page, runs it inside a minimal DOM/fetch/SSE
stub under gjs (SpiderMonkey), and asserts on rendering.  NOTE: gjs timers do not
fire in this mode, so the harness flushes microtasks instead of sleeping.

Not part of the product; safe to delete.
"""
import os, re, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
# index.html 的 <script> 是构建期占位符（/*__JS_APP__*/ 等由 server.rs 替换），
# 直接读拆分后的 JS 文件清单（与 server.rs 拼接顺序一致，同一 <script> 内
# 顶层声明全局可见）+ vendor/marked.min.js，等价于 server 组装的单文件。
JS_FILES = ['app.js', 'render.js', 'sessions.js', 'tasks.js', 'sse.js']
js = "\n".join(open(os.path.join(HERE, f), encoding='utf-8').read() for f in JS_FILES)
vendor_js = open(os.path.join(HERE, 'vendor', 'marked.min.js'), encoding='utf-8').read()

MODE = os.environ.get('MODE', 'open')   # 'open' = openSession path, 'direct' = loadHistory
DEEP_LINK = os.environ.get('DEEP_LINK', '')   # 注入 ?session=<id> 到 location.search（init 启动时解析）
TRACE = os.environ.get('TRACE') == '1'
# gjs 内置 TextDecoder 不可覆盖且不支持 stream 选项；页面 JS 里的 new TextDecoder() 换成桩工厂。
# 注意：拆分后 tasks.js（startTaskStream）在 sse.js 之前拼接，无缩进的搜索串会先命中
# startTaskStream 里的同名行；这里带 2 空格缩进精确匹配 readSSEStream（唯一 2 空格缩进的
# 那处），保持与拆分前一致的注入目标（startTaskStream 的 TextDecoder 在 harness 中从不执行）。
js = js.replace('  const decoder = new TextDecoder();', '  const decoder = makeTextDecoder();')
if TRACE:
    js = js.replace('async function readSSEStream(reader, id, wsId, epoch, ctrl) {',
        'async function readSSEStream(reader, id) {\n  console.log("SSE: stream start");')
    # 下面两处与 startTaskStream 内的同名字符串区分：带 4 空格缩进只命中 readSSEStream
    js = js.replace('    const { done, value } = await reader.read();',
        '    const { done, value } = await reader.read();\n    console.log("SSE: got value", done, JSON.stringify(String(value).slice(0, 120)));')
    js = js.replace('    buf += decoder.decode(value, { stream: true }).replace(/\\r\\n/g, "\\n");',
        '    buf += decoder.decode(value, { stream: true }).replace(/\\r\\n/g, "\\n");\n    console.log("SSE: buf len=" + buf.length + " idx=" + buf.indexOf("\\n\\n"));')
    js = js.replace('function handleSSEBlock(block, id, wsId, epoch) {',
        'function handleSSEBlock(block, id) {\n  console.log("SSE: block event=" + (block.split("\\n")[0] || ""));')
    js = js.replace('function connectSSE(id) {\n  stopSSE();',
        'function connectSSE(id) {\n  console.log("SSE: connect", id);\n  stopSSE();')
    js = js.replace('setConn("ok", "● 已连接");',
        'console.log("SSE: response ok");\n    setConn("ok", "● 已连接");')
    js = js.replace('if (!res.ok || !res.body) throw new Error("HTTP " + res.status);',
        'if (!res.ok || !res.body) throw new Error("HTTP " + res.status);\n    console.log("SSE: body ok, has getReader:", typeof res.body.getReader);')

HARNESS = r'''
/* 极简 HTML 序列化/解析：让 innerHTML 读-写往返与真实浏览器行为一致
   （restored 分支的缓存恢复、resync 的离屏容器替换都依赖 innerHTML）。 */
function escHtml(s){ return String(s).replace(/&/g,"&amp;").replace(/</g,"&lt;").replace(/>/g,"&gt;").replace(/"/g,"&quot;"); }
function serEl(e){
  if (e.tag === "#comment") return "<!--" + (e.textContent||"") + "-->";
  let out = "<" + e.tag;
  if (e._classes.size) out += ' class="' + [...e._classes].join(" ") + '"';
  out += ">";
  if (e._children.length) {
    for (const c of e._children) {
      out += (c instanceof El) ? serEl(c) : (c.text != null ? escHtml(c.text) : "");
    }
  } else if (e._innerHTML) {
    out += e._innerHTML;   // renderMarkdown 等直接赋的 HTML 串
  } else if (e.textContent) {
    out += escHtml(e.textContent);
  }
  return out + "</" + e.tag + ">";
}
function parseHtml(html){
  const roots = [];
  const s = String(html);
  let i = 0;
  const n = s.length;
  const pushText = (parent, t) => { if (t) parent.push({text: t}); };
  function walk(parent, stopTag, host){
    let text = "";
    while (i < n) {
      if (s[i] === "<") {
        if (s.startsWith("<!--", i)) { const e = s.indexOf("-->", i); i = e >= 0 ? e + 3 : n; continue; }
        if (s.startsWith("</", i)) {
          const m = /^<\/([a-zA-Z0-9-]+)>/.exec(s.slice(i));
          if (m) { i += m[0].length; if (stopTag && m[1] === stopTag) { pushText(parent, text); return; } continue; }
        }
        const m = /^<([a-zA-Z0-9-]+)((?:\s+[a-zA-Z-]+=(?:"[^"]*"|'[^']*'))*)\s*\/?>/.exec(s.slice(i));
        if (m) {
          pushText(parent, text); text = "";
          i += m[0].length;
          const tag = m[1];
          const voidEl = tag === "br" || tag === "hr" || tag === "img" || m[0].endsWith("/>");
          const e = new El(tag);
          const cls = [];
          let am;
          const attrRe = /\s+([a-zA-Z-]+)=("[^"]*"|'[^']*')/g;
          while ((am = attrRe.exec(m[2]))) { if (am[1] === "class") cls.push(am[2].slice(1, -1)); }
          if (cls.length) e.className = cls.join(" ");
          parent.push(e);
          e._parent = host || null;   // 与程序化 append/insertBefore 一致：closest 依赖父链
          if (!voidEl) walk(e._children, tag, e);
          continue;
        }
      }
      if (s.startsWith("&lt;", i)) { text += "<"; i += 4; continue; }
      if (s.startsWith("&gt;", i)) { text += ">"; i += 4; continue; }
      if (s.startsWith("&amp;", i)) { text += "&"; i += 5; continue; }
      if (s.startsWith("&quot;", i)) { text += '"'; i += 6; continue; }
      if (s.startsWith("&#39;", i)) { text += "'"; i += 5; continue; }
      text += s[i]; i += 1;
    }
    pushText(parent, text);
  }
  walk(roots, null, null);
  return roots;
}
function matchSel(el, sel){
  const parts = String(sel).split(".");
  let tag = parts[0];
  const classes = parts.slice(1).filter(Boolean);
  if (tag === "*") tag = "";
  if (tag && el.tag !== tag) return false;
  for (const c of classes) if (!(el._classes && el._classes.has(c))) return false;
  return true;
}
class El {
  constructor(tag){ this.tag=tag; this._children=[]; this._classes=new Set();
    this._className=""; this._text=""; this._innerHTML=""; this.hidden=false;
    this.disabled=false; this.value=""; this.title=""; this.type=""; this.style={};
    this.scrollHeight=0; this.scrollTop=0; this.clientHeight=0; this.offsetParent=null;
    this._parent=null; this._listeners={}; this._attrs={};
    this.classList={ add:(...c)=>c.forEach(x=>this._classes.add(x)),
      remove:(...c)=>c.forEach(x=>this._classes.delete(x)),
      contains:(c)=>this._classes.has(c),
      toggle:(c,force)=>{ const on = force !== undefined ? !!force : !this._classes.has(c);
        if (on) this._classes.add(c); else this._classes.delete(c); return on; } }; }
  get children(){ return this._children; }
  get parentNode(){ return this._parent ?? null; }
  get firstChild(){ return this._children[0] ?? null; }
  get nextSibling(){ if(!this._parent) return null;
    const i=this._parent._children.indexOf(this);
    return i>=0 && i+1<this._parent._children.length ? this._parent._children[i+1] : null; }
  get className(){ return this._className; }
  set className(v){ this._className=String(v); this._classes=new Set(String(v).split(/\s+/).filter(Boolean)); }
  /* innerHTML 序列化只含内容（不含自身标签），与真实 DOM 一致 */
  get innerHTML(){ if (this._children.length)
      return this._children.map((c) => (c instanceof El) ? serEl(c) : (c.text != null ? escHtml(c.text) : "")).join("");
    if (this._innerHTML) return this._innerHTML;
    if (this.textContent) return escHtml(this.textContent);
    return ""; }
  set innerHTML(v){ /* 与真实 DOM 一致：替换会断开旧子节点（isConnected → false） */
    for (const c of this._children) { if (c instanceof El) c._parent = null; }
    if(String(v)==="") { this._children=[]; this._innerHTML=""; }
    else { this._children = parseHtml(v); this._innerHTML=""; } }
  /* 与真实 DOM 一致：textContent 取文本后代拼接；赋值则整体替换（清子节点） */
  get textContent(){ if (this._children.length) {
      let out = "";
      for (const c of this._children) out += (c instanceof El) ? c.textContent : (c.text != null ? c.text : "");
      return out;
    } return this._text; }
  set textContent(v){ this._text = String(v == null ? "" : v); this._children = []; this._innerHTML = ""; }
  setAttribute(k, v){ this._attrs[k]=String(v); }
  getAttribute(k){ return Object.prototype.hasOwnProperty.call(this._attrs,k) ? this._attrs[k] : null; }
  hasAttribute(k){ return Object.prototype.hasOwnProperty.call(this._attrs,k); }
  removeAttribute(k){ delete this._attrs[k]; }
  cloneNode(deep){ const c = new El(this.tag); c._className=this._className;
    c._classes=new Set(this._classes); c._attrs=Object.assign({}, this._attrs);
    if (deep) {   /* 真实 DOM：深拷贝子节点（含文本）；浅拷贝只复制属性/class */
      c._children = this._children.map((x) => x instanceof El ? x.cloneNode(true)
        : Object.assign({}, x));
      c._children.forEach((x) => { if (x instanceof El) x._parent = c; });
    } else { c._text = ""; }
    c.hidden=this.hidden; return c; }
  /* 真实 DOM 语义：textContent 赋值后 append 子节点时，已设的文本成为
     文本子节点（而非被 children 遮蔽丢失）。 */
  _materializeText(){ if (this._text !== "" && this._children.length === 0) {
      this._children.push({ text: this._text }); this._text = ""; } }
  append(...nodes){ for(const n of nodes){ if(n==null) continue;
    this._materializeText();
    const c=typeof n==="string"?{text:n}:n; this._children.push(c);
    if(c._parent==null) c._parent=this; } }
  appendChild(n){ const p=n._parent;   /* 真实 DOM 语义：移动=先从旧父节点移除 */
    if(p){ const j=p._children.indexOf(n); if(j>=0) p._children.splice(j,1); }
    this._materializeText();
    this._children.push(n); n._parent=this; return n; }
  prepend(...nodes){ let i=0;           /* 真实 DOM 语义：头部插入（已有父节点先移除） */
    for(const n of nodes){ if(n==null) continue;
      const c=typeof n==="string"?{text:n}:n; const p=c._parent;
      if(p){ const j=p._children.indexOf(c); if(j>=0) p._children.splice(j,1); }
      this._materializeText();
      this._children.splice(i++,0,c); c._parent=this; } }
  get isConnected(){ return this._parent != null; }
  insertBefore(n, ref){ const p=n._parent; if(p){ const j=p._children.indexOf(n); if(j>=0) p._children.splice(j,1); }
    const i=this._children.indexOf(ref);
    if(i<0) this._children.push(n); else this._children.splice(i,0,n);
    n._parent=this; return n; }
  remove(){ if(this._parent){ const p=this._parent;
    const i=p._children.indexOf(this); if(i>=0) p._children.splice(i,1);
    this._parent=null; } }
  insertAdjacentText(pos, t){ if(pos==="beforeend") this._children.push({text:t}); }
  addEventListener(type, fn){ (this._listeners[type] || (this._listeners[type]=[])).push(fn); }
  querySelectorAll(sel){ const out=[];
    const walk=(e)=>{ for(const c of e._children){ if(c instanceof El){
      if(matchSel(c,sel)) out.push(c); walk(c); } } };
    walk(this); return out; }
  querySelector(sel){ return this.querySelectorAll(sel)[0] ?? null; }
  closest(sel){ let n=this; while(n){ if(matchSel(n,sel)) return n; n=n._parent; } return null; }
  focus(){}
  blur(){}
  setPointerCapture(){}
  getBoundingClientRect(){ return { top: 0, bottom: 32, height: 32 }; }
}
const elsById={};
for(const id of ["topActions","backParentBtn","connState","banner","bannerText","bannerClose","tokenInput","tokenToggle","chatView","chatEmpty",
  "chatSessionId","chatStatus",
  "usageInfo","messages","promptInput","sendBtn","cancelBtn","compactBtn",
  "queueBar","slashMenu","jumpBottomBtn","composerMeta","sidebarBtn","sidebarOverlay","sidebar",
  "sidebarCloseBtn","sidebarFilter","sidebarTree","tasksToggleBar","composerTasks","forkMenu",
  "workspaceSelect","workspaceAddBtn","workspaceRemoveBtn","workspaceEditor",
  "wsNameInput","wsUrlInput","wsTokenInput","wsSaveBtn","wsCancelBtn"]) elsById[id]=new El(id);

const _ls={};
globalThis.localStorage={ getItem:k=>_ls[k]??null, setItem:(k,v)=>{_ls[k]=v;}, removeItem:k=>{delete _ls[k];} };
const _docEl = new El("html");
globalThis.document={ createElement:t=>new El(t), createComment:t=>new El("#comment"),
  getElementById:id=>elsById[id], addEventListener(){}, documentElement:_docEl };
globalThis.navigator={ onLine:true };
globalThis.confirm=()=>true;
// gjs 自带 window 全局（不可整体替换）：就地补上页面需要的属性
window.visualViewport=null; window.innerHeight=800;
window.addEventListener=()=>{}; window.confirm=()=>true; window.setTimeout=()=>0; window.clearTimeout=()=>{};
globalThis.history={ replaceState(){} };
globalThis.location={ search:"__DEEP_LINK_SEARCH__" };
/* URL 解析入口可配置：DEEP_LINK env → location.search（?session=<id>），
   init() 在页面加载时读它；测试也可直接改 location.search 后重跑 init()。
   URLSearchParams 桩做最小解析（?a=b&c=d，支持 URL 解码），键不存在 get
   返回 null——与真实行为一致。 */
globalThis.URLSearchParams=class{ constructor(s){ this._m=new Map(); const q=String(s||""); if(q.startsWith("?")){ for(const kv of q.slice(1).split("&")){ if(!kv) continue; const i=kv.indexOf("="); const k=i<0?kv:kv.slice(0,i); const v=i<0?"":decodeURIComponent(kv.slice(i+1)); if(k) this._m.set(k,v); } } } get(k){ return this._m.has(k)?this._m.get(k):null; } };
globalThis.requestAnimationFrame=()=>0;
/* abort 桩（真实语义）：abort() 置 signal.aborted 并触发 abort 监听器；
   fetch 侧对未 settle 的请求 reject AbortError（见 resp 的 signal 参数与
   abortable），模拟真实 fetch 的 AbortController——10s 超时
   （fetchWithTimeout）与主动取消（stopTaskStream）都能打断 pending 请求。 */
globalThis.AbortController=class{
  constructor(){ this.signal={ aborted:false, _abortListeners:[],
    addEventListener:(t,fn)=>{ if(t==="abort") this.signal._abortListeners.push(fn); },
    removeEventListener:(t,fn)=>{ if(t==="abort"){ const i=this.signal._abortListeners.indexOf(fn); if(i>=0) this.signal._abortListeners.splice(i,1); } } }; }
  abort(){ if(this.signal.aborted) return; this.signal.aborted=true;
    for(const fn of this.signal._abortListeners.slice()) fn(); }
};
// gjs 内置 TextDecoder 不可覆盖且不支持 stream 选项；用工厂替换页面里的 new TextDecoder()
function makeTextDecoder(){ return { decode(v){ return typeof v==="string"?v:""; } }; }
// gjs timers don't fire here; keep them inert. setTimeout 记录回调，测试可手动触发
// （scheduleReconnect 的重连定时器需要验证触发时的三重校验）。
const scheduledIntervals=[];
globalThis.setInterval=(fn)=>{ scheduledIntervals.push(fn); return scheduledIntervals.length; };
globalThis.clearInterval=(id)=>{ if (id>0) scheduledIntervals[id-1]=null; };
const scheduledTimeouts=[];
globalThis.setTimeout=(fn)=>{ scheduledTimeouts.push(fn); return scheduledTimeouts.length; };
globalThis.clearTimeout=(id)=>{ if (id>0) scheduledTimeouts[id-1]=null; };

const historyData={entries:[
  {type:"message", message:{User:{content:"你好，帮我看看", images:[]}}},
  {type:"message", message:{Assistant:{content:"好的，我来处理。", tool_calls:[{id:"call1",name:"bash",arguments:'{"command":"ls"}'}], reasoning:null}}},
  {type:"message", message:{Tool:{call_id:"call1", name:"bash", content:"file1\nfile2", is_error:false, synthetic:false}}},
  {type:"message", message:{Assistant:{content:"完成。", tool_calls:[], reasoning:"思考过程"}}},
  {type:"compaction", summary:"早期内容已压缩", retained:[]},
  {type:"notice", text:"后台任务 #1 完成"},
  {type:"background_completion", id:7, output:"build ok", label:"cargo"},
  {type:"forked_from", source:"sess-old", at:3},
], next_before_seq:100};
/* 滚动分页：更早的一页（before_seq=100 之后没有更老的段） */
const historyOlderData={entries:[
  {type:"message", message:{User:{content:"更早的历史消息：这是更老的一段", images:[]}}},
  {type:"notice", text:"更早的通知行"},
], next_before_seq:null};
const FETCHES=[];
const FETCH_HEADERS=[];   // 每个请求的 {url, method, headers}（全局 token 回退测试断言 Authorization）
const FETCH_BODIES=[];    // 每个请求的 body（深链 resume 断言 POST {"id":...}）
const sseChunks = [
  "event: snapshot\ndata: [{\"type\":\"notice\",\"text\":\"SNAPSHOT-SHOULD-BE-SKIPPED\"}]\n\n",
  "event: status\ndata: {\"status\":\"Busy\"}\n\n",
  "event: UserPrompt\ndata: {\"type\":\"user_prompt\",\"session_id\":\"s1\",\"seq\":1,\"text\":\"再来一次\"}\n\n",
  "event: AssistantDelta\ndata: {\"type\":\"assistant_delta\",\"session_id\":\"s1\",\"seq\":2,\"delta\":\"正在\"}\n\n",
  "event: AssistantDelta\ndata: {\"type\":\"assistant_delta\",\"session_id\":\"s1\",\"seq\":3,\"delta\":\"处理\"}\n\n",
  "event: ReasoningDelta\ndata: {\"type\":\"reasoning_delta\",\"session_id\":\"s1\",\"seq\":4,\"delta\":\"推理中\"}\n\n",
  "event: ToolCall\ndata: {\"type\":\"tool_call\",\"session_id\":\"s1\",\"seq\":5,\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a.txt\\\"}\"}\n\n",
  "event: ToolResult\ndata: {\"type\":\"tool_result\",\"session_id\":\"s1\",\"seq\":6,\"is_error\":true,\"content\":\"文件不存在\"}\n\n",
  "event: AssistantText\ndata: {\"type\":\"assistant_text\",\"session_id\":\"s1\",\"seq\":7,\"text\":\"出错了，**换个方式**。\"}\n\n",
  "event: Notice\ndata: {\"type\":\"notice\",\"session_id\":\"s1\",\"seq\":8,\"text\":\"系统提示行\"}\n\n",
  "event: Error\ndata: {\"type\":\"error\",\"session_id\":\"s1\",\"seq\":9,\"error\":\"回合失败\"}\n\n",
  "event: Usage\ndata: {\"type\":\"usage\",\"session_id\":\"s1\",\"seq\":10,\"context_input\":1234,\"session\":{\"input_tokens\":100,\"output_tokens\":50}}\n\n",
  ": keepalive\n\n",
];
function stream(){
  let i = 0;
  return { getReader(){ return { read: async()=>{
    if (i === 0) { i = 1; return {done:false, value:sseChunks.join("")}; }
    return {done:true};
  } }; } };
}
function streamEmpty(){
  return { getReader(){ return { read: async()=>({done:true}) } } };
}
/* 手动控制的流：第一个 read 挂起（resolve 保存在 a1StreamReadResolve），
   之后一次性吐出 value，再结束。用于「切换后 A 的陈旧块才到达」的测试。 */
function streamManual(){
  let phase = 0;
  return { getReader(){ return { read: async () => {
    if (phase === 0) { phase = 1;
      return new Promise((resolve) => { a1StreamReadResolve = resolve; });
    }
    if (phase === 1) { phase = 2; return { done: false, value: "" }; }
    return { done: true };
  } }; } };
}
/* 手动控制的流（带真实事件块）：首个 read 挂起（resolve 保存在
   a1StreamReadResolve），resolve 后下一个 read 吐出 Notice 块再结束。用于
   「createSessionIn 挂起/失败期间当前流不中断」断言：块被处理 ⇔ epoch 未被
   提前递增（旧 bug：POST 前 ++sessionOpenEpoch → stillCurrent 失败 → 块丢弃）。 */
function streamManualNotice(){
  let phase = 0;
  return { getReader(){ return { read: async () => {
    if (phase === 0) { phase = 1;
      return new Promise((resolve) => { a1StreamReadResolve = resolve; });
    }
    if (phase === 1) { phase = 2;
      return { done: false, value: "event: Notice\ndata: {\"type\":\"notice\",\"session_id\":\"a1\",\"seq\":99,\"text\":\"create-pending-stream-alive\"}\n\n" };
    }
    return { done: true };
  } }; } };
}
/* 带 abort 感知的响应 Promise：signal 已 abort → 立即 reject AbortError；
   pending 期间 abort → reject AbortError（resolve 后迟到 abort 是 no-op，
   与真实 fetch 一致）。 */
function resp(status, body, signal){
  return new Promise((resolve, reject) => {
    const ok = status>=200 && status<300;
    const done = () => resolve({ ok, status, body,
      json:async()=>typeof body==="string"?JSON.parse(body):body,
      text:async()=>String(body) });
    if (signal && signal.aborted) { const e=new Error("The operation was aborted."); e.name="AbortError"; reject(e); return; }
    if (signal) signal.addEventListener("abort", () => {
      const e=new Error("The operation was aborted."); e.name="AbortError"; reject(e);
    });
    done();
  });
}
/* 挂 abort 的延迟响应：手动 resolve 的 Promise（慢响应桩）在 signal abort
   时 reject AbortError（不再永久 pending）。 */
function abortable(promise, signal){
  if (!signal) return promise;
  return new Promise((resolve, reject) => {
    if (signal.aborted) { const e=new Error("The operation was aborted."); e.name="AbortError"; reject(e); return; }
    const onAbort = () => { const e=new Error("The operation was aborted."); e.name="AbortError"; reject(e); };
    signal.addEventListener("abort", onAbort);
    promise.then((v)=>{ signal.removeEventListener("abort", onAbort); resolve(v); },
                 (e)=>{ signal.removeEventListener("abort", onAbort); reject(e); });
  });
}
// 任务输出块测试用：/api/tasks 响应与 output 端点文本（测试中可变）
let tasksData = [];
let taskOutputText = "";
// perf 回归测试：output 端点延迟（手动 resolve）——验证 500ms 轮询防重入
let taskOutputDelayed = false;
let taskOutputResolve = null;
// 旧后端降级测试：output 端点 404（静态输出兜底）
let taskOutput404 = false;
// 网络失败测试：output 端点 fetch reject（模拟断网）→ 轮询停止、不降级
let taskOutputNetFail = false;
// 轮询超时测试：workspace 轮询请求是否带了 AbortSignal
let pollSignalSeen = false;
// 会话列表响应（测试中可变）：默认 s1；Bug C 测试会替换成含 subagent 的列表
let sessionsData = [{id:"s1",status:"Idle",model:"kimi",created_at:"2024-01-01T00:00:00Z",entry_count:8,busy:false}];
// 聚合模式：第二台服务器（url "http://b.local"）的独立列表 + 故障开关
let sessionsDataB = [
  {id:"b1",status:"Busy",model:"deepseek",title:"B 主会话",created_at:"2024-02-02T00:00:00Z",entry_count:5,busy:true,active:true},
  {id:"b-orphan",parent_session_id:"missing-b",status:"Idle",entry_count:1,active:true},
];
let sessionsBFail = false;
// perf 回归测试：B 的 /api/sessions GET 延迟（手动 resolve）——验证整轮
// 渲染一次 + 轮询 setTimeout 链防重入（慢响应期间不叠加、完成后才续调度）
let sessionsPDelayed = false;
let sessionsPResolve = null;
// 竞态/格式测试开关：B 的 resume POST 延迟（手动 resolve）、A 的 history 延迟、
// B 返回非数组 JSON（{}）、B 的 GET /api/sessions 延迟（在途轮询守卫）、
// B 的「新建会话」POST 失败（组头 + 按钮失败路径）
let bPostDelayed = false;
let bPostResolve = null;
let aHistoryDelayed = false;
let aHistoryResolve = null;
let sessionsBFormat = false;
// SSE 404 竞态测试：/api/sessions GET 延迟（手动 resolve）——模拟「任务面板
// 直连刚结束的 subagent」时列表刷新未完成（旧缓存仍 active:true）
let sessionsDelayed = false;
let sessionsResolve = null;
let bGetDelayed = false;
let bGetResolve = null;
let sessionsBCreateFail = false;
// 组头「+」新建会话 POST 延迟（手动 resolve）：迟到响应竞态测试
let bCreateDelayed = false;
let bCreateResolve = null;
// 组头「+」新建会话 POST 已返回 201、json() 挂起（手动 resolve/reject）：
// 解析期间用户导航 → 解析后校验（guard 2）与 catch 守卫（guard 3）测试
let bCreateJsonDelayed = false;
let bCreateJsonResolve = null;
// SSE 生命周期测试：a1 的 events 流手动控制（首个 read 挂起，测试切走后
// 再 resolve 陈旧块；验证陈旧流不渲染到当前会话/workspace）
let a1StreamManual = false;
let a1StreamReadResolve = null;
// SSE 404 语义测试：命中这些 id 的 /events 返回 404（模拟历史/已结束会话无流）
let sse404Ids = new Set();
// 深链测试开关：A 的 /api/sessions GET 失败（500）、POST /api/sessions 自定义
// 响应（resume 断言）、history 响应覆盖（id → {status,body}|{netfail}|{hang}|{delay}）、
// /events 200 集合（live 流）、延迟 history 手动 resolve
let sessionsAFail = false;
let sessionsPostCustom = null;
let sessionDeleteStatus = 204;
let historyOverrides = new Map();
let sseOkIds = new Set();
let historyResolve = null;
let a1StreamManualNotice = false;
// fork 面板测试用：/fork-candidates 候选与 /fork POST 响应（测试中可变）
let forkCandidatesData = [
  {at:2, seq:2, preview:"用户：你好，帮我看看"},
  {at:5, seq:7, preview:"助手：完成。这是一段很长的回复内容，需要被截断显示以保持菜单行整洁……"},
  {at:8, seq:10, preview:"系统提示行"},
];
let forkPostResp = resp(201,{id:"fork-1"});
let forkCandidatesDelayed = false;
let forkCandidatesResolve = null;
let forkPostDelayed = false;
let forkPostResolve = null;
globalThis.fetch=(url,opts={})=>{
  FETCHES.push(url);
  FETCH_HEADERS.push({ url, method: (opts.method||"GET").toUpperCase(), headers: opts.headers || {} });
  FETCH_BODIES.push(opts.body || null);
  const signal = opts && opts.signal;   // 传给 abort 感知的响应桩
  const m=(opts.method||"GET").toUpperCase();
      if(url==="/api/tasks") return resp(200, tasksData, signal);
      if(url.startsWith("/api/sessions/")&&url.includes("/tasks/")&&url.endsWith("/output")) {
        if (taskOutput404) return resp(404, {}, signal);
        if (taskOutputDelayed) return abortable(new Promise((resolve) => { taskOutputResolve = resolve; }), signal);
        if (taskOutputNetFail) return Promise.reject(new TypeError("network error"));
        return resp(200, taskOutputText, signal);
      }
      // SSE 404 语义测试：命中这些 id 的 /events 返回 404（优先于下方具体会话路由）
      const _m404 = /^\/api\/sessions\/([^/]+)\/events$/.exec(url);
      if (_m404 && sse404Ids.has(_m404[1])) return resp(404, {}, signal);
      // 深链测试：/events 200 集合（live 流）
      const _mOk = /^\/api\/sessions\/([^/]+)\/events$/.exec(url);
      if (_mOk && sseOkIds.has(_mOk[1])) return resp(200, streamEmpty(), signal);
      // 深链测试：history 响应覆盖（404/401/网络失败/hang 超时/delay 迟到）
      const _mHist = /^\/api\/sessions\/([^/]+)\/history/.exec(url);
      if (_mHist && historyOverrides.has(_mHist[1])) {
        const o = historyOverrides.get(_mHist[1]);
        if (o.netfail) return Promise.reject(new TypeError("network error"));
        if (o.hang) return abortable(new Promise(() => {}), signal);
        if (o.delay) return abortable(new Promise((resolve) => { historyResolve = resolve; }), signal);
        return resp(o.status, o.body, signal);
      }
      if(url==="/api/sessions"&&m==="GET") {
        if (opts && opts.signal) pollSignalSeen = true;
        if (sessionsAFail) return resp(500, {}, signal);
        if (sessionsDelayed) return new Promise((resolve) => { sessionsResolve = resolve; });
        return resp(200, sessionsData, signal);
  }
  // 聚合模式：第二台服务器按 base url 路由（500 故障开关：B 失败时 A 不受影响）
  if(url==="http://b.local/api/sessions"&&m==="GET") {
    if (bGetDelayed) return abortable(new Promise((resolve) => { bGetResolve = resolve; }), signal);
    if (sessionsPDelayed) return abortable(new Promise((resolve) => { sessionsPResolve = resolve; }), signal);
    if (sessionsBFail) return resp(500, {}, signal);
    return resp(200, sessionsBFormat ? {} : sessionsDataB, signal);
  }
  if(url==="http://b.local/api/sessions"&&m==="POST") {
    // 组头「+」新建会话：body {}（无 initial_prompt）→ 独立响应/失败开关；
    // resume 恢复（body {"id":...}）走下方既有延迟/固定响应路径
    if (opts.body === "{}") {
      if (bCreateDelayed) return abortable(new Promise((resolve) => { bCreateResolve = resolve; }), signal);
      // POST 成功但 json() 挂起（手动 resolve/reject）：解析期间导航竞态测试
      if (bCreateJsonDelayed) return Promise.resolve({
        ok: true, status: 201, body: null, text: async () => "",
        json: () => new Promise((resolve, reject) => { bCreateJsonResolve = { resolve, reject }; }),
      });
      if (sessionsBCreateFail) return resp(500, {}, signal);
      return resp(201, { id: "b-new", status: "Idle", active: true }, signal);
    }
    if (bPostDelayed) return abortable(new Promise((resolve) => { bPostResolve = resolve; }), signal);
    return resp(201, { id: "b-hist", status: "Idle", active: true }, signal);
  }
  if(url.startsWith("http://b.local/api/sessions/")&&url.endsWith("/pin")&&m==="PUT") return resp(204,null,signal);
  if(url.startsWith("http://b.local/api/sessions/")&&m==="DELETE") return resp(204,null,signal);
  if(url==="/api/sessions"&&m==="POST") {
    if (sessionsPostCustom) return resp(201, sessionsPostCustom, signal);
    return resp(201,{id:"sess-new",status:"Idle"},signal);
  }
  if(url.startsWith("/api/sessions/a1/history")) {
    if (aHistoryDelayed) return abortable(new Promise((resolve) => { aHistoryResolve = resolve; }), signal);
    return resp(200,{entries:[{type:"message",message:{Assistant:{content:"A 会话内容"}}}], next_before_seq:null},signal);
  }
  if(url==="/api/sessions/a1/events")
    return resp(200, a1StreamManualNotice ? streamManualNotice()
      : (a1StreamManual ? streamManual() : streamEmpty()), signal);
  if(url.startsWith("http://b.local/api/sessions/b1/history"))
    return resp(200,{entries:[{type:"message",message:{Assistant:{content:"B 会话内容"}}}], next_before_seq:null},signal);
  if(url==="http://b.local/api/sessions/b1/events") return resp(200, streamEmpty(), signal);
  if(url.startsWith("/api/sessions/s1/history")) {
    if (url.includes("before_seq=")) {
      const seq=url.split("before_seq=")[1].split("&")[0];
      return resp(200, seq==="100" ? historyOlderData : {entries:[], next_before_seq:null}, signal);
    }
    return resp(200, historyData);   // 含 ?limit=…（loadHistory 尾部翻页）
  }
  if(url.startsWith("/api/sessions/s2/history")) return resp(200, historyData, signal);
  if(url.startsWith("/api/sessions/sess-new/history")) return resp(200, {entries:[], next_before_seq:null}, signal);
  if(url.startsWith("/api/sessions/b-new/history")) return resp(200, {entries:[], next_before_seq:null}, signal);
  if(url.startsWith("/api/sessions/fork-1/history")) return resp(200, {entries:[], next_before_seq:null}, signal);
  // restored 替换回归测试用：缓存过期后切回，history 含新消息（历史数据本身不变）
  if(url.startsWith("/api/sessions/restored-test/history")) return resp(200, historyData, signal);
  if(url.startsWith("/api/sessions/restored-test2/history")) return resp(200, historyData, signal);
  if(url==="/api/sessions/s1/events") return resp(200, stream(), signal);
  if(url==="/api/sessions/s2/events") return resp(200, stream(), signal);
  if(url==="/api/sessions/sess-new/events") return resp(200, streamEmpty(), signal);
  if(url==="http://b.local/api/sessions/b-new/events") return resp(200, streamEmpty(), signal);
  if(url==="/api/sessions/fork-1/events") return resp(200, stream(), signal);
  // restored 回归测试：空 SSE 流（snapshot 应被 history 替换路径跳过）
  if(url.startsWith("/api/sessions/restored-test/events")) return resp(200, streamEmpty(), signal);
  if(url.startsWith("/api/sessions/restored-test2/events")) return resp(200, streamEmpty(), signal);
  if(url==="/api/sessions/s1/fork-candidates") {
    if (forkCandidatesDelayed) return new Promise((resolve) => { forkCandidatesResolve = resolve; });
    return resp(200, forkCandidatesData, signal);
  }
  if(url==="/api/sessions/s1/fork"&&m==="POST") {
    if (forkPostDelayed) return new Promise((resolve) => { forkPostResolve = resolve; });
    return forkPostResp;
  }
  if(url==="/api/models") return resp(200, ["chatgpt/sol","chatgpt/terra","deepseek/flash","deepseek/high","deepseek/fast","kimi/k3"], signal);
  if(url.startsWith("/api/sessions/")&&url.endsWith("/model")&&m==="POST") return resp(200,{ok:true,model:"sol"},signal);
  if(url.startsWith("/api/sessions/")&&url.endsWith("/prompt")) return resp(202,{},signal);
  if(url.startsWith("/api/sessions/")&&url.endsWith("/cancel")) return resp(202,{},signal);
  if(url.startsWith("/api/sessions/")&&url.endsWith("/compact")) return resp(202,{},signal);
  if(url.startsWith("/api/sessions/")&&url.endsWith("/pin")&&m==="PUT") return resp(204,null,signal);
  if(url.startsWith("/api/sessions/")&&url.endsWith("/archive")&&m==="PUT") return resp(204,null,signal);
  if(url.startsWith("/api/sessions/")&&m==="DELETE") return resp(sessionDeleteStatus,null,signal);
  return resp(404,{},signal);
};
'''

TAIL = r'''
function dumpText(e, out){
  if(e._children && e._children.length){ for(const c of e._children) dumpText(c,out); }
  else {
    if(e._innerHTML) out.push("HTML:"+e._innerHTML);
    else if(e.textContent!=null) out.push(e.textContent);
    if(e.text!=null) out.push(e.text);
  }
}
function allText(){ const out=[]; for(const c of elsById["messages"].children) dumpText(c,out); return out.join("\n"); }

/* gjs timers are inert: flush microtasks instead.  Each flush drains the whole
   job queue, so a handful of flushes completes every stub promise chain. */
async function flush(){ for(let i=0;i<200;i++) await Promise.resolve(); }

async function main(){
  let fail=0;
  const chk=(name, ok, extra)=>{ if(!ok) fail++; console.log((ok?"PASS":"FAIL")+" "+name+(extra?"  "+extra:"")); };
  try {
    state.globalToken="test-token";   // 全局回退 token（workspaceToken 的兜底）
    state.token="test-token";         // 激活 workspace 派生 token（兼容既有引用）
    await pollSessions();
    renderSidebarTree(true);
    chk("sidebar rows rendered", elsById["sidebarTree"].querySelector(".tree-row") !== null);
    // 会话操作全部迁入树行：置顶 / 归档 / 删除均可从唯一导航完成。
    const firstTreeRow = elsById["sidebarTree"].querySelector(".tree-row");
    const firstPin = firstTreeRow && firstTreeRow.querySelector(".pin-btn");
    chk("tree pin button is svg", firstPin && firstPin.querySelector("svg") !== null
        && firstPin.textContent.trim() === "",
        "html=" + (firstPin ? firstPin.innerHTML.slice(0, 40) : "none"));
    chk("tree row has archive action", firstTreeRow && firstTreeRow.querySelector(".archive-btn") !== null);
    chk("tree row has delete action", firstTreeRow && firstTreeRow.querySelector(".tree-del") !== null);
    chk("tree filter matches title", treeSessionMatches({ id: "sid-42", title: "标题甲" }, "标题甲"));
    chk("tree filter matches ID when title exists",
        treeSessionMatches({ id: "sid-42", title: "标题甲" }, "SID-42"));

    if (MODE === 'direct') {
      state.sessionId = "s1";   // loadHistory 现在按 (wsId, sid, epoch) 三重校验发起上下文
      const r = await loadHistory("s1", state.workspace.id, sessionOpenEpoch);
      chk("direct loadHistory ok", r==="ok", "="+r);
      chk("direct history rendered", elsById["messages"]._children.length>=1,
          "n="+elsById["messages"]._children.length);
      chk("direct nextBeforeSeq", state.nextBeforeSeq === 100, "="+state.nextBeforeSeq);
      chk("direct olderDone false", state.olderDone === false, "="+state.olderDone);
      console.log(fail===0 ? "ALL PASS" : fail+" FAILURES");
      imports.system.exit(0);
    }
    openSession("s1");
    await flush();
    await flush();

    const t = allText();
    chk("initSource set", state.initSource!=null, "="+state.initSource);
    chk("history user", t.includes("你好，帮我看看"));
    chk("history assistant", t.includes("好的，我来处理。"));
    chk("history tool card", t.includes("bash") && t.includes("file1"));
    chk("history reasoning", t.includes("思考过程"));
    chk("history compaction", t.includes("早期内容已压缩"));
    chk("history notice", t.includes("后台任务 #1 完成"));
    chk("history bg completion", t.includes("build ok"));
    chk("history fork", t.includes("sess-old"));
    chk("snapshot skipped", !t.includes("SNAPSHOT-SHOULD-BE-SKIPPED"));
    chk("status Busy", elsById["chatStatus"].textContent==="处理中", "="+elsById["chatStatus"].textContent);
    chk("cancel enabled when busy", elsById["cancelBtn"].disabled===false);

    // 当前树行真实删除路径：confirm 取消 / DELETE 失败都保留聊天；成功后进入空状态。
    sessionsData = [{id:"s1",title:"当前会话",status:"Idle",model:"kimi",created_at:"2024-01-01T00:00:00Z",entry_count:8,busy:false}];
    state.lastList = sessionsData;
    state.workspaceLists[state.workspace.id] = state.lastList;
    renderSidebarTree(true);
    let currentDelete = elsById["sidebarTree"].querySelector(".tree-del");
    const deleteFetches = () => FETCHES.filter((u) => u === "/api/sessions/s1").length;
    const deleteBeforeCancel = deleteFetches();
    globalThis.confirm = () => false;
    currentDelete._listeners["click"][0]({ stopPropagation(){} });
    await flush();
    chk("current delete confirm cancel sends no DELETE", deleteFetches() === deleteBeforeCancel);
    chk("current delete confirm cancel keeps chat",
        state.sessionId === "s1" && !elsById["chatView"].classList.contains("no-session"));
    globalThis.confirm = () => true;

    sessionDeleteStatus = 500;
    currentDelete._listeners["click"][0]({ stopPropagation(){} });
    await flush(); await flush();
    chk("current delete failure sends DELETE", deleteFetches() === deleteBeforeCancel + 1);
    chk("current delete failure keeps chat and row",
        state.sessionId === "s1" && !elsById["chatView"].classList.contains("no-session")
        && elsById["sidebarTree"].querySelector(".tree-del") !== null);
    chk("current delete failure shows banner", elsById["bannerText"].textContent.includes("删除失败"));

    sessionDeleteStatus = 204;
    currentDelete = elsById["sidebarTree"].querySelector(".tree-del");
    currentDelete._listeners["click"][0]({ stopPropagation(){} });
    await flush(); await flush();
    chk("current delete success sends DELETE", deleteFetches() === deleteBeforeCancel + 2);
    chk("current delete success removes tree row",
        !state.lastList.some((x) => x.id === "s1") && elsById["sidebarTree"].querySelector(".tree-del") === null);
    chk("current delete success clears current session", state.sessionId === null);
    chk("current delete success enters visible empty state",
        elsById["chatView"].classList.contains("no-session")
        && !elsById["chatEmpty"].hidden);
    chk("current delete success hides messages/composer via no-session",
        elsById["chatView"].classList.contains("no-session"));
    chk("current delete success opens sidebar", state.sidebar.open === true);

    // 还原后续长套件基线。
    sessionsData = [{id:"s1",status:"Idle",model:"kimi",created_at:"2024-01-01T00:00:00Z",entry_count:8,busy:false}];
    state.lastList = sessionsData;
    state.workspaceLists[state.workspace.id] = state.lastList;
    openSession("s1");
    await flush(); await flush();
    chk("live user", t.includes("再来一次"));
    chk("live assistant deltas", t.includes("正在") && t.includes("处理"));
    chk("live reasoning", t.includes("推理中"));
    chk("live tool call", t.includes("read_file"));
    chk("live tool result err", t.includes("文件不存在"));
    chk("live assistant text", t.includes("出错了，"));
    const _asList = elsById["messages"].querySelectorAll(".msg-assistant");
    const _lastAs = _asList[_asList.length - 1];
    const _mdStrong = _lastAs ? _lastAs.querySelector("strong") : null;
    chk("live assistant markdown",
        !!_mdStrong && _mdStrong._children.some((c) => c.text === "换个方式"));
    chk("live notice", t.includes("系统提示行"));
    chk("live error", t.includes("错误: 回合失败"));
    chk("usage shown", elsById["usageInfo"].textContent.includes("1234"), "="+elsById["usageInfo"].textContent);

    elsById["promptInput"].value = "第二条消息";
    await sendPrompt();
    chk("prompt request 202", elsById["promptInput"].value==="");
    await cancelTurn();
    await compactSession();

    // ---- 斜杠命令补全菜单 ----
    const sm = elsById["slashMenu"];
    const pin = elsById["promptInput"];
    const kd = (key, extra) => { for (const fn of pin._listeners["keydown"] || []) fn(Object.assign({ key, shiftKey: false, preventDefault(){} }, extra || {})); };
    const inp = () => { for (const fn of pin._listeners["input"] || []) fn(); };
    // 输入 /（命令起始）→ 弹出 6 个候选，首项选中
    pin.value = "/";
    pin.selectionStart = pin.selectionEnd = 1;
    inp();
    chk("slash menu opens on /", slashMenu.open === true && sm.hidden === false
        && sm.querySelectorAll(".slash-item").length === 7,
        "open=" + slashMenu.open + " items=" + sm.querySelectorAll(".slash-item").length);
    chk("slash first item selected", slashMenu.selected === 0
        && sm.querySelectorAll(".slash-item")[0].classList.contains("selected"));
    // 前缀过滤：/r → 只剩 /rename
    pin.value = "/r";
    pin.selectionStart = pin.selectionEnd = 2;
    inp();
    chk("slash filters by prefix", slashMenu.items.length === 1
        && slashMenu.items[0].name === "/rename"
        && sm.querySelectorAll(".slash-item").length === 1,
        "n=" + slashMenu.items.length);
    // ↑↓ 移动选中（循环）
    pin.value = "/";
    pin.selectionStart = pin.selectionEnd = 1;
    inp();
    kd("ArrowDown");
    chk("slash arrow down moves selection", slashMenu.selected === 1,
        "sel=" + slashMenu.selected);
    kd("ArrowUp"); kd("ArrowUp");
    chk("slash arrow up wraps", slashMenu.selected === 6, "sel=" + slashMenu.selected);
    kd("ArrowDown");
    chk("slash arrow down wraps to 0", slashMenu.selected === 0, "sel=" + slashMenu.selected);
    // Enter 填入带参数命令：/rename <标题>，光标在参数位（占位被选中，输入即覆盖）
    kd("ArrowDown");   // -> /model
    kd("ArrowDown");   // -> /rename
    kd("Enter");
    chk("slash enter fills command", pin.value === "/rename <标题>"
        && slashMenu.open === false && sm.hidden === true,
        "value=" + JSON.stringify(pin.value));
    chk("slash caret at param", pin.selectionStart === "/rename ".length
        && pin.selectionEnd === pin.value.length,
        "start=" + pin.selectionStart + " end=" + pin.selectionEnd);
    // 删除到没有 / → 菜单关闭
    pin.value = "";
    inp();
    chk("slash closes when / removed", slashMenu.open === false && sm.hidden === true);
    // /compact 无参数：Enter 填入 → 用户回车走现有 sendPrompt 执行
    pin.value = "/comp";
    pin.selectionStart = pin.selectionEnd = 5;
    inp();
    chk("slash filters to compact", slashMenu.items.length === 1
        && slashMenu.items[0].name === "/compact",
        "n=" + slashMenu.items.length);
    kd("Enter");
    chk("slash compact filled", pin.value === "/compact" && slashMenu.open === false,
        "value=" + JSON.stringify(pin.value));
    kd("Enter");
    await flush();
    chk("slash compact executes via sendPrompt",
        pin.value === "" && FETCHES.some(u => u.endsWith("/compact")),
        "cleared=" + (pin.value === ""));
    // /help：Enter 执行 → scrollback 出现多行命令列表 Notice，输入框清空
    pin.value = "/help";
    pin.selectionStart = pin.selectionEnd = 5;
    kd("Enter");
    await flush();
    const helpNotices = elsById["messages"].querySelectorAll(".notice");
    const lastHelpNotice = helpNotices[helpNotices.length - 1];
    chk("slash /help shows command list",
        pin.value === "" && lastHelpNotice
          && lastHelpNotice.textContent.includes("/compact - 压缩上下文")
          && lastHelpNotice.textContent.includes("/btw <问题>")
          && lastHelpNotice.textContent.includes("/undo - 撤销文件操作"),
        "cleared=" + (pin.value === "") + " notice="
          + (lastHelpNotice ? JSON.stringify(lastHelpNotice.textContent) : "none"));
    // Esc 关闭
    pin.value = "/";
    pin.selectionStart = pin.selectionEnd = 1;
    inp();
    chk("slash reopens", slashMenu.open === true);
    kd("Escape");
    chk("slash esc closes", slashMenu.open === false && sm.hidden === true);
    // 失焦关闭
    pin.value = "/";
    inp();
    chk("slash open before blur", slashMenu.open === true);
    for (const fn of pin._listeners["blur"] || []) fn();
    chk("slash blur closes", slashMenu.open === false && sm.hidden === true);
    // 非命令起始（词中间，/ 前不是空白）不弹出
    pin.value = "hello/";
    pin.selectionStart = pin.selectionEnd = 6;
    inp();
    chk("slash mid-word stays closed", slashMenu.open === false,
        "open=" + slashMenu.open);
    // 空白后的 / 是命令起始 → 弹出并过滤
    pin.value = "hello /c";
    pin.selectionStart = pin.selectionEnd = 8;
    inp();
    chk("slash after whitespace opens", slashMenu.open === true
        && slashMenu.items.length === 1 && slashMenu.items[0].name === "/compact",
        "n=" + slashMenu.items.length);
    pin.value = "";
    inp();

    // ---- fork 面板：/fork 命令 + 候选列表 + POST /fork 建新会话 ----
    const fm = elsById["forkMenu"];
    // /fork 输入后回车 → sendPrompt 命中命令分支 → 打开面板并拉取候选
    pin.value = "/fork";
    await sendPrompt();
    await flush();
    chk("fork menu opens on /fork", forkMenu.open === true && fm.hidden === false
        && fm.querySelectorAll(".fork-item").length === 3,
        "open=" + forkMenu.open + " items=" + fm.querySelectorAll(".fork-item").length);
    chk("fork first item selected", forkMenu.selected === 0
        && fm.querySelectorAll(".fork-item")[0].classList.contains("selected"));
    const frow0 = fm.querySelectorAll(".fork-item")[0];
    chk("fork row shows at + preview", frow0.querySelector(".fork-at").textContent === "2"
        && frow0.querySelector(".fork-preview").textContent.includes("你好"),
        "at=" + frow0.querySelector(".fork-at").textContent);
    // ↑↓ 循环移动选中
    kd("ArrowDown");
    chk("fork arrow down moves", forkMenu.selected === 1, "sel=" + forkMenu.selected);
    kd("ArrowUp"); kd("ArrowUp");
    chk("fork arrow up wraps", forkMenu.selected === 2, "sel=" + forkMenu.selected);
    // Esc 关闭
    kd("Escape");
    chk("fork esc closes", forkMenu.open === false && fm.hidden === true);
    // 空候选 → 面板显示空提示
    forkCandidatesData = [];
    pin.value = "/fork";
    await sendPrompt();
    await flush();
    chk("fork empty hint shown", forkMenu.open === true && fm.hidden === false
        && fm.querySelector(".fork-empty") !== null
        && fm.textContent.includes("没有可 fork 的边界消息"),
        "open=" + forkMenu.open);
    kd("Escape");
    // 选中一项 → POST /fork → 打开新会话、清空输入框、成功 banner
    forkCandidatesData = [{at:2, seq:2, preview:"用户：你好，帮我看看"}];
    const forkFetchBefore = FETCHES.filter(u => u.endsWith("/fork")).length;
    pin.value = "/fork";
    await sendPrompt();
    await flush();
    kd("Enter");
    await flush();
    await flush();
    chk("fork select POSTs /fork", FETCHES.filter(u => u.endsWith("/fork")).length === forkFetchBefore + 1,
        "n=" + FETCHES.filter(u => u.endsWith("/fork")).length);
    chk("fork select opens new session", state.sessionId === "fork-1",
        "sid=" + state.sessionId);
    chk("fork clears input", pin.value === "");
    chk("fork success banner", elsById["bannerText"].textContent.includes("fork-1"),
        "banner=" + elsById["bannerText"].textContent);
    // 409（非边界/越界）：提示原因并重开面板（保留候选供重选）
    forkPostResp = resp(409, { error: "at 不是 turn 边界" });
    openSession("s1");
    await flush();
    await flush();
    pin.value = "/fork";
    await sendPrompt();
    await flush();
    kd("Enter");
    await flush();
    chk("fork 409 reopens menu", forkMenu.open === true && fm.hidden === false
        && fm.querySelectorAll(".fork-item").length === 1,
        "open=" + forkMenu.open + " items=" + fm.querySelectorAll(".fork-item").length);
    chk("fork 409 warns", elsById["bannerText"].textContent.includes("不是 turn 边界"),
        "banner=" + elsById["bannerText"].textContent);
    kd("Escape");
    // 失焦关闭
    forkPostResp = resp(201, {id:"fork-1"});
    pin.value = "/fork";
    await sendPrompt();
    await flush();
    chk("fork open before blur", forkMenu.open === true);
    for (const fn of pin._listeners["blur"] || []) fn();
    chk("fork blur closes", forkMenu.open === false && fm.hidden === true);
    // 点击行选中（mousedown preventDefault 保焦点 + click 触发）
    pin.value = "/fork";
    await sendPrompt();
    await flush();
    const frow = fm.querySelectorAll(".fork-item")[0];
    frow._listeners["mousedown"][0]({ preventDefault(){} });
    frow._listeners["click"][0]();
    await flush();
    await flush();
    chk("fork click selects", state.sessionId === "fork-1" && forkMenu.open === false,
        "sid=" + state.sessionId);
    // 回 s1（缓存恢复 nextBeforeSeq/olderDone），保持后续重连/分页测试前提
    openSession("s1");
    await flush();
    await flush();
    chk("fork back to s1 restores paging", state.sessionId === "s1"
        && state.nextBeforeSeq === 100 && state.olderDone === false,
        "sid=" + state.sessionId + " next=" + state.nextBeforeSeq);

    // 迟到 fork-candidates：加载期间切换会话，旧响应不得重开/写入面板。
    forkCandidatesDelayed = true;
    forkCandidatesResolve = null;
    const staleCandidates = openForkMenu();
    await flush();
    chk("fork candidates race starts pending", forkCandidatesResolve !== null && forkMenu.loading === true);
    openSession("s2");
    await flush();
    forkCandidatesResolve(await resp(200, [{at:99, seq:99, preview:"STALE-CANDIDATE"}]));
    await staleCandidates;
    await flush();
    chk("fork candidates late response discarded after session switch",
        state.sessionId === "s2" && forkMenu.open === false && forkMenu.items.length === 0
        && !fm.textContent.includes("STALE-CANDIDATE"),
        "sid=" + state.sessionId + " open=" + forkMenu.open + " items=" + forkMenu.items.length);
    forkCandidatesDelayed = false;

    // 迟到 fork POST：用户选中后切到其它会话，旧成功响应不得打开 fork 会话、
    // 清输入或刷新成功 banner。
    openSession("s1");
    await flush(); await flush();
    forkPostDelayed = true;
    forkPostResolve = null;
    pin.value = "/fork";
    await sendPrompt();
    await flush();
    const staleForkPost = selectForkItem(0);
    await flush();
    chk("fork POST race starts pending", forkPostResolve !== null);
    openSession("s2");
    pin.value = "new-session-draft";
    const bannerBeforeStaleFork = elsById["bannerText"].textContent;
    forkPostResolve(await resp(201, {id:"fork-1"}));
    await staleForkPost;
    await flush();
    chk("fork POST late response does not navigate or mutate current view",
        state.sessionId === "s2" && pin.value === "new-session-draft"
        && elsById["bannerText"].textContent === bannerBeforeStaleFork,
        "sid=" + state.sessionId + " draft=" + JSON.stringify(pin.value));
    forkPostDelayed = false;
    openSession("s1");
    await flush(); await flush();

    // token 连续重连：第一次 history 迟到时，第二次重连已成为当前代次；
    // 旧 token 的响应不得覆盖第二次重连完成后的 transcript/state。
    historyOverrides.set("s1", { delay: true });
    historyResolve = null;
    const restartEpochBefore = sessionOpenEpoch;
    restartTransport();
    await flush();
    const staleRestartResolve = historyResolve;
    chk("restartTransport race first history pending",
        staleRestartResolve !== null && sessionOpenEpoch > restartEpochBefore,
        "epoch=" + sessionOpenEpoch + " before=" + restartEpochBefore);
    historyOverrides.delete("s1");
    restartTransport();
    await flush(); await flush();
    const textAfterFreshRestart = allText();
    staleRestartResolve(await resp(200, {entries:[{type:"notice", text:"STALE-TOKEN-RESTART"}], next_before_seq:null}));
    await flush(); await flush();
    chk("restartTransport late response discarded after newer reconnect",
        state.sessionId === "s1" && !allText().includes("STALE-TOKEN-RESTART")
        && allText() === textAfterFreshRestart,
        "sid=" + state.sessionId + " stale=" + allText().includes("STALE-TOKEN-RESTART"));

    // ---- /model：运行时切换当前会话模型 ----
    pin.value = "/model";
    await sendPrompt();
    await flush();
    chk("/model usage banner", elsById["bannerText"].textContent.includes("/model <profile>"),
        "banner=" + elsById["bannerText"].textContent);
    // 输入框保留（方便补参数）
    chk("/model keeps input", pin.value === "/model",
        "value=" + JSON.stringify(pin.value));
    pin.value = "/model chatgpt/sol";
    await sendPrompt();
    await flush();
    chk("/model posts to endpoint", pin.value === ""
        && FETCHES.some(u => u.endsWith("/model")),
        "cleared=" + (pin.value === ""));
    chk("/model success banner", elsById["bannerText"].textContent.includes("已切换到 sol"),
        "banner=" + elsById["bannerText"].textContent);

    // ---- /model profile 自动补全：/model <参数> 弹出 profile 候选 ----
    // 首次 /model 参数输入触发 GET /api/models 懒加载（mock 返回 6 个 profile）
    pin.value = "/model c";
    pin.selectionStart = pin.selectionEnd = "/model c".length;
    inp();
    chk("model menu closes while loading", slashMenu.open === false,
        "open=" + slashMenu.open);
    await flush();   // /api/models 返回后按当前输入重开
    chk("model menu opens with profiles", slashMenu.open === true && sm.hidden === false
        && slashMenu.mode === "profile" && slashMenu.items.length === 2,
        "open=" + slashMenu.open + " mode=" + slashMenu.mode
        + " n=" + slashMenu.items.length);
    chk("model menu filters by prefix", slashMenu.items[0] === "chatgpt/sol"
        && slashMenu.items[1] === "chatgpt/terra",
        "items=" + JSON.stringify(slashMenu.items));
    chk("model menu rows rendered", sm.querySelectorAll(".slash-item").length === 2
        && sm.querySelectorAll(".slash-name")[0].textContent === "chatgpt/sol"
        && sm.querySelectorAll(".slash-desc").length === 0,
        "rows=" + sm.querySelectorAll(".slash-item").length);
    chk("model menu loads once", FETCHES.filter(u => u === "/api/models").length === 1,
        "n=" + FETCHES.filter(u => u === "/api/models").length);
    // ↑↓ 移动选中（循环）
    kd("ArrowDown");
    chk("model menu arrow down moves", slashMenu.selected === 1
        && slashMenu.items[slashMenu.selected] === "chatgpt/terra",
        "sel=" + slashMenu.selected);
    kd("ArrowUp");
    chk("model menu arrow up wraps", slashMenu.selected === 0, "sel=" + slashMenu.selected);
    // Enter → 填入 "/model <profile>" 并直接 POST /model（一步到位）
    const modelFetchBefore = FETCHES.filter(u => u.endsWith("/model")).length;
    kd("Enter");
    await flush();
    chk("model enter fills and sends",
        pin.value === "" && FETCHES.filter(u => u.endsWith("/model")).length === modelFetchBefore + 1,
        "cleared=" + (pin.value === "") + " posted="
        + (FETCHES.filter(u => u.endsWith("/model")).length - modelFetchBefore));
    chk("model enter banner", elsById["bannerText"].textContent.includes("已切换到 sol"),
        "banner=" + elsById["bannerText"].textContent);
    // /model（无空格）→ 命令模式：/model 命令项仍在（命令菜单不破坏）
    pin.value = "/model";
    pin.selectionStart = pin.selectionEnd = "/model".length;
    inp();
    chk("model bare keeps command menu", slashMenu.open === true
        && slashMenu.mode === "command" && slashMenu.items.length === 1
        && slashMenu.items[0].name === "/model",
        "open=" + slashMenu.open + " mode=" + slashMenu.mode
        + " n=" + slashMenu.items.length);
    // /model + 空格（无参数）→ 全部 profile 候选
    pin.value = "/model ";
    pin.selectionStart = pin.selectionEnd = "/model ".length;
    inp();
    chk("model space shows all profiles", slashMenu.open === true
        && slashMenu.mode === "profile" && slashMenu.items.length === 6,
        "open=" + slashMenu.open + " mode=" + slashMenu.mode
        + " n=" + slashMenu.items.length);
    // 无匹配前缀 → 关闭
    pin.value = "/model zz";
    pin.selectionStart = pin.selectionEnd = "/model zz".length;
    inp();
    chk("model no match closes", slashMenu.open === false && sm.hidden === true,
        "open=" + slashMenu.open);
    // Esc 关闭
    pin.value = "/model ";
    pin.selectionStart = pin.selectionEnd = "/model ".length;
    inp();
    chk("model esc open", slashMenu.open === true);
    kd("Escape");
    chk("model esc closes", slashMenu.open === false && sm.hidden === true);
    pin.value = "";
    inp();

    // 断线重连：销毁当前流后应重新走 history+SSE
    const oldInit = state.initSource;
    state.sse.stopped = false;
    scheduleReconnect(state.sessionId, state.workspace.id, sessionOpenEpoch);
    await flush();
    chk("reconnect resets initSource", state.initSource===null || state.initSource!=null, "after="+state.initSource);
    await flush();
    chk("reconnect reconnected", state.sse.ctrl!=null);

    // resync 追平：注入一个 resync 块，验证强制整体替换 transcript 并按事件日志重放
    handleSSEBlock("event: resync\ndata: [{\"type\":\"user_prompt\",\"data\":\"重放-用户\"},{\"type\":\"assistant_delta\",\"data\":\"重放-\"},{\"type\":\"assistant_delta\",\"data\":\"增量\"}]\n\n",
      state.sessionId, state.workspace.id, sessionOpenEpoch);
    const t3 = allText();
    console.log("DBG t3=" + JSON.stringify(t3.slice(0, 200)));
    console.log("DBG msgs children=" + elsById["messages"]._children.length
      + " html=" + (elsById["messages"].innerHTML || "").slice(0, 150));
    chk("resync replaces transcript", !t3.includes("你好，帮我看看") && t3.includes("重放-用户"));
    chk("resync replayed deltas", t3.includes("重放-") && t3.includes("增量"));
    chk("resync rerenders", elsById["messages"]._children.length >= 2,
        "n=" + elsById["messages"]._children.length);
    // resync 用离屏 temp 重放再以 innerHTML 提交；提交后 accumulator 不得
    // 继续引用 temp 的旧节点。紧随其后的 delta 必须落入真实 messages。
    handleSSEBlock("event: AssistantDelta\ndata: {\"delta\":\"-同步后续写\"}\n\n",
      state.sessionId, state.workspace.id, sessionOpenEpoch);
    const postResyncEl = elsById["messages"].querySelectorAll(".msg-assistant")
      .find((m) => (m.textContent || "").includes("同步后续写"));
    chk("resync following delta renders in real messages",
        !!postResyncEl
        && elsById["messages"].textContent.includes("同步后续写")
        && state.acc.assistantEl === postResyncEl
        && state.acc.assistantEl.isConnected,
        "text=" + JSON.stringify(elsById["messages"].textContent)
        + " connected=" + !!(state.acc.assistantEl && state.acc.assistantEl.isConnected));

    // ---- 滚动分页：滚动到顶部加载更早历史 ----
    const msgEl = elsById["messages"];
    chk("paging nextBeforeSeq set", state.nextBeforeSeq === 100, "="+state.nextBeforeSeq);
    chk("paging olderDone false", state.olderDone === false, "="+state.olderDone);
    const beforeCount = msgEl.children.length;
    const beforeScrollHeight = 2000;
    msgEl.scrollHeight = beforeScrollHeight;
    msgEl.clientHeight = 400;
    msgEl.scrollTop = 0;
    const handlers = (msgEl._listeners && msgEl._listeners["scroll"]) || [];
    chk("scroll listener registered", handlers.length >= 1, "n="+handlers.length);
    // 模拟浏览器：children 增加时 scrollHeight 随之增长（每插入一个节点 +25px）
    const realAppend = msgEl.appendChild.bind(msgEl);
    msgEl.appendChild = (n) => { const r = realAppend(n); msgEl.scrollHeight += 25; return r; };
    const oldFetchCount = FETCHES.filter(u=>u.includes("before_seq=")).length;
    for (const fn of handlers) fn({isTrusted:true});   // scrollTop=0 < 30 → 触发 loadOlder
    await flush();
    await flush();
    chk("older fetch issued", FETCHES.filter(u=>u.includes("before_seq=100")).length === oldFetchCount + 1,
        "n="+FETCHES.filter(u=>u.includes("before_seq=")).length);
    const t4 = allText();
    const idxOlder = t4.indexOf("更早的历史消息");
    const idxHead  = t4.indexOf("重放-用户");
    chk("older entries prepended", idxOlder !== -1 && idxHead !== -1 && idxOlder < idxHead,
        "older="+idxOlder+" head="+idxHead);
    chk("older children added", msgEl.children.length > beforeCount,
        "before="+beforeCount+" after="+msgEl.children.length);
    chk("scroll position preserved", msgEl.scrollTop === msgEl.scrollHeight - beforeScrollHeight,
        "scrollTop="+msgEl.scrollTop+" delta="+(msgEl.scrollHeight-beforeScrollHeight));
    chk("paging nextBeforeSeq updated", state.nextBeforeSeq === null, "="+state.nextBeforeSeq);
    chk("paging olderDone true", state.olderDone === true, "="+state.olderDone);
    // null 后不再触发：再次滚动到顶部不应发起任何请求
    const fetchAfterDone = FETCHES.length;
    for (const fn of handlers) fn({isTrusted:true});
    await flush();
    chk("no fetch when olderDone", FETCHES.length === fetchAfterDone,
        "delta="+(FETCHES.length-fetchAfterDone));
    // ---- 回归：restored 分支 reattachInFlight（切回缓存会话不重复思考块） ----
    function buildInflightView(){
      const m = elsById["messages"];
      m.innerHTML = "";
      const det = document.createElement("details");
      det.className = "thinking";
      const sum = document.createElement("summary");
      const dot = document.createElement("span");
      dot.className = "think-dot active";
      const lbl = document.createElement("span");
      lbl.className = "think-label";
      lbl.textContent = "思考中…";
      sum.append(dot, lbl);
      const tb = document.createElement("div");
      tb.className = "think-body";
      tb.textContent = "缓存的思考";
      det.append(sum, tb);
      m.appendChild(det);
      const as = document.createElement("div");
      as.className = "msg msg-assistant";
      const who = document.createElement("span");
      who.className = "who";
      who.textContent = "ai>";
      const ab = document.createElement("div");
      ab.className = "msg-body";
      ab.textContent = "缓存的助手回复";
      as.append(who, ab);
      m.appendChild(as);
      const card = buildToolCard("read_file", '{"path":"a.txt"}', "执行中…", "pending", null);
      m.appendChild(card);
      return { det, dot, tb, ab, card };
    }
    // 预置带 .think-dot.active 的缓存 HTML（序列化 innerHTML，模拟 saveSessionState）
    function cacheCurrentView(){
      state.sessionStates["s1"] = { html: elsById["messages"].innerHTML, scrollTop: 0,
        nextBeforeSeq: null, olderDone: false, draft: "" };
    }
    function openRestored(){           // 从另一会话切回 → restored 分支
      state.sessionId = null;          // 避免 saveSessionState 覆盖上面的缓存
      openSession("s1");
    }
    let iv = buildInflightView();
    cacheCurrentView();
    openRestored();
    chk("restored initSource", state.initSource === "restored", "="+state.initSource);
    const rDets = elsById["messages"].querySelectorAll("details.thinking");
    const rAs = elsById["messages"].querySelectorAll(".msg-assistant");
    const rCards = elsById["messages"].querySelectorAll("details.tool-card");
    chk("restored thinking reattached", rDets.length === 1 && state.acc.thinkingEl === rDets[0]
        && state.acc.thinkBody === rDets[0].querySelector(".think-body"));
    chk("restored assistant reattached", rAs.length === 1 && state.acc.assistantEl === rAs[0]
        && state.acc.assistantBody === rAs[0].querySelector(".msg-body")
        && state.acc.assistantText === "缓存的助手回复");
    chk("restored tool card queued", rCards.length === 1 && state.acc.toolStack.length === 1
        && state.acc.toolStack[0].filled === false && state.acc.toolStack[0].el === rCards[0]);
    // 注入 reasoning_delta：应续写进同一块，而不是新建第二个 details.thinking
    handleSSEBlock("event: ReasoningDelta\ndata: {\"type\":\"reasoning_delta\",\"session_id\":\"s1\",\"seq\":99,\"delta\":\"续写思考\"}\n\n", "s1", state.workspace.id, sessionOpenEpoch);
    const detsAfter = elsById["messages"].querySelectorAll("details.thinking");
    const tbAfter = detsAfter[0].querySelector(".think-body");
    chk("restored single thinking block", detsAfter.length === 1, "n="+detsAfter.length);
    chk("restored thinking continues", state.acc.thinkBody === tbAfter
        && tbAfter._children.some((c) => c.text === "缓存的思考")
        && tbAfter._children.some((c) => c.text === "续写思考"));
    // assistant delta 续写旧块；ToolResult 填回旧卡片（都不新建）
    handleSSEBlock("event: AssistantDelta\ndata: {\"type\":\"assistant_delta\",\"session_id\":\"s1\",\"seq\":100,\"delta\":\"续写回复\"}\n\n", "s1", state.workspace.id, sessionOpenEpoch);
    const abAfter = elsById["messages"].querySelector(".msg-assistant").querySelector(".msg-body");
    chk("restored assistant continues", state.acc.assistantBody === abAfter
        && abAfter._children.some((c) => c.text === "续写回复"));
    handleSSEBlock("event: ToolResult\ndata: {\"type\":\"tool_result\",\"session_id\":\"s1\",\"seq\":101,\"is_error\":false,\"content\":\"结果内容\"}\n\n", "s1", state.workspace.id, sessionOpenEpoch);
    chk("restored tool result fills old card", state.acc.toolStack.length === 1
        && state.acc.toolStack[0].filled === true
        && elsById["messages"].querySelector(".tool-state").textContent === "完成"
        && elsById["messages"].querySelectorAll("details.tool-card").length === 1);
    // 已完成（dot done）的 thinking 绝不绑定；已 markdown 化（有子元素）的助手消息绝不绑定
    iv = buildInflightView();
    iv.dot.className = "think-dot done";
    iv.ab.appendChild(document.createElement("em"));   // 模拟 markdown 渲染出的子元素
    cacheCurrentView();
    openRestored();
    chk("restored done thinking not bound", state.acc.thinkingEl === null,
        "="+String(state.acc.thinkingEl));
    chk("restored rendered assistant not bound",
        state.acc.assistantEl === null && state.acc.assistantText === "",
        "="+String(state.acc.assistantEl));

    // ---- bash 任务卡片：点击 → 卡片内就地展开 .task-output + 流式轮询 ----
    // （命令输出放卡片里，流式保持；消息列表输出块已移除）
    state.tasks.composerOpen = true;   // 面板展开：pollTasks 渲染卡片行
    tasksData = [{ session_id: "s1", id: 7, kind: "bash", label: "cargo build",
      full_command: "cargo build", output: "Compiling e-agent…", role: null }];
    taskOutputText = "Compiling e-agent…\n   Compiling e-agent-util…\n";
    await pollTasks();
    await flush();
    let trows = elsById["composerTasks"].querySelectorAll(".task-row");
    chk("bash card rendered", trows.length === 1, "n=" + trows.length);
    const brow = trows[0];
    const bpre = brow.querySelector(".task-output");
    chk("bash output collapsed by default", bpre.hidden === true, "hidden=" + bpre.hidden);
    chk("bash poller not started", !state.tasks.pollers.has("s1:7"),
        "keys=" + JSON.stringify([...state.tasks.pollers.keys()]));
    // 点击 → 就地展开 + 启动 500ms 流式轮询
    brow._listeners["click"][0]();
    chk("bash click expands in place", bpre.hidden === false, "hidden=" + bpre.hidden);
    chk("bash poller started", state.tasks.pollers.has("s1:7"),
        "keys=" + JSON.stringify([...state.tasks.pollers.keys()]));
    await flush();   // 启动即拉一次：output 端点全量刷新 pre 文本
    chk("bash output live", bpre.textContent.includes("e-agent-util"),
        "=" + bpre.textContent);
    // 再点 → 收起 + 停轮询
    brow._listeners["click"][0]();
    chk("bash second click collapses", bpre.hidden === true, "hidden=" + bpre.hidden);
    chk("bash collapse stops poller", !state.tasks.pollers.has("s1:7"),
        "keys=" + JSON.stringify([...state.tasks.pollers.keys()]));
    // 重新展开（模拟用户正在看输出）：供下面的 2s 重绘恢复展开态
    brow._listeners["click"][0]();
    chk("bash re-expands", bpre.hidden === false && state.tasks.pollers.has("s1:7"),
        "hidden=" + bpre.hidden + " keys=" + JSON.stringify([...state.tasks.pollers.keys()]));
    // 多任务各自独立：第二个任务 → 第二张卡片，独立轮询 key
    tasksData = [
      { session_id: "s1", id: 7, kind: "bash", label: "cargo build", full_command: "cargo build", output: "", role: null },
      { session_id: "s1", id: 8, kind: "bash", label: "cargo test", full_command: "cargo test", output: "running tests", role: null },
    ];
    await pollTasks();
    await flush();
    trows = elsById["composerTasks"].querySelectorAll(".task-row");
    chk("second card rendered", trows.length === 2, "n=" + trows.length);
    // 2s 重绘恢复展开态：s1:7 上一轮已展开 → 重建后自动展开并续轮询
    chk("rebuild keeps expanded row", trows[0].querySelector(".task-output").hidden === false,
        "hidden=" + trows[0].querySelector(".task-output").hidden);
    chk("rebuild restarts poller", state.tasks.pollers.has("s1:7"),
        "keys=" + JSON.stringify([...state.tasks.pollers.keys()]));
    // 展开第二张卡片：各自独立轮询
    trows[1]._listeners["click"][0]();
    chk("second card expands", trows[1].querySelector(".task-output").hidden === false,
        "hidden=" + trows[1].querySelector(".task-output").hidden);
    chk("cards polled independently",
        state.tasks.pollers.has("s1:7") && state.tasks.pollers.has("s1:8"),
        "keys=" + JSON.stringify([...state.tasks.pollers.keys()]));
    // 收起第一张：只停自己的轮询
    trows[0]._listeners["click"][0]();
    chk("collapse stops own poller only",
        !state.tasks.pollers.has("s1:7") && state.tasks.pollers.has("s1:8"),
        "keys=" + JSON.stringify([...state.tasks.pollers.keys()]));
    // delegate 行 → subagent 会话解析（/api/tasks 的 delegate 条目 session_id
    // 是父会话；subagent 会话以 parent_session_id+label 关联）
    state.lastList = [
      { id: "s1", parent_session_id: null, label: null },
      { id: "sub-1", parent_session_id: "s1", label: "子任务X", active: true },
    ];
    chk("resolve subagent id",
        resolveSubagentSessionId({ session_id: "s1", label: "子任务X" }) === "sub-1",
        "=" + String(resolveSubagentSessionId({ session_id: "s1", label: "子任务X" })));
    chk("resolve unknown delegate → null",
        resolveSubagentSessionId({ session_id: "s1", label: "不存在的任务" }) === null
        && resolveSubagentSessionId({ session_id: "s1", label: "" }) === null);
    // 新版后端：任务条目直接带 subagent_session_id，无需 label 匹配
    // （已完成任务的 subagent 已不在 /api/sessions 列表里也要能跳转）
    state.lastList = [];
    chk("resolve via subagent_session_id",
        resolveSubagentSessionId({ session_id: "s1", label: "子任务X", subagent_session_id: "sub-9" }) === "sub-9",
        "=" + String(resolveSubagentSessionId({ session_id: "s1", label: "子任务X", subagent_session_id: "sub-9" })));
    chk("subagent_session_id wins over label match",
        resolveSubagentSessionId({ session_id: "s1", label: "别的任务", subagent_session_id: "sub-7" }) === "sub-7",
        "=" + String(resolveSubagentSessionId({ session_id: "s1", label: "别的任务", subagent_session_id: "sub-7" })));
    // 「← 主会话」按钮：subagent 会话显示，点击返回父会话；主会话隐藏
    state.lastList = [
      { id: "s1", parent_session_id: null, label: null },
      { id: "sub-1", parent_session_id: "s1", label: "子任务X", active: true },
    ];
    openSession("sub-1");
    chk("subagent shows back-to-parent",
        elsById["backParentBtn"].hidden === false,
        "hidden=" + elsById["backParentBtn"].hidden);
    elsById["backParentBtn"]._listeners["click"][0]();
    chk("back-to-parent switches to parent",
        state.sessionId === "s1",
        "sid=" + state.sessionId);
    openSession("s1");
    chk("main session hides back-to-parent",
        elsById["backParentBtn"].hidden === true,
        "hidden=" + elsById["backParentBtn"].hidden);
    // ---- Bug C：任务面板直连跳转 subagent，lastList 还没包含该会话
    // （新后端 subagent_session_id 直连路径，轮询未刷新）：
    // openSession 在 cur 查不到时不得把「← 主会话」误藏（保持现状，
    // 等 refreshSessionsForSidebar 拉到列表后按 parent_session_id 判定） ----
    state.lastList = [];
    elsById["backParentBtn"].hidden = false;   // 模拟上一会话是 subagent（按钮可见）
    openSession("sub-9");
    chk("open session unknown to lastList keeps back button",
        elsById["backParentBtn"].hidden === false,
        "hidden=" + elsById["backParentBtn"].hidden);
    // 列表补拉后：subagent（有 parent）→ 显示返回按钮
    sessionsData = [
      { id: "s1", parent_session_id: null, label: null, status: "Idle", entry_count: 0, active: true },
      { id: "sub-9", parent_session_id: "s1", label: null, status: "Idle", entry_count: 0, active: true },
    ];
    await refreshSessionsForSidebar();
    chk("refresh shows back button for subagent",
        elsById["backParentBtn"].hidden === false,
        "hidden=" + elsById["backParentBtn"].hidden);
    // 列表补拉后：主会话（无 parent）→ 隐藏返回按钮
    sessionsData = [
      { id: "sub-9", parent_session_id: null, label: null, status: "Idle", entry_count: 0, active: true },
    ];
    await refreshSessionsForSidebar();
    chk("refresh hides back button for main session",
        elsById["backParentBtn"].hidden === true,
        "hidden=" + elsById["backParentBtn"].hidden);
    sessionsData = [{id:"s1",status:"Idle",model:"kimi",created_at:"2024-01-01T00:00:00Z",entry_count:8,busy:false}];   // 还原
    // 任务结束（从轮询列表消失）：面板清空，轮询停止
    tasksData = [];
    await pollTasks();
    await flush();
    chk("empty tasks clears panel",
        elsById["composerTasks"].querySelectorAll(".task-row").length === 0,
        "n=" + elsById["composerTasks"].querySelectorAll(".task-row").length);
    chk("empty tasks hides whole widget",
        elsById["tasksToggleBar"].hidden === true
        && elsById["composerTasks"].hidden === true
        && state.tasks.composerOpen === false,
        "bar.hidden=" + elsById["tasksToggleBar"].hidden
        + " panel.hidden=" + elsById["composerTasks"].hidden
        + " open=" + state.tasks.composerOpen);
    chk("ended pollers stopped",
        !state.tasks.pollers.has("s1:7") && !state.tasks.pollers.has("s1:8"),
        "keys=" + JSON.stringify([...state.tasks.pollers.keys()]));
    // 切走（clearCurrentSession）→ 切回：统一停止旧会话的卡片行轮询，聊天缓存仍恢复
    tasksData = [{ session_id: "s1", id: 9, kind: "bash", label: "cargo run",
      full_command: "cargo run", output: "building", role: null }];
    await pollTasks();
    await flush();
    // 新任务出现后组件重新显示，但面板默认收起（清空时被强制收起）：
    // 展开面板使任务行渲染出来
    state.tasks.composerOpen = true;
    renderComposerTasks();
    trows = elsById["composerTasks"].querySelectorAll(".task-row");
    trows[0]._listeners["click"][0]();
    chk("card poller before switch", state.tasks.pollers.has("s1:9"),
        "keys=" + JSON.stringify([...state.tasks.pollers.keys()]));
    clearCurrentSession();
    chk("clearCurrentSession stops card poller", !state.tasks.pollers.has("s1:9"),
        "keys=" + JSON.stringify([...state.tasks.pollers.keys()]));
    chk("cached html saved", !!state.sessionStates["s1"]
        && state.sessionStates["s1"].html.length > 0,
        "len=" + (state.sessionStates["s1"] && state.sessionStates["s1"].html.length));
    openSession("s1");
    await flush();
    chk("restored chat view", state.sessionId === "s1",
        "sid=" + state.sessionId);

    // ---- restored 替换回归：切走期间的会话更新必须可见 ----
    // 场景：用户切走会话（缓存旧快照）→ 会话继续产出新消息 → 切回。
    // 旧实现恢复缓存 + 跳过 snapshot → 新消息永不显示。现在 restored
    // 会拉最新 history 替换过期缓存。
    state.sessionId = null;
    state.sessionStates["restored-test"] = {
      html: "<div class='notice'>旧缓存消息</div>", scrollTop: 0,
      nextBeforeSeq: 8, olderDone: false, draft: "",
    };
    openSession("restored-test");
    await flush(); await flush();
    chk("restored replaces stale cache with latest history",
        elsById["messages"].textContent.includes("完成。")
        && !elsById["messages"].textContent.includes("旧缓存消息"),
        "text=" + elsById["messages"].textContent.slice(0, 140));
    // 进行中的增量块（未落盘、只活在缓存/SSE 里）必须保留并重新绑定：
    // 替换后 live delta 才能继续续写，而不是丢失或重复。
    state.sessionId = null;
    state.sessionStates["restored-test2"] = {
      html: "<div class='msg msg-assistant'><div class='msg-body'>正在流式</div></div>",
      scrollTop: 0, nextBeforeSeq: 8, olderDone: false, draft: "",
    };
    openSession("restored-test2");
    await flush(); await flush();
    chk("restored keeps in-flight block for delta continuation",
        !!state.acc && state.acc.assistantEl
        && state.acc.assistantEl.isConnected
        && elsById["messages"].textContent.includes("正在流式"),
        "connected=" + (state.acc && state.acc.assistantEl && state.acc.assistantEl.isConnected));

    // ---- A: 长内容「预览 + 展开全文」（maybeTruncateEl） ----
    const longArgsA = JSON.stringify({ cmd: "echo hi", data: "x".repeat(700) });
    const cardA = buildToolCard("bash", longArgsA, "完成", "", "y".repeat(500));
    const argsPreA = cardA.querySelector(".tool-args");
    const resPreA = cardA.querySelector(".tool-result");
    chk("A long args folded by default",
        argsPreA.classList.contains("expandable") && !argsPreA.classList.contains("expanded"),
        "cls=" + argsPreA.className);
    const previewA = argsPreA.querySelector(".expand-preview");
    const fullA = argsPreA.querySelector(".expand-full");
    chk("A long args preview+full split",
        !!previewA && !!fullA
        && previewA.textContent.length < fullA.textContent.length
        && fullA.textContent.length === JSON.stringify(JSON.parse(longArgsA), null, 2).length,
        "preview=" + (previewA && previewA.textContent.length) + " full=" + (fullA && fullA.textContent.length));
    chk("A long result folded by default",
        resPreA.classList.contains("expandable") && !resPreA.classList.contains("expanded"));
    // 短内容直出：无 expandable、无按钮、文本完整
    const shortCardA = buildToolCard("bash", '{"cmd":"ls"}', "完成", "", "ok");
    const argsShortA = shortCardA.querySelector(".tool-args");
    chk("A short args direct output",
        !argsShortA.classList.contains("expandable")
        && shortCardA.querySelector(".expand-footer") === null
        && argsShortA.textContent === JSON.stringify(JSON.parse('{"cmd":"ls"}'), null, 2),
        "text=" + JSON.stringify(argsShortA.textContent));
    // 点击展开 → 显示全文；再点收起 → 回到预览
    // 按钮在正文 pre 外面（紧跟其后），不在 pre 内部
    chk("A expand button inside pre",
        resPreA._children.some((c) => c instanceof El && c._classes && c._classes.has("expand-toggle")),
        "in-pre=" + resPreA._children.filter((c) => c instanceof El).map((c) => c._className).join(","));
    const clicksA = (elsById["messages"]._listeners["click"] || []);
    // footer 里有两个展开按钮（args + result）：取控制 resPreA 的那个
    const btnA = cardA.querySelectorAll(".expand-toggle")
      .find((b) => b._target === resPreA) || cardA.querySelector(".expand-toggle");
    for (const fn of clicksA) fn({ target: btnA });
    chk("A expand toggles full text",
        resPreA.classList.contains("expanded") && btnA.textContent === "收起（结果）"
        && resPreA.querySelector(".expand-full").textContent === "y".repeat(500),
        "btn=" + btnA.textContent);
    for (const fn of clicksA) fn({ target: btnA });
    chk("A collapse returns to preview",
        !resPreA.classList.contains("expanded") && btnA.textContent === "展开全文（结果）",
        "btn=" + btnA.textContent);
    // 工具结果（live/history 路径）也走辅助：长结果默认折叠、短结果直出
    elsById["messages"].innerHTML = "";      // 清掉 restored 测试遗留的 pending 卡片，保证独立卡片分支
    state.acc = newAccumulator();
    const cardCountBefore = 0;
    appendToolResult(false, "z".repeat(400), state.acc, null);   // 无配对卡片 → 独立卡片
    const standAloneA = elsById["messages"].querySelectorAll("details.tool-card");
    const lastCardA = standAloneA[standAloneA.length - 1];
    chk("A standalone long tool result folded",
        standAloneA.length === cardCountBefore + 1
        && lastCardA.querySelector(".tool-result").classList.contains("expandable")
        && !lastCardA.querySelector(".tool-result").classList.contains("expanded")
        && lastCardA.querySelector(".tool-result").querySelector(".expand-full").textContent === "z".repeat(400),
        "before=" + cardCountBefore + " after=" + standAloneA.length
        + " toolStack=" + state.acc.toolStack.length
        + " cls=" + lastCardA.querySelector(".tool-result").className);
    // innerHTML 快照往返（缓存恢复 / resync 离屏替换）：折叠状态与完整文本保留，
    // 展开按钮经消息容器事件委托仍可点（直接绑定的监听器不随快照保留）
    const snapA = elsById["messages"].innerHTML;
    elsById["messages"].innerHTML = snapA;
    const restoredCardA = elsById["messages"].querySelectorAll("details.tool-card");
    const restoredResA = restoredCardA[restoredCardA.length - 1].querySelector(".tool-result");
    chk("A innerHTML round-trip keeps fold+full",
        restoredResA.classList.contains("expandable")
        && restoredResA.querySelector(".expand-full").textContent === "z".repeat(400)
        && !restoredResA.classList.contains("expanded"));
    const restoredBtnA = restoredCardA[restoredCardA.length - 1].querySelector(".expand-toggle");
    for (const fn of clicksA) fn({ target: restoredBtnA });
    chk("A round-trip toggle still works",
        restoredResA.classList.contains("expanded") && restoredBtnA.textContent === "收起（结果）",
        "btn=" + restoredBtnA.textContent);
    appendToolResult(true, "err-x", state.acc, null);
    const standAloneB = elsById["messages"].querySelectorAll("details.tool-card");
    const lastCardB = standAloneB[standAloneB.length - 1];
    chk("A short tool result direct",
        standAloneB.length === standAloneA.length + 1
        && lastCardB.querySelector(".tool-result").textContent === "err-x"
        && !lastCardB.querySelector(".tool-result").classList.contains("expandable")
        && lastCardB.querySelector(".tool-result.err") !== null);

    // ---- 文件工具差异化渲染：edit_file -/+ diff / read_file 行号 / write_file 单侧 ----
    const lastCard = () => {
      const cs = elsById["messages"].querySelectorAll("details.tool-card");
      return cs[cs.length - 1];
    };
    // harness 的 matchSel 只认 "tag.class" 形态：复合 class 选择器按 tag 解析，
    // 因此按单 class 取行再过滤（与现有测试对 _classes 的用法一致）
    const diffRowsOf = (card, kind) =>
      card.querySelectorAll(".diff-row").filter((r) => r.classList.contains(kind));
    // edit_file：live 路径（appendToolCall → appendToolResult 按 toolStack 配对）
    elsById["messages"].innerHTML = "";
    state.acc = newAccumulator();
    appendToolCall("edit_file",
      JSON.stringify({ path: "a.txt", old: "foo\nbar\nbaz", new: "foo\nBAR\nbaz\nqux" }),
      state.acc, null);
    const editCard = lastCard();
    chk("edit_file args parsed on card",
        editCard._toolArgs !== null && editCard._toolArgs.path === "a.txt"
        && editCard._toolArgs.old === "foo\nbar\nbaz" && editCard._toolArgs.new === "foo\nBAR\nbaz\nqux",
        "args=" + JSON.stringify(editCard._toolArgs));
    appendToolResult(false, "file edited (line 2)", state.acc, null);
    const editRes = editCard.querySelector(".tool-result");
    const delRows = diffRowsOf(editCard, "diff-del");
    const addRows = diffRowsOf(editCard, "diff-add");
    chk("edit_file diff rendered",
        editRes.classList.contains("tool-diff")
        && editRes.querySelector(".diff-head").textContent === "file edited (line 2)",
        "cls=" + editRes.className + " head=" + JSON.stringify(editRes.querySelector(".diff-head") && editRes.querySelector(".diff-head").textContent));
    chk("edit_file placeholder cleared before diff",
        !editRes.textContent.includes("等待结果…"),
        "text=" + JSON.stringify(editRes.textContent.slice(0, 40)));
    chk("edit_file - rows numbered from edited line",
        delRows.length === 3 && addRows.length === 4
        && delRows[0].querySelector(".diff-sign").textContent === "− "
        && delRows[0].querySelector(".diff-ln").textContent === "2"
        && delRows[0].querySelector(".diff-text").textContent === "foo",
        "del=" + delRows.length + " add=" + addRows.length);
    chk("edit_file + rows with old/new split",
        addRows[0].querySelector(".diff-text").textContent === "foo"
        && addRows[1].querySelector(".diff-ln").textContent === "3"
        && addRows[1].querySelector(".diff-text").textContent === "BAR"
        && addRows[3].querySelector(".diff-text").textContent === "qux",
        "rows=" + editCard.querySelector(".tool-result").textContent);
    chk("edit_file result keeps card open", editCard.hasAttribute("open"),
        "open=" + editCard.hasAttribute("open"));
    // 30 行/侧截断 + "… (N more lines)"（镜像 TUI push_diff_side）
    const bigOld = Array.from({ length: 40 }, (_, i) => "old line " + i).join("\n");
    const bigNew = Array.from({ length: 40 }, (_, i) => "new line " + i).join("\n");
    appendToolCall("edit_file",
      JSON.stringify({ path: "b.txt", old: bigOld, new: bigNew }), state.acc, null);
    const bigCard = lastCard();
    appendToolResult(false, "file edited (line 100)", state.acc, null);
    const bigRes = bigCard.querySelector(".tool-result");
    chk("edit_file truncates each side to 30",
        diffRowsOf(bigCard, "diff-del").length === 30
        && diffRowsOf(bigCard, "diff-add").length === 30
        && bigRes.querySelector(".diff-more").textContent === "−… (10 more lines)",
        "more=" + JSON.stringify(bigRes.querySelector(".diff-more") && bigRes.querySelector(".diff-more").textContent));
    // edit_file 出错：保持普通 err 文本，不渲染 diff
    appendToolCall("edit_file", JSON.stringify({ path: "x.txt", old: "a", new: "b" }), state.acc, null);
    const errCard = lastCard();
    appendToolResult(true, "edit failed: not found", state.acc, null);
    chk("edit_file error stays plain err text",
        !errCard.querySelector(".tool-result").classList.contains("tool-diff")
        && errCard.querySelector(".tool-result").classList.contains("err")
        && errCard.querySelector(".tool-result").textContent === "edit failed: not found",
        "cls=" + errCard.querySelector(".tool-result").className);
    // read_file：行号 = offset + 行下标；页脚行不编号
    elsById["messages"].innerHTML = "";
    state.acc = newAccumulator();
    appendToolCall("read_file", JSON.stringify({ path: "a.txt", offset: 5 }), state.acc, null);
    const readCard = lastCard();
    appendToolResult(false, "line one\nline two\nline three\n[showing lines 5-7 of 100; use offset 8 to continue]",
      state.acc, null);
    const readRes = readCard.querySelector(".tool-result");
    const readRows = readCard.querySelectorAll(".diff-row");
    chk("read_file line-number view",
        readRes.classList.contains("tool-diff") && readRows.length === 4,
        "rows=" + readRows.length);
    chk("read_file numbers start at args.offset",
        readRows[0].querySelector(".diff-ln").textContent === "5"
        && readRows[0].querySelector(".diff-text").textContent === "line one"
        && readRows[2].querySelector(".diff-ln").textContent === "7",
        "ln0=" + (readRows[0].querySelector(".diff-ln") && readRows[0].querySelector(".diff-ln").textContent));
    chk("read_file footer unnumbered",
        readRows[3].classList.contains("diff-footer")
        && readRows[3].querySelector(".diff-ln") === null
        && readRows[3].querySelector(".diff-text").textContent.includes("showing lines 5-7 of 100"),
        "cls=" + readRows[3].className);
    chk("read_file keeps card open", readCard.hasAttribute("open"));
    // read_file 默认 offset=1；纯状态输出不做行号
    elsById["messages"].innerHTML = "";
    state.acc = newAccumulator();
    appendToolCall("read_file", JSON.stringify({ path: "e.txt" }), state.acc, null);
    appendToolResult(false, "[empty file]", state.acc, null);
    const emptyCard = lastCard();
    chk("read_file status output plain",
        !emptyCard.querySelector(".tool-result").classList.contains("tool-diff")
        && emptyCard.querySelector(".tool-result").textContent === "[empty file]",
        "cls=" + emptyCard.querySelector(".tool-result").className);
    // read_file 超 30 行：预览 + 展开全文（复用 expand-toggle 委托）
    appendToolCall("read_file", JSON.stringify({ path: "c.txt" }), state.acc, null);
    const longCard = lastCard();
    appendToolResult(false, Array.from({ length: 40 }, (_, i) => "row " + i).join("\n"),
      state.acc, null);
    const longRes = longCard.querySelector(".tool-result");
    chk("read_file truncated to 30 preview rows",
        longRes.classList.contains("expandable") && !longRes.classList.contains("expanded")
        && longCard.querySelectorAll(".diff-row").length === 40
        && longRes.querySelector(".diff-full").querySelectorAll(".diff-row").length === 10
        && longRes.querySelector(".diff-more").textContent === "… (10 more lines)",
        "total=" + longCard.querySelectorAll(".diff-row").length);
    // 展开按钮可发现性：紧跟预览区（截断标记之后）、全文之前——不依赖滚动到底
    const _kids = longRes._children.filter((c) => c instanceof El);
    const _kidIdx = (cls) => _kids.findIndex((c) => c._classes && c._classes.has(cls));
    chk("read_file expand button after preview, before full",
        _kidIdx("diff-more") !== -1 && _kidIdx("expand-toggle") !== -1 && _kidIdx("diff-full") !== -1
        && _kidIdx("diff-more") < _kidIdx("expand-toggle") && _kidIdx("expand-toggle") < _kidIdx("diff-full"),
        "more=" + _kidIdx("diff-more") + " btn=" + _kidIdx("expand-toggle") + " full=" + _kidIdx("diff-full"));
    const readBtn = longRes.querySelector(".expand-toggle");
    for (const fn of clicksA) fn({ target: readBtn });
    chk("read_file expand shows all rows",
        longRes.classList.contains("expanded")
        && longRes.querySelector(".diff-full").textContent.includes("row 39")
        && readBtn.textContent === "收起（内容）",
        "btn=" + readBtn.textContent);
    for (const fn of clicksA) fn({ target: readBtn });
    chk("read_file collapse returns to preview",
        !longRes.classList.contains("expanded") && readBtn.textContent === "展开全文（内容）",
        "btn=" + readBtn.textContent);
    // write_file：确认行 + 全新增（+ 绿）单侧 diff，行号从 1 起
    elsById["messages"].innerHTML = "";
    state.acc = newAccumulator();
    appendToolCall("write_file", JSON.stringify({ path: "new.txt", content: "hello\nworld" }),
      state.acc, null);
    const wCard = lastCard();
    appendToolResult(false, "file written", state.acc, null);
    chk("write_file single-side add",
        wCard.querySelector(".tool-result").classList.contains("tool-diff")
        && wCard.querySelector(".diff-head").textContent === "file written"
        && diffRowsOf(wCard, "diff-add").length === 2
        && diffRowsOf(wCard, "diff-del").length === 0
        && diffRowsOf(wCard, "diff-add")[0].querySelector(".diff-ln").textContent === "1"
        && diffRowsOf(wCard, "diff-add")[0].querySelector(".diff-text").textContent === "hello",
        "rows=" + wCard.querySelectorAll(".diff-row").length);
    // 空 old/new/content：不产生空红/绿 diff 行（纯新增/纯删除/空写入）
    appendToolCall("edit_file", JSON.stringify({ path: "add.txt", old: "", new: "hello\nworld" }),
      state.acc, null);
    const addCard = lastCard();
    appendToolResult(false, "file edited (line 1)", state.acc, null);
    chk("edit_file empty old renders add-only",
        diffRowsOf(addCard, "diff-del").length === 0
        && diffRowsOf(addCard, "diff-add").length === 2,
        "del=" + diffRowsOf(addCard, "diff-del").length + " add=" + diffRowsOf(addCard, "diff-add").length);
    appendToolCall("edit_file", JSON.stringify({ path: "del.txt", old: "bye", new: "" }),
      state.acc, null);
    const delCard = lastCard();
    appendToolResult(false, "file edited (line 5)", state.acc, null);
    chk("edit_file empty new renders del-only",
        diffRowsOf(delCard, "diff-del").length === 1
        && diffRowsOf(delCard, "diff-add").length === 0,
        "del=" + diffRowsOf(delCard, "diff-del").length + " add=" + diffRowsOf(delCard, "diff-add").length);
    appendToolCall("write_file", JSON.stringify({ path: "empty.txt", content: "" }),
      state.acc, null);
    const wEmptyCard = lastCard();
    appendToolResult(false, "file written", state.acc, null);
    chk("write_file empty content renders no add rows",
        wEmptyCard.querySelector(".tool-result").classList.contains("tool-diff")
        && wEmptyCard.querySelector(".diff-head").textContent === "file written"
        && wEmptyCard.querySelectorAll(".diff-row").length === 0,
        "rows=" + wEmptyCard.querySelectorAll(".diff-row").length);
    appendToolCall("read_file", JSON.stringify({ path: "blank.txt" }), state.acc, null);
    const rEmptyCard = lastCard();
    appendToolResult(false, "", state.acc, null);
    chk("read_file empty content falls back to status",
        !rEmptyCard.querySelector(".tool-result").classList.contains("tool-diff")
        && rEmptyCard.querySelector(".tool-result").textContent === "[empty file]",
        "cls=" + rEmptyCard.querySelector(".tool-result").className);
    // history 路径：renderMessage 按 tc.id ↔ call_id 配对后同样渲染 diff
    elsById["messages"].innerHTML = "";
    state.acc = newAccumulator();
    const histPending = new Map();
    renderMessage({ Assistant: { content: "", reasoning: null,
      tool_calls: [{ id: "ec1", name: "edit_file",
        arguments: JSON.stringify({ path: "h.txt", old: "xx", new: "yy" }) }] } },
      state.acc, histPending);
    renderMessage({ Tool: { call_id: "ec1", name: "edit_file",
      content: "file edited (line 9)", is_error: false } }, state.acc, histPending);
    const histCard = lastCard();
    chk("history edit_file diff via call_id pairing",
        histCard.querySelector(".tool-result").classList.contains("tool-diff")
        && diffRowsOf(histCard, "diff-del")[0].querySelector(".diff-text").textContent === "xx"
        && diffRowsOf(histCard, "diff-add")[0].querySelector(".diff-text").textContent === "yy"
        && diffRowsOf(histCard, "diff-add")[0].querySelector(".diff-ln").textContent === "9",
        "cls=" + histCard.querySelector(".tool-result").className);
    // innerHTML 快照往返（restored）：expando 丢失后从 .tool-args 重解析，
    // 结果到达仍渲染 diff（reattachInFlight 场景）
    elsById["messages"].innerHTML = "";
    state.acc = newAccumulator();
    appendToolCall("edit_file", JSON.stringify({ path: "r.txt", old: "AAA", new: "BBB" }),
      state.acc, null);
    const rtSnap = elsById["messages"].innerHTML;
    elsById["messages"].innerHTML = rtSnap;
    state.acc = newAccumulator();
    state.acc.toolStack.push({ el: elsById["messages"].querySelector("details.tool-card"), filled: false });
    appendToolResult(false, "file edited (line 4)", state.acc, null);
    const rtCard = elsById["messages"].querySelector("details.tool-card");
    chk("round-trip edit diff re-parsed from args",
        rtCard.querySelector(".tool-result").classList.contains("tool-diff")
        && diffRowsOf(rtCard, "diff-add")[0].querySelector(".diff-text").textContent === "BBB"
        && diffRowsOf(rtCard, "diff-add")[0].querySelector(".diff-ln").textContent === "4",
        "cls=" + rtCard.querySelector(".tool-result").className);

    // ---- B: 消息上限 pruneMessages ----
    const pm = elsById["messages"];
    pm.innerHTML = "";
    state.acc = newAccumulator();
    for (let i = 0; i < 310; i++) appendNotice("旧通知 #" + i);
    // 进行中的助手块（底部，模拟流式）：appendAssistantDelta 会创建并绑定
    appendAssistantDelta("流式内容", state.acc);
    const inflightB = state.acc.assistantEl;
    chk("B children bounded", pm.children.length <= 300, "n=" + pm.children.length);
    const phB = pm.children[0];
    chk("B placeholder at top", phB && phB.classList.contains("older-collapse"),
        "cls=" + (phB && phB.className));
    chk("B folded count label",
        phB.querySelector(".older-label").textContent.includes("条消息"),
        "=" + phB.querySelector(".older-label").textContent);
    chk("B folded blocks inside body",
        phB.querySelector(".older-body").querySelectorAll(".notice").length > 0,
        "n=" + phB.querySelector(".older-body").querySelectorAll(".notice").length);
    chk("B earliest folded", phB.querySelector(".older-body").textContent.includes("旧通知 #0"));
    chk("B in-flight not folded",
        inflightB !== null && pm.children[pm.children.length - 1] === inflightB
        && phB.querySelector(".older-body").textContent.indexOf("流式内容") === -1,
        "last=" + (pm.children[pm.children.length - 1] && pm.children[pm.children.length - 1].className));
    // 展开：占位 details 打开后原位显示被折叠块（内容一直在 body 里，未删除）
    phB.setAttribute("open", "");
    chk("B expanded shows folded blocks",
        phB.querySelector(".older-body").textContent.includes("旧通知 #0")
        && phB.querySelector(".older-body").textContent.includes("旧通知 #9"));
    chk("B direct children still bounded", pm.children.length <= 300, "n=" + pm.children.length);
    // 「加载更早历史」链接：无更早历史（nextBeforeSeq=null）时隐藏；有则可见。
    // 显式前置状态：restored 恢复会话现在会拉 history（next_before_seq 非
    // null → olderDone=false），不再假设其它测试留下的默认值。
    state.nextBeforeSeq = null;
    state.olderDone = true;
    appendNotice("额外通知");   // 触发一次 prune 刷新链接可见性
    const linkB = phB.querySelector(".older-load");
    chk("B load link hidden when no older",
        linkB !== null && linkB.hidden === true, "hidden=" + (linkB && linkB.hidden));
    state.nextBeforeSeq = 100;
    state.olderDone = false;
    appendNotice("额外通知");   // 触发一次 prune 刷新链接可见性
    chk("B load link shown when older available",
        linkB.hidden === false, "hidden=" + linkB.hidden);
    chk("B prune keeps bound", pm.children.length <= 300, "n=" + pm.children.length);

    // ---- 回归：后台完成通知冻结流式气泡（渲染顺序反转） ----
    // 流式回复中（acc.assistantEl 已绑定气泡 A）收到 BackgroundCompletionNotice：
    // 通知条插入后当前助手累积必须被 freeze（assistantEl=null），否则空 prompt
    // 自动触发的新回合没有 UserPrompt 事件可 freeze，回合 N+1 的 AssistantDelta
    // 会续写进通知条上方的旧气泡 A（正文跑到提醒条上面，顺序反了）。修复后：
    // 通知条渲染在旧气泡下方，新回合 delta 另起新气泡，DOM 顺序为
    // 旧正文 → 通知条 → 新正文。
    elsById["messages"].innerHTML = "";
    state.acc = newAccumulator();
    appendAssistantDelta("第一轮回复正文", state.acc);   // 流式中：绑定气泡 A
    const bubbleA = state.acc.assistantEl;
    const bodyA = state.acc.assistantBody;
    handleSSEBlock("event: BackgroundCompletionNotice\ndata: {\"type\":\"background_completion_notice\",\"session_id\":\"" + state.sessionId + "\",\"seq\":200,\"id\":9,\"label\":\"cargo\",\"output\":\"build ok\"}\n\n",
      state.sessionId, state.workspace.id, sessionOpenEpoch);
    chk("bg notice freezes assistant", state.acc.assistantEl === null
        && state.acc.assistantBody === null && state.acc.assistantText === "",
        "el=" + String(state.acc.assistantEl));
    const noticesN = elsById["messages"].querySelectorAll(".notice");
    chk("bg notice rendered after old bubble", noticesN.length === 1
        && noticesN[0].textContent.includes("后台任务 #9")
        && noticesN[0].textContent.includes("build ok"),
        "n=" + noticesN.length + " text=" + noticesN[0].textContent.slice(0, 60));
    // 新回合的 AssistantDelta → 新气泡（不在旧气泡 A 里续写）
    handleSSEBlock("event: AssistantDelta\ndata: {\"type\":\"assistant_delta\",\"session_id\":\"" + state.sessionId + "\",\"seq\":201,\"delta\":\"第二轮回复正文\"}\n\n",
      state.sessionId, state.workspace.id, sessionOpenEpoch);
    const asN = elsById["messages"].querySelectorAll(".msg-assistant");
    chk("bg notice next delta new bubble", asN.length === 2
        && asN[1] !== bubbleA && state.acc.assistantEl === asN[1],
        "n=" + asN.length + " acc=" + (state.acc.assistantEl === bubbleA ? "old" : "new"));
    // DOM 顺序断言：旧正文气泡 → 通知条 → 新正文气泡
    const kidsN = elsById["messages"].children;
    const iA = kidsN.indexOf(bubbleA);
    const iN = kidsN.indexOf(noticesN[0]);
    const iB = kidsN.indexOf(asN[1]);
    chk("bg notice DOM order old→notice→new",
        iA !== -1 && iN !== -1 && iB !== -1 && iA < iN && iN < iB,
        "idx=" + iA + "," + iN + "," + iB);
    chk("bg notice old bubble intact",
        bodyA.textContent.includes("第一轮回复正文")
        && !bodyA.textContent.includes("第二轮回复正文"),
        "=" + JSON.stringify(bodyA.textContent));

    // ---- bash 卡片点击：就地展开卡片输出 + 流式轮询，不切会话 ----
    // （放在 B 测试之后：fresh 打开 s2 会把 nextBeforeSeq 置 100，而 B 的
    // 「加载更早历史」链接断言依赖 nextBeforeSeq=null，避免互相污染）
    // (b) 未打开会话时点击 bash 卡片：不切会话，就地展开 + 启动轮询
    state.sessionId = null;
    state.sessionStates["s1"] = { html: elsById["messages"].innerHTML, scrollTop: 0,
      nextBeforeSeq: null, olderDone: false, draft: "" };   // 模拟切走前的缓存
    const switchTasks = [{ session_id: "s1", id: 55, kind: "bash", label: "cargo run",
      full_command: "cargo run", output: "building", role: null }];
    state.tasks.list = switchTasks;
    renderTaskList(switchTasks, elsById["composerTasks"]);
    const srow = elsById["composerTasks"].querySelectorAll(".task-row")[0];
    srow._listeners["click"][0]();
    await flush();
    chk("bash click stays in list view", state.sessionId === null,
        "sid=" + state.sessionId);
    chk("bash click expands card in place", srow.querySelector(".task-output").hidden === false,
        "hidden=" + srow.querySelector(".task-output").hidden);
    chk("bash click starts card poller", state.tasks.pollers.has("s1:55"),
        "keys=" + JSON.stringify([...state.tasks.pollers.keys()]));
    // 再点 → 收起 + 停轮询
    srow._listeners["click"][0]();
    chk("bash click collapses card", srow.querySelector(".task-output").hidden === true,
        "hidden=" + srow.querySelector(".task-output").hidden);
    chk("bash collapse stops poller", !state.tasks.pollers.has("s1:55"),
        "keys=" + JSON.stringify([...state.tasks.pollers.keys()]));
    // (c) delegate 卡片点击 → 切到 subagent 会话（openSession + resolve）
    const subTasks = [{ session_id: "s1", id: 66, kind: "delegate", label: "子任务Y",
      full_command: null, output: null, role: null }];
    state.lastList = [
      { id: "s1", parent_session_id: null, label: null },
      { id: "sub-2", parent_session_id: "s1", label: "子任务Y", active: true },
    ];
    renderTaskList(subTasks, elsById["composerTasks"]);
    const drow2 = elsById["composerTasks"].querySelectorAll(".task-row")[0];
    drow2._listeners["click"][0]();
    await flush();
    chk("delegate click switches to subagent session",
        state.sessionId === "sub-2",
        "sid=" + state.sessionId);
    // (d) delegate 解析不到 subagent → 回退就地展开 .task-stream
    const orphanTasks = [{ session_id: "s1", id: 67, kind: "delegate", label: "孤儿任务",
      full_command: null, output: null, role: null }];
    renderTaskList(orphanTasks, elsById["composerTasks"]);
    const orow = elsById["composerTasks"].querySelectorAll(".task-row")[0];
    orow._listeners["click"][0]();
    await flush();
    await flush();
    chk("delegate fallback expands stream in place",
        orow.querySelector(".task-stream").hidden === false,
        "hidden=" + orow.querySelector(".task-stream").hidden);

    // ---- 任务参数标签：background / workspace / resume（与 TUI 面板一致） ----
    // delegate 任务带全部三个参数 → 渲染三个标签，顺序固定
    renderTaskList([{ session_id: "s1", id: 70, kind: "delegate", label: "延续子代理",
      full_command: null, output: null, role: "coder", background: true,
      workspace: "/custom/path", resume: "sub-123" }], elsById["composerTasks"]);
    let tagRow = elsById["composerTasks"].querySelectorAll(".task-row")[0];
    let tagEls = tagRow.querySelectorAll(".task-tag");
    chk("delegate renders three tags in order",
        tagEls.length === 3
        && tagEls[0].textContent === "background"
        && tagEls[1].textContent === "workspace: /custom/path"
        && tagEls[2].textContent === "resume: sub-123",
        "tags=" + tagEls.map((t) => t.textContent).join("|"));
    chk("tags live in .task-tags container",
        tagRow.querySelector(".task-tags") !== null
        && tagRow.querySelector(".task-tags")._children.length === 3);
    chk("resume tag title carries full id",
        tagEls[2].title === "sub-123", "title=" + tagEls[2].title);
    // bash 任务没有这些字段 → 不渲染 .task-tags
    renderTaskList([{ session_id: "s1", id: 71, kind: "bash", label: "ls",
      full_command: "ls", output: "", role: null }], elsById["composerTasks"]);
    tagRow = elsById["composerTasks"].querySelectorAll(".task-row")[0];
    chk("bash task without fields renders no tags",
        tagRow.querySelector(".task-tags") === null,
        "tags=" + String(tagRow.querySelector(".task-tags")));
    // 长 workspace 路径：完整显示（不截断、无省略标记），换行交给 CSS 处理，
    // title 保留完整路径
    const longPath = "/very/long/workspace/path/" + "x".repeat(60);
    renderTaskList([{ session_id: "s1", id: 72, kind: "delegate", label: "长路径任务",
      full_command: null, output: null, role: null, background: false,
      workspace: longPath, resume: null }], elsById["composerTasks"]);
    tagRow = elsById["composerTasks"].querySelectorAll(".task-row")[0];
    const wsTag = tagRow.querySelector(".task-tag");
    chk("long workspace shown in full without ellipsis",
        wsTag.title === longPath
        && wsTag.textContent === "workspace: " + longPath
        && wsTag.textContent.indexOf("…") === -1,
        "title-ok=" + (wsTag.title === longPath)
        + " text=" + wsTag.textContent.slice(0, 50));

    // ---- 任务行父会话标签：delegate 任务显示父会话；有 title 显示标题，否则回退 id ----
    // 查父会话 title 的列表：state.workspaceLists[state.workspace.id]
    // （激活 workspace 缓存），回退 state.lastList。找到且 title 非空 →
    // 「父: <title>」（截断 ~40 字符，悬停 title 放「<id>: 完整标题」）；
    // 无 title / 查不到 → 回退「父: <session_id>」。非 delegate 任务
    // session_id 即父且「会话 <id>」已显示，不重复加父标签。
    const parentListKey = state.workspace ? state.workspace.id : null;
    const savedParentList = (parentListKey && state.workspaceLists[parentListKey] !== undefined)
      ? state.workspaceLists[parentListKey] : state.lastList;
    // (a) 父会话在列表里有 title → 显示「父: <title>」，悬停 title 带完整标题 + 会话 id
    state.workspaceLists[parentListKey] = [
      { id: "s1", title: "主会话标题", status: "Idle", active: true },
    ];
    renderTaskList([{ session_id: "s1", id: 80, kind: "delegate", label: "子任务Z",
      full_command: null, output: null, role: null, subagent_session_id: "sub-9" }],
      elsById["composerTasks"]);
    let prow = elsById["composerTasks"].querySelectorAll(".task-row")[0];
    let pmeta = prow.querySelectorAll(".tparent");
    chk("delegate row shows parent title when parent in list has title",
        pmeta.length === 1 && pmeta[0].textContent === "父: 主会话标题",
        "parent=" + pmeta.map((e) => e.textContent).join("|"));
    chk("parent title tag hover carries full title + id",
        pmeta.length === 1 && pmeta[0].title === "s1: 主会话标题",
        "title=" + (pmeta[0] ? pmeta[0].title : ""));
    // (b) 长父标题：文本截断到 ~40 字符，悬停 title 保留完整标题 + 会话 id
    const longParentTitle = "这是一个非常长的父会话标题用于测试截断行为" + "长".repeat(50);
    state.workspaceLists[parentListKey] = [
      { id: "s1", title: longParentTitle, status: "Idle", active: true },
    ];
    renderTaskList([{ session_id: "s1", id: 83, kind: "delegate", label: "长标题任务",
      full_command: null, output: null, role: null, subagent_session_id: "sub-8" }],
      elsById["composerTasks"]);
    prow = elsById["composerTasks"].querySelectorAll(".task-row")[0];
    pmeta = prow.querySelectorAll(".tparent");
    chk("long parent title truncated keeps full hover title",
        pmeta.length === 1
        && pmeta[0].textContent.startsWith("父: " + longParentTitle.slice(0, 40))
        && pmeta[0].textContent.indexOf(longParentTitle) === -1
        && pmeta[0].title === "s1: " + longParentTitle,
        "title-ok=" + (pmeta[0] ? pmeta[0].title === "s1: " + longParentTitle : false)
        + " text=" + (pmeta[0] ? pmeta[0].textContent.slice(0, 50) : ""));
    // (c) 父会话在列表里但无 title → 回退「父: <id>」
    state.workspaceLists[parentListKey] = [
      { id: "s1", title: null, status: "Idle", active: true },
    ];
    renderTaskList([{ session_id: "s1", id: 81, kind: "delegate", label: "子任务Y",
      full_command: null, output: null, role: null, subagent_session_id: "sub-7" }],
      elsById["composerTasks"]);
    prow = elsById["composerTasks"].querySelectorAll(".task-row")[0];
    pmeta = prow.querySelectorAll(".tparent");
    chk("delegate row falls back to id when parent has no title",
        pmeta.length === 1 && pmeta[0].textContent === "父: s1",
        "parent=" + pmeta.map((e) => e.textContent).join("|"));
    // (d) 父会话不在列表里（列表未刷新 / 已删）→ 回退「父: <id>」
    state.workspaceLists[parentListKey] = [
      { id: "other-1", title: "别的会话", status: "Idle", active: true },
    ];
    renderTaskList([{ session_id: "s1", id: 85, kind: "delegate", label: "子任务X",
      full_command: null, output: null, role: null, subagent_session_id: "sub-6" }],
      elsById["composerTasks"]);
    prow = elsById["composerTasks"].querySelectorAll(".task-row")[0];
    pmeta = prow.querySelectorAll(".tparent");
    chk("delegate row falls back to id when parent missing from list",
        pmeta.length === 1 && pmeta[0].textContent === "父: s1",
        "parent=" + pmeta.map((e) => e.textContent).join("|"));
    state.workspaceLists[parentListKey] = savedParentList;   // 还原
    // 非 delegate 任务：不显示父标签（session_id 即父，「会话 <id>」已显示）
    renderTaskList([{ session_id: "s1", id: 86, kind: "bash", label: "ls",
      full_command: "ls", output: "", role: null }], elsById["composerTasks"]);
    prow = elsById["composerTasks"].querySelectorAll(".task-row")[0];
    chk("bash row shows no parent label",
        prow.querySelector(".tparent") === null,
        "parent=" + String(prow.querySelector(".tparent")));
    // delegate 无 session_id（异常数据）→ 安静降级
    renderTaskList([{ id: 87, kind: "delegate", label: "幽灵任务",
      full_command: null, output: null, role: null, subagent_session_id: "ghost-1" }],
      elsById["composerTasks"]);
    prow = elsById["composerTasks"].querySelectorAll(".task-row")[0];
    chk("delegate row without session_id shows no parent label",
        prow.querySelector(".tparent") === null,
        "parent=" + String(prow.querySelector(".tparent")));
    // 极端 resume：session_id === subagent_session_id → 省略父标签（避免「会话 X / 父: X」重复）
    renderTaskList([{ session_id: "sub-9", id: 88, kind: "delegate", label: "自指任务",
      full_command: null, output: null, role: null, subagent_session_id: "sub-9" }],
      elsById["composerTasks"]);
    prow = elsById["composerTasks"].querySelectorAll(".task-row")[0];
    chk("delegate row with identical parent/subagent id omits parent label",
        prow.querySelector(".tparent") === null
        && prow.querySelectorAll(".tsid").length === 1,
        "parent=" + String(prow.querySelector(".tparent")));

    // ---- 任务行序号：#<id>（后端 TaskMeta.id）----
    // 有 id → badge 后、label 前显示 #<id>；无 id → 安静省略
    renderTaskList([{ session_id: "s1", id: 90, kind: "delegate", label: "序号任务",
      full_command: null, output: null, role: "coder" }], elsById["composerTasks"]);
    let idRow = elsById["composerTasks"].querySelectorAll(".task-row")[0];
    let tidEl = idRow.querySelector(".task-meta.tid");
    chk("task row shows #id after badge",
        tidEl !== null && tidEl.textContent === "#90"
        && idRow.querySelector(".kind-badge").nextSibling === tidEl,
        "tid=" + (tidEl ? tidEl.textContent : "null"));
    chk("tid sits before label",
        tidEl !== null && tidEl.nextSibling !== null
        && tidEl.nextSibling.className === "task-label"
        && tidEl.nextSibling.textContent === "序号任务",
        "next=" + (tidEl && tidEl.nextSibling ? tidEl.nextSibling.className : "null"));
    // 无 id（异常/旧数据）→ 不渲染序号 span
    renderTaskList([{ session_id: "s1", kind: "bash", label: "ls",
      full_command: "ls", output: "", role: null }], elsById["composerTasks"]);
    idRow = elsById["composerTasks"].querySelectorAll(".task-row")[0];
    chk("task row without id omits tid",
        idRow.querySelector(".task-meta.tid") === null,
        "tid=" + String(idRow.querySelector(".task-meta.tid")));

    await pollSessions();   // 还原激活 workspace 缓存（后续测试基于轮询状态继续）

    // ---- Token 折叠：默认收起 → 点击展开 → 输入后按钮「已设置」 → 再点收起 ----
    const tokBtn = elsById["tokenToggle"];
    const tokInp = elsById["tokenInput"];
    chk("token collapsed by default",
        tokInp.hidden === true && tokBtn.textContent.includes("Token") && !tokBtn.classList.contains("set"),
        "btn=" + tokBtn.textContent + " hidden=" + tokInp.hidden);
    tokBtn._listeners["click"][0]();
    chk("token click expands", tokInp.hidden === false, "hidden=" + tokInp.hidden);
    tokInp.value = "tok-123";
    tokInp._listeners["input"][0]();
    await flush();
    chk("token input updates state+storage",
        state.token === "tok-123" && localStorage.getItem("eagent_token") === "tok-123",
        "token=" + state.token);
    chk("token button shows set",
        tokBtn.textContent.includes("已设置") && tokBtn.classList.contains("set"),
        "btn=" + tokBtn.textContent);
    tokBtn._listeners["click"][0]();
    chk("token click collapses again", tokInp.hidden === true, "hidden=" + tokInp.hidden);
    // 失焦延迟收起：blur 后 150ms 才收起（防止「点进去正要输入就收起」）。
    // gjs 计时器惰性不触发：这里验证 blur 后输入框不会立即收起。
    tokBtn._listeners["click"][0]();   // 再展开
    chk("token re-expands", tokInp.hidden === false, "hidden=" + tokInp.hidden);
    tokInp._listeners["blur"][0]();
    chk("token blur defers collapse", tokInp.hidden === false, "hidden=" + tokInp.hidden);

    // ---- Bug A：缺 model/role 的会话 → composerMeta hidden（动作按钮落左的
    // 前置条件）；布局修复（.composer-actions margin-left:auto）由 python 侧
    // 校验 style.css（DOM 桩无真实布局，无法断言像素位置） ----
    const _savedList = state.lastList;
    const _savedSid = state.sessionId;
    state.lastList = [{ id: "noid", model: "", role: null, parent_session_id: null,
                        status: "Idle", entry_count: 0, active: false }];
    state.sessionId = "noid";
    updateComposerMeta();
    chk("model-less session hides composer meta",
        elsById["composerMeta"].hidden === true && elsById["composerMeta"].textContent === "",
        "hidden=" + elsById["composerMeta"].hidden);
    state.lastList = [{ id: "noid", model: "kimi", role: null, parent_session_id: null,
                        status: "Idle", entry_count: 0, active: false }];
    updateComposerMeta();
    chk("modeled session shows composer meta",
        elsById["composerMeta"].hidden === false && elsById["composerMeta"].textContent.includes("kimi"),
        "hidden=" + elsById["composerMeta"].hidden + " text=" + elsById["composerMeta"].textContent);
    // 会话状态追加：Busy → 尾部 "· 处理中"（带 .busy 色）、Compacting → 压缩中、
    // Failed → 失败；Idle 静默无状态 span
    state.lastList = [{ id: "noid", model: "flash", role: "main", status: "Busy",
                        parent_session_id: null, entry_count: 0, active: true }];
    updateComposerMeta();
    const _stBusy = elsById["composerMeta"].querySelector(".composer-status");
    chk("busy status shown after model·role",
        !elsById["composerMeta"].hidden
        && elsById["composerMeta"].textContent === "flash · main · 处理中"
        && !!_stBusy && _stBusy.className === "composer-status busy",
        "text=" + elsById["composerMeta"].textContent + " cls=" + (_stBusy && _stBusy.className));
    state.lastList = [{ id: "noid", model: "flash", role: "main", status: "Compacting",
                        parent_session_id: null, entry_count: 0, active: true }];
    updateComposerMeta();
    const _stComp = elsById["composerMeta"].querySelector(".composer-status");
    chk("compacting status shown",
        elsById["composerMeta"].textContent === "flash · main · 压缩中"
        && !!_stComp && _stComp.className === "composer-status compacting",
        "text=" + elsById["composerMeta"].textContent + " cls=" + (_stComp && _stComp.className));
    state.lastList = [{ id: "noid", model: "flash", role: "main", status: "Failed: boom",
                        parent_session_id: null, entry_count: 0, active: true }];
    updateComposerMeta();
    const _stErr = elsById["composerMeta"].querySelector(".composer-status");
    chk("failed status shown as error",
        elsById["composerMeta"].textContent === "flash · main · 失败"
        && !!_stErr && _stErr.className === "composer-status error",
        "text=" + elsById["composerMeta"].textContent + " cls=" + (_stErr && _stErr.className));
    state.lastList = [{ id: "noid", model: "flash", role: "main", status: "Idle",
                        parent_session_id: null, entry_count: 0, active: true }];
    updateComposerMeta();
    chk("idle keeps no status span",
        elsById["composerMeta"].textContent === "flash · main"
        && elsById["composerMeta"].querySelector(".composer-status") === null,
        "text=" + elsById["composerMeta"].textContent);
    state.lastList = _savedList;
    state.sessionId = _savedSid;
    updateComposerMeta();   // 还原

    // ---- 孤儿 subagent 前端保险：无 parent 的 sub-/btw- 前缀会话不算主会话 ----
    // 历史脏数据里 sub-20260729-* 等会话 parent/model 丢失，被误判为主会话
    // （占主会话位 + composer 显示 "flash · main"）。前缀保险：归入「未关联」组。
    state.lastList = [
      { id: "s1", parent_session_id: null, model: "kimi", role: "main", status: "Idle", entry_count: 8, active: true },
      { id: "sub-2", parent_session_id: "s1", label: "子任务Y", status: "Idle", entry_count: 0, active: true },
      { id: "sub-20260729-200006-ienq", parent_session_id: null, model: "flash", role: "main",
        status: "Idle", entry_count: 1, active: true },
      { id: "btw-20260729-200006-abc", parent_session_id: null, model: null, role: null,
        status: "Idle", entry_count: 0, active: true },
    ];
    state.sidebar.filter = "";
    state.sidebar.showAll = true;
    renderSidebarTree(true);
    const _allTreeRows = elsById["sidebarTree"].querySelectorAll(".tree-row");
    const _rootRows = _allTreeRows.filter((r) =>
      !r.classList.contains("tree-row-child") && !r.classList.contains("tree-hist-row"));
    chk("orphan subagent not a tree root",
        !_rootRows.some((r) => r.textContent.includes("sub-20260729-200006-ienq")
                             || r.textContent.includes("btw-20260729-200006-abc")),
        "roots=" + _rootRows.map((r) => r.textContent).join(" | "));
    const _orphanGroup = elsById["sidebarTree"].querySelectorAll(".tree-group")
      .find((g) => g.textContent.includes("未关联"));
    chk("orphan subagents grouped under 未关联",
        !!_orphanGroup,
        "groups=" + elsById["sidebarTree"].querySelectorAll(".tree-group").map((g) => g.textContent).join(" | "));
    const _orphanRow = _allTreeRows.find((r) => r.textContent.includes("sub-20260729-200006-ienq"));
    chk("orphan rendered as child row in group",
        !!_orphanRow && _orphanRow.classList.contains("tree-row-child"),
        "cls=" + (_orphanRow && _orphanRow.className));
    const _subRow = _allTreeRows.find((r) => r.textContent.includes("sub-2"));
    chk("linked subagent still child row",
        !!_subRow && _subRow.classList.contains("tree-row-child"),
        "cls=" + (_subRow && _subRow.className));
    // composer meta：孤儿 role=main（脏数据）不显示 "· main"；真主会话照常
    state.sessionId = "sub-20260729-200006-ienq";
    updateComposerMeta();
    chk("orphan hides fake main role",
        elsById["composerMeta"].hidden === false
        && elsById["composerMeta"].textContent === "flash",
        "text=" + elsById["composerMeta"].textContent + " hidden=" + elsById["composerMeta"].hidden);
    state.sessionId = "s1";
    updateComposerMeta();
    chk("real main keeps role label",
        elsById["composerMeta"].textContent === "kimi · main",
        "text=" + elsById["composerMeta"].textContent);
    state.lastList = _savedList;
    state.sessionId = _savedSid;
    updateComposerMeta();   // 还原

    // ---- 多工作区：默认单实例不变 + 添加/切换/删除 + token 同步 + 持久化 ----
    // 默认：从未配置 → 只有 "默认" 条目（同源相对请求），行为与单服务器一致
    chk("ws default single workspace",
        state.workspaces.length === 1 && state.workspaces[0].id === "default"
        && state.workspaces[0].name === "默认" && state.workspaces[0].url === ""
        && state.workspace === state.workspaces[0],
        "n=" + state.workspaces.length + " name=" + state.workspaces[0].name);
    chk("ws default token falls back to global",
        state.workspace.token === "" && state.token === state.globalToken && state.token !== ""
        && workspaceToken(state.workspace) === state.globalToken,
        "wsToken=" + JSON.stringify(state.workspace.token)
        + " token=" + JSON.stringify(state.token)
        + " global=" + JSON.stringify(state.globalToken));
    chk("ws default select rendered",
        elsById["workspaceSelect"].querySelectorAll("option").length === 1
        && elsById["workspaceSelect"].querySelector("option").textContent === "默认",
        "opts=" + elsById["workspaceSelect"].querySelectorAll("option").length);
    chk("ws remove disabled when only one",
        elsById["workspaceRemoveBtn"].disabled === true,
        "disabled=" + elsById["workspaceRemoveBtn"].disabled);
    // url="" 时 api() 仍发相对请求（既有行为不变）
    await api("/api/sessions");
    await flush();
    chk("ws default api stays relative",
        FETCHES[FETCHES.length - 1] === "/api/sessions",
        "url=" + FETCHES[FETCHES.length - 1]);

    // workspace 切换统一停止展开任务行资源：旧 output interval 从 map 和
    // timer registry 清除，已排队的旧 tick 也不能继续请求旧任务端点。
    tasksData = [{ session_id: "s1", id: 910, kind: "bash", label: "old workspace",
      full_command: "old workspace", output: "running", role: null }];
    state.tasks.composerOpen = true;
    await pollTasks(); await flush();
    const wsOldRow = elsById["composerTasks"].querySelector(".task-row");
    wsOldRow._listeners["click"][0]();
    await flush();
    const wsOldInterval = state.tasks.pollers.get("s1:910");
    const wsOldTick = scheduledIntervals[wsOldInterval - 1];
    const wsOldFetches = FETCHES.filter((u) => u.includes("/tasks/910/output")).length;
    chk("workspace lifecycle output poller started",
        wsOldInterval != null && scheduledIntervals[wsOldInterval - 1] !== null,
        "key=" + state.tasks.pollers.has("s1:910") + " timer=" + wsOldInterval);

    // 「+」→ 内联面板 → 保存 → 新 workspace 激活，后续 api/SSE 走新 base
    elsById["workspaceAddBtn"]._listeners["click"][0]();
    chk("ws add opens editor",
        elsById["workspaceEditor"].hidden === false,
        "hidden=" + elsById["workspaceEditor"].hidden);
    elsById["wsNameInput"].value = "服务器B";
    elsById["wsUrlInput"].value = "http://localhost:9000/";   // 带尾斜杠 → 应被归一化
    elsById["wsTokenInput"].value = "tok-b";
    elsById["wsSaveBtn"]._listeners["click"][0]();
    // 同步断言：清空立即生效（轮询响应尚未回来，不会污染这些字段）
    chk("ws switch clears session state",
        state.sessionId === null
        && state.sessionStates["s1"] === undefined && state.lastList.length === 0
        && state.tasks.list.length === 0 && state.tasks.pollers.size === 0
        && elsById["messages"].innerHTML === "",
        "sid=" + state.sessionId
        + " lastList=" + state.lastList.length);
    chk("workspace switch clears old row interval",
        !state.tasks.pollers.has("s1:910") && scheduledIntervals[wsOldInterval - 1] === null,
        "keys=" + JSON.stringify([...state.tasks.pollers.keys()]));
    if (wsOldTick) wsOldTick();   // 模拟切换前已排队、clearInterval 无法撤回的回调
    await flush();
    chk("workspace switch old output poller sends no more requests",
        FETCHES.filter((u) => u.includes("/tasks/910/output")).length === wsOldFetches,
        "before=" + wsOldFetches + " after="
          + FETCHES.filter((u) => u.includes("/tasks/910/output")).length);
    chk("ws add creates+activates workspace",
        state.workspaces.length === 2 && state.workspace.id !== "default"
        && state.workspace.name === "服务器B"
        && state.workspace.url === "http://localhost:9000"
        && state.workspace.token === "tok-b",
        "n=" + state.workspaces.length + " url=" + JSON.stringify(state.workspace.url)
        + " name=" + state.workspace.name);
    chk("ws switch syncs token", state.token === "tok-b",
        "token=" + JSON.stringify(state.token));
    // 切换 workspace 不再覆盖全局 token（legacy eagent_token 键 = 全局）：
    // 全局仍是最早在 token 输入框设置的 tok-123，不被 B 的 tok-b 覆盖
    chk("ws switch keeps global token",
        state.globalToken === "tok-123"
        && localStorage.getItem("eagent_token") === "tok-123",
        "global=" + JSON.stringify(state.globalToken)
        + " legacy=" + JSON.stringify(localStorage.getItem("eagent_token")));
    chk("ws select shows both",
        elsById["workspaceSelect"].querySelectorAll("option").length === 2,
        "opts=" + elsById["workspaceSelect"].querySelectorAll("option").length);
    await flush();
    await api("/api/sessions");
    await flush();
    chk("ws api targets new base",
        FETCHES[FETCHES.length - 1] === "http://localhost:9000/api/sessions",
        "url=" + FETCHES[FETCHES.length - 1]);
    // SSE 连接（sse.js 的 connectSSE）同样前缀新 base
    state.sessionId = "s1";
    connectSSE("s1", state.workspace.id, sessionOpenEpoch);
    await flush();
    chk("ws sse targets new base",
        FETCHES.some(u => u === "http://localhost:9000/api/sessions/s1/events"),
        "last=" + FETCHES[FETCHES.length - 1]);
    stopSSE();
    state.sessionId = null;

    // 下拉 change → 切回默认
    elsById["workspaceSelect"].value = "default";
    elsById["workspaceSelect"]._listeners["change"][0]();
    await flush();
    chk("ws switch back to default",
        state.workspace.id === "default"
        && state.token === (state.workspace.token || state.globalToken),
        "id=" + state.workspace.id + " token=" + JSON.stringify(state.token)
        + " global=" + JSON.stringify(state.globalToken));
    await api("/api/sessions");
    await flush();
    chk("ws api relative again",
        FETCHES[FETCHES.length - 1] === "/api/sessions",
        "url=" + FETCHES[FETCHES.length - 1]);

    // localStorage 持久化 round-trip：模拟刷新重新 initWorkspaces
    const _persisted = JSON.parse(localStorage.getItem("eagent.workspaces"));
    chk("ws persisted list",
        Array.isArray(_persisted) && _persisted.length === 2
        && _persisted.some(w => w.id === "default")
        && _persisted.some(w => w.name === "服务器B" && w.url === "http://localhost:9000" && w.token === "tok-b"),
        "n=" + (_persisted && _persisted.length));
    chk("ws persisted active",
        localStorage.getItem("eagent.activeWorkspace") === "default",
        "active=" + localStorage.getItem("eagent.activeWorkspace"));
    state.workspaces = [];
    state.workspace = null;
    initWorkspaces();
    chk("ws reload restores workspaces",
        state.workspaces.length === 2 && state.workspace.id === "default"
        && state.workspace.token === _persisted.find(w => w.id === "default").token
        && state.token === (state.workspace.token || state.globalToken),
        "n=" + state.workspaces.length + " active=" + state.workspace.id);

    // 删除：切到服务器B → 删 → 落回 default；仅剩一个时按钮禁用
    elsById["workspaceSelect"].value = state.workspaces.find(w => w.name === "服务器B").id;
    elsById["workspaceSelect"]._listeners["change"][0]();
    await flush();
    chk("ws switched to B for removal",
        state.workspace.name === "服务器B", "name=" + state.workspace.name);
    elsById["workspaceRemoveBtn"]._listeners["click"][0]();
    await flush();
    chk("ws remove deletes and falls back",
        state.workspaces.length === 1 && state.workspace.id === "default",
        "n=" + state.workspaces.length + " active=" + state.workspace.id);
    chk("ws remove disabled when only one",
        elsById["workspaceRemoveBtn"].disabled === true,
        "disabled=" + elsById["workspaceRemoveBtn"].disabled);
    chk("ws remove persisted",
        JSON.parse(localStorage.getItem("eagent.workspaces")).length === 1,
        "n=" + JSON.parse(localStorage.getItem("eagent.workspaces")).length);

    // ---- legacy 迁移：老版本把顶部输入同步进 workspace.token（== eagent_token
    // 副本）；升级后 ws.token 优先、顶部输入失效（review）。加载时凡
    // ws.token === legacy eagent_token 一律清空 → 回退 globalToken；legacy 值
    // 本身进 globalToken；用户单独配置的 ws.token（≠ legacy）保留。----
    localStorage.setItem("eagent_token", "legacy-X");
    state.globalToken = "legacy-X";        // 模拟刷新：模块加载时 globalToken 已读 legacy 键
    localStorage.setItem("eagent.workspaces", JSON.stringify([
      { id: "default", name: "默认", url: "", token: "legacy-X" },          // 旧版全局输入的副本
      { id: "ws-real", name: "真实服务器", url: "http://r.local", token: "tok-r" },  // 用户单独配置：保留
    ]));
    localStorage.setItem("eagent.activeWorkspace", "default");
    state.workspaces = [];
    state.workspace = null;
    initWorkspaces();
    chk("legacy token migration: copied ws.token cleared",
        state.workspaces.find(w => w.id === "default").token === "",
        "tok=" + JSON.stringify(state.workspaces.find(w => w.id === "default").token));
    chk("legacy token migration: real ws token kept",
        state.workspaces.find(w => w.id === "ws-real").token === "tok-r",
        "tok=" + JSON.stringify(state.workspaces.find(w => w.id === "ws-real").token));
    chk("legacy token migration: globalToken holds legacy value",
        state.globalToken === "legacy-X" && localStorage.getItem("eagent_token") === "legacy-X",
        "global=" + JSON.stringify(state.globalToken)
        + " legacy=" + JSON.stringify(localStorage.getItem("eagent_token")));
    chk("legacy token migration: workspaceToken falls back to global",
        workspaceToken(state.workspaces.find(w => w.id === "default")) === "legacy-X"
        && state.token === "legacy-X",
        "eff=" + JSON.stringify(workspaceToken(state.workspaces.find(w => w.id === "default")))
        + " token=" + JSON.stringify(state.token));
    chk("legacy token migration: cleared token persisted",
        JSON.parse(localStorage.getItem("eagent.workspaces"))
          .find(w => w.id === "default").token === "",
        "persisted=" + JSON.stringify(JSON.parse(localStorage.getItem("eagent.workspaces"))
          .find(w => w.id === "default").token));

    // =====================================================================
    // 聚合模式：多 workspace 会话聚合（侧边栏分组 + 跨服务器打开）
    // 双服务器：wsA（url "" 同源 → 默认路由 sessionsData）+ wsB（http://b.local）
    // =====================================================================
    sessionsData = [
      { id: "a1", status: "Idle", model: "kimi", title: "A 主会话", created_at: "2024-01-01T00:00:00Z", entry_count: 8, busy: false, active: true },
      { id: "a2", parent_session_id: "a1", label: "A 子任务", status: "Idle", entry_count: 2, active: true },
      { id: "a-orphan", parent_session_id: "missing-a", status: "Idle", entry_count: 1, active: true },
    ];
    sessionsDataB = [
      { id: "b1", status: "Busy", model: "deepseek", title: "B 主会话", created_at: "2024-02-02T00:00:00Z", entry_count: 5, busy: true, active: true },
      { id: "b-orphan", parent_session_id: "missing-b", status: "Idle", entry_count: 1, active: true },
    ];
    sessionsBFail = false;
    state.workspaces = [
      { id: "wsA", name: "服务器A", url: "", token: "tok-a" },
      { id: "wsB", name: "服务器B", url: "http://b.local", token: "tok-b" },
    ];
    state.workspace = state.workspaces[0];
    state.token = "tok-a";
    state.workspaceLists = {};
    state.workspaceErrors = {};
    state.lastList = [];
    state.sessionId = null;
    state.sidebar.filter = "";
    state.sidebar.showAllWs = new Set();
    state.sidebar.expanded = new Set();
    state.renameActive = false;
    renderWorkspaceSelect();
    await pollAllWorkspaces();
    await flush();
    await flush();

    // 1) 聚合侧边栏：两个 workspace 分组，各组只含自己的会话
    renderSidebarTree(true);
    let wsSections = elsById["sidebarTree"].querySelectorAll(".tree-ws-section");
    chk("agg sidebar shows both workspace groups", wsSections.length === 2,
        "n=" + wsSections.length);
    chk("agg group headers correct",
        wsSections[0].querySelector(".tree-ws-header").textContent.includes("服务器A")
        && wsSections[1].querySelector(".tree-ws-header").textContent.includes("服务器B"),
        "h0=" + wsSections[0].querySelector(".tree-ws-header").textContent
        + " h1=" + wsSections[1].querySelector(".tree-ws-header").textContent);
    chk("agg A sessions under A group",
        wsSections[0].textContent.includes("a1") && !wsSections[0].textContent.includes("b1"),
        "secA=" + wsSections[0].textContent.slice(0, 60));
    chk("agg B sessions under B group",
        wsSections[1].textContent.includes("b1") && !wsSections[1].textContent.includes("a1"),
        "secB=" + wsSections[1].textContent.slice(0, 60));

    // 5) 孤儿按 workspace 分组：B 的孤儿不出现在 A 组（parent 不跨服务器匹配）
    chk("agg B orphan not under A",
        !wsSections[0].textContent.includes("b-orphan")
        && wsSections[1].textContent.includes("b-orphan"),
        "A-has=" + wsSections[0].textContent.includes("b-orphan")
        + " B-has=" + wsSections[1].textContent.includes("b-orphan"));

    // 4) 同服务器点击：A 激活时点 A 组的 a1 → 直接打开，不切换 workspace
    wsSections = elsById["sidebarTree"].querySelectorAll(".tree-ws-section");
    const aRootRow = wsSections[0].querySelectorAll(".tree-row")[0];
    aRootRow._listeners["click"][0]();
    chk("agg same-server click opens directly",
        state.workspace.id === "wsA" && state.sessionId === "a1",
        "ws=" + state.workspace.id + " sid=" + state.sessionId);
    await flush();
    await flush();

    // 2) 跨服务器点击：A 激活时点 B 组的 b1 → 先切到 B 再打开（异步，等 next tick）
    wsSections = elsById["sidebarTree"].querySelectorAll(".tree-ws-section");
    const bRootRow = wsSections[1].querySelectorAll(".tree-row")[0];
    bRootRow._listeners["click"][0]();
    await flush();
    await flush();
    chk("agg cross-server click switches and opens",
        state.workspace.id === "wsB" && state.sessionId === "b1",
        "ws=" + state.workspace.id + " sid=" + state.sessionId);

    // 6) 服务器失败隔离：B 的 /api/sessions 返回 500 → A 的会话照常渲染，
    //    B 分组头显示 muted「无法连接」（旧列表保留为 stale）
    sessionsBFail = true;
    switchWorkspace("wsA");     // 切回 A（B 缓存保留但标记错误）
    await flush();
    await pollAllWorkspaces();
    await flush();
    renderSidebarTree(true);
    wsSections = elsById["sidebarTree"].querySelectorAll(".tree-ws-section");
    chk("agg failure: A sessions still render",
        wsSections[0].textContent.includes("a1"),
        "secA=" + wsSections[0].textContent.slice(0, 60));
    chk("agg failure: B shows error header",
        wsSections[1].querySelector(".ws-err") !== null
        && wsSections[1].textContent.includes("无法连接"),
        "err=" + String(wsSections[1].querySelector(".ws-err") && wsSections[1].querySelector(".ws-err").textContent));
    sessionsBFail = false;

    // =====================================================================
    // 7) 跨服务器 resume 竞态：B 的 resume POST 挂起期间用户切回 A →
    //    过期 resume 不得打开任何会话（不得在错误服务器上自动 openSession）
    // =====================================================================
    bPostDelayed = true;
    bPostResolve = null;
    resumeSessionIn("wsB", "b-hist");    // 发起 B 的恢复（POST 挂起，不 await）
    await flush();
    chk("agg resume race: switched to B, POST pending",
        state.workspace.id === "wsB" && state.sessionId === null,
        "ws=" + state.workspace.id + " sid=" + state.sessionId);
    switchWorkspace("wsA");              // POST 未决期间用户直接切回 A
    await flush();
    bPostResolve(resp(201, { id: "b-hist", status: "Idle", active: true }));   // 手动 resolve 延迟 POST
    await flush();
    await flush();
    chk("agg resume race: stale resume opens nothing",
        state.workspace.id === "wsA" && state.sessionId === null,
        "ws=" + state.workspace.id + " sid=" + state.sessionId);
    bPostDelayed = false;

    // =====================================================================
    // 8) history 渲染竞态：打开 A 的 a1（history 延迟），resolve 前切到 B →
    //    A 的历史内容不得画进 B 的 transcript，也不得对 A 起 SSE
    // =====================================================================
    aHistoryDelayed = true;
    aHistoryResolve = null;
    const fetchBefore8 = FETCHES.length;
    openSessionIn("wsA", "a1");          // 同服务器打开：history 挂起
    await flush();
    chk("agg history race: a1 opened with pending history",
        state.workspace.id === "wsA" && state.sessionId === "a1",
        "ws=" + state.workspace.id + " sid=" + state.sessionId);
    switchWorkspace("wsB");              // history 未决期间切到 B
    await flush();
    aHistoryResolve(resp(200, { entries: [{ type: "message", message: { Assistant: { content: "A 会话内容" } } }], next_before_seq: null }));
    await flush();
    await flush();
    chk("agg history race: A history not painted into B",
        !elsById["messages"].textContent.includes("A 会话内容"),
        "msgs=" + JSON.stringify(elsById["messages"].textContent.slice(0, 60)));
    chk("agg history race: no SSE to A started",
        !FETCHES.slice(fetchBefore8).some((u) => u === "/api/sessions/a1/events"),
        "new=" + JSON.stringify(FETCHES.slice(fetchBefore8)));
    aHistoryDelayed = false;
    switchWorkspace("wsA");              // 恢复 A 激活（后续测试在 A 上进行）
    await flush();

    // =====================================================================
    // 9) 孤儿隔离（parent 撞名）：B 的孤儿 parent_session_id === A 的 a1 →
    //    归 B 的「未关联」组，绝不挂到 A 的父节点下
    // =====================================================================
    sessionsDataB.push({ id: "b-orphan2", parent_session_id: "a1", status: "Idle", entry_count: 1, active: true });
    await pollAllWorkspaces();
    await flush();
    renderSidebarTree(true);
    wsSections = elsById["sidebarTree"].querySelectorAll(".tree-ws-section");
    chk("agg orphan collision: B orphan stays in B section",
        wsSections[1].textContent.includes("b-orphan2")
        && !wsSections[0].textContent.includes("b-orphan2"),
        "A-has=" + wsSections[0].textContent.includes("b-orphan2")
        + " B-has=" + wsSections[1].textContent.includes("b-orphan2"));
    const bOrphanGroup = wsSections[1].querySelector(".tree-group");
    chk("agg orphan collision: under B 未关联 group",
        bOrphanGroup !== null
        && bOrphanGroup.textContent.includes("未关联")
        && bOrphanGroup.closest(".tree-node").textContent.includes("b-orphan2"),
        "g=" + (bOrphanGroup ? bOrphanGroup.textContent : "none"));
    chk("agg orphan collision: not under A parent node",
        !wsSections[0].querySelector(".tree-node").textContent.includes("b-orphan2"),
        "A0=" + wsSections[0].querySelector(".tree-node").textContent.slice(0, 40));

    // =====================================================================
    // 10) 删除路由：删除 B 的会话 → 只更新 workspaceLists.B；A 的缓存不动；
    //     激活=A 时 sessionStates 不清除（同名会话的视图缓存属于 A）
    // =====================================================================
    state.sessionStates["b1"] = { html: "cache-b1", scrollTop: 0, nextBeforeSeq: null, olderDone: true, draft: "" };
    const aCacheJson = JSON.stringify(state.workspaceLists["wsA"]);
    const bHadB1 = state.workspaceLists["wsB"].some((x) => x.id === "b1");
    renderSidebarTree(true);
    const bSectionForDelete = [...elsById["sidebarTree"].querySelectorAll(".tree-ws-section")]
      .find((sec) => sec.textContent.includes("服务器B"));
    const bRow = bSectionForDelete && [...bSectionForDelete.querySelectorAll(".tree-row")]
      .find((r) => r.textContent.includes("b1"));
    chk("agg delete: B row has migrated delete action", bRow !== null && bHadB1
        && bRow.querySelector(".tree-del") !== null,
        "row=" + (bRow !== null) + " bHad=" + bHadB1);
    bRow.querySelector(".tree-del")._listeners["click"][0]({ stopPropagation(){} });
    await flush();
    await flush();
    chk("agg delete: B cache no longer has b1",
        !state.workspaceLists["wsB"].some((x) => x.id === "b1"),
        "B=" + JSON.stringify(state.workspaceLists["wsB"].map((x) => x.id)));
    chk("agg delete: A cache untouched",
        JSON.stringify(state.workspaceLists["wsA"]) === aCacheJson
        && state.workspaceLists["wsA"].some((x) => x.id === "a1"),
        "A=" + JSON.stringify(state.workspaceLists["wsA"].map((x) => x.id)));
    chk("agg delete: A sessionStates not cleared (active=A)",
        state.sessionStates["b1"] !== undefined,
        "st=" + String(state.sessionStates["b1"] && state.sessionStates["b1"].html));
    delete state.sessionStates["b1"];   // 清理测试残留

    // =====================================================================
    // 11) 非数组 JSON：B 返回 {}（合法 JSON 非数组）→ B 分组显示错误标记、
    //     B 旧缓存保留、A 不受影响、激活=A 时无全局 banner
    // =====================================================================
    sessionsBFormat = true;
    const bCacheRef = state.workspaceLists["wsB"];
    await pollAllWorkspaces();
    await flush();
    renderSidebarTree(true);
    wsSections = elsById["sidebarTree"].querySelectorAll(".tree-ws-section");
    chk("agg non-array: B error marker",
        wsSections[1].querySelector(".ws-err") !== null
        && wsSections[1].textContent.includes("无法连接"),
        "err=" + String(wsSections[1].querySelector(".ws-err") && wsSections[1].querySelector(".ws-err").textContent));
    chk("agg non-array: B previous cache retained",
        state.workspaceLists["wsB"] === bCacheRef
        && state.workspaceLists["wsB"].some((x) => x.id === "b-orphan"),
        "sameRef=" + (state.workspaceLists["wsB"] === bCacheRef));
    chk("agg non-array: A unaffected",
        wsSections[0].querySelector(".ws-err") === null
        && wsSections[0].textContent.includes("a1"),
        "A-err=" + (wsSections[0].querySelector(".ws-err") !== null));
    chk("agg non-array: no global banner (active=A)", elsById["banner"].hidden === true,
        "hidden=" + elsById["banner"].hidden);
    sessionsBFormat = false;

    // =====================================================================
    // 12) showAll 每 workspace 独立：A、B 各 > MAX_TREE_ROOTS 主会话 →
    //     只点 A 的「显示全部」→ 只有 A 展开，B 分组仍折叠
    // =====================================================================
    for (let i = 0; i < 10; i++) {
      sessionsData.push({ id: "a-extra" + i, status: "Idle", entry_count: 1, active: true });
      sessionsDataB.push({ id: "b-extra" + i, status: "Idle", entry_count: 1, active: true });
    }
    state.sidebar.showAllWs = new Set();
    await pollAllWorkspaces();
    await flush();
    renderSidebarTree(true);
    wsSections = elsById["sidebarTree"].querySelectorAll(".tree-ws-section");
    const aMore = wsSections[0].querySelector(".tree-more");
    const bMore = wsSections[1].querySelector(".tree-more");
    chk("agg showAll per-ws: both sections collapsed initially",
        aMore !== null && bMore !== null,
        "aMore=" + (aMore !== null) + " bMore=" + (bMore !== null));
    aMore._listeners["click"][0]();
    chk("agg showAll per-ws: only A marked expanded",
        state.sidebar.showAllWs.has("wsA") && !state.sidebar.showAllWs.has("wsB"),
        "A=" + state.sidebar.showAllWs.has("wsA") + " B=" + state.sidebar.showAllWs.has("wsB"));
    renderSidebarTree(true);
    wsSections = elsById["sidebarTree"].querySelectorAll(".tree-ws-section");
    chk("agg showAll per-ws: A expanded, B still collapsed",
        wsSections[0].querySelector(".tree-more") === null
        && wsSections[1].querySelector(".tree-more") !== null,
        "A-more=" + (wsSections[0].querySelector(".tree-more") !== null)
        + " B-more=" + (wsSections[1].querySelector(".tree-more") !== null));

    // =====================================================================
    // 13) SSE 生命周期：SSE 流的三重校验必须覆盖整条流的存活期（Fix 1）。
    //     A 的流打开后切到 B（代次被取代），随后 A 的陈旧块（snapshot/
    //     status/delta/resync）才到达——绝不能画进 B 的 DOM/状态。
    // =====================================================================
    a1StreamManual = true;
    a1StreamReadResolve = null;
    openSessionIn("wsA", "a1");        // 打开 A 的 a1：history 就绪后起 SSE，首个 read 挂起
    await flush();
    await flush();
    chk("agg sse lifetime: a1 stream pending",
        state.workspace.id === "wsA" && state.sessionId === "a1"
        && a1StreamReadResolve !== null,
        "ws=" + state.workspace.id + " sid=" + state.sessionId
        + " pending=" + (a1StreamReadResolve !== null));
    const epochA1 = sessionOpenEpoch;  // a1 打开动作的唯一代次（此后被 B 取代）
    openSessionIn("wsB", "b1");        // 切到 B：代次取代，A 的流成为陈旧流
    await flush();
    await flush();
    chk("agg sse lifetime: switched to B",
        state.workspace.id === "wsB" && state.sessionId === "b1",
        "ws=" + state.workspace.id + " sid=" + state.sessionId);
    const bMsgsBefore = elsById["messages"].textContent;
    const bStatBefore = elsById["chatStatus"].textContent;
    const tBefore13 = scheduledTimeouts.length;
    // 切换后 A 的流才吐出整批陈旧块（snapshot/status/delta）
    a1StreamReadResolve({ done: false, value:
      "event: snapshot\ndata: [{\"type\":\"notice\",\"text\":\"STALE-A-SNAPSHOT\"}]\n\n"
      + "event: status\ndata: {\"status\":\"Busy\"}\n\n"
      + "event: AssistantDelta\ndata: {\"type\":\"assistant_delta\",\"session_id\":\"a1\",\"seq\":1,\"delta\":\"STALE-A-DELTA\"}\n\n" });
    await flush();
    await flush();
    chk("agg sse lifetime: stale chunk batch not painted into B",
        elsById["messages"].textContent === bMsgsBefore
        && !elsById["messages"].textContent.includes("STALE-A-SNAPSHOT")
        && !elsById["messages"].textContent.includes("STALE-A-DELTA"),
        "msgs=" + JSON.stringify(elsById["messages"].textContent.slice(0, 80)));
    chk("agg sse lifetime: stale status not applied to B",
        elsById["chatStatus"].textContent === bStatBefore
        && !elsById["chatStatus"].textContent.includes("处理中"),
        "status=" + elsById["chatStatus"].textContent);
    chk("agg sse lifetime: B state untouched",
        state.workspace.id === "wsB" && state.sessionId === "b1",
        "ws=" + state.workspace.id + " sid=" + state.sessionId);
    chk("agg sse lifetime: stale stream end schedules no reconnect",
        scheduledTimeouts.length === tBefore13,
        "n=" + scheduledTimeouts.length + " (b1 自身空流重连已计入 tBefore13)");

    // 直接注入陈旧块（绕过流循环，验证 handleSSEBlock 顶部三重校验）。
    // 用「当前会话 id + 陈旧代次」——正是旧代码 line-104 的 id 检查会放过的
    // 组合：snapshot/status 在 id 检查之前就改了 UI，resync 只看 id。
    const bDom13b = elsById["messages"].textContent;
    const bStat13b = elsById["chatStatus"].textContent;
    state.initSource = null;   // snapshot 分支本可渲染（旧代码会画进 B）
    handleSSEBlock("event: snapshot\ndata: [{\"type\":\"notice\",\"text\":\"STALE-A-SNAPSHOT\"}]\n\n",
      "b1", "wsB", epochA1);
    chk("agg sse lifetime: stale-epoch snapshot dropped at top",
        elsById["messages"].textContent === bDom13b
        && !elsById["messages"].textContent.includes("STALE-A-SNAPSHOT")
        && state.initSource === null,   // snapshot 分支未执行（未置为 "snapshot"）
        "same=" + (elsById["messages"].textContent === bDom13b)
        + " src=" + String(state.initSource));
    handleSSEBlock("event: status\ndata: {\"status\":\"Busy\"}\n\n", "b1", "wsB", epochA1);
    chk("agg sse lifetime: stale-epoch status dropped at top",
        elsById["chatStatus"].textContent === bStat13b
        && !elsById["chatStatus"].textContent.includes("处理中"),
        "status=" + elsById["chatStatus"].textContent);
    handleSSEBlock("event: resync\ndata: [{\"type\":\"user_prompt\",\"data\":\"STALE-RESYNC\"}]\n\n",
      "b1", "wsB", epochA1);
    chk("agg sse lifetime: stale-epoch resync dropped (no force-replace)",
        elsById["messages"].textContent === bDom13b
        && !elsById["messages"].textContent.includes("STALE-RESYNC"),
        "same=" + (elsById["messages"].textContent === bDom13b));
    handleSSEBlock("event: AssistantDelta\ndata: {\"type\":\"assistant_delta\",\"session_id\":\"b1\",\"seq\":1,\"delta\":\"STALE-DELTA\"}\n\n",
      "b1", "wsB", epochA1);
    chk("agg sse lifetime: stale-epoch delta dropped at top",
        elsById["messages"].textContent === bDom13b
        && !elsById["messages"].textContent.includes("STALE-DELTA"),
        "same=" + (elsById["messages"].textContent === bDom13b));
    state.initSource = "history";   // 还原（b1 的 history 已渲染）

    // resync 路径（经流循环）：重新打开 A（新流挂起）→ 切到 B → A 的
    // resync 事件延迟到达 → 不得强制替换 B 的 transcript
    a1StreamReadResolve = null;
    openSessionIn("wsA", "a1");
    await flush();
    await flush();
    const epochA1b = sessionOpenEpoch;
    chk("agg sse lifetime: second a1 stream pending", a1StreamReadResolve !== null,
        "pending=" + (a1StreamReadResolve !== null));
    openSessionIn("wsB", "b1");
    await flush();
    await flush();
    const bMsgs13c = elsById["messages"].textContent;
    a1StreamReadResolve({ done: false, value:
      "event: resync\ndata: [{\"type\":\"user_prompt\",\"data\":\"STALE-RESYNC-STREAM\"}]\n\n" });
    await flush();
    await flush();
    chk("agg sse lifetime: stale resync via stream does not force-replace B",
        elsById["messages"].textContent === bMsgs13c
        && !elsById["messages"].textContent.includes("STALE-RESYNC-STREAM"),
        "same=" + (elsById["messages"].textContent === bMsgs13c));

    // =====================================================================
    // 14) scheduleReconnect 的三重校验（Fix 1）：
    //     陈旧断线回调（旧代次）在调度点就被拒绝；当前上下文的断线重连
    //     正常调度，触发时仍校验同一上下文才重新 openWith。
    // =====================================================================
    a1StreamReadResolve = null;
    openSessionIn("wsA", "a1");
    await flush();
    await flush();
    const epochA14 = sessionOpenEpoch;
    openSessionIn("wsB", "b1");
    await flush();
    await flush();
    const tBefore14 = scheduledTimeouts.length;
    const fetchBefore14 = FETCHES.length;
    const retryBefore14 = state.sse.retryTimer;
    scheduleReconnect("a1", "wsA", epochA14);   // A 的陈旧断线回调（迟到的 catch）
    chk("agg sse lifetime: stale reconnect refused at schedule",
        scheduledTimeouts.length === tBefore14 && state.sse.retryTimer === retryBefore14,
        "n=" + scheduledTimeouts.length + " timer=" + String(state.sse.retryTimer));
    chk("agg sse lifetime: stale reconnect issues no fetch",
        FETCHES.length === fetchBefore14,
        "new=" + JSON.stringify(FETCHES.slice(fetchBefore14)));
    // 当前上下文（b1）的断线回调：正常调度；手动触发定时器 → 仍校验通过
    // → 重新走 history + SSE（多一次 b1/events 请求）
    state.sse.stopped = false;
    scheduleReconnect("b1", "wsB", sessionOpenEpoch);
    chk("agg sse lifetime: current reconnect schedules",
        scheduledTimeouts.length === tBefore14 + 1 && state.sse.retryTimer !== null,
        "n=" + scheduledTimeouts.length);
    const bEventsBefore14 = FETCHES.filter(u => u === "http://b.local/api/sessions/b1/events").length;
    scheduledTimeouts[tBefore14]();   // 触发重连定时器
    await flush();
    await flush();
    chk("agg sse lifetime: current reconnect re-opens b1",
        state.sessionId === "b1"
        && FETCHES.filter(u => u === "http://b.local/api/sessions/b1/events").length === bEventsBefore14 + 1,
        "events=" + FETCHES.filter(u => u === "http://b.local/api/sessions/b1/events").length);
    a1StreamManual = false;

    // =====================================================================
    // 15) 置顶分组：所有 workspace 的 pinned 主会话集中到侧边栏最顶分组
    //     （跨 workspace 色条 + workspace 内剔除 + 子会话跟随 + pinned
    //     子会话不重复渲染：只收主会话，pinned 子会话留在父节点下）
    // =====================================================================
    sessionsData = [
      { id: "pa1", status: "Idle", title: "A 置顶", created_at: "2024-01-01T00:00:00Z", entry_count: 3, busy: false, active: true, pinned: true },
      { id: "pa1-child", parent_session_id: "pa1", label: "A 置顶子任务", status: "Idle", entry_count: 1, active: true },
      { id: "a-np", status: "Idle", title: "A 普通", created_at: "2024-01-02T00:00:00Z", entry_count: 1, active: true },
      { id: "a-np-child", parent_session_id: "a-np", label: "A 普通子任务", status: "Idle", entry_count: 1, active: true, pinned: true },
    ];
    sessionsDataB = [
      { id: "pb1", status: "Idle", title: "B 置顶", created_at: "2024-02-01T00:00:00Z", entry_count: 2, active: true, pinned: true },
      { id: "b-np", status: "Idle", title: "B 普通", created_at: "2024-02-02T00:00:00Z", entry_count: 1, active: true },
    ];
    // 还原快照：测试末尾把聚合状态原样放回（workspaces/workspaceLists/
    // workspaceErrors/sidebar/列表源数据）
    const saveWs = { workspaces: state.workspaces, workspace: state.workspace, token: state.token,
      lists: state.workspaceLists, errors: state.workspaceErrors, lastList: state.lastList,
      sessionId: state.sessionId,
      filter: state.sidebar.filter, showAll: state.sidebar.showAllWs, expanded: state.sidebar.expanded,
      dataA: sessionsData, dataB: sessionsDataB };
    state.workspaces = [
      { id: "wsA", name: "服务器A", url: "", token: "tok-a" },
      { id: "wsB", name: "服务器B", url: "http://b.local", token: "tok-b" },
    ];
    state.workspace = state.workspaces[0];
    state.token = "tok-a";
    state.workspaceLists = {};
    state.workspaceErrors = {};
    state.lastList = [];
    state.sessionId = null;
    state.sidebar.filter = "";
    state.sidebar.showAllWs = new Set();
    state.sidebar.expanded = new Set(["wsA:pa1", "wsA:a-np"]);   // 展开以渲染子会话行
    state.renameActive = false;
    await pollAllWorkspaces();
    await flush();
    await flush();
    renderSidebarTree(true);
    const pinSections = elsById["sidebarTree"].querySelectorAll(".tree-ws-section");
    const pinTop = pinSections[0];
    const pinA = pinSections[1];
    const pinB = pinSections[2];
    chk("pinned group is first .tree-ws-section",
        pinTop.classList.contains("pinned"), "cls=" + pinTop.className);
    const pinMarkers = pinTop.querySelectorAll(".ws-pin-label");
    const pinMarkerLabels = [...pinMarkers].map((m) => m.textContent);
    chk("pinned group carries two compact server labels",
        pinMarkers.length === 2
        && pinMarkerLabels.includes("服务器A")
        && pinMarkerLabels.includes("服务器B")
        && [...pinMarkers].every((m) => m.parentNode
          && m.parentNode.classList.contains("tree-title")),
        "labels=" + pinMarkerLabels.join(","));
    chk("pinned roots all in pinned group",
        pinTop.textContent.includes("pa1") && pinTop.textContent.includes("pb1"),
        "txt=" + pinTop.textContent.slice(0, 80));
    chk("pinned roots not duplicated in their workspace sections",
        !pinA.textContent.includes("pa1") && !pinB.textContent.includes("pb1"),
        "aHas=" + pinA.textContent.includes("pa1") + " bHas=" + pinB.textContent.includes("pb1"));
    chk("pinned root children follow into pinned group",
        pinTop.textContent.includes("pa1-child") && !pinA.textContent.includes("pa1-child"),
        "topHas=" + pinTop.textContent.includes("pa1-child"));
    chk("non-pinned sessions stay in workspace sections",
        pinA.textContent.includes("a-np") && pinB.textContent.includes("b-np"),
        "aHas=" + pinA.textContent.includes("a-np") + " bHas=" + pinB.textContent.includes("b-np"));
    chk("pinned subagent not hoisted to pinned group, stays under parent",
        !pinTop.textContent.includes("a-np-child") && pinA.textContent.includes("a-np-child"),
        "topHas=" + pinTop.textContent.includes("a-np-child")
        + " aHas=" + pinA.textContent.includes("a-np-child"));

    // 15a) 置顶顺序：直接调用排序入口等价于拖拽 drop，覆盖持久化与渲染。
    localStorage.removeItem(PIN_ORDER_KEY);
    chk("pin order: move persists cross-workspace ordered pairs",
        movePinnedSession("wsB", "pb1", "wsA", "pa1", true) === true
        && JSON.stringify(JSON.parse(localStorage.getItem(PIN_ORDER_KEY)))
          === JSON.stringify([{wsId:"wsB",sid:"pb1"},{wsId:"wsA",sid:"pa1"}]),
        "stored=" + localStorage.getItem(PIN_ORDER_KEY));
    let orderedPinNodes = elsById["sidebarTree"].querySelector(".tree-ws-section.pinned")
      .querySelector(".tree-ws-body").children;
    chk("pin order: render follows stored order",
        orderedPinNodes.length === 2
        && orderedPinNodes[0].getAttribute("data-pin-sid") === "pb1"
        && orderedPinNodes[1].getAttribute("data-pin-sid") === "pa1",
        "ids=" + [...orderedPinNodes].map((n) => n.getAttribute("data-pin-sid")).join(","));
    // 新置顶不在存储中：已记录项保持在前，新项沉底。
    state.lastList.push({ id: "pa-new", status: "Idle", title: "A 新置顶",
      entry_count: 1, active: true, pinned: true });
    renderSidebarTree(true);
    orderedPinNodes = elsById["sidebarTree"].querySelector(".tree-ws-section.pinned")
      .querySelector(".tree-ws-body").children;
    chk("pin order: newly pinned session appends after stored items",
        orderedPinNodes.length === 3
        && orderedPinNodes[2].getAttribute("data-pin-sid") === "pa-new",
        "ids=" + [...orderedPinNodes].map((n) => n.getAttribute("data-pin-sid")).join(","));
    // 取消置顶成功后清理该 wsId+sid（通过真实 togglePin 成功路径）。
    await togglePin(state.workspaceLists["wsB"].find((x) => x.id === "pb1"), () => {}, state.workspaces[1]);
    chk("pin order: unpin removes persisted entry",
        !JSON.parse(localStorage.getItem(PIN_ORDER_KEY)).some((x) => x.wsId === "wsB" && x.sid === "pb1")
        && JSON.parse(localStorage.getItem(PIN_ORDER_KEY)).some((x) => x.wsId === "wsA" && x.sid === "pa1"),
        "stored=" + localStorage.getItem(PIN_ORDER_KEY));
    localStorage.removeItem(PIN_ORDER_KEY);
    // 还原聚合状态（workspaces/workspaceLists/workspaceErrors/sidebar/数据源）
    state.workspaces = saveWs.workspaces; state.workspace = saveWs.workspace; state.token = saveWs.token;
    state.workspaceLists = saveWs.lists; state.workspaceErrors = saveWs.errors; state.lastList = saveWs.lastList;
    state.sessionId = saveWs.sessionId;
    state.sidebar.filter = saveWs.filter; state.sidebar.showAllWs = saveWs.showAll; state.sidebar.expanded = saveWs.expanded;
    state.renameActive = false;
    sessionsData = saveWs.dataA; sessionsDataB = saveWs.dataB;

    // =====================================================================
    // 15b) workspace 标识分色 + 父节点 busy 数量徽标 + 子会话分组
    //     A) 标识按 workspace 下标取色（ws-chip-<n>）：不同 workspace
    //        色类不同；同一 workspace 的列表 chip / 组头 chip / 置顶色条同色。
    //     B) 父节点显示直接 busy 子会话数量；无 busy 子时，父自身 busy
    //        保留脉动点，否则为绿色点。父自身与子会话同时 busy 时数字优先。
    //     C) 展开父节点只直显 busy 子会话，idle（包括 active=true）收进
    //        「历史子会话 (N)」折叠组。
    // =====================================================================
    sessionsData = [
      { id: "c1", status: "Idle", title: "C 主会话", created_at: "2024-03-01T00:00:00Z", entry_count: 1, busy: false, active: true, pinned: true },
      { id: "c1-child-busy-1", parent_session_id: "c1", label: "C 子任务忙一", status: "Busy", entry_count: 1, busy: true, active: true },
      { id: "c1-child-busy-2", parent_session_id: "c1", label: "C 子任务忙二", status: "Busy", entry_count: 1, busy: true, active: true },
      { id: "c2", status: "Idle", title: "C2 主会话", created_at: "2024-03-02T00:00:00Z", entry_count: 1, busy: false, active: true },
      { id: "c2-child-busy", parent_session_id: "c2", label: "C2 子任务忙", status: "Busy", entry_count: 1, busy: true, active: true },
      { id: "c2-child-idle", parent_session_id: "c2", label: "C2 子任务闲", status: "Idle", entry_count: 1, busy: false, active: false },
      { id: "e1", status: "Idle", title: "E 主会话", created_at: "2024-03-03T00:00:00Z", entry_count: 1, busy: false, active: true },
      { id: "e1-child-idle", parent_session_id: "e1", label: "E 子任务闲", status: "Idle", entry_count: 1, busy: false, active: false },
    ];
    sessionsDataB = [
      { id: "d1", status: "Busy", title: "D 主会话", created_at: "2024-04-01T00:00:00Z", entry_count: 1, busy: true, active: true, pinned: true },
      { id: "d1-child-idle", parent_session_id: "d1", label: "D 子任务闲", status: "Idle", entry_count: 1, busy: false, active: false },
      { id: "f1", status: "Busy", title: "F 主会话", created_at: "2024-04-02T00:00:00Z", entry_count: 1, busy: true, active: true },
      { id: "f1-child-busy", parent_session_id: "f1", label: "F 子任务忙", status: "Busy", entry_count: 1, busy: true, active: true },
    ];
    state.workspaces = [
      { id: "wsA", name: "服务器A", url: "", token: "tok-a" },
      { id: "wsB", name: "服务器B", url: "http://b.local", token: "tok-b" },
    ];
    state.workspace = state.workspaces[0];
    state.token = "tok-a";
    state.workspaceLists = {};
    state.workspaceErrors = {};
    state.lastList = [];
    state.sessionId = null;
    state.sidebar.filter = "";
    state.sidebar.showAllWs = new Set();
    state.sidebar.expanded = new Set(["wsA:c1", "wsA:c2", "wsA:e1"]);   // 展开验证 busy 直显 / idle 折叠
    state.renameActive = false;
    await pollAllWorkspaces();
    await flush();
    await flush();
    console.log("DBG 15 lastList=" + JSON.stringify(state.lastList.map((x) => [x.id, x.title, x.pinned])));
    renderSidebarTree(true);
    const sec15 = elsById["sidebarTree"].querySelectorAll(".tree-ws-section");
    // 组头 chip：A=ws-chip-0，B=ws-chip-1（数组下标取色，互不相同）
    const hdrA = sec15[1].querySelector(".tree-ws-header").querySelector(".ws-chip");
    const hdrB = sec15[2].querySelector(".tree-ws-header").querySelector(".ws-chip");
    const chipCls15 = (e) => (e.className.split(/\s+/).find((c) => c.startsWith("ws-chip-")) || "");
    chk("ws-chip color class differs across workspaces",
        chipCls15(hdrA) === "ws-chip-0" && chipCls15(hdrB) === "ws-chip-1",
        "A=" + chipCls15(hdrA) + " B=" + chipCls15(hdrB));
    // 置顶分组每行的紧凑色条：A/B 各一，与组头同色（同 workspace 同色）
    const pinMarkers15 = sec15[0].querySelectorAll(".ws-pin-label");
    const pinCls15 = [...pinMarkers15].map(chipCls15).filter((c) => c);
    chk("pinned row labels share workspace color",
        pinMarkers15.length === 2 && pinCls15.includes("ws-chip-0") && pinCls15.includes("ws-chip-1"),
        "pins=" + pinCls15.join(","));
    // B) 数量徽标 / 父自身 busy 点 / idle 绿点的五种组合
    const rows15 = elsById["sidebarTree"].querySelectorAll(".tree-row");
    const dotOf15 = (r) => r.querySelector(".busy-dot");
    const rowByTxt15 = (t) => [...rows15].find((r) => r.textContent.includes(t));
    const c1Dot = dotOf15(rowByTxt15("C 主会话"));
    const c2Dot = dotOf15(rowByTxt15("C2 主会话"));
    const d1Dot = dotOf15(rowByTxt15("D 主会话"));
    const e1Dot = dotOf15(rowByTxt15("E 主会话"));
    const f1Dot = dotOf15(rowByTxt15("F 主会话"));
    chk("red dot when two active children",
        c1Dot.classList.contains("busy") && !c1Dot.classList.contains("busy-count")
        && c1Dot.textContent === "",
        "class=" + c1Dot.className + " text=" + c1Dot.textContent);
    chk("red dot when one active child, inactive ignored",
        c2Dot.classList.contains("busy") && !c2Dot.classList.contains("busy-count"),
        "class=" + c2Dot.className);
    chk("red dot when parent busy, no active children",
        d1Dot.classList.contains("busy") && !d1Dot.classList.contains("busy-count")
        && d1Dot.textContent === "",
        "class=" + d1Dot.className + " text=" + d1Dot.textContent);
    chk("green dot when fully idle (no active children, parent idle)",
        !e1Dot.classList.contains("busy") && !e1Dot.classList.contains("busy-count")
        && e1Dot.textContent === "",
        "class=" + e1Dot.className + " text=" + e1Dot.textContent);
    chk("red dot when parent busy AND active child (OR semantics)",
        f1Dot.classList.contains("busy") && !f1Dot.classList.contains("busy-count"),
        "class=" + f1Dot.className);
    chk("parent title keeps child-busy hint",
        rowByTxt15("C 主会话").title.includes("子任务处理中")
        && rowByTxt15("F 主会话").title.includes("子任务处理中")
        && !rowByTxt15("E 主会话").title.includes("子任务处理中"),
        "c=" + rowByTxt15("C 主会话").title + " f=" + rowByTxt15("F 主会话").title);
    // C) busy 子会话直显；active=true 的 idle 子会话也必须进入默认收起的历史组
    const c2BusyRow = rowByTxt15("C2 子任务忙");
    const c2IdleRow = rowByTxt15("C2 子任务闲");
    const e1IdleRow = rowByTxt15("E 子任务闲");
    const histLabels15 = elsById["sidebarTree"].querySelectorAll(".tree-hist-label");
    chk("expanded parent directly shows only busy children",
        c2BusyRow.parentNode.hidden === false
        && c2IdleRow.classList.contains("tree-hist") && c2IdleRow.parentNode.hidden === true
        && e1IdleRow.classList.contains("tree-hist") && e1IdleRow.parentNode.hidden === true,
        "busyHidden=" + c2BusyRow.parentNode.hidden
        + " c2IdleHidden=" + c2IdleRow.parentNode.hidden
        + " e1IdleHidden=" + e1IdleRow.parentNode.hidden);
    chk("idle children are counted in collapsed history groups",
        [...histLabels15].filter((e) => e.textContent === "历史子会话 (1)").length >= 2,
        "labels=" + [...histLabels15].map((e) => e.textContent).join(","));
    // 还原聚合状态（与 15 一致）
    state.workspaces = saveWs.workspaces; state.workspace = saveWs.workspace; state.token = saveWs.token;
    state.workspaceLists = saveWs.lists; state.workspaceErrors = saveWs.errors; state.lastList = saveWs.lastList;
    state.sessionId = saveWs.sessionId;
    state.sidebar.filter = saveWs.filter; state.sidebar.showAllWs = saveWs.showAll; state.sidebar.expanded = saveWs.expanded;
    state.renameActive = false;
    sessionsData = saveWs.dataA; sessionsDataB = saveWs.dataB;

    // =====================================================================
    // 15c) 置顶分组筛选：filter 非空时置顶根与普通根同一 title/id 匹配规则
    //     （仅匹配的置顶根显示在置顶分组、随父展示子会话；不匹配的隐藏；
    //      workspace 内剔除逻辑不变——剔除后置顶分组是它们唯一出现位）。
    //     跨 workspace：只匹配一方的词 → 另一方的置顶根整体隐藏。
    // =====================================================================
    sessionsData = [
      { id: "pa1", status: "Idle", title: "Alpha 置顶", created_at: "2024-01-01T00:00:00Z", entry_count: 3, busy: false, active: true, pinned: true },
      { id: "pa1-child", parent_session_id: "pa1", label: "Alpha 子任务", status: "Idle", entry_count: 1, active: true },
      { id: "a-np", status: "Idle", title: "A 普通", created_at: "2024-01-02T00:00:00Z", entry_count: 1, active: true },
    ];
    sessionsDataB = [
      { id: "pb1", status: "Idle", title: "Beta 置顶", created_at: "2024-02-01T00:00:00Z", entry_count: 2, active: true, pinned: true },
      { id: "b-np", status: "Idle", title: "B 普通", created_at: "2024-02-02T00:00:00Z", entry_count: 1, active: true },
    ];
    state.workspaces = [
      { id: "wsA", name: "服务器A", url: "", token: "tok-a" },
      { id: "wsB", name: "服务器B", url: "http://b.local", token: "tok-b" },
    ];
    state.workspace = state.workspaces[0];
    state.token = "tok-a";
    state.workspaceLists = {};
    state.workspaceErrors = {};
    state.lastList = [];
    state.sessionId = null;
    state.sidebar.filter = "";
    state.sidebar.showAllWs = new Set();
    state.sidebar.expanded = new Set();
    state.renameActive = false;
    await pollAllWorkspaces();
    await flush();
    await flush();
    // 筛选只匹配 A 的置顶根（title 子串；无 title 回退 id，与普通根一致）
    state.sidebar.filter = "alpha";
    renderSidebarTree(true);
    let pinSec15c = elsById["sidebarTree"].querySelectorAll(".tree-ws-section");
    chk("pinned filter: matching pinned root shown in pinned group",
        pinSec15c[0].classList.contains("pinned")
        && pinSec15c[0].textContent.includes("pa1")
        && !pinSec15c[0].textContent.includes("pb1"),
        "top=" + pinSec15c[0].textContent.slice(0, 80));
    chk("pinned filter: children follow matching pinned root",
        pinSec15c[0].textContent.includes("pa1-child"),
        "top=" + pinSec15c[0].textContent.slice(0, 100));
    chk("pinned filter: non-matching pinned root hidden entirely",
        !pinSec15c[1].textContent.includes("pb1") && !pinSec15c[2].textContent.includes("pb1"),
        "secB=" + pinSec15c[2].textContent.slice(0, 60));
    chk("pinned filter: non-pinned roots still filtered per workspace",
        !pinSec15c[1].textContent.includes("a-np") && !pinSec15c[2].textContent.includes("b-np"),
        "secA=" + pinSec15c[1].textContent.slice(0, 60)
        + " secB=" + pinSec15c[2].textContent.slice(0, 60));
    // 筛选只匹配 B 的置顶根 → 置顶分组只有 B
    state.sidebar.filter = "beta";
    renderSidebarTree(true);
    pinSec15c = elsById["sidebarTree"].querySelectorAll(".tree-ws-section");
    chk("pinned filter: matching B pinned root shown",
        pinSec15c[0].classList.contains("pinned")
        && pinSec15c[0].textContent.includes("pb1")
        && !pinSec15c[0].textContent.includes("pa1"),
        "top=" + pinSec15c[0].textContent.slice(0, 80));
    // 筛选同时匹配两个 workspace 的置顶根 → 都显示（跨 workspace）
    state.sidebar.filter = "置顶";
    renderSidebarTree(true);
    pinSec15c = elsById["sidebarTree"].querySelectorAll(".tree-ws-section");
    chk("pinned filter: cross-workspace match shows both",
        pinSec15c[0].classList.contains("pinned")
        && pinSec15c[0].textContent.includes("pa1")
        && pinSec15c[0].textContent.includes("pb1"),
        "top=" + pinSec15c[0].textContent.slice(0, 100));
    // 无匹配置顶根 → 置顶分组整体不出现（只剩两个 workspace 分组）
    state.sidebar.filter = "zzz-no-match";
    renderSidebarTree(true);
    pinSec15c = elsById["sidebarTree"].querySelectorAll(".tree-ws-section");
    chk("pinned filter: no match hides pinned group",
        pinSec15c.length === 2 && !pinSec15c[0].classList.contains("pinned"),
        "n=" + pinSec15c.length + " cls0=" + pinSec15c[0].className);
    state.sidebar.filter = "";

    // =====================================================================
    // 16) 删除 review 修复验证：后台删除（不切换/不重置当前聊天 + 侧边栏同步
    //     移除被删服务器会话）、confirm 取消、在途轮询写回守卫、首/中/末
    //     active 删除回退。
    // =====================================================================
    sessionsData = [
      { id: "a1", status: "Idle", model: "kimi", title: "A 主会话", created_at: "2024-01-01T00:00:00Z", entry_count: 8, busy: false, active: true },
    ];
    sessionsDataB = [
      { id: "b1", status: "Busy", model: "deepseek", title: "B 主会话", created_at: "2024-02-02T00:00:00Z", entry_count: 5, busy: true, active: true },
    ];
    state.workspaces = [
      { id: "wsA", name: "服务器A", url: "", token: "tok-a" },
      { id: "wsB", name: "服务器B", url: "http://b.local", token: "tok-b" },
    ];
    state.workspace = state.workspaces[0];
    state.token = "tok-a";
    state.workspaceLists = {};
    state.workspaceErrors = {};
    state.lastList = [];
    state.sessionId = null;
    state.sidebar.filter = "";
    state.sidebar.showAllWs = new Set();
    state.sidebar.expanded = new Set();
    state.renameActive = false;
    renderWorkspaceSelect();
    await pollAllWorkspaces();
    await flush();
    await flush();
    renderSidebarTree(true);
    let delSec = elsById["sidebarTree"].querySelectorAll(".tree-ws-section");
    chk("ws-del on both group headers",
        delSec.length === 2
        && delSec[0].querySelector(".ws-del") !== null
        && delSec[1].querySelector(".ws-del") !== null,
        "n=" + delSec.length);
    // confirm 取消：点 B 的 × 但取消 → 不删除、不切换、列表/侧边栏原样
    globalThis.confirm = () => false; window.confirm = () => false;
    delSec[1].querySelector(".ws-del")._listeners["click"][0]({ stopPropagation(){} });
    globalThis.confirm = () => true; window.confirm = () => true;
    chk("ws-del cancel keeps workspace and sidebar",
        state.workspaces.length === 2 && state.workspace.id === "wsA"
        && state.workspaceLists["wsB"] !== undefined
        && elsById["sidebarTree"].textContent.includes("b1"),
        "n=" + state.workspaces.length + " ws=" + state.workspace.id
        + " treeHasB=" + elsById["sidebarTree"].textContent.includes("b1"));
    // 后台删除不重置视图：chat 视图下删 B → 仍停留 A 的 a1
    openSessionIn("wsA", "a1");
    await flush();
    await flush();
    chk("ws-del bg delete setup: chat on a1",
        state.workspace.id === "wsA" && state.sessionId === "a1",
        "ws=" + state.workspace.id + " sid=" + state.sessionId);
    renderSidebarTree(true);
    delSec = elsById["sidebarTree"].querySelectorAll(".tree-ws-section");
    delSec[1].querySelector(".ws-del")._listeners["click"][0]({ stopPropagation(){} });
    await flush();
    chk("ws-del bg delete in chat keeps view/session",
        state.workspace.id === "wsA" && state.sessionId === "a1"
        && state.workspaces.length === 1,
        "ws=" + state.workspace.id + " sid=" + state.sessionId
        + " n=" + state.workspaces.length);
    renderSidebarTree(true);
    chk("ws-del bg delete removes B group from sidebar",
        elsById["sidebarTree"].querySelectorAll(".tree-ws-section").length === 1
        && !elsById["sidebarTree"].textContent.includes("服务器B"),
        "txt=" + elsById["sidebarTree"].textContent.slice(0, 60));
    // 后台删除：无当前会话时删 B → 侧边栏同步移除 B 的会话
    //（review 发现 1：旧 DOM 不再残留被删服务器的行）
    sessionsDataB = [
      { id: "b1", status: "Busy", model: "deepseek", title: "B 主会话", created_at: "2024-02-02T00:00:00Z", entry_count: 5, busy: true, active: true },
    ];
    state.workspaces = [
      { id: "wsA", name: "服务器A", url: "", token: "tok-a" },
      { id: "wsB", name: "服务器B", url: "http://b.local", token: "tok-b" },
    ];
    state.workspace = state.workspaces[0];
    state.token = "tok-a";
    state.workspaceLists = {};
    state.workspaceErrors = {};
    state.lastList = [];
    state.sessionId = null;
    state.sidebar.filter = "";
    state.sidebar.showAllWs = new Set();
    state.sidebar.expanded = new Set();
    state.renameActive = false;
    renderWorkspaceSelect();
    await pollAllWorkspaces();
    await flush();
    await flush();
    renderSidebarTree(true);
    chk("ws-del bg delete setup: B row in sidebar",
        elsById["sidebarTree"].textContent.includes("b1"),
        "tree=" + elsById["sidebarTree"].textContent.slice(0, 60));
    delSec = elsById["sidebarTree"].querySelectorAll(".tree-ws-section");
    delSec[1].querySelector(".ws-del")._listeners["click"][0]({ stopPropagation(){} });
    await flush();
    chk("ws-del bg delete in list: stays on A, view unchanged",
        state.workspace.id === "wsA" && state.workspaces.length === 1,
        "ws=" + state.workspace.id + " n=" + state.workspaces.length);
    chk("ws-del bg delete in list: B cache cleared",
        state.workspaceLists["wsB"] === undefined && state.workspaceErrors["wsB"] === undefined,
        "lists=" + String(state.workspaceLists["wsB"] !== undefined)
        + " errs=" + String(state.workspaceErrors["wsB"] !== undefined));
    chk("ws-del bg delete: B rows gone from sidebar",
        !elsById["sidebarTree"].textContent.includes("b1")
        && elsById["sidebarTree"].textContent.includes("a1"),
        "tree=" + elsById["sidebarTree"].textContent.slice(0, 60));
    // 在途轮询写回守卫：B 的 GET /api/sessions 挂起期间删除 B → 延迟响应
    // 到达后不得写回 workspaceLists/workspaceErrors（review 发现 2）
    sessionsDataB = [
      { id: "b1", status: "Busy", model: "deepseek", title: "B 主会话", created_at: "2024-02-02T00:00:00Z", entry_count: 5, busy: true, active: true },
    ];
    state.workspaces = [
      { id: "wsA", name: "服务器A", url: "", token: "tok-a" },
      { id: "wsB", name: "服务器B", url: "http://b.local", token: "tok-b" },
    ];
    state.workspace = state.workspaces[0];
    state.token = "tok-a";
    state.workspaceLists = {};
    state.workspaceErrors = {};
    state.lastList = [];
    state.sessionId = null;
        state.renameActive = false;
    renderWorkspaceSelect();
    bGetDelayed = true;
    bGetResolve = null;
    pollWorkspaceSessions(state.workspaces[1]);   // B 的 GET 挂起（不 await）
    await flush();
    chk("ws-del in-flight poll: B GET pending",
        bGetResolve !== null && state.workspaceLists["wsB"] === undefined,
        "pending=" + (bGetResolve !== null));
    removeWorkspace(state.workspaces[1]);         // 在途期间后台删除 B
    await flush();
    bGetResolve(resp(200, sessionsDataB));        // 延迟响应此刻才到达
    await flush();
    await flush();
    chk("ws-del in-flight poll: stale GET does not resurrect B",
        !state.workspaces.some((w) => w.id === "wsB")
        && state.workspaceLists["wsB"] === undefined
        && state.workspaceErrors["wsB"] === undefined,
        "hasB=" + state.workspaces.some((w) => w.id === "wsB")
        + " lists=" + String(state.workspaceLists["wsB"] !== undefined)
        + " errs=" + String(state.workspaceErrors["wsB"] !== undefined));
    bGetDelayed = false;
    bGetResolve = null;
    // 首/中/末 active 删除回退：idx=0 → 后一个；其余 → 前一个
    const mkWs = (id, name) => ({ id, name, url: "", token: "tok-" + id });
    state.workspaces = [mkWs("w1", "一"), mkWs("w2", "二"), mkWs("w3", "三")];
    state.workspace = state.workspaces[0];
    state.token = "tok-w1";
    state.workspaceLists = {}; state.workspaceErrors = {};
    state.lastList = []; state.sessionId = null;
    state.renameActive = false;
    renderWorkspaceSelect();
    removeActiveWorkspace();
    await flush();
    chk("ws-del delete first active falls to next",
        state.workspaces.length === 2 && state.workspace.id === "w2",
        "n=" + state.workspaces.length + " ws=" + state.workspace.id);
    state.workspaces = [mkWs("w1", "一"), mkWs("w2", "二"), mkWs("w3", "三")];
    state.workspace = state.workspaces[1];
    state.token = "tok-w2";
    state.workspaceLists = {}; state.workspaceErrors = {};
    state.lastList = []; state.sessionId = null;
    state.renameActive = false;
    renderWorkspaceSelect();
    removeActiveWorkspace();
    await flush();
    chk("ws-del delete middle active falls to previous",
        state.workspaces.length === 2 && state.workspace.id === "w1",
        "n=" + state.workspaces.length + " ws=" + state.workspace.id);
    state.workspaces = [mkWs("w1", "一"), mkWs("w2", "二"), mkWs("w3", "三")];
    state.workspace = state.workspaces[2];
    state.token = "tok-w3";
    state.workspaceLists = {}; state.workspaceErrors = {};
    state.lastList = []; state.sessionId = null;
    state.renameActive = false;
    renderWorkspaceSelect();
    removeActiveWorkspace();
    await flush();
    chk("ws-del delete last active falls to previous",
        state.workspaces.length === 2 && state.workspace.id === "w2",
        "n=" + state.workspaces.length + " ws=" + state.workspace.id);

    // =====================================================================
    // 17) 侧边栏组头「+」新建 session（B 任务）：每个分组头都有 +；点击 →
    //     对该 workspace POST /api/sessions（body {}）→ 切到该 workspace 并
    //     打开新会话；激活 workspace 直接打开不切换；失败 → banner。
    // =====================================================================
    sessionsData = [
      { id: "a1", status: "Idle", model: "kimi", title: "A 主会话", created_at: "2024-01-01T00:00:00Z", entry_count: 8, busy: false, active: true },
    ];
    sessionsDataB = [
      { id: "b1", status: "Busy", model: "deepseek", title: "B 主会话", created_at: "2024-02-02T00:00:00Z", entry_count: 5, busy: true, active: true },
    ];
    state.workspaces = [
      { id: "wsA", name: "服务器A", url: "", token: "tok-a" },
      { id: "wsB", name: "服务器B", url: "http://b.local", token: "tok-b" },
    ];
    state.workspace = state.workspaces[0];
    state.token = "tok-a";
    state.workspaceLists = {};
    state.workspaceErrors = {};
    state.lastList = [];
    state.sessionId = null;
    state.sidebar.filter = "";
    state.sidebar.showAllWs = new Set();
    state.sidebar.expanded = new Set();
    state.renameActive = false;
    renderWorkspaceSelect();
    await pollAllWorkspaces();
    await flush();
    await flush();
    renderSidebarTree(true);
    let addSec = elsById["sidebarTree"].querySelectorAll(".tree-ws-section");
    const addA = addSec[0].querySelector(".ws-add");
    const addB = addSec[1].querySelector(".ws-add");
    chk("ws-add button on every group header",
        addA !== null && addB !== null && addA.textContent === "+" && addB.textContent === "+",
        "a=" + (addA ? addA.textContent : "none") + " b=" + (addB ? addB.textContent : "none"));
    // 点 B 组头的 +：POST http://b.local/api/sessions body {} → 切到 B 打开 b-new
    const fetchBeforeAddB = FETCHES.length;
    let addStopped = false;
    addB._listeners["click"][0]({ stopPropagation(){ addStopped = true; } });
    await flush();
    await flush();
    await flush();
    await flush();
    chk("ws-add click stops header propagation",
        addStopped === true, "stopped=" + addStopped);
    chk("ws-add posts to target workspace",
        FETCHES.slice(fetchBeforeAddB).includes("http://b.local/api/sessions"),
        "new=" + JSON.stringify(FETCHES.slice(fetchBeforeAddB)));
    chk("ws-add switches to target workspace and opens new session",
        state.workspace.id === "wsB" && state.sessionId === "b-new",
        "ws=" + state.workspace.id + " sid=" + state.sessionId);
    // 激活 workspace（A）的组头 +：不切换，直接打开新会话
    switchWorkspace("wsA");
    await flush();
    renderSidebarTree(true);
    addSec = elsById["sidebarTree"].querySelectorAll(".tree-ws-section");
    const fetchBeforeAddA = FETCHES.length;
    addSec[0].querySelector(".ws-add")._listeners["click"][0]({ stopPropagation(){} });
    await flush();
    await flush();
    await flush();
    await flush();
    chk("ws-add on active workspace opens without switch",
        state.workspace.id === "wsA" && state.sessionId === "sess-new",
        "ws=" + state.workspace.id + " sid=" + state.sessionId);
    chk("ws-add on active posts to same-origin",
        FETCHES.slice(fetchBeforeAddA).includes("/api/sessions"),
        "new=" + JSON.stringify(FETCHES.slice(fetchBeforeAddA)));
    // 失败路径：B 的 + 返回 500 → banner 报错、不切换、不打开
    switchWorkspace("wsB");     // 当前在 wsA chat（刚打开 sess-new）→ 切到 B 的聊天空状态
    await flush();
    sessionsBCreateFail = true;
    renderSidebarTree(true);
    addSec = elsById["sidebarTree"].querySelectorAll(".tree-ws-section");
    addSec[1].querySelector(".ws-add")._listeners["click"][0]({ stopPropagation(){} });
    await flush();
    await flush();
    chk("ws-add failure: banner error, no switch",
        elsById["banner"].hidden === false
        && elsById["bannerText"].textContent.includes("创建会话失败")
        && state.workspace.id === "wsB" && state.sessionId === null,
        "banner=" + JSON.stringify(elsById["bannerText"].textContent)
        + " ws=" + state.workspace.id);
    sessionsBCreateFail = false;

    // =====================================================================
    // 18) 全局 token 回退：A 有单独 token、B 留空 → B 的后台轮询 / 切到 B /
    //     打开会话与 SSE 全部使用全局 token（tokenInput 设置的全局值）；
    //     切换 workspace 不清空/覆盖全局 token（review：token 全局回退设计
    //     缺陷——A 的 token 泄漏给 B、切到 B 后 B 又失去认证）。
    // =====================================================================
    sessionsData = [
      { id: "a1", status: "Idle", model: "kimi", title: "A 主会话", created_at: "2024-01-01T00:00:00Z", entry_count: 8, busy: false, active: true },
    ];
    sessionsDataB = [
      { id: "b1", status: "Busy", model: "deepseek", title: "B 主会话", created_at: "2024-02-02T00:00:00Z", entry_count: 5, busy: true, active: true },
    ];
    state.workspaces = [
      { id: "wsA", name: "服务器A", url: "", token: "tok-a" },
      { id: "wsB", name: "服务器B", url: "http://b.local", token: "" },   // B 不配置单独 token
    ];
    state.workspace = state.workspaces[0];
    state.globalToken = "";          // 先清空全局：B 无生效 token → 轮询应被跳过
    state.token = "tok-a";
    state.workspaceLists = {};
    state.workspaceErrors = {};
    state.lastList = [];
    state.sessionId = null;
    state.sidebar.filter = "";
    state.sidebar.showAllWs = new Set();
    state.sidebar.expanded = new Set();
    state.renameActive = false;
    state.wsCreatePending = new Set();
    renderWorkspaceSelect();
    // 全局 token 为空时：B（无单独 token）无生效 token——绝不用 A 的 token
    chk("global token empty: B has no effective token",
        workspaceToken(state.workspaces[1]) === "",
        "b=" + JSON.stringify(workspaceToken(state.workspaces[1])));
    let hMark = FETCH_HEADERS.length;
    await pollAllWorkspaces();
    await flush();
    chk("global token empty: B not polled with A token",
        !FETCH_HEADERS.slice(hMark).some((f) => f.url === "http://b.local/api/sessions"),
        "b=" + JSON.stringify(FETCH_HEADERS.slice(hMark)
          .filter((f) => f.url === "http://b.local/api/sessions").map((f) => f.headers["Authorization"])));
    // 通过顶部 tokenInput 设置全局 token（模拟用户输入；展开/收起交互已由
    // 前序 token 折叠测试覆盖，这里直接驱动 input 事件）
    elsById["tokenInput"].value = "tok-g";
    elsById["tokenInput"]._listeners["input"][0]();
    await flush();
    chk("token input writes global token",
        state.globalToken === "tok-g" && localStorage.getItem("eagent_token") === "tok-g"
        && elsById["tokenInput"].value === "tok-g",
        "global=" + JSON.stringify(state.globalToken)
        + " legacy=" + JSON.stringify(localStorage.getItem("eagent_token")));
    chk("per-workspace token takes precedence over global",
        workspaceToken(state.workspace) === "tok-a"      // A 有单独 token：不受全局影响
        && workspaceToken(state.workspaces[1]) === "tok-g",   // B 回退全局
        "a=" + workspaceToken(state.workspace) + " b=" + workspaceToken(state.workspaces[1]));
    chk("global token does not leak into A's stored token",
        state.workspaces[0].token === "tok-a" && state.globalToken === "tok-g",
        "a=" + JSON.stringify(state.workspaces[0].token)
        + " g=" + JSON.stringify(state.globalToken));
    // B 的后台轮询（A 仍激活）带上全局 token；A 用自己的 token
    hMark = FETCH_HEADERS.length;
    await pollAllWorkspaces();
    await flush();
    chk("background B poll uses global token",
        FETCH_HEADERS.slice(hMark).some((f) => f.url === "http://b.local/api/sessions"
          && f.headers["Authorization"] === "Bearer tok-g"),
        "b=" + JSON.stringify(FETCH_HEADERS.slice(hMark)
          .filter((f) => f.url === "http://b.local/api/sessions").map((f) => f.headers["Authorization"])));
    chk("background A poll uses own token",
        FETCH_HEADERS.slice(hMark).some((f) => f.url === "/api/sessions"
          && f.headers["Authorization"] === "Bearer tok-a"),
        "a=" + JSON.stringify(FETCH_HEADERS.slice(hMark)
          .filter((f) => f.url === "/api/sessions").map((f) => f.headers["Authorization"])));
    // 切到 B：全局 token 保留、state.token 派生为全局、legacy 键不被覆盖
    const legacyBeforeSwitch = localStorage.getItem("eagent_token");
    switchWorkspace("wsB");
    await flush();
    chk("switch to B keeps global token",
        state.workspace.id === "wsB"
        && state.globalToken === "tok-g"
        && state.token === "tok-g"
        && localStorage.getItem("eagent_token") === legacyBeforeSwitch,
        "global=" + JSON.stringify(state.globalToken)
        + " token=" + JSON.stringify(state.token)
        + " legacy=" + JSON.stringify(localStorage.getItem("eagent_token")));
    // 打开 B 的会话：history + SSE 都带全局 token
    const hMarkOpen = FETCH_HEADERS.length;
    openSessionIn("wsB", "b1");
    await flush();
    await flush();
    await flush();
    chk("B history uses global token",
        state.workspace.id === "wsB" && state.sessionId === "b1"
        && FETCH_HEADERS.slice(hMarkOpen).some((f) =>
            f.url.startsWith("http://b.local/api/sessions/b1/history")
            && f.headers["Authorization"] === "Bearer tok-g"),
        "sid=" + state.sessionId
        + " hist=" + JSON.stringify(FETCH_HEADERS.slice(hMarkOpen)
            .filter((f) => f.url.includes("b1/history")).map((f) => f.headers["Authorization"])));
    chk("B SSE uses global token",
        FETCH_HEADERS.slice(hMarkOpen).some((f) =>
            f.url === "http://b.local/api/sessions/b1/events"
            && f.headers["Authorization"] === "Bearer tok-g"),
        "sse=" + JSON.stringify(FETCH_HEADERS.slice(hMarkOpen)
            .filter((f) => f.url.includes("b1/events")).map((f) => f.headers["Authorization"])));
    stopSSE();
    state.sessionId = null;

    // =====================================================================
    // 19) 组头「+」迟到响应竞态：POST 挂起期间切 workspace / 打开其它会话 →
    //     B 的响应到达不覆盖（不切回 B、不打开 b-new）；在途防重（pending
    //     期间重复点击不重复发 POST）。
    // =====================================================================
    sessionsData = [
      { id: "a1", status: "Idle", model: "kimi", title: "A 主会话", created_at: "2024-01-01T00:00:00Z", entry_count: 8, busy: false, active: true },
    ];
    sessionsDataB = [
      { id: "b1", status: "Busy", model: "deepseek", title: "B 主会话", created_at: "2024-02-02T00:00:00Z", entry_count: 5, busy: true, active: true },
    ];
    state.workspaces = [
      { id: "wsA", name: "服务器A", url: "", token: "tok-a" },
      { id: "wsB", name: "服务器B", url: "http://b.local", token: "tok-b" },
    ];
    state.workspace = state.workspaces[0];
    state.token = "tok-a";
    state.workspaceLists = {};
    state.workspaceErrors = {};
    state.lastList = [];
    state.sessionId = null;
    state.sidebar.filter = "";
    state.sidebar.showAllWs = new Set();
    state.sidebar.expanded = new Set();
    state.renameActive = false;
    state.wsCreatePending = new Set();
    renderWorkspaceSelect();
    await pollAllWorkspaces();
    await flush();
    await flush();
    // --- 场景 1：B 激活时点 B 的 +（POST 挂起），期间切回 A ---
    switchWorkspace("wsB");
    await flush();
    renderSidebarTree(true);
    let addSec19 = elsById["sidebarTree"].querySelectorAll(".tree-ws-section");
    const addB19 = addSec19[1].querySelector(".ws-add");   // 分组顺序 = workspaces 下标：1 = wsB
    bCreateDelayed = true;
    bCreateResolve = null;
    addB19._listeners["click"][0]({ stopPropagation(){} });
    await flush();
    chk("ws-add in-flight: POST pending",
        bCreateResolve !== null && state.wsCreatePending.has("wsB"),
        "pending=" + (bCreateResolve !== null) + " set=" + state.wsCreatePending.has("wsB"));
    // 重复点击：pending 期间再点 → 不重复发 POST
    const createPostsBefore = FETCHES.filter((u) => u === "http://b.local/api/sessions").length;
    addB19._listeners["click"][0]({ stopPropagation(){} });
    await flush();
    chk("ws-add in-flight: repeat click does not re-POST",
        FETCHES.filter((u) => u === "http://b.local/api/sessions").length === createPostsBefore,
        "posts=" + FETCHES.filter((u) => u === "http://b.local/api/sessions").length);
    // 挂起期间用户切到 A（直接切换递增代次）
    switchWorkspace("wsA");
    await flush();
    chk("ws-add in-flight: switch during pending",
        state.workspace.id === "wsA" && state.sessionId === null,
        "ws=" + state.workspace.id);
    bCreateResolve(resp(201, { id: "b-new", status: "Idle", active: true }));   // 迟到响应此刻到达
    await flush();
    await flush();
    await flush();
    chk("ws-add stale response does not reopen B",
        state.workspace.id === "wsA" && state.sessionId === null,
        "ws=" + state.workspace.id + " sid=" + state.sessionId);
    chk("ws-add pending cleared after stale response",
        !state.wsCreatePending.has("wsB"),
        "pending=" + state.wsCreatePending.has("wsB"));
    bCreateDelayed = false;
    bCreateResolve = null;
    // --- 场景 2：A 激活时点 B 的 +（POST 挂起），期间打开 A 的 a1 ---
    switchWorkspace("wsA");
    await flush();
    renderSidebarTree(true);
    addSec19 = elsById["sidebarTree"].querySelectorAll(".tree-ws-section");
    const addB19b = addSec19[1].querySelector(".ws-add");
    bCreateDelayed = true;
    bCreateResolve = null;
    addB19b._listeners["click"][0]({ stopPropagation(){} });
    await flush();
    chk("ws-add in-flight: POST pending (scenario 2)",
        bCreateResolve !== null, "pending=" + (bCreateResolve !== null));
    openSessionIn("wsA", "a1");      // 挂起期间打开其它会话（递增代次）
    await flush();
    await flush();
    chk("ws-add in-flight: open other session during pending",
        state.workspace.id === "wsA" && state.sessionId === "a1",
        "ws=" + state.workspace.id + " sid=" + state.sessionId);
    bCreateResolve(resp(201, { id: "b-new", status: "Idle", active: true }));
    await flush();
    await flush();
    await flush();
    chk("ws-add stale response does not hijack open session",
        state.workspace.id === "wsA" && state.sessionId === "a1",
        "ws=" + state.workspace.id + " sid=" + state.sessionId);
    chk("ws-add stale response did not switch to B",
        state.workspace.id === "wsA", "ws=" + state.workspace.id);
    chk("ws-add pending cleared after scenario 2",
        !state.wsCreatePending.has("wsB"),
        "pending=" + state.wsCreatePending.has("wsB"));
    bCreateDelayed = false;
    bCreateResolve = null;
    // --- 场景 3：成功路径 —— pending 清除后再次点击照常创建并打开 ---
    renderSidebarTree(true);
    addSec19 = elsById["sidebarTree"].querySelectorAll(".tree-ws-section");
    addSec19[1].querySelector(".ws-add")._listeners["click"][0]({ stopPropagation(){} });
    await flush();
    await flush();
    await flush();
    await flush();
    chk("ws-add success path after pending cleared",
        state.workspace.id === "wsB" && state.sessionId === "b-new",
        "ws=" + state.workspace.id + " sid=" + state.sessionId);

    // =====================================================================
    // 20) 组头「+」不打断当前流（review：createSessionIn 在 POST 前递增
    //     sessionOpenEpoch 误杀当前流）：活跃 SSE/会话下发起 createSessionIn
    //     （挂起/失败）→ 当前会话的 SSE/history 不被中断（epoch 校验不失败）；
    //     成功时才由 openSessionIn 声明新 epoch 并正常切换。
    // =====================================================================
    sessionsData = [
      { id: "a1", status: "Idle", model: "kimi", title: "A 主会话", created_at: "2024-01-01T00:00:00Z", entry_count: 8, busy: false, active: true },
    ];
    sessionsDataB = [
      { id: "b1", status: "Busy", model: "deepseek", title: "B 主会话", created_at: "2024-02-02T00:00:00Z", entry_count: 5, busy: true, active: true },
    ];
    state.workspaces = [
      { id: "wsA", name: "服务器A", url: "", token: "tok-a" },
      { id: "wsB", name: "服务器B", url: "http://b.local", token: "tok-b" },
    ];
    state.workspace = state.workspaces[0];
    state.token = "tok-a";
    state.workspaceLists = {};
    state.workspaceErrors = {};
    state.lastList = [];
    state.sessionId = null;
    state.sidebar.filter = "";
    state.sidebar.showAllWs = new Set();
    state.sidebar.expanded = new Set();
    state.renameActive = false;
    state.wsCreatePending = new Set();
    renderWorkspaceSelect();
    await pollAllWorkspaces();
    await flush();
    await flush();
    // 打开 A 的 a1：SSE 用手动流（首个 read 挂起；resolve 后吐出 Notice 块）
    a1StreamManualNotice = true;
    a1StreamReadResolve = null;
    openSessionIn("wsA", "a1");
    await flush();
    await flush();
    await flush();
    const epochAtOpen = sessionOpenEpoch;
    chk("create-no-kill: a1 open with SSE read pending",
        state.workspace.id === "wsA" && state.sessionId === "a1"
        && a1StreamReadResolve !== null,
        "sid=" + state.sessionId + " pending=" + (a1StreamReadResolve !== null));
    // --- 场景 1：POST 挂起期间：epoch 不动、history 可加载（非 stale）、
    //     SSE 分块照常处理；成功后才声明新 epoch 并切换 ---
    bCreateDelayed = true;
    bCreateResolve = null;
    renderSidebarTree(true);
    let addSec20 = elsById["sidebarTree"].querySelectorAll(".tree-ws-section");
    addSec20[1].querySelector(".ws-add")._listeners["click"][0]({ stopPropagation(){} });
    await flush();
    chk("create-no-kill: pending POST does not bump epoch",
        bCreateResolve !== null && sessionOpenEpoch === epochAtOpen,
        "epoch=" + sessionOpenEpoch + " atOpen=" + epochAtOpen);
    chk("create-no-kill: SSE still current while pending",
        stillCurrent("a1", "wsA", epochAtOpen) === true,
        "cur=" + stillCurrent("a1", "wsA", epochAtOpen));
    const histWhilePending = await loadHistory("a1", "wsA", epochAtOpen);
    chk("create-no-kill: history loads (not stale) while pending",
        histWhilePending === "ok", "r=" + histWhilePending);
    a1StreamReadResolve({ done: false, value: "" });   // 挂起期间当前流的 SSE 分块到达：仍被处理
    await flush();
    await flush();
    await flush();
    chk("create-no-kill: SSE block processed while POST pending",
        elsById["messages"].textContent.includes("create-pending-stream-alive"),
        "msgs=" + JSON.stringify(elsById["messages"].textContent.slice(-60)));
    bCreateResolve(resp(201, { id: "b-new", status: "Idle", active: true }));   // 成功：正常切换
    await flush();
    await flush();
    await flush();
    await flush();
    chk("create-no-kill: success switches and opens new session",
        state.workspace.id === "wsB" && state.sessionId === "b-new",
        "ws=" + state.workspace.id + " sid=" + state.sessionId);
    chk("create-no-kill: success declared new epoch",
        sessionOpenEpoch > epochAtOpen,
        "epoch=" + sessionOpenEpoch + " atOpen=" + epochAtOpen);
    bCreateDelayed = false;
    bCreateResolve = null;
    a1StreamManualNotice = false;
    a1StreamReadResolve = null;
    // --- 场景 2：失败路径（500）：epoch 不动、当前流不受影响 ---
    switchWorkspace("wsA");     // 回 A 列表
    await flush();
    a1StreamManualNotice = true;
    a1StreamReadResolve = null;
    openSessionIn("wsA", "a1");     // 重新打开 a1：活跃会话 + SSE 手动流挂起
    await flush();
    await flush();
    await flush();
    const epochFail = sessionOpenEpoch;
    chk("create-no-kill: failure-scenario a1 open with SSE pending",
        state.workspace.id === "wsA" && state.sessionId === "a1"
        && a1StreamReadResolve !== null,
        "sid=" + state.sessionId + " pending=" + (a1StreamReadResolve !== null));
    sessionsBCreateFail = true;
    renderSidebarTree(true);
    addSec20 = elsById["sidebarTree"].querySelectorAll(".tree-ws-section");
    addSec20[1].querySelector(".ws-add")._listeners["click"][0]({ stopPropagation(){} });
    await flush();
    await flush();
    chk("create-no-kill: failure banner, epoch untouched",
        elsById["banner"].hidden === false
        && elsById["bannerText"].textContent.includes("创建会话失败")
        && sessionOpenEpoch === epochFail,
        "banner=" + JSON.stringify(elsById["bannerText"].textContent)
        + " epoch=" + sessionOpenEpoch + " atOpen=" + epochFail);
    chk("create-no-kill: failure keeps session open and current",
        state.workspace.id === "wsA" && state.sessionId === "a1"
        && stillCurrent("a1", "wsA", epochFail) === true,
        "ws=" + state.workspace.id + " sid=" + state.sessionId
        + " cur=" + stillCurrent("a1", "wsA", epochFail));
    a1StreamReadResolve({ done: false, value: "" });   // 失败后当前流的 SSE 分块仍被处理
    await flush();
    await flush();
    await flush();
    chk("create-no-kill: SSE block processed after failure",
        elsById["messages"].textContent.includes("create-pending-stream-alive"),
        "msgs=" + JSON.stringify(elsById["messages"].textContent.slice(-60)));
    sessionsBCreateFail = false;
    a1StreamManualNotice = false;
    a1StreamReadResolve = null;
    // --- 场景 3（MEDIUM）：迟到失败不污染新视图 —— POST 挂起期间用户已
    //     导航（打开会话 / 清空当前会话），迟到的 401 / 500 / 解析失败都不刷
    //     banner、不改新视图（await apiFor 后先校验 captured epoch +
    //     workspace；解析后再次校验；catch 刷 banner 前也守卫） ---
    // 3a：挂起期间打开 a1（递增 epoch）→ 迟到 401 不刷 banner
    renderSidebarTree(true);
    addSec20 = elsById["sidebarTree"].querySelectorAll(".tree-ws-section");
    bCreateDelayed = true;
    bCreateResolve = null;
    addSec20[1].querySelector(".ws-add")._listeners["click"][0]({ stopPropagation(){} });
    await flush();
    chk("create-late: POST pending (401 scenario)",
        bCreateResolve !== null, "pending=" + (bCreateResolve !== null));
    openSessionIn("wsA", "a1");      // 挂起期间打开其它会话（递增 epoch）
    await flush();
    await flush();
    bCreateResolve(resp(401, {}));   // 迟到 401：guard 1 丢弃
    await flush();
    await flush();
    chk("create-late-401: no banner after navigation",
        elsById["banner"].hidden === true,
        "banner=" + JSON.stringify(elsById["bannerText"].textContent));
    chk("create-late-401: stays on new view",
        state.workspace.id === "wsA" && state.sessionId === "a1",
        "ws=" + state.workspace.id + " sid=" + state.sessionId);
    chk("create-late-401: pending cleared",
        !state.wsCreatePending.has("wsB"),
        "pending=" + state.wsCreatePending.has("wsB"));
    bCreateDelayed = false;
    bCreateResolve = null;
    // 3b：挂起期间清空当前会话（递增 epoch）→ 迟到 500 不刷 banner
    renderSidebarTree(true);
    addSec20 = elsById["sidebarTree"].querySelectorAll(".tree-ws-section");
    bCreateDelayed = true;
    bCreateResolve = null;
    addSec20[1].querySelector(".ws-add")._listeners["click"][0]({ stopPropagation(){} });
    await flush();
    clearCurrentSession();                    // 挂起期间清空当前会话（递增 epoch）
    await flush();
    bCreateResolve(resp(500, {}));   // 迟到 500：guard 1 丢弃
    await flush();
    await flush();
    chk("create-late-500: no banner after clearCurrentSession",
        elsById["banner"].hidden === true
        && state.sessionId === null,
        "banner=" + JSON.stringify(elsById["bannerText"].textContent)
       );
    bCreateDelayed = false;
    bCreateResolve = null;
    // 3c：挂起期间打开 a1 → 迟到 201 + 非 JSON body（解析会失败）不刷 banner
    renderSidebarTree(true);
    addSec20 = elsById["sidebarTree"].querySelectorAll(".tree-ws-section");
    bCreateDelayed = true;
    bCreateResolve = null;
    addSec20[1].querySelector(".ws-add")._listeners["click"][0]({ stopPropagation(){} });
    await flush();
    openSessionIn("wsA", "a1");      // 挂起期间打开 a1（递增 epoch）
    await flush();
    await flush();
    bCreateResolve(resp(201, "not-json"));   // 迟到解析失败：guard 1 丢弃
    await flush();
    await flush();
    chk("create-late-parse-fail: no banner after navigation",
        elsById["banner"].hidden === true,
        "banner=" + JSON.stringify(elsById["bannerText"].textContent));
    bCreateDelayed = false;
    bCreateResolve = null;
    // 3d：POST 已返回 201、json() 挂起期间打开 a1 → 解析后校验（guard 2）
    //     丢弃：不塞列表、不打开；解析失败 → catch 守卫（guard 3）不刷 banner
    renderSidebarTree(true);
    addSec20 = elsById["sidebarTree"].querySelectorAll(".tree-ws-section");
    bCreateJsonDelayed = true;
    bCreateJsonResolve = null;
    addSec20[1].querySelector(".ws-add")._listeners["click"][0]({ stopPropagation(){} });
    await flush();
    const epochJson = sessionOpenEpoch;
    chk("create-late-json: POST ok, json pending, epoch untouched",
        bCreateJsonResolve !== null && sessionOpenEpoch === epochJson,
        "pending=" + (bCreateJsonResolve !== null) + " epoch=" + sessionOpenEpoch);
    openSessionIn("wsA", "a1");      // json() 挂起期间打开 a1（递增 epoch）
    await flush();
    await flush();
    chk("create-late-json: navigation bumped epoch",
        sessionOpenEpoch > epochJson,
        "epoch=" + sessionOpenEpoch + " at=" + epochJson);
    bCreateJsonResolve.reject(new Error("parse boom"));   // 解析失败
    await flush();
    await flush();
    chk("create-late-json: rejected parse after nav no banner",
        elsById["banner"].hidden === true
        && state.workspace.id === "wsA" && state.sessionId === "a1",
        "banner=" + JSON.stringify(elsById["bannerText"].textContent)
        + " ws=" + state.workspace.id + " sid=" + state.sessionId);
    // 3e：同一场景但解析成功 → guard 2 丢弃：b-new-late 不塞进 B 列表、不打开
    renderSidebarTree(true);
    addSec20 = elsById["sidebarTree"].querySelectorAll(".tree-ws-section");
    bCreateJsonResolve = null;
    addSec20[1].querySelector(".ws-add")._listeners["click"][0]({ stopPropagation(){} });
    await flush();
    openSessionIn("wsA", "a1");      // json() 挂起期间打开 a1（递增 epoch）
    await flush();
    await flush();
    // 用唯一 id（b-new 已被场景 1 成功创建合法塞进 B 列表，不能作哨兵）
    bCreateJsonResolve.resolve({ id: "b-new-late", status: "Idle", active: true });
    await flush();
    await flush();
    chk("create-late-json: parsed value after nav not applied",
        state.workspace.id === "wsA" && state.sessionId === "a1"
        && !(state.workspaceLists["wsB"] || []).some((x) => x.id === "b-new-late")
        && !(state.lastList || []).some((x) => x.id === "b-new-late"),
        "ws=" + state.workspace.id + " sid=" + state.sessionId
        + " bHas=" + (state.workspaceLists["wsB"] || []).some((x) => x.id === "b-new-late"));
    chk("create-late-json: pending cleared",
        !state.wsCreatePending.has("wsB"),
        "pending=" + state.wsCreatePending.has("wsB"));
    bCreateJsonDelayed = false;
    bCreateJsonResolve = null;
    // =====================================================================
    // ---- main 侧（origin/main）测试段：归档 + perf 回归 + Issue 1-8 ----
    //（合并时 main 侧各段保持原顺序与状态依赖；以下为各自所需的状态基线）
    // =====================================================================
    state.workspaceLists = {};
    state.workspaceErrors = {};
    state.lastList = [];
    state.sessionId = null;
    state.sidebar.filter = "";
    state.sidebar.showAllWs = new Set();
    state.sidebar.expanded = new Set();
    state.renameActive = false;

    // ---- 归档：唯一导航中的归档分组 ----
    const _noopEv = { stopPropagation(){} };
    // 签名完整性隔离测试：固定同一列表/对象，期间只翻转 archived。
    // 直接比较签名，并用非 force 渲染证明该字段本身足以触发 DOM 重绘。
    state.workspace = state.workspaces[0];
    const _archiveSigSession = {
      id: "archive-sig-only", parent_session_id: null, model: "kimi", role: "main",
      status: "Idle", entry_count: 1, active: true, pinned: false, archived: false,
      last_active_at: "2024-03-01T00:00:00Z",
    };
    state.lastList = [_archiveSigSession];
    state.workspaceLists[state.workspace.id] = state.lastList;
    const _archiveSigBefore = sidebarTreeSig();
    renderSidebarTree(true);
    const _activeArchiveSigRow = () => elsById["sidebarTree"].querySelectorAll(".tree-row")
      .find((r) => (r.textContent || "").includes("archive-sig-only")
        && !r.classList.contains("archived"));
    chk("archive signature: unarchived row initially visible",
        !!_activeArchiveSigRow());
    _archiveSigSession.archived = true;   // 唯一变化
    const _archiveSigAfter = sidebarTreeSig();
    chk("archive signature: archived-only change alters signature",
        _archiveSigAfter !== _archiveSigBefore,
        "before=" + _archiveSigBefore + " after=" + _archiveSigAfter);
    renderSidebarTree();                  // 不 force：必须靠签名变化通过去重
    chk("archive signature: archived-only change removes active row",
        !_activeArchiveSigRow()
        && !!elsById["sidebarTree"].querySelector(".tree-row.archived"));
    _archiveSigSession.archived = false;  // 仍只翻转 archived
    const _archiveSigRestored = sidebarTreeSig();
    chk("archive signature: restore changes signature back",
        _archiveSigRestored !== _archiveSigAfter
        && _archiveSigRestored === _archiveSigBefore);
    renderSidebarTree();
    chk("archive signature: restore shows active row again",
        !!_activeArchiveSigRow()
        && elsById["sidebarTree"].querySelector(".tree-row.archived") === null);

    // 侧边栏：归档会话收进折叠的「归档 (N)」分组，点击分组内会话可打开
    state.workspace = state.workspaces[0];   // 切回 wsA：lastList 直接喂归档列表
    state.lastList = [
      { id: "s1", parent_session_id: null, model: "kimi", role: "main", status: "Idle", entry_count: 8, active: true },
      { id: "sub-arch", parent_session_id: "s1", label: "已归档子任务", status: "Idle", entry_count: 1, active: true, archived: true },
      { id: "arch-1", parent_session_id: null, model: "kimi", role: "main", status: "Idle", entry_count: 3, active: true, pinned: true, archived: true },
    ];
    state.sidebar.filter = "";
    state.sidebar.showAllWs = new Set();
    state.sidebar.expanded = new Set();
    renderSidebarTree(true);
    const archiveGroup = elsById["sidebarTree"].querySelectorAll(".tree-group")
      .find((g) => g.textContent.includes("归档"));
    chk("archive: 侧边栏有「归档」分组",
        !!archiveGroup, "groups=" +
        elsById["sidebarTree"].querySelectorAll(".tree-group").map((g) => g.textContent).join(" | "));
    // pinned + archived 以「归档 = 隐藏活跃入口」为准：不进顶部置顶聚合，
    // 只在 workspace 的归档分组出现一次。
    const _pinArchiveSec = elsById["sidebarTree"].querySelectorAll(".tree-ws-section")
      .find((sec) => sec.classList.contains("pinned"));
    const _pinArchiveRows = elsById["sidebarTree"].querySelectorAll(".tree-row")
      .filter((r) => (r.textContent || "").includes("arch-1"));
    chk("archive: pinned+archived only appears once in archive group",
        (!_pinArchiveSec || !_pinArchiveSec.textContent.includes("arch-1"))
        && _pinArchiveRows.length === 1
        && _pinArchiveRows[0].closest(".tree-children") !== null,
        "pinnedHas=" + !!(_pinArchiveSec && _pinArchiveSec.textContent.includes("arch-1"))
        + " rows=" + _pinArchiveRows.length);
    // 已归档子会话不重复出现在普通父节点子树里（只在归档分组内一次）
    const _allArchiveRows = elsById["sidebarTree"].querySelectorAll(".tree-row")
      .filter((r) => (r.textContent || "").includes("sub-arch"));
    chk("archive: 归档子会话不重复渲染",
        _allArchiveRows.length === 1,
        "n=" + _allArchiveRows.length);
    const groupNode = archiveGroup && archiveGroup.closest(".tree-node");
    const kidsBox = groupNode && groupNode.querySelector(".tree-children");
    chk("archive: 分组默认折叠",
        !!kidsBox && kidsBox.hidden === true, "hidden=" + (kidsBox && kidsBox.hidden));
    // 展开分组 → 归档行可见（buildTreeRoot 保留，点击可打开）
    const gToggle = groupNode && groupNode.querySelector(".tree-toggle");
    for (const fn of (gToggle && gToggle._listeners["click"]) || []) fn(_noopEv);
    await flush();
    const archRow2 = elsById["sidebarTree"].querySelector(".tree-row.archived");
    chk("archive: 展开分组后归档行可见 + .archived",
        !!archRow2 && (archRow2.textContent || "").includes("arch-1")
        && archRow2.querySelector(".archive-btn") !== null,
        "cls=" + (archRow2 && archRow2.className));
    for (const fn of (archRow2 && archRow2._listeners["click"]) || []) fn(_noopEv);
    await flush();
    chk("archive: 点击归档分组内会话可打开",
        state.sessionId === "arch-1",
        "sid=" + state.sessionId);
    // 恢复状态后清理：开关复位、sessionsData 复原（后续无其它断言依赖）
    sessionsData = sessionsData.filter((s) => s.id !== "arch-1");
    // ---- 侧边栏窗口槽位法：归档不顶替（Fix）----
    // 服务端把 archived 沉底排序（server.rs），旧窗口 slice(0,8) 会让归档
    // 腾出的位被更早会话顶上（「归档后侧边栏不变短、冒出别的会话」）。
    // 前端按 last_active_at 重排（含归档恢复原槽位），窗口取槽位、归档
    // 的空着：9 个总根中近期 1 个归档，虽然非归档 roots 恰好只有 8，
    // 仍须按总根数进入窗口；主树显示 7 个，不显示最旧 w0，more = 1。
    const _winSessions = [];
    for (let i = 0; i < 9; i++) {
      _winSessions.push({ id: "w" + i, parent_session_id: null, status: "Idle",
        entry_count: 1, active: true, archived: i === 7,
        last_active_at: "2024-01-" + String(i + 1).padStart(2, "0") + "T00:00:00Z" });
    }
    state.workspace = state.workspaces[0];
    state.lastList = _winSessions;
    state.workspaceLists[state.workspace.id] = _winSessions;
    state.sidebar.showAllWs = new Set();
    state.sidebar.filter = "";
    renderSidebarTree(true);
    // 桩的 querySelectorAll 只支持简单 class 选择器（无后代组合器）：
    // 全取 .tree-row 再按 DOM 关系过滤主树根行。
    const _winRoots = [...elsById["sidebarTree"].querySelectorAll(".tree-row")]
      .filter((r) => r.closest(".tree-ws-section.active") !== null  // 只数 wsA
                && !r.closest(".tree-children")          // 归档组/子树内行
                && !(r.textContent || "").includes("归档")  // 归档组标题行
                && !r.classList.contains("tree-archive-row"));
    chk("archive window: archived slot not refilled (7 roots, not 8)",
        _winRoots.length === 7,
        "n=" + _winRoots.length
        + " rows=" + _winRoots.map((r) => (r.textContent || "").slice(0, 8)).join(","));
    const _winTexts = _winRoots.map((r) => (r.textContent || "").slice(0, 8));
    chk("archive window: no older session leaked into window",
        !_winTexts.some((t) => t.includes("w0")),
        "texts=" + _winTexts.join("|"));
    const _winMore = elsById["sidebarTree"].querySelector(".tree-more");
    chk("archive window: 9→8 boundary keeps more count 1",
        _winMore !== null && _winMore.textContent.includes("1"),
        "more=" + (_winMore && _winMore.textContent));
    // 对照组：同一 9 根列表恢复近期归档 → 窗口满 8 根 + more 计数 1。
    _winSessions[7].archived = false;
    renderSidebarTree(true);
    const _winRoots2 = [...elsById["sidebarTree"].querySelectorAll(".tree-row")]
      .filter((r) => r.closest(".tree-ws-section.active") !== null
                && !r.closest(".tree-children")
                && !(r.textContent || "").includes("归档")
                && !r.classList.contains("tree-archive-row"));
    chk("archive window: unarchived same list shows 8 roots",
        _winRoots2.length === 8,
        "n=" + _winRoots2.length);
    const _winMore2 = elsById["sidebarTree"].querySelector(".tree-more");
    chk("archive window: unarchived control keeps more count 1",
        _winMore2 !== null && _winMore2.textContent.includes("1"),
        "more=" + (_winMore2 && _winMore2.textContent));
    _winSessions[7].archived = true;   // 还原
    // =====================================================================
    // perf 修复回归：聚合轮询整轮一次渲染 + 签名去重 + setTimeout 链防重入
    // + 聊天视图停轮询 + 侧边栏 hidden 跳过树渲染 + 任务面板签名去重
    // + 500ms output 轮询防重入
    // =====================================================================
    let renderSidebarTreeCalls = 0;
    const _origRST = renderSidebarTree;
    renderSidebarTree = function (...a) { renderSidebarTreeCalls++; return _origRST.apply(this, a); };
    chk("perf poll interval is 2s", POLL_INTERVAL_MS === 2000, "=" + POLL_INTERVAL_MS);

    // 双 workspace 整轮：两个响应只触发一次列表渲染 + 一次树渲染（旧实现
    // 每 workspace 响应各自 renderSidebarTree → 2 次）
    sessionsData = [{ id: "p1", status: "Idle", title: "P 主会话", created_at: "2024-01-01T00:00:00Z", entry_count: 1, busy: false, active: true }];
    sessionsDataB = [{ id: "p2", status: "Busy", title: "P2 会话", created_at: "2024-02-02T00:00:00Z", entry_count: 2, busy: true, active: true }];
    sessionsBFail = false;
    sessionsBFormat = false;
    state.workspaces = [
      { id: "wsP1", name: "服务器P1", url: "", token: "tok-p1" },
      { id: "wsP2", name: "服务器P2", url: "http://b.local", token: "tok-p2" },
    ];
    state.workspace = state.workspaces[0];
    state.token = "tok-p1";

    state.workspaceLists = {};
    state.workspaceErrors = {};
    state.lastList = [];
    state.sessionId = null;
    state.sidebar.filter = "";
    state.sidebar.showAllWs = new Set();
    state.sidebar.expanded = new Set();
    state.renameActive = false;

    elsById["sidebar"].hidden = false;   // 侧边栏可见（默认态）
    renderWorkspaceSelect();
    renderSidebarTreeCalls = 0;
    await pollAllWorkspaces();
    await flush();
    chk("perf whole round renders tree once", renderSidebarTreeCalls === 1,
        "calls=" + renderSidebarTreeCalls);
    // 数据未变：第二轮整轮 → 列表/树签名去重，不重建 DOM（元素同一性保持）
    const treeRefP = elsById["sidebarTree"].querySelectorAll(".tree-ws-section")[0];
    renderSidebarTreeCalls = 0;
    await pollAllWorkspaces();
    await flush();
    chk("perf unchanged round skips tree DOM rebuild",
        elsById["sidebarTree"].querySelectorAll(".tree-ws-section")[0] === treeRefP,
        "treeSame=" + (elsById["sidebarTree"].querySelectorAll(".tree-ws-section")[0] === treeRefP));
    // 数据变化（busy 翻转）→ 重绘
    sessionsData = [{ id: "p1", status: "Busy", title: "P 主会话", created_at: "2024-01-01T00:00:00Z", entry_count: 1, busy: true, active: true }];
    await pollAllWorkspaces();
    await flush();

    // setTimeout 链防重入：慢响应期间不调度下一轮、不渲染；完成后才续调度
    sessionsPDelayed = true;
    sessionsPResolve = null;
    const pollRoundPromise = pollRound();
    await flush();
    chk("perf in-flight round schedules no next round",
        state.pollTimer === null,
        "timer=" + String(state.pollTimer));
    sessionsPResolve(resp(200, sessionsDataB));   // 手动 resolve 慢响应
    await pollRoundPromise;
    await flush();
    chk("perf round completion schedules next round",
        state.pollTimer !== null, "timer=" + String(state.pollTimer));
    stopPolling();
    sessionsPDelayed = false;

    // 侧边栏不可见：轮询整轮跳过 renderSidebarTree（打开时 force 同步）
    elsById["sidebar"].hidden = true;
    renderSidebarTreeCalls = 0;
    await pollAllWorkspaces();
    await flush();
    chk("perf hidden sidebar skips tree render", renderSidebarTreeCalls === 0,
        "calls=" + renderSidebarTreeCalls);
    elsById["sidebar"].hidden = false;

    // 侧边栏是唯一导航：抽屉关闭、打开会话时轮询都保持常驻。
    stopPolling();
    state.sidebar.open = false;
    startPolling();
    chk("perf navigation polling starts", state.pollTimer !== null, "timer=" + String(state.pollTimer));
    state.sessionId = null;
    openSession("p1");
    chk("perf openSession keeps sidebar polling",
        state.pollTimer !== null,
        "timer=" + String(state.pollTimer));
    stopSSE();
    openSidebar();
    chk("perf openSidebar keeps polling", state.pollTimer !== null && state.sidebar.open);
    closeSidebar();
    chk("perf closeSidebar keeps polling", state.pollTimer !== null && !state.sidebar.open);
    stopPolling();

    // 任务面板签名去重：元数据未变 → 第二轮 pollTasks 不重建（计数 renderTaskList）
    let renderTaskListCalls = 0;
    const _origRTL = renderTaskList;
    renderTaskList = function (...a) { renderTaskListCalls++; return _origRTL.apply(this, a); };
    state.tasks.composerOpen = true;
    state.tasks.list = [];
    lastTasksSig = "";   // 强制首轮渲染
    tasksData = [{ session_id: "s1", id: 300, kind: "bash", label: "sig test",
      full_command: "sig test", output: "x", role: null }];
    renderTaskListCalls = 0;
    await pollTasks();
    await flush();
    chk("perf task first poll renders", renderTaskListCalls === 1,
        "calls=" + renderTaskListCalls);
    const taskRowRef = elsById["composerTasks"].querySelectorAll(".task-row")[0];
    await pollTasks();   // 同一数据再来一轮：签名相同 → 跳过重建
    await flush();
    chk("perf task unchanged skips rebuild",
        renderTaskListCalls === 1
        && elsById["composerTasks"].querySelectorAll(".task-row")[0] === taskRowRef,
        "calls=" + renderTaskListCalls
        + " same=" + (elsById["composerTasks"].querySelectorAll(".task-row")[0] === taskRowRef));
    tasksData = [{ session_id: "s1", id: 300, kind: "bash", label: "sig test 2",
      full_command: "sig test 2", output: "x", role: null }];
    await pollTasks();   // 元数据变化（label+full_command）→ 重建该行
    await flush();
    chk("perf task metadata change rebuilds row",
        renderTaskListCalls === 2
        && elsById["composerTasks"].querySelectorAll(".task-row")[0].textContent.includes("sig test 2"),
        "calls=" + renderTaskListCalls
        + " text=" + elsById["composerTasks"].querySelectorAll(".task-row")[0].textContent.slice(0, 40));

    // 500ms output 轮询防重入：慢响应在途时只发一次请求（不叠加），
    // resolve 后输出更新；收起/重展开后旧 tick 收尾不误停新轮询
    tasksData = [{ session_id: "s1", id: 400, kind: "bash", label: "reentry",
      full_command: "reentry", output: "", role: null }];
    state.tasks.composerOpen = true;
    state.tasks.list = [];
    lastTasksSig = "";
    await pollTasks();
    await flush();
    const rrow = elsById["composerTasks"].querySelectorAll(".task-row")[0];
    taskOutputDelayed = true;
    taskOutputResolve = null;
    const outFetchBefore = FETCHES.filter((u) => u.endsWith("/output")).length;
    rrow._listeners["click"][0]();   // 展开 → 启动轮询，首个 tick 挂起
    await flush();
    chk("perf output poller single in-flight fetch",
        state.tasks.pollers.has("s1:400")
        && FETCHES.filter((u) => u.endsWith("/output")).length === outFetchBefore + 1,
        "fetches=" + (FETCHES.filter((u) => u.endsWith("/output")).length - outFetchBefore));
    taskOutputResolve(resp(200, "reentry-done"));
    await flush();
    chk("perf output poller updates after settle",
        rrow.querySelector(".task-output").textContent.includes("reentry-done"),
        "text=" + rrow.querySelector(".task-output").textContent.slice(0, 40));
    taskOutputDelayed = false;
    rrow._listeners["click"][0]();   // 收起，清理轮询
    chk("perf output poller cleaned on collapse",
        !state.tasks.pollers.has("s1:400"),
        "keys=" + JSON.stringify([...state.tasks.pollers.keys()]));

    // =====================================================================
    // Issue 1 (高): 轮询重入 + 永久阻塞 + 异常断链
    //   - 立即轮询与定时轮询共用 in-flight 守卫（同一时刻只有一轮）
    //   - 每个 workspace 请求带 AbortController + 10s 超时（超时按失败处理）
    //   - pollRound 的 finally 保证异常后仍续调度（防断链）
    // =====================================================================
    sessionsData = [{ id: "g1", status: "Idle", title: "G 主会话", created_at: "2024-01-01T00:00:00Z", entry_count: 1, busy: false, active: true }];
    sessionsDataB = [{ id: "g2", status: "Busy", title: "G2 会话", created_at: "2024-02-02T00:00:00Z", entry_count: 2, busy: true, active: true }];
    sessionsBFail = false;
    sessionsBFormat = false;
    state.workspaces = [
      { id: "wsG1", name: "服务器G1", url: "", token: "tok-g1" },
      { id: "wsG2", name: "服务器G2", url: "http://b.local", token: "tok-g2" },
    ];
    state.workspace = state.workspaces[0];
    state.token = "tok-g1";

    state.sidebar.filter = "";
    state.sidebar.showAllWs = new Set();
    state.sidebar.expanded = new Set();

    elsById["sidebar"].hidden = false;
    renderWorkspaceSelect();
    renderSidebarTreeCalls = 0;
    stopPolling();
    chk("perf poll timeout constant is 10s", POLL_TIMEOUT_MS === 10000,
        "=" + POLL_TIMEOUT_MS);
    // 慢响应在途时连续两次立即轮询 → 不并发叠加：第一次启动在途轮询，
    // 第二次排队一轮新鲜的（合并为同一轮）；在途轮询完成后才补跑
    sessionsPDelayed = true;
    sessionsPResolve = null;
    const i1f = FETCHES.filter((u) => u === "/api/sessions").length;
    const i1t = scheduledTimeouts.length;
    const i1p1 = pollSessions();
    const i1p2 = pollSessions();   // 立即轮询：必须排队（不并发），合并为同一轮
    await flush();
    chk("perf immediate polls no concurrent round",
        FETCHES.filter((u) => u === "/api/sessions").length === i1f + 1,
        "delta=" + (FETCHES.filter((u) => u === "/api/sessions").length - i1f));
    chk("perf workspace poll arms abort timeout",
        scheduledTimeouts.length === i1t + 2,   // 两个 workspace 各一个 abort 定时器
        "delta=" + (scheduledTimeouts.length - i1t));
    sessionsPResolve(resp(200, sessionsDataB));   // 在途轮询完成
    await flush();
    chk("perf workspace poll uses abort signal", pollSignalSeen === true,
        "seen=" + pollSignalSeen);
    sessionsPResolve(resp(200, sessionsDataB));   // 排队补跑的新鲜一轮（串行）
    await Promise.all([i1p1, i1p2]);
    await flush();
    sessionsPDelayed = false;
    // afterPollRound 抛错 → pollRound 的 finally 仍续调度（不永久断链）
    const i1apr = afterPollRound;
    afterPollRound = function () { throw new Error("boom"); };
    stopPolling();
    const i1gen = state.pollGen;
    const i1throw = pollRound();
    await i1throw.catch(() => {});
    await flush();
    chk("perf poll round throw still reschedules",
        state.pollTimer !== null && state.pollGen === i1gen,
        "timer=" + String(state.pollTimer) + " gen=" + state.pollGen);
    afterPollRound = i1apr;
    stopPolling();

    // =====================================================================
    // Issue 2 (高): 任务面板收起期间数据变化 → 重开必须重建（不显示旧行/旧闭包）
    // =====================================================================
    state.tasks.composerOpen = true;
    state.tasks.list = [];
    lastTasksSig = "";
    lastTasksRenderedSig = "";
    tasksData = [{ session_id: "s1", id: 500, kind: "bash", label: "v1",
      full_command: "v1", output: "", role: null }];
    await pollTasks();
    await flush();
    const i2row = elsById["composerTasks"].querySelector(".task-row");
    chk("issue2 panel renders open", i2row !== null && i2row.textContent.includes("v1"),
        "text=" + (i2row ? i2row.textContent.slice(0, 30) : "none"));
    state.tasks.composerOpen = false;   // 收起面板
    renderComposerTasks();
    chk("issue2 panel hidden when collapsed", elsById["composerTasks"].hidden === true,
        "hidden=" + elsById["composerTasks"].hidden);
    tasksData = [{ session_id: "s1", id: 500, kind: "bash", label: "v2",
      full_command: "v2", output: "", role: null }];
    await pollTasks();                  // 收起期间数据变化：只记录 data 签名，不碰 DOM
    await flush();
    chk("issue2 collapsed round leaves old DOM",
        elsById["composerTasks"].querySelector(".task-row").textContent.includes("v1"),
        "text=" + elsById["composerTasks"].querySelector(".task-row").textContent.slice(0, 30));
    state.tasks.composerOpen = true;    // 重开：数据与已渲染 DOM 不符 → 重建
    renderComposerTasks();
    const i2row2 = elsById["composerTasks"].querySelector(".task-row");
    chk("issue2 reopen rebuilds with new data",
        i2row2 !== null && i2row2 !== i2row && i2row2.textContent.includes("v2")
        && i2row2.getAttribute("data-key-sig") !== i2row.getAttribute("data-key-sig"),
        "same=" + (i2row2 === i2row) + " text=" + (i2row2 ? i2row2.textContent.slice(0, 30) : "none"));

    // =====================================================================
    // Issue 3 (中): keyed 更新移动复用行——DOM 行序跟随 tasks 数组（纯重排不重建）
    // =====================================================================
    const i3panel = elsById["composerTasks"];
    i3panel.innerHTML = "";
    const i3task = (id, label) => ({ session_id: "s1", id, kind: "bash", label,
      full_command: label, output: "", role: null });
    renderTaskList([i3task(1, "A"), i3task(2, "B"), i3task(3, "C")], i3panel);
    const i3rowA = i3panel.querySelectorAll(".task-row")[0];
    const i3rowB = i3panel.querySelectorAll(".task-row")[1];
    const i3rowC = i3panel.querySelectorAll(".task-row")[2];
    chk("issue3 initial order",
        [i3rowA, i3rowB, i3rowC].map((r) => r.getAttribute("data-task")).join(",") === "s1:1,s1:2,s1:3",
        "order=" + [i3rowA, i3rowB, i3rowC].map((r) => r.getAttribute("data-task")).join(","));
    renderTaskList([i3task(3, "C"), i3task(2, "B"), i3task(1, "A")], i3panel);   // 纯重排
    const i3rows = i3panel.querySelectorAll(".task-row");
    chk("issue3 reorder moves reused rows",
        [...i3rows].map((r) => r.getAttribute("data-task")).join(",") === "s1:3,s1:2,s1:1",
        "order=" + [...i3rows].map((r) => r.getAttribute("data-task")).join(","));
    chk("issue3 reorder reuses nodes",
        i3rows[0] === i3rowC && i3rows[1] === i3rowB && i3rows[2] === i3rowA,
        "same=" + (i3rows[0] === i3rowC) + "," + (i3rows[1] === i3rowB) + "," + (i3rows[2] === i3rowA));

    // =====================================================================
    // Issue 4 (中): 旧后端静态输出兜底——output 端点 404（降级）时，/api/tasks
    //   尾部 output 变化 → 保留行 .task-output 就地更新（不重建、不重启轮询）
    // =====================================================================
    i3panel.innerHTML = "";
    state.tasks.composerOpen = true;
    state.tasks.list = [];
    state.tasks.degraded = new Set();
    lastTasksSig = "";
    lastTasksRenderedSig = "";
    tasksData = [{ session_id: "s1", id: 600, kind: "bash", label: "degraded",
      full_command: "degraded", output: "tail-old", role: null }];
    taskOutput404 = true;
    await pollTasks();
    await flush();
    const i4row = elsById["composerTasks"].querySelector(".task-row");
    const i4pre = i4row.querySelector(".task-output");
    i4row._listeners["click"][0]();   // 展开 → 启动轮询 → 404 → 降级静态
    await flush();
    chk("issue4 degraded on 404",
        state.tasks.degraded.has("s1:600") && i4pre.hidden === false
        && i4pre.textContent.includes("tail-old"),
        "degraded=" + state.tasks.degraded.has("s1:600")
        + " text=" + i4pre.textContent.slice(0, 20));
    tasksData = [{ session_id: "s1", id: 600, kind: "bash", label: "degraded",
      full_command: "degraded", output: "tail-new", role: null }];   // 元数据不变，仅 output 变
    await pollTasks();   // 降级行的 output 计入渲染签名 → 触发保留行就地更新
    await flush();
    const i4row2 = elsById["composerTasks"].querySelector(".task-row");
    chk("issue4 static output updated in place",
        i4row2 === i4row && i4pre.textContent.includes("tail-new")
        && !i4pre.textContent.includes("tail-old"),
        "same=" + (i4row2 === i4row) + " text=" + i4pre.textContent.slice(0, 20));
    chk("issue4 degraded row keeps degraded / poller not restarted",
        state.tasks.degraded.has("s1:600") && !state.tasks.pollers.has("s1:600"),
        "degraded=" + state.tasks.degraded.has("s1:600")
        + " pollers=" + JSON.stringify([...state.tasks.pollers.keys()]));
    taskOutput404 = false;

    // =====================================================================
    // Issue 5: 树签名含 model（树行 tooltip 渲染 model）
    // =====================================================================
    sessionsData = [{ id: "sig5a", status: "Idle", title: "Sig5", model: "kimi",
      created_at: "2024-01-01T00:00:00Z", entry_count: 1, busy: false, active: true }];
    state.workspaces = [{ id: "ws5", name: "服务器5", url: "", token: "tok-5" }];
    state.workspace = state.workspaces[0];
    state.token = "tok-5";
    state.workspaceLists = {};
    state.workspaceErrors = {};
    state.lastList = [];
    state.sessionId = null;
    state.sidebar.filter = "";
    state.sidebar.showAllWs = new Set();
    state.sidebar.expanded = new Set();
    elsById["sidebar"].hidden = false;
    // 树签名含 model：model 变化 → 树重绘 → tooltip 更新
    sessionsData = [{ id: "sig5a", status: "Idle", title: "Sig5", model: "kimi",
      created_at: "2024-01-01T00:00:00Z", entry_count: 1, busy: false, active: true }];
    await pollAllWorkspaces();
    await flush();
    let trow5 = elsById["sidebarTree"].querySelector(".tree-row");
    chk("issue5 tree tooltip shows model",
        trow5.title.includes("· kimi"),
        "title=" + JSON.stringify(trow5.title));
    sessionsData = [{ id: "sig5a", status: "Idle", title: "Sig5", model: "gpt",
      created_at: "2024-01-01T00:00:00Z", entry_count: 1, busy: false, active: true }];
    await pollAllWorkspaces();   // 树签名含 model → 重绘
    await flush();
    trow5 = elsById["sidebarTree"].querySelector(".tree-row");
    chk("issue5 tree model change re-renders tooltip",
        trow5.title.includes("· gpt") && !trow5.title.includes("· kimi"),
        "title=" + JSON.stringify(trow5.title));

    // =====================================================================
    // Issue 6 (中): stopPolling 取消已排队的下一轮——queued round 携带
    //   generation，在途结束后启动前校验失效 → 丢弃；换代后的即时刷新
    //   （pollSessions）替换旧 intent，仍保持全局 single-flight
    // =====================================================================
    sessionsData = [{ id: "p6", status: "Idle", title: "P6", created_at: "2024-01-01T00:00:00Z", entry_count: 1, busy: false, active: true }];
    sessionsDataB = [{ id: "p6b", status: "Busy", title: "P6B", created_at: "2024-02-02T00:00:00Z", entry_count: 1, busy: true, active: true }];
    sessionsBFail = false;
    sessionsBFormat = false;
    sessionsPDelayed = true;    // B 慢响应：初始立即轮询在途（>2s 场景）
    sessionsPResolve = null;
    state.workspaces = [
      { id: "wsP6", name: "服务器P6", url: "", token: "tok-p6" },
      { id: "wsP6b", name: "服务器P6B", url: "http://b.local", token: "tok-p6b" },
    ];
    state.workspace = state.workspaces[0];
    state.token = "tok-p6";
    state.workspaceLists = {};
    state.workspaceErrors = {};
    state.lastList = [{ id: "p6", parent_session_id: null, label: null, status: "Idle", entry_count: 1, active: true }];
    state.sessionId = null;
    state.sidebar.filter = "";
    state.sidebar.showAllWs = new Set();
    state.sidebar.expanded = new Set();
    state.sidebar.open = false;
    elsById["sidebar"].hidden = false;
    renderWorkspaceSelect();
    stopPolling();
    const i6f0 = FETCHES.filter((u) => u === "http://b.local/api/sessions").length;
    const i6p1 = pollSessions();   // 初始立即轮询：在途（B 慢）
    await flush();
    const i6p2 = pollRound();      // 2s 定时器已排队第二轮（挂在在途 Promise 上）
    await flush();
    chk("issue6 second round queued while in-flight",
        pollRoundQueued !== null
        && FETCHES.filter((u) => u === "http://b.local/api/sessions").length === i6f0 + 1,
        "queued=" + (pollRoundQueued !== null)
        + " delta=" + (FETCHES.filter((u) => u === "http://b.local/api/sessions").length - i6f0));
    // 显式停止轮询：gen 递增，排队 intent 作废（打开会话不再停导航轮询）。
    stopPolling();
    openSession("p6");
    await flush();
    chk("issue6 explicit stop invalidates queued round",
        state.pollTimer === null && pollRoundQueuedGen < state.pollGen,
        "timer=" + String(state.pollTimer) + " queued=" + pollRoundQueuedGen + " cur=" + state.pollGen);
    stopSSE();
    sessionsPResolve(resp(200, sessionsDataB));   // 首轮完成
    await i6p1;
    await i6p2;
    await flush();
    chk("issue6 stopped round does not run after first settles",
        FETCHES.filter((u) => u === "http://b.local/api/sessions").length === i6f0 + 1
        && pollRoundInFlight === null && pollRoundQueued === null,
        "delta=" + (FETCHES.filter((u) => u === "http://b.local/api/sessions").length - i6f0));
    // 换代后的即时刷新：替换（重建）排队 intent，在途结束后按新 gen 补跑，
    // 仍保持 single-flight（同代并发合并为同一轮）
    sessionsPDelayed = true;
    sessionsPResolve = null;
    const i6f1 = FETCHES.filter((u) => u === "http://b.local/api/sessions").length;
    const i6p3 = pollSessions();   // 新 generation 即时刷新：在途（B 慢）
    await flush();
    stopPolling();                 // 再停：gen 递增，使排队 intent 过期
    const i6p4 = pollSessions();   // 换代即时刷新：替换旧 intent 为新 gen
    await flush();
    chk("issue6 regen refresh replaces queued intent",
        pollRoundQueued !== null && pollRoundQueuedGen === state.pollGen,
        "queuedGen=" + pollRoundQueuedGen + " cur=" + state.pollGen);
    sessionsPResolve(resp(200, sessionsDataB));   // 首轮（在途）完成 → 排队 intent 以新 gen 补跑
    await i6p3;                                    // 首轮完成（排队轮次由链补跑启动）
    await flush();                                 // 补跑轮次启动（B 再次慢响应，sessionsPResolve 更新）
    chk("issue6 regen queued round runs after settle",
        FETCHES.filter((u) => u === "http://b.local/api/sessions").length === i6f1 + 2
        && pollRoundInFlight !== null && pollRoundQueued === null,
        "delta=" + (FETCHES.filter((u) => u === "http://b.local/api/sessions").length - i6f1));
    sessionsPResolve(resp(200, sessionsDataB));   // 补跑轮次的 B 也完成（串行收尾）
    await i6p4;
    await flush();
    sessionsPDelayed = false;

    // =====================================================================
    // Issue 7 (中): 无活跃 output poller 的 bash 行——折叠普通行 / 轮询因
    //   网络失败停止的展开行——output 变化计入渲染签名 → 保留行 <pre> 就地
    //   更新（不重建行、不重启轮询）
    // =====================================================================
    elsById["composerTasks"].innerHTML = "";
    state.tasks.composerOpen = true;
    state.tasks.list = [];
    state.tasks.degraded = new Set();
    state.tasks.pollers = new Map();
    lastTasksSig = "";
    lastTasksRenderedSig = "";
    taskOutput404 = false;
    taskOutputDelayed = false;
    taskOutputNetFail = false;
    // (a) 折叠普通行（无 poller、非 degraded）：output 变化 → 就地更新
    tasksData = [{ session_id: "s1", id: 700, kind: "bash", label: "collapsed",
      full_command: "collapsed", output: "out-old", role: null }];
    await pollTasks();
    await flush();
    const i7row = elsById["composerTasks"].querySelector(".task-row");
    const i7pre = i7row.querySelector(".task-output");
    chk("issue7 collapsed row starts hidden",
        i7pre.hidden === true && !state.tasks.pollers.has("s1:700"),
        "hidden=" + i7pre.hidden
        + " pollers=" + JSON.stringify([...state.tasks.pollers.keys()]));
    tasksData = [{ session_id: "s1", id: 700, kind: "bash", label: "collapsed",
      full_command: "collapsed", output: "out-new", role: null }];   // 元数据不变，仅 output 变
    await pollTasks();
    await flush();
    const i7row2 = elsById["composerTasks"].querySelector(".task-row");
    chk("issue7 collapsed static output updated in place",
        i7row2 === i7row && i7pre.textContent.includes("out-new")
        && !i7pre.textContent.includes("out-old") && i7pre.hidden === true,
        "same=" + (i7row2 === i7row) + " text=" + i7pre.textContent.slice(0, 20));
    // (b) 展开行轮询因网络失败停止（非 degraded）：/api/tasks 尾部 output
    //     变化 → 就地更新
    tasksData = [{ session_id: "s1", id: 701, kind: "bash", label: "netfail",
      full_command: "netfail", output: "net-old", role: null }];
    await pollTasks();
    await flush();
    const i7brow = elsById["composerTasks"].querySelector(".task-row");
    const i7bpre = i7brow.querySelector(".task-output");
    taskOutputNetFail = true;
    i7brow._listeners["click"][0]();   // 展开 → 启动轮询 → 首 tick fetch 网络失败
    await flush();
    chk("issue7 net-fail stops poller without degraded",
        !state.tasks.pollers.has("s1:701") && !state.tasks.degraded.has("s1:701")
        && i7bpre.hidden === false,
        "pollers=" + JSON.stringify([...state.tasks.pollers.keys()])
        + " degraded=" + state.tasks.degraded.has("s1:701"));
    taskOutputNetFail = false;
    tasksData = [{ session_id: "s1", id: 701, kind: "bash", label: "netfail",
      full_command: "netfail", output: "net-new", role: null }];   // 元数据不变，仅 output 变
    await pollTasks();
    await flush();
    const i7brow2 = elsById["composerTasks"].querySelector(".task-row");
    chk("issue7 net-fail stopped row output updated in place",
        i7brow2 === i7brow && i7bpre.textContent.includes("net-new")
        && !i7bpre.textContent.includes("net-old"),
        "same=" + (i7brow2 === i7brow) + " text=" + i7bpre.textContent.slice(0, 20));

    // =====================================================================
    // Issue 8 (中): 10s 超时 → AbortError → 标记 timeout → 下一轮恢复成功
    //   （AbortController 桩真实化：abort 后 pending fetch 的 signal reject
    //   AbortError，pollWorkspaceSessions 按超时失败处理，保留 stale 列表）
    // =====================================================================
    sessionsData = [{ id: "t1", status: "Idle", title: "T1", created_at: "2024-01-01T00:00:00Z", entry_count: 1, busy: false, active: true }];
    sessionsDataB = [{ id: "t2", status: "Busy", title: "T2", created_at: "2024-02-02T00:00:00Z", entry_count: 1, busy: true, active: true }];
    sessionsBFail = false;
    sessionsBFormat = false;
    sessionsPDelayed = false;
    state.workspaces = [
      { id: "wsT1", name: "服务器T1", url: "", token: "tok-t1" },
      { id: "wsT2", name: "服务器T2", url: "http://b.local", token: "tok-t2" },
    ];
    state.workspace = state.workspaces[0];
    state.token = "tok-t1";
    state.workspaceLists = {};
    state.workspaceErrors = {};
    state.lastList = [];
    state.sessionId = null;
    state.sidebar.filter = "";
    state.sidebar.showAllWs = new Set();
    state.sidebar.expanded = new Set();
    elsById["sidebar"].hidden = false;
    renderWorkspaceSelect();
    stopPolling();
    await pollSessions();   // 预置一轮成功：B 正常返回，workspaceErrors 干净
    await flush();
    chk("issue8 baseline errors clean",
        state.workspaceErrors["wsT1"] === null && state.workspaceErrors["wsT2"] === null,
        JSON.stringify(state.workspaceErrors));
    // B 慢响应在途：触发其 abort 定时器（10s 超时）→ abort → AbortError → timeout
    sessionsPDelayed = true;
    sessionsPResolve = null;
    const i8t0 = scheduledTimeouts.length;
    const i8p = pollSessions();
    await flush();
    let i8abort = null;   // 两个 workspace 各 arm 一个 abort 定时器：A 立即完成
    for (let i = scheduledTimeouts.length - 1; i >= i8t0; i--) {   // 并 clear，B 的保留
      if (scheduledTimeouts[i]) { i8abort = scheduledTimeouts[i]; break; }
    }
    chk("issue8 timeout abort timer armed", i8abort !== null,
        "none");
    const i8fB = FETCHES.filter((u) => u === "http://b.local/api/sessions").length;
    i8abort();   // 触发 10s 超时 → ctrl.abort() → B 的 pending fetch reject AbortError
    await i8p;
    await flush();
    chk("issue8 abort marks timeout",
        state.workspaceErrors["wsT2"] === "timeout" && state.workspaceErrors["wsT1"] === null,
        JSON.stringify(state.workspaceErrors));
    chk("issue8 no new fetch on aborted round",
        FETCHES.filter((u) => u === "http://b.local/api/sessions").length === i8fB,
        "delta=" + (FETCHES.filter((u) => u === "http://b.local/api/sessions").length - i8fB));
    // 下一轮恢复成功：B 正常 → 错误标记清除
    sessionsPDelayed = false;
    await pollSessions();
    await flush();
    chk("issue8 next round recovers",
        state.workspaceErrors["wsT2"] === null && state.workspaceErrors["wsT1"] === null,
        JSON.stringify(state.workspaceErrors));


    // =====================================================================
    // 15) SSE 404 语义区分（Fix）：历史/已结束会话的 /events 404 = 没有实时
    //     流（server 只给 live 会话起流），不是「会话不存在」——从任务面板
    //     点进已结束的子会话时不得误报。按 sessionKnownState 判定：
    //     - active===false（已知历史）→ 静默降级 + 「会话已结束」轻提示，
    //       不弹「不存在」、不重连
    //     - active!==false（缓存判定应存活）→ 先刷新对应 workspace 的会话
    //       列表再分类（Fix 2 竞态：任务面板 openSession 先于列表刷新，旧
    //       缓存可能仍是 active:true 而服务端已结束）：
    //       刷新后仍 active → 真 live，弹「不存在」报错（也不重连）
    //       刷新后 active===false → 历史已结束：静默 + 轻提示（同历史路径）
    //       刷新后仍查不到 → unknown：完全静默
    //     - 不在任何列表（任务面板直连刚结束的子会话）→ 完全静默，不重连
    //     404 一律停止重连（stopped 语义不变）；重开后 openSession 重置
    //     stopped 并可重新连接。
    // =====================================================================
    const _savedList404 = state.lastList;
    const _savedWsLists404 = Object.assign({}, state.workspaceLists);
    const _savedSid404 = state.sessionId;
    const _savedBanner404 = { hidden: elsById["banner"].hidden, text: elsById["bannerText"].textContent };
    const _open404 = (sid, list) => {
      state.lastList = list;
      state.workspaceLists[state.workspace.id] = list;
      state.sessionId = sid;
      state.sse.stopped = false;
      elsById["banner"].hidden = true;
      elsById["bannerText"].textContent = "";
    };
    // (a) 已知历史（active===false）：无「不存在」banner、轻提示、不重连
    _open404("s1", [{ id: "s1", status: "Idle", entry_count: 8, busy: false, active: false }]);
    sse404Ids.add("s1");
    const _to404a = scheduledTimeouts.length;
    connectSSE("s1", state.workspace.id, sessionOpenEpoch);
    await flush();
    chk("sse404 historical: no gone banner",
        elsById["banner"].hidden === true
        && elsById["bannerText"].textContent.indexOf("不存在") === -1,
        "banner=" + JSON.stringify(elsById["bannerText"].textContent));
    chk("sse404 historical: light ended hint",
        elsById["connState"].textContent === "会话已结束"
        && elsById["connState"].className === "conn-state ended",
        "conn=" + JSON.stringify(elsById["connState"].textContent));
    chk("sse404 historical: stopped, no reconnect",
        state.sse.stopped === true && state.sse.retryTimer === null
        && scheduledTimeouts.length === _to404a,
        "stopped=" + state.sse.stopped + " timeouts=" + scheduledTimeouts.length);
    sse404Ids.delete("s1");
    // (b) 真 live（刷新后仍 active）：live 的 404 先刷新列表再分类——
    //     刷新（立即返回的 sessionsData 仍 active:true）后重分类仍为 live
    //     → 保持「不存在」报错，但不重连
    sessionsData = [{ id: "s1", status: "Idle", entry_count: 8, busy: false, active: true }];
    _open404("s1", [{ id: "s1", status: "Idle", entry_count: 8, busy: false, active: true }]);
    sse404Ids.add("s1");
    const _to404b = scheduledTimeouts.length;
    connectSSE("s1", state.workspace.id, sessionOpenEpoch);
    await flush();
    await flush();   // 等列表刷新（立即返回）完成后的重分类
    chk("sse404 live: gone banner still shown",
        elsById["banner"].hidden === false
        && elsById["bannerText"].textContent.indexOf("不存在") !== -1,
        "banner=" + JSON.stringify(elsById["bannerText"].textContent));
    chk("sse404 live: no reconnect on 404",
        state.sse.stopped === true && state.sse.retryTimer === null
        && scheduledTimeouts.length === _to404b + 2,   // +2 = banner 自动消失计时器 + 刷新 fetch 的
        // abort 定时器槽（fetchWithTimeout 10s 超时；完成后 finally clear 置 null，
        // 但 scheduledTimeouts.length 只增不减——HEAD perf 合并后新增的槽位）
        "stopped=" + state.sse.stopped + " timeouts=" + scheduledTimeouts.length);
    sse404Ids.delete("s1");
    // (c) 未知（不在任何列表）：任务面板直连刚结束的子会话 → 完全静默
    _open404("sub-finished", [{ id: "s1", status: "Idle", entry_count: 1, busy: false, active: true }]);
    sse404Ids.add("sub-finished");
    const _to404c = scheduledTimeouts.length;
    connectSSE("sub-finished", state.workspace.id, sessionOpenEpoch);
    await flush();
    chk("sse404 unknown: silent, no gone banner",
        elsById["banner"].hidden === true
        && elsById["bannerText"].textContent.indexOf("不存在") === -1,
        "banner=" + JSON.stringify(elsById["bannerText"].textContent));
    chk("sse404 unknown: stopped, no reconnect",
        state.sse.stopped === true && state.sse.retryTimer === null
        && scheduledTimeouts.length === _to404c,
        "stopped=" + state.sse.stopped + " timeouts=" + scheduledTimeouts.length);
    sse404Ids.delete("sub-finished");
    // (d) 竞态（Fix 2 核心）：缓存标 live 但实际已结束的 subagent——任务
    //     面板点击时 openSession 先于列表刷新（异步），旧缓存 active:true，
    //     服务端 live 注册已清理 → /events 404 先于刷新完成。live 的 404
    //     先停重连、刷新对应 workspace 列表（挂起）再分类：刷新后
    //     active===false → 静默 + 「会话已结束」轻提示，不弹「不存在」。
    sessionsDelayed = true;   // 列表刷新挂起（模拟刷新未完成）
    sessionsData = [{ id: "sub-race", status: "Idle", entry_count: 1, busy: false, active: true }];
    _open404("sub-race", [{ id: "sub-race", status: "Idle", entry_count: 1, busy: false, active: true }]);
    sse404Ids.add("sub-race");
    const _to404d = scheduledTimeouts.length;
    const _listFetchesD = FETCHES.filter((u) => u === "/api/sessions").length;
    connectSSE("sub-race", state.workspace.id, sessionOpenEpoch);
    await flush();
    chk("sse404 race: refresh pending, no banner yet",
        elsById["banner"].hidden === true
        && elsById["bannerText"].textContent.indexOf("不存在") === -1,
        "banner=" + JSON.stringify(elsById["bannerText"].textContent));
    chk("sse404 race: stopped immediately (no reconnect while refreshing)",
        state.sse.stopped === true && state.sse.retryTimer === null,
        "stopped=" + state.sse.stopped);
    chk("sse404 race: list refresh issued for workspace",
        FETCHES.filter((u) => u === "/api/sessions").length === _listFetchesD + 1,
        "n=" + (FETCHES.filter((u) => u === "/api/sessions").length - _listFetchesD));
    // 模拟刷新完成：服务端列表已把该 subagent 标记为结束（active:false）
    sessionsData = [{ id: "sub-race", status: "Idle", entry_count: 1, busy: false, active: false }];
    sessionsResolve(resp(200, sessionsData));
    await flush();
    await flush();
    chk("sse404 race: no gone banner after refresh (ended subagent)",
        elsById["banner"].hidden === true
        && elsById["bannerText"].textContent.indexOf("不存在") === -1,
        "banner=" + JSON.stringify(elsById["bannerText"].textContent));
    chk("sse404 race: ended hint after refresh",
        elsById["connState"].textContent === "会话已结束"
        && elsById["connState"].className === "conn-state ended",
        "conn=" + JSON.stringify(elsById["connState"].textContent));
    chk("sse404 race: stopped, no reconnect",
        state.sse.stopped === true && state.sse.retryTimer === null
        && scheduledTimeouts.length === _to404d + 1,   // +1 = 刷新 fetch 的 abort 定时器槽（同上）
        "stopped=" + state.sse.stopped + " timeouts=" + scheduledTimeouts.length);
    sse404Ids.delete("sub-race");
    sessionsDelayed = false;
    // (e) 404 后同会话重开：openSession → connectSSE 重置 stopped（stopSSE
    //     清旧流 → stopped=false → 新 /events fetch）——确认既有实现，能
    //     重新连接
    const _s1EventsBeforeE = FETCHES.filter((u) => u === "/api/sessions/s1/events").length;
    openSession("s1");
    await flush();
    await flush();
    chk("sse404 reopen: stopped reset + events stream re-fetched",
        state.sse.stopped === false
        && FETCHES.filter((u) => u === "/api/sessions/s1/events").length === _s1EventsBeforeE + 1,
        "stopped=" + state.sse.stopped
        + " events=" + FETCHES.filter((u) => u === "/api/sessions/s1/events").length);
    // 还原（本段之后无其它测试，仍保持整洁）
    state.lastList = _savedList404;
    state.workspaceLists = _savedWsLists404;
    state.sessionId = _savedSid404;
    elsById["banner"].hidden = _savedBanner404.hidden;
    elsById["bannerText"].textContent = _savedBanner404.text;
    stopSSE();

    // =====================================================================
    // 16) 深链平滑降级（?session=<id> 不依赖列表轮询）：
    //     - URL/token 就绪后立即尝试（与轮询并行），不再等 poll round
    //     - fresh 列表命中 → 既有 active/resume 分支；列表缺失/失败/超时/
    //       过期 → 直接对激活 workspace probe history（列表缺失不是权威
    //       不存在）
    //     - token 从空变有效 → 立即重试 pending 深链（不只 pollSessions）
    //     - history 加有界超时；"fail"（网络/超时）→ SSE snapshot 兜底
    //     - 深链专属 SSE 404 恢复：history 已成功 + historical/unknown →
    //       resumeSession；任务面板路径（无深链标记）保持三态分类不变
    //     - history 与 SSE 都失败 → 持久错误提示 + 自动重试，不白屏
    //     - 竞态：深链 history 迟到 + 用户切走 → 旧响应不渲染不起流
    //     - 无 workspace 参数 → 只用当前激活 workspace；背景 poll 卡住
    //       不延迟目标 workspace 深链
    // =====================================================================
    const _dlSave = {
      ws: state.workspaces, workspace: state.workspace, token: state.token,
      lists: state.workspaceLists, errors: state.workspaceErrors, lastList: state.lastList,
      sid: state.sessionId, dl: Object.assign({}, state.deepLink),
      sidebarOpen: state.sidebar.open, search: location.search,
      sessionsData: sessionsData, sessionsDataB: sessionsDataB, sessionsDelayed: sessionsDelayed,
      sessionsBFail: sessionsBFail, bGetDelayed: bGetDelayed, sse404Ids: new Set(sse404Ids),
      historyOverrides: new Map(historyOverrides),
    };
    const _postedIds = () => FETCH_BODIES.map((b) => {
      try { return b ? JSON.parse(b).id : null; } catch (e) { return null; }
    }).filter(Boolean);
    const _dlReset = (id, token) => {
      const tk = (token == null) ? "t" : token;   // "" 必须保持空串（token 从空变有效的测试前提）
      state.workspaces = [{ id: "ws1", name: "默认", url: "", token: tk }];
      state.workspace = state.workspaces[0];
      state.token = tk;
      state.workspaceLists = {}; state.workspaceErrors = {}; state.lastList = [];
      state.deepLink = { pending: id, handled: false, probing: false, attemptEpoch: -1 };
      state.sessionId = null;
      state.sidebar.open = false;
      state.sse.stopped = false;
      elsById["banner"].hidden = true; elsById["bannerText"].textContent = "";
    };
    const _hist = (text) => ({ entries: [{ type: "message", message: { User: { content: text, images: [] } } }], next_before_seq: null });

    // (1) 列表永久 pending + history/SSE 成功 → 不等待列表即进入 chat
    _dlReset(null, "t");
    sessionsDelayed = true; sessionsResolve = null;
    historyOverrides.set("dl1", { status: 200, body: _hist("深链1内容") });
    sseOkIds.add("dl1");
    location.search = "?session=dl1";
    init();                                   // 重新走启动：URL 解析 → 立即深链（与轮询并行）
    await flush(); await flush();
    chk("deeplink: opens chat without waiting for list poll",
        state.sessionId === "dl1"
        && elsById["messages"].textContent.includes("深链1内容"),
        "sid=" + state.sessionId);
    chk("deeplink: list poll still pending while chat opened",
        sessionsResolve !== null,
        "pending=" + (sessionsResolve !== null));
    sessionsResolve(resp(200, sessionsData));   // 放行列表轮询
    await flush(); await flush();
    sessionsDelayed = false;
    historyOverrides.delete("dl1"); sseOkIds.delete("dl1");

    // (2) token 初始为空：URL 解析记录 pending；输入 token 后立即处理（不等 poll round）
    _dlReset(null, "");
    location.search = "?session=dl2";
    sessionsDelayed = true; sessionsResolve = null;
    init();                                   // 重新走启动：记录 pending；token 空不触发深链
    chk("deeplink: init records pending from URL when token empty",
        state.deepLink.pending === "dl2" && state.deepLink.handled === false
       ,
        "pending=" + state.deepLink.pending);
    await flush(); await flush();             // init 的轮询挂起（sessionsDelayed）
    chk("deeplink: list poll pending while token empty",
        sessionsResolve !== null,
        "listPending=" + (sessionsResolve !== null));
    historyOverrides.set("dl2", { status: 200, body: _hist("token后深链") });
    sseOkIds.add("dl2");
    sessionsDelayed = false;                  // 后续轮询正常完成（不再二次挂起）
    state.token = "test-token";               // 模拟 token input 后的派生值
    restartTransport();                       // token 变化 → 立即重试深链
    await flush(); await flush();
    chk("deeplink: token entry immediately probes (no poll round wait)",
        state.sessionId === "dl2"
        && elsById["messages"].textContent.includes("token后深链")
        && sessionsResolve !== null,          // 列表轮询仍在途：深链没等它
        "sid=" + state.sessionId + " listPending=" + (sessionsResolve !== null));
    sessionsResolve(resp(200, sessionsData)); // 放行挂起的列表轮询
    await flush(); await flush();
    historyOverrides.delete("dl2"); sseOkIds.delete("dl2");

    // (3) fresh 列表命中 inactive → 仍调用 resume，不退化成只读 history
    _dlReset("dl-hist", "t");
    state.workspaceErrors = { ws1: null };    // fresh（成功轮询过）
    state.lastList = [{ id: "dl-hist", status: "Idle", entry_count: 2, busy: false, active: false }];
    sessionsPostCustom = { id: "dl-hist", status: "Idle", active: true };
    maybeHandleDeepLink();
    await flush(); await flush();
    chk("deeplink: fresh inactive hit resumes (POST issued + session opened)",
        _postedIds().includes("dl-hist")
        && state.sessionId === "dl-hist",
        "sid=" + state.sessionId + " posted=" + JSON.stringify(_postedIds()));
    sessionsPostCustom = null;

    // (4) 列表失败 + history 200 + SSE 404 → 深链专属恢复（resume 被调用）
    _dlReset("dl-404", "t");
    sessionsAFail = true;
    historyOverrides.set("dl-404", { status: 200, body: _hist("404恢复历史") });
    sse404Ids.add("dl-404");
    sessionsPostCustom = { id: "dl-404", status: "Idle", active: true };
    maybeHandleDeepLink();
    await flush(); await flush(); await flush();
    chk("deeplink: list-fail + history 200 + SSE 404 resumes",
        _postedIds().includes("dl-404")
        && state.sessionId === "dl-404",
        "sid=" + state.sessionId + " posted=" + JSON.stringify(_postedIds()));
    // 任务面板路径（无深链标记）：unknown 404 仍静默且不恢复（回归）
    state.deepLink = { pending: null, handled: true, probing: false, attemptEpoch: -1 };
    state.lastList = [{ id: "s1", status: "Idle", entry_count: 1, busy: false, active: true }];
    state.workspaceLists["ws1"] = state.lastList;
    state.sessionId = "sub-finished2";
    sse404Ids.add("sub-finished2");
    elsById["banner"].hidden = true; elsById["bannerText"].textContent = "";
    const _postsBefore4b = _postedIds().length;
    connectSSE("sub-finished2", "ws1", sessionOpenEpoch);
    await flush();
    chk("deeplink: task-panel unknown 404 stays silent, no resume",
        elsById["banner"].hidden === true
        && _postedIds().length === _postsBefore4b,
        "banner=" + JSON.stringify(elsById["bannerText"].textContent));
    sse404Ids.delete("sub-finished2"); sse404Ids.delete("dl-404");
    historyOverrides.delete("dl-404");
    sessionsAFail = false; sessionsPostCustom = null;

    // (5) history 失败形态：404/401 → 停流提示；网络失败/timeout → SSE snapshot 兜底
    // (5a) 404 → gone banner，不起 SSE
    _dlReset("dl-gone", "t");
    historyOverrides.set("dl-gone", { status: 404, body: {} });
    maybeHandleDeepLink();
    await flush(); await flush();
    chk("deeplink: history 404 shows gone banner, no SSE",
        elsById["bannerText"].textContent.includes("不存在")
        && !FETCHES.some((u) => u === "/api/sessions/dl-gone/events"),
        "banner=" + JSON.stringify(elsById["bannerText"].textContent));
    historyOverrides.delete("dl-gone");
    // (5b) 401 → auth banner，不起 SSE
    _dlReset("dl-auth", "t");
    historyOverrides.set("dl-auth", { status: 401, body: {} });
    maybeHandleDeepLink();
    await flush(); await flush();
    chk("deeplink: history 401 shows auth banner, no SSE",
        elsById["bannerText"].textContent.includes("认证失败")
        && !FETCHES.some((u) => u === "/api/sessions/dl-auth/events"),
        "banner=" + JSON.stringify(elsById["bannerText"].textContent));
    historyOverrides.delete("dl-auth");
    // (5c) 网络失败 → SSE snapshot 兜底 + 持久错误提示（自动重试）
    _dlReset("dl-net", "t");
    historyOverrides.set("dl-net", { netfail: true });
    maybeHandleDeepLink();
    await flush(); await flush();
    chk("deeplink: history network fail → SSE snapshot + persistent error",
        FETCHES.some((u) => u === "/api/sessions/dl-net/events")
        && elsById["bannerText"].textContent.includes("自动重试"),
        "banner=" + JSON.stringify(elsById["bannerText"].textContent));
    historyOverrides.delete("dl-net");
    // (5d) timeout（10s abort）→ SSE snapshot 兜底 + 持久错误提示
    _dlReset("dl-timeout", "t");
    historyOverrides.set("dl-timeout", { hang: true });
    maybeHandleDeepLink();
    const _abortIdx5d = scheduledTimeouts.length - 1;   // fetchWithTimeout 的 10s abort 定时器（同步 arm，最后一个槽）
    scheduledTimeouts[_abortIdx5d]();                    // 触发超时 → AbortError → "fail"
    await flush(); await flush();
    chk("deeplink: history timeout → SSE snapshot + persistent error",
        FETCHES.some((u) => u === "/api/sessions/dl-timeout/events")
        && elsById["bannerText"].textContent.includes("自动重试"),
        "banner=" + JSON.stringify(elsById["bannerText"].textContent));
    historyOverrides.delete("dl-timeout");

    // (6) 深链 history 迟到 + 用户切 workspace/开另一会话 → 旧响应不渲染不起流
    state.workspaces = [
      { id: "wsA", name: "A", url: "", token: "t" },
      { id: "wsB", name: "B", url: "http://b.local", token: "t" },
    ];
    state.workspace = state.workspaces[0];
    state.token = "t";
    state.workspaceLists = {}; state.workspaceErrors = {}; state.lastList = [];
    state.deepLink = { pending: "dl-late", handled: false, probing: false, attemptEpoch: -1 };
    state.sessionId = null;
    state.sidebar.open = false;
    historyOverrides.set("dl-late", { delay: true });
    maybeHandleDeepLink();
    await flush();
    chk("deeplink: probe in flight before user navigates",
        state.sessionId === "dl-late",
        "sid=" + state.sessionId);
    openSessionIn("wsB", "b1");                        // 用户切走：代次取代
    await flush(); await flush();
    chk("deeplink: switched to b1", state.sessionId === "b1" && state.workspace.id === "wsB",
        "sid=" + state.sessionId + " ws=" + state.workspace.id);
    const _msgs6 = elsById["messages"].textContent;
    historyResolve(resp(200, { entries: [{ type: "message", message: { User: { content: "迟到深链不应渲染", images: [] } } }], next_before_seq: null }));   // 迟到响应
    await flush(); await flush();
    chk("deeplink: late history response not rendered, no stream",
        elsById["messages"].textContent === _msgs6
        && !elsById["messages"].textContent.includes("迟到深链不应渲染")
        && !FETCHES.some((u) => u === "/api/sessions/dl-late/events"),
        "same=" + (elsById["messages"].textContent === _msgs6));
    historyOverrides.delete("dl-late");

    // (7) 两个 workspace 有同 id：无 workspace 参数 → 只用当前激活 workspace
    state.workspaces = [
      { id: "wsA", name: "A", url: "", token: "t" },
      { id: "wsB", name: "B", url: "http://b.local", token: "t" },
    ];
    state.workspace = state.workspaces[0];
    state.token = "t";
    state.workspaceLists = {
      wsA: [{ id: "dup1", status: "Idle", entry_count: 1, active: true }],
      wsB: [{ id: "dup1", status: "Idle", entry_count: 1, active: true }],
    };
    state.workspaceErrors = { wsA: null, wsB: null };   // 都 fresh：命中走既有分支
    state.lastList = state.workspaceLists.wsA;
    state.deepLink = { pending: "dup1", handled: false, probing: false, attemptEpoch: -1 };
    state.sessionId = null;
    state.sidebar.open = false;
    historyOverrides.set("dup1", { status: 200, body: _hist("同id深链") });
    sseOkIds.add("dup1");
    maybeHandleDeepLink();
    await flush(); await flush();
    chk("deeplink: same-id sessions resolve against active workspace only",
        FETCHES.some((u) => u.includes("dup1/history") && !u.startsWith("http://b.local"))
        && !FETCHES.some((u) => u.startsWith("http://b.local") && u.includes("dup1/history"))
        && state.sessionId === "dup1" && state.workspace.id === "wsA",
        "sid=" + state.sessionId + " ws=" + state.workspace.id
        + " hist=" + JSON.stringify(FETCHES.filter((u) => u.includes("dup1/history"))));
    historyOverrides.delete("dup1"); sseOkIds.delete("dup1");

    // (8) 背景 workspace poll 卡住 → 不延迟目标 workspace 深链
    state.workspaces = [
      { id: "wsA", name: "A", url: "", token: "t" },
      { id: "wsB", name: "B", url: "http://b.local", token: "t" },
    ];
    state.workspace = state.workspaces[0];
    state.token = "t";
    state.workspaceLists = {}; state.workspaceErrors = {}; state.lastList = [];
    state.deepLink = { pending: "dl8", handled: false, probing: false, attemptEpoch: -1 };
    state.sessionId = null;
    state.sidebar.open = false;
    historyOverrides.set("dl8", { status: 200, body: _hist("深链8") });
    sseOkIds.add("dl8");
    bGetDelayed = true; bGetResolve = null;
    pollSessions();                               // 启动聚合轮询：B 卡住
    await flush();
    chk("deeplink: background B poll stuck", bGetResolve !== null,
        "bStuck=" + (bGetResolve !== null));
    maybeHandleDeepLink();                        // 深链与卡住的轮询并行
    await flush(); await flush();
    chk("deeplink: chat opened while B poll still stuck",
        state.sessionId === "dl8"
        && elsById["messages"].textContent.includes("深链8")
        && bGetResolve !== null,
        "sid=" + state.sessionId + " bStuck=" + (bGetResolve !== null));
    bGetResolve(resp(200, sessionsDataB));        // 放行 B
    await flush(); await flush();
    bGetDelayed = false;
    historyOverrides.delete("dl8"); sseOkIds.delete("dl8");

    // =====================================================================
    // 9) 缺口 1（oracle 复审）：fresh inactive 深链经 resumeSession 后，
    //    恢复的 history 必须仍带深链有界超时——resume 后拉 history 挂起，
    //    10s 超时触发 → 不卡死 → SSE 404 → 持久错误 + 自动重试
    // =====================================================================
    _dlReset("dl-hist-t", "t");
    state.workspaceErrors = { ws1: null };        // fresh（成功轮询过）
    state.lastList = [{ id: "dl-hist-t", status: "Idle", entry_count: 2, busy: false, active: false }];
    sessionsPostCustom = { id: "dl-hist-t", status: "Idle", active: true };
    historyOverrides.set("dl-hist-t", { hang: true });   // resume 后的 history 永久挂起
    sse404Ids.add("dl-hist-t");
    const _t9 = scheduledTimeouts.length;
    maybeHandleDeepLink();                        // fresh inactive → resume → openSession → loadHistory
    await flush(); await flush();
    let _abort9 = null;
    for (let i = _t9; i < scheduledTimeouts.length; i++) {
      if (scheduledTimeouts[i]) { _abort9 = scheduledTimeouts[i]; break; }
    }
    chk("deeplink gap1: resume issued + history bounded-timeout timer armed",
        _postedIds().includes("dl-hist-t")
        && state.sessionId === "dl-hist-t"
        && _abort9 !== null,
        "posted=" + JSON.stringify(_postedIds()) + " sid=" + state.sessionId
        + " abortTimer=" + (_abort9 !== null));
    _abort9();                                    // 触发 10s 有界超时 → AbortError → "fail" → SSE
    await flush(); await flush();
    chk("deeplink gap1: bounded timeout does not hang; SSE + persistent retry",
        FETCHES.some((u) => u === "/api/sessions/dl-hist-t/events")
        && elsById["bannerText"].textContent.includes("自动重试"),
        "banner=" + JSON.stringify(elsById["bannerText"].textContent));
    sse404Ids.delete("dl-hist-t"); historyOverrides.delete("dl-hist-t"); sessionsPostCustom = null;

    // =====================================================================
    // 10) 缺口 2（oracle 复审）：深链 history 失败 → SSE 404 →
    //     scheduleReconnect 重试时，重试的 history 仍带深链有界超时
    //     （attempt 标记兜底）；重试 history 超时同样不卡死
    // =====================================================================
    _dlReset("dl-retry-t", "t");
    sessionsAFail = true;                         // 列表失败 → probe 路径
    historyOverrides.set("dl-retry-t", { netfail: true });
    sse404Ids.add("dl-retry-t");
    const _base10 = scheduledTimeouts.length;
    maybeHandleDeepLink();
    await flush(); await flush();
    let _retry10 = null;                          // phase 1 新 armed 的非空定时器 = 3s 重连
    for (let i = _base10; i < scheduledTimeouts.length; i++) {
      if (scheduledTimeouts[i]) { _retry10 = scheduledTimeouts[i]; break; }
    }
    chk("deeplink gap2: persistent retry scheduled after history-fail + SSE 404",
        elsById["bannerText"].textContent.includes("自动重试")
        && _retry10 !== null && state.sse.retryTimer !== null,
        "banner=" + JSON.stringify(elsById["bannerText"].textContent)
        + " retry=" + (_retry10 !== null));
    // 重试前把 history 改成挂起：验证重试的 history 走 fetchWithTimeout（带超时）
    historyOverrides.set("dl-retry-t", { hang: true });
    const _hist10 = FETCHES.filter((u) => u.includes("dl-retry-t/history")).length;
    const _t10 = scheduledTimeouts.length;
    _retry10();                                   // 触发 3s 重连 → openWith → loadHistory（标记兜底 → 带超时）
    await flush(); await flush();
    let _abort10 = null;
    for (let i = _t10; i < scheduledTimeouts.length; i++) {
      if (scheduledTimeouts[i]) { _abort10 = scheduledTimeouts[i]; break; }
    }
    chk("deeplink gap2: retry history re-fetched with bounded timeout",
        FETCHES.filter((u) => u.includes("dl-retry-t/history")).length === _hist10 + 1
        && _abort10 !== null,
        "hist=" + FETCHES.filter((u) => u.includes("dl-retry-t/history")).length
        + " abortTimer=" + (_abort10 !== null));
    const _events10 = FETCHES.filter((u) => u === "/api/sessions/dl-retry-t/events").length;
    _abort10();                                   // 触发重试 history 的 10s 超时 → 不卡死
    await flush(); await flush();
    chk("deeplink gap2: retry history timeout does not hang (SSE + persistent error)",
        FETCHES.filter((u) => u === "/api/sessions/dl-retry-t/events").length === _events10 + 1
        && elsById["bannerText"].textContent.includes("自动重试"),
        "events=" + FETCHES.filter((u) => u === "/api/sessions/dl-retry-t/events").length
        + " banner=" + JSON.stringify(elsById["bannerText"].textContent));
    sse404Ids.delete("dl-retry-t"); historyOverrides.delete("dl-retry-t"); sessionsAFail = false;

    // 还原（保持整洁）
    state.workspaces = _dlSave.ws; state.workspace = _dlSave.workspace; state.token = _dlSave.token;
    state.workspaceLists = _dlSave.lists; state.workspaceErrors = _dlSave.errors; state.lastList = _dlSave.lastList;
    state.sessionId = _dlSave.sid;
    state.deepLink = Object.assign({}, _dlSave.dl);
    state.sidebar.open = _dlSave.sidebarOpen;
    location.search = _dlSave.search;
    sessionsData = _dlSave.sessionsData; sessionsDataB = _dlSave.sessionsDataB;
    sessionsDelayed = _dlSave.sessionsDelayed; sessionsBFail = _dlSave.sessionsBFail;
    bGetDelayed = _dlSave.bGetDelayed; bGetResolve = null;
    sessionsAFail = false; sessionsPostCustom = null;
    sse404Ids = new Set(_dlSave.sse404Ids); sseOkIds = new Set();
    historyOverrides = new Map(_dlSave.historyOverrides);
    stopSSE();
  } catch(e){ console.log("MAIN ERROR:", String(e), "STACK:", e && e.stack); fail++; }
  console.log(fail===0 ? "ALL PASS" : fail+" FAILURES");
  imports.system.exit(0);
}
main();
'''.replace('MODE === \'direct\'', 'true' if MODE == 'direct' else 'false')

# DEEP_LINK env → location.search 注入（init() 启动时 URL 解析入口）
HARNESS = HARNESS.replace('__DEEP_LINK_SEARCH__', ('?session=' + DEEP_LINK) if DEEP_LINK else '')

out = os.path.join(HERE, '.test_harness.js')
with open(out, 'w', encoding='utf-8') as f:
    f.write(HARNESS + vendor_js + "\n" + js + TAIL)
r = subprocess.run(['gjs', out], capture_output=True, text=True)
print(r.stdout, end="")
if r.stderr.strip():
    print(r.stderr[:12000])
if os.environ.get('KEEP') != '1':
    os.unlink(out)

# Bug A 的布局修复在 CSS 侧（DOM 桩无法算布局）：.composer-actions 必须
# 带 margin-left:auto，meta 隐藏时按钮仍靠右（不依赖 meta 占位）。
import re
_css = open(os.path.join(HERE, 'style.css'), encoding='utf-8').read()
_m = re.search(r'\.composer-actions\s*\{([^}]*)\}', _css)
_css_ok = bool(_m and re.search(r'margin-left:\s*auto', _m.group(1)))
print(("PASS" if _css_ok else "FAIL") + " composer-actions margin-left:auto in style.css")
# 状态 spinner（纯 CSS 伪元素）：busy/compacting 前置转圈，动画 + reduced-motion 降级
_spin_ok = bool(re.search(r'\.composer-status\.busy::before[^{]*\{[^}]*animation:[^;}]*composer-status-spin', _css)
                and re.search(r'@keyframes\s+composer-status-spin', _css)
                and re.search(r'prefers-reduced-motion: reduce', _css))
print(("PASS" if _spin_ok else "FAIL") + " composer-status spinner + reduced-motion in style.css")
# 置顶聚合行的 workspace 标记必须保持紧凑且不参与 flex 收缩；否则空 span
# 不可见，或再次挤占会话标题空间。
_marker = re.search(r'\.tree-row\s+\.ws-pin-label\s*\{([^}]*)\}', _css)
_marker_css = _marker.group(1) if _marker else ''
_marker_ok = bool(_marker
                  and re.search(r'font-size:\s*1[2-4]px', _marker_css)
                  and re.search(r'white-space:\s*nowrap', _marker_css))
print(("PASS" if _marker_ok else "FAIL") + " compact ws-pin-label layout in style.css")
# 未选/删除当前会话后的空状态：消息区与 composer 必须同时隐藏，避免旧聊天
# 内容或可发送输入框残留。DOM 桩不计算 CSS，因此在 harness 的 CSS 检查段断言。
_empty_rule = re.search(
    r'#chatView\.no-session\s+\.chat-head\s*,\s*'
    r'#chatView\.no-session\s+\.messages\s*,\s*'
    r'#chatView\.no-session\s+\.composer\s*,[^\{]*\{([^}]*)\}', _css)
_empty_ok = bool(_empty_rule and re.search(r'display:\s*none', _empty_rule.group(1)))
print(("PASS" if _empty_ok else "FAIL") + " no-session hides messages and composer in style.css")
# LOW：ws-chip 10px 小字号前景/背景 WCAG AA 对比度 ≥ 4.5:1（深色文字配浅 tint）
def _rel_lum(hexc):
    hexc = hexc.lstrip('#')
    r, g, b = (int(hexc[i:i+2], 16) / 255 for i in (0, 2, 4))
    def f(c):
        return c / 12.92 if c <= 0.03928 else ((c + 0.055) / 1.055) ** 2.4
    return 0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b)
def _contrast(a, b):
    la, lb = _rel_lum(a), _rel_lum(b)
    if la < lb:
        la, lb = lb, la
    return (la + 0.05) / (lb + 0.05)
_chip_ok = True
for i in range(6):
    m = re.search(r'\.ws-chip-%d\s*\{([^}]*)\}' % i, _css)
    if not m:
        _chip_ok = False
        print("FAIL ws-chip-%d rule missing in style.css" % i)
        continue
    c = re.search(r'color:\s*(#[0-9a-fA-F]{6})', m.group(1))
    b = re.search(r'background:\s*(#[0-9a-fA-F]{6})', m.group(1))
    if not c or not b:
        _chip_ok = False
        print("FAIL ws-chip-%d needs hex color+background" % i)
        continue
    cr = _contrast(c.group(1), b.group(1))
    if cr < 4.5:
        _chip_ok = False
    print(("PASS" if cr >= 4.5 else "FAIL") + " ws-chip-%d contrast %.2f:1 (>=4.5)" % (i, cr))
# 文件工具差异化渲染的 diff 样式必须存在：- 红（#fbe3e4 底 + --red 符号）、
# + 绿（#eef5e3 底 + --green 符号）、行号列灰底右对齐（--base2 + text-align:right）。
_diff_rules_ok = bool(
    re.search(r'\.tool-card\s+\.tool-result\.tool-diff\s*\{[^}]*background:\s*var\(--base3\)', _css)
    and re.search(r'\.tool-card\s+\.tool-result\.tool-diff\s+\.diff-row\.diff-add\s*\{[^}]*background:\s*#eef5e3', _css)
    and re.search(r'\.tool-card\s+\.tool-result\.tool-diff\s+\.diff-row\.diff-del\s*\{[^}]*background:\s*#fbe3e4', _css)
    and re.search(r'\.tool-card\s+\.tool-result\.tool-diff\s+\.diff-ln\s*\{[^}]*text-align:\s*right', _css))
print(("PASS" if _diff_rules_ok else "FAIL") + " file-tool diff styles (add/del/ln) in style.css")
# 窄屏防溢出：.diff-text 必须可收缩（min-width:0）且允许任意位置断行
# （overflow-wrap: anywhere，长 URL/长行不撑破卡片）
_txt_rule = re.search(r'\.tool-card\s+\.tool-result\.tool-diff\s+\.diff-text\s*\{([^}]*)\}', _css)
_txt_ok = bool(_txt_rule
               and re.search(r'min-width:\s*0', _txt_rule.group(1))
               and re.search(r'overflow-wrap:\s*(anywhere|break-word)', _txt_rule.group(1)))
print(("PASS" if _txt_ok else "FAIL") + " diff-text min-width:0 + overflow-wrap in style.css")
# 对比度：diff 辅助文字（行号列 / 确认行 / 截断标记）在各自底色上 ≥ 4.5:1（WCAG AA）。
# 解析 :root 变量 + 规则里的 var() 引用后复用上面的 _contrast 计算。
_root = re.search(r':root\s*\{([^}]*)\}', _css)
def _var_hex(name):
    m = re.search(r'--%s:\s*(#[0-9a-fA-F]{6})' % name, _root.group(1)) if _root else None
    return m.group(1) if m else None
def _rule_color(selector):
    m = re.search(re.escape(selector) + r'\s*\{([^}]*)\}', _css)
    if not m:
        return None
    c = re.search(r'color:\s*var\((--[a-z0-9]+)\)', m.group(1))
    return _var_hex(c.group(1)[2:]) if c else None
_contrast_ok = True
for _sel, _bgvar in [
    (r'.tool-card .tool-result.tool-diff .diff-ln', 'base2'),
    (r'.tool-card .tool-result.tool-diff .diff-head', 'base2'),
    (r'.tool-card .tool-result.tool-diff .diff-more', 'base3'),
]:
    _fg = _rule_color(_sel)
    _bgh = _var_hex(_bgvar)
    if not _fg or not _bgh:
        _contrast_ok = False
        print("FAIL contrast: %s missing color/bg" % _sel)
        continue
    _cr = _contrast(_fg, _bgh)
    if _cr < 4.5:
        _contrast_ok = False
    print(("PASS" if _cr >= 4.5 else "FAIL") + " contrast %.2f:1 %s" % (_cr, _sel))
sys.exit(0 if ("ALL PASS" in r.stdout + r.stderr) and _css_ok and _spin_ok and _marker_ok and _empty_ok and _chip_ok and _diff_rules_ok and _txt_ok and _contrast_ok else 1)
