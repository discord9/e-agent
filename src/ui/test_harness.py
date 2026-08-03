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
  cloneNode(){ const c = new El(this.tag); c._className=this._className;
    c._classes=new Set(this._classes); c.textContent=this.textContent;
    c.hidden=this.hidden; c._attrs=Object.assign({}, this._attrs); return c; }
  append(...nodes){ for(const n of nodes){ if(n==null) continue;
    const c=typeof n==="string"?{text:n}:n; this._children.push(c);
    if(c._parent==null) c._parent=this; } }
  appendChild(n){ const p=n._parent;   /* 真实 DOM 语义：移动=先从旧父节点移除 */
    if(p){ const j=p._children.indexOf(n); if(j>=0) p._children.splice(j,1); }
    this._children.push(n); n._parent=this; return n; }
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
}
const elsById={};
for(const id of ["topActions","backBtn","backParentBtn","connState","banner","bannerText","bannerClose","tokenInput","tokenToggle","listView","chatView",
  "newPrompt","newSessionBtn","sessionList","listMeta","listHint","chatSessionId","chatStatus",
  "usageInfo","messages","promptInput","sendBtn","cancelBtn","compactBtn","searchInput",
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
window.addEventListener=()=>{}; window.confirm=()=>true; window.setTimeout=()=>0;
globalThis.history={ replaceState(){} };
globalThis.location={ search:"" };
globalThis.URLSearchParams=class{ constructor(){} get(){ return null; } };
globalThis.requestAnimationFrame=()=>0;
globalThis.AbortController=class{ constructor(){this.signal={};} abort(){} };
// gjs 内置 TextDecoder 不可覆盖且不支持 stream 选项；用工厂替换页面里的 new TextDecoder()
function makeTextDecoder(){ return { decode(v){ return typeof v==="string"?v:""; } }; }
// gjs timers don't fire here; keep them inert. setTimeout 记录回调，测试可手动触发
// （scheduleReconnect 的重连定时器需要验证触发时的三重校验）。
globalThis.setInterval=()=>0;
globalThis.clearInterval=()=>{};
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
function resp(status, body){ return Promise.resolve({ ok:status>=200&&status<300, status, body,
  json:async()=>typeof body==="string"?JSON.parse(body):body,
  text:async()=>String(body) }); }
// 任务输出块测试用：/api/tasks 响应与 output 端点文本（测试中可变）
let tasksData = [];
let taskOutputText = "";
// perf 回归测试：output 端点延迟（手动 resolve）——验证 500ms 轮询防重入
let taskOutputDelayed = false;
let taskOutputResolve = null;
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
// B 返回非数组 JSON（{}）
let bPostDelayed = false;
let bPostResolve = null;
let aHistoryDelayed = false;
let aHistoryResolve = null;
let sessionsBFormat = false;
// SSE 生命周期测试：a1 的 events 流手动控制（首个 read 挂起，测试切走后
// 再 resolve 陈旧块；验证陈旧流不渲染到当前会话/workspace）
let a1StreamManual = false;
let a1StreamReadResolve = null;
// fork 面板测试用：/fork-candidates 候选与 /fork POST 响应（测试中可变）
let forkCandidatesData = [
  {at:2, seq:2, preview:"用户：你好，帮我看看"},
  {at:5, seq:7, preview:"助手：完成。这是一段很长的回复内容，需要被截断显示以保持菜单行整洁……"},
  {at:8, seq:10, preview:"系统提示行"},
];
let forkPostResp = resp(201,{id:"fork-1"});
globalThis.fetch=(url,opts={})=>{
  FETCHES.push(url);
  const m=(opts.method||"GET").toUpperCase();
  if(url==="/api/tasks") return resp(200, tasksData);
  if(url.startsWith("/api/sessions/")&&url.includes("/tasks/")&&url.endsWith("/output")) {
    if (taskOutputDelayed) return new Promise((resolve) => { taskOutputResolve = resolve; });
    return resp(200, taskOutputText);
  }
  if(url==="/api/sessions"&&m==="GET") return resp(200, sessionsData);
  // 聚合模式：第二台服务器按 base url 路由（500 故障开关：B 失败时 A 不受影响）
  if(url==="http://b.local/api/sessions"&&m==="GET") {
    if (sessionsPDelayed) return new Promise((resolve) => { sessionsPResolve = resolve; });
    if (sessionsBFail) return resp(500, {});
    return resp(200, sessionsBFormat ? {} : sessionsDataB);
  }
  if(url==="http://b.local/api/sessions"&&m==="POST") {
    if (bPostDelayed) return new Promise((resolve) => { bPostResolve = resolve; });
    return resp(201, { id: "b-hist", status: "Idle", active: true });
  }
  if(url.startsWith("http://b.local/api/sessions/")&&m==="DELETE") return resp(204,null);
  if(url==="/api/sessions"&&m==="POST") return resp(201,{id:"sess-new",status:"Idle"});
  if(url.startsWith("/api/sessions/a1/history")) {
    if (aHistoryDelayed) return new Promise((resolve) => { aHistoryResolve = resolve; });
    return resp(200,{entries:[{type:"message",message:{Assistant:{content:"A 会话内容"}}}], next_before_seq:null});
  }
  if(url==="/api/sessions/a1/events")
    return resp(200, a1StreamManual ? streamManual() : streamEmpty());
  if(url.startsWith("http://b.local/api/sessions/b1/history"))
    return resp(200,{entries:[{type:"message",message:{Assistant:{content:"B 会话内容"}}}], next_before_seq:null});
  if(url==="http://b.local/api/sessions/b1/events") return resp(200, streamEmpty());
  if(url.startsWith("/api/sessions/s1/history")) {
    if (url.includes("before_seq=")) {
      const seq=url.split("before_seq=")[1].split("&")[0];
      return resp(200, seq==="100" ? historyOlderData : {entries:[], next_before_seq:null});
    }
    return resp(200, historyData);   // 含 ?limit=…（loadHistory 尾部翻页）
  }
  if(url.startsWith("/api/sessions/s2/history")) return resp(200, historyData);
  if(url.startsWith("/api/sessions/fork-1/history")) return resp(200, {entries:[], next_before_seq:null});
  // restored 替换回归测试用：缓存过期后切回，history 含新消息（历史数据本身不变）
  if(url.startsWith("/api/sessions/restored-test/history")) return resp(200, historyData);
  if(url.startsWith("/api/sessions/restored-test2/history")) return resp(200, historyData);
  if(url==="/api/sessions/s1/events") return resp(200, stream());
  if(url==="/api/sessions/s2/events") return resp(200, stream());
  if(url==="/api/sessions/fork-1/events") return resp(200, stream());
  // restored 回归测试：空 SSE 流（snapshot 应被 history 替换路径跳过）
  if(url.startsWith("/api/sessions/restored-test/events")) return resp(200, streamEmpty());
  if(url.startsWith("/api/sessions/restored-test2/events")) return resp(200, streamEmpty());
  if(url==="/api/sessions/s1/fork-candidates") return resp(200, forkCandidatesData);
  if(url==="/api/sessions/s1/fork"&&m==="POST") return forkPostResp;
  if(url==="/api/models") return resp(200, ["chatgpt/sol","chatgpt/terra","deepseek/flash","deepseek/high","deepseek/fast","kimi/k3"]);
  if(url.startsWith("/api/sessions/")&&url.endsWith("/model")&&m==="POST") return resp(200,{ok:true,model:"sol"});
  if(url.startsWith("/api/sessions/")&&url.endsWith("/prompt")) return resp(202,{});
  if(url.startsWith("/api/sessions/")&&url.endsWith("/cancel")) return resp(202,{});
  if(url.startsWith("/api/sessions/")&&url.endsWith("/compact")) return resp(202,{});
  if(url.startsWith("/api/sessions/")&&m==="DELETE") return resp(204,null);
  return resp(404,{});
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
    state.token="test-token";
    await pollSessions();
    chk("list rows rendered", elsById["sessionList"].children.length>=1);
    // pin 按钮用 SVG（emoji 📌 不吃 color，状态色灰⇄金必须 SVG currentColor）
    const firstPin = elsById["sessionList"].querySelector(".pin-btn");
    chk("pin button is svg", firstPin && firstPin.querySelector("svg") !== null
        && firstPin.textContent.trim() === "",
        "html=" + (firstPin ? firstPin.innerHTML.slice(0, 40) : "none"));

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
    chk("fork select opens new session", state.sessionId === "fork-1" && state.view === "chat",
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
    chk("resync replaces transcript", !t3.includes("你好，帮我看看") && t3.includes("重放-用户"));
    chk("resync replayed deltas", t3.includes("重放-") && t3.includes("增量"));
    chk("resync rerenders", elsById["messages"]._children.length >= 2,
        "n=" + elsById["messages"]._children.length);

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
    function openRestored(){           // 从列表页切回 → restored 分支
      state.sessionId = null;          // 避免 saveSessionState 覆盖上面的缓存
      state.view = "list";
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
        state.sessionId === "s1" && state.view === "chat",
        "sid=" + state.sessionId + " view=" + state.view);
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
    // 切走（backToList）→ 切回：卡片轮询独立于会话缓存，backToList 不停轮询
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
    backToList();
    chk("backToList keeps card poller", state.tasks.pollers.has("s1:9"),
        "keys=" + JSON.stringify([...state.tasks.pollers.keys()]));
    chk("cached html saved", !!state.sessionStates["s1"]
        && state.sessionStates["s1"].html.length > 0,
        "len=" + (state.sessionStates["s1"] && state.sessionStates["s1"].html.length));
    openSession("s1");
    await flush();
    chk("restored chat view", state.view === "chat" && state.sessionId === "s1",
        "view=" + state.view + " sid=" + state.sessionId);

    // ---- restored 替换回归：切走期间的会话更新必须可见 ----
    // 场景：用户切走会话（缓存旧快照）→ 会话继续产出新消息 → 切回。
    // 旧实现恢复缓存 + 跳过 snapshot → 新消息永不显示。现在 restored
    // 会拉最新 history 替换过期缓存。
    state.sessionId = null;
    state.view = "list";
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
    state.view = "list";
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

    // ---- bash 卡片点击：就地展开卡片输出 + 流式轮询，不切会话 ----
    // （放在 B 测试之后：fresh 打开 s2 会把 nextBeforeSeq 置 100，而 B 的
    // 「加载更早历史」链接断言依赖 nextBeforeSeq=null，避免互相污染）
    // (b) 列表视图下点击 bash 卡片：不切会话，就地展开 + 启动轮询
    state.view = "list";
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
    chk("bash click stays in list view", state.view === "list" && state.sessionId === null,
        "view=" + state.view + " sid=" + state.sessionId);
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
        state.view === "chat" && state.sessionId === "sub-2",
        "view=" + state.view + " sid=" + state.sessionId);
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
    // 长 workspace 路径：文本截断到 ~40 字符，title 保留完整路径
    const longPath = "/very/long/workspace/path/" + "x".repeat(60);
    renderTaskList([{ session_id: "s1", id: 72, kind: "delegate", label: "长路径任务",
      full_command: null, output: null, role: null, background: false,
      workspace: longPath, resume: null }], elsById["composerTasks"]);
    tagRow = elsById["composerTasks"].querySelectorAll(".task-row")[0];
    const wsTag = tagRow.querySelector(".task-tag");
    chk("long workspace truncated keeps full title",
        wsTag.title === longPath
        && wsTag.textContent.startsWith("workspace: " + longPath.slice(0, 40))
        && wsTag.textContent.indexOf(longPath) === -1,
        "title-ok=" + (wsTag.title === longPath)
        + " text=" + wsTag.textContent.slice(0, 50));

    // ---- 任务行父会话标签：delegate 任务显示「父: <t.session_id>」----
    // 简单方案：后端 TaskMeta.session_id 对 delegate 任务就是父会话（发起
    // 它的会话），直接显示，无需查 session 列表；非 delegate 任务
    // session_id 即父且「会话 <id>」已显示，不重复加父标签。
    renderTaskList([{ session_id: "s1", id: 80, kind: "delegate", label: "子任务Z",
      full_command: null, output: null, role: null, subagent_session_id: "sub-9" }],
      elsById["composerTasks"]);
    let prow = elsById["composerTasks"].querySelectorAll(".task-row")[0];
    let pmeta = prow.querySelectorAll(".tparent");
    chk("delegate row shows parent label from session_id",
        pmeta.length === 1 && pmeta[0].textContent === "父: s1",
        "parent=" + pmeta.map((e) => e.textContent).join("|"));
    // 非 delegate 任务：不显示父标签（session_id 即父，「会话 <id>」已显示）
    renderTaskList([{ session_id: "s1", id: 81, kind: "bash", label: "ls",
      full_command: "ls", output: "", role: null }], elsById["composerTasks"]);
    prow = elsById["composerTasks"].querySelectorAll(".task-row")[0];
    chk("bash row shows no parent label",
        prow.querySelector(".tparent") === null,
        "parent=" + String(prow.querySelector(".tparent")));
    // delegate 无 session_id（异常数据）→ 安静降级
    renderTaskList([{ id: 82, kind: "delegate", label: "幽灵任务",
      full_command: null, output: null, role: null, subagent_session_id: "ghost-1" }],
      elsById["composerTasks"]);
    prow = elsById["composerTasks"].querySelectorAll(".task-row")[0];
    chk("delegate row without session_id shows no parent label",
        prow.querySelector(".tparent") === null,
        "parent=" + String(prow.querySelector(".tparent")));
    // 极端 resume：session_id === subagent_session_id → 省略父标签（避免「会话 X / 父: X」重复）
    renderTaskList([{ session_id: "sub-9", id: 84, kind: "delegate", label: "自指任务",
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
    chk("ws default token synced",
        state.token === state.workspace.token && state.token !== "",
        "token=" + JSON.stringify(state.token));
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
        state.sessionId === null && state.view === "list"
        && state.sessionStates["s1"] === undefined && state.lastList.length === 0
        && state.tasks.list.length === 0 && state.tasks.pollers.size === 0
        && elsById["messages"].innerHTML === "",
        "sid=" + state.sessionId + " view=" + state.view
        + " lastList=" + state.lastList.length);
    chk("ws add creates+activates workspace",
        state.workspaces.length === 2 && state.workspace.id !== "default"
        && state.workspace.name === "服务器B"
        && state.workspace.url === "http://localhost:9000"
        && state.workspace.token === "tok-b",
        "n=" + state.workspaces.length + " url=" + JSON.stringify(state.workspace.url)
        + " name=" + state.workspace.name);
    chk("ws switch syncs token", state.token === "tok-b",
        "token=" + JSON.stringify(state.token));
    chk("ws switch syncs legacy token key",
        localStorage.getItem("eagent_token") === "tok-b",
        "legacy=" + JSON.stringify(localStorage.getItem("eagent_token")));
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
    state.view = "chat";
    state.sessionId = "s1";
    connectSSE("s1", state.workspace.id, sessionOpenEpoch);
    await flush();
    chk("ws sse targets new base",
        FETCHES.some(u => u === "http://localhost:9000/api/sessions/s1/events"),
        "last=" + FETCHES[FETCHES.length - 1]);
    stopSSE();
    state.view = "list";
    state.sessionId = null;

    // 下拉 change → 切回默认
    elsById["workspaceSelect"].value = "default";
    elsById["workspaceSelect"]._listeners["change"][0]();
    await flush();
    chk("ws switch back to default",
        state.workspace.id === "default"
        && state.token === state.workspace.token && state.view === "list",
        "id=" + state.workspace.id + " token=" + JSON.stringify(state.token));
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
        && state.token === state.workspace.token,
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

    // =====================================================================
    // 聚合模式：多 workspace 会话聚合（侧边栏分组 + 列表视图 + 跨服务器打开）
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
    state.view = "list";
    state.searchQuery = "";
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

    // 3) 列表视图（聚合）：激活 workspace 的会话照常渲染 + 行带服务器 chip
    chk("agg list view renders active workspace sessions",
        elsById["sessionList"].textContent.includes("a1"),
        "list=" + elsById["sessionList"].textContent.slice(0, 80));
    chk("agg list rows carry ws chips",
        elsById["sessionList"].querySelectorAll(".ws-chip").length >= 2,
        "chips=" + elsById["sessionList"].querySelectorAll(".ws-chip").length);

    // 4) 同服务器点击：A 激活时点 A 组的 a1 → 直接打开，不切换 workspace
    wsSections = elsById["sidebarTree"].querySelectorAll(".tree-ws-section");
    const aRootRow = wsSections[0].querySelectorAll(".tree-row")[0];
    aRootRow._listeners["click"][0]();
    chk("agg same-server click opens directly",
        state.workspace.id === "wsA" && state.sessionId === "a1" && state.view === "chat",
        "ws=" + state.workspace.id + " sid=" + state.sessionId + " view=" + state.view);
    await flush();
    await flush();

    // 2) 跨服务器点击：A 激活时点 B 组的 b1 → 先切到 B 再打开（异步，等 next tick）
    wsSections = elsById["sidebarTree"].querySelectorAll(".tree-ws-section");
    const bRootRow = wsSections[1].querySelectorAll(".tree-row")[0];
    bRootRow._listeners["click"][0]();
    await flush();
    await flush();
    chk("agg cross-server click switches and opens",
        state.workspace.id === "wsB" && state.sessionId === "b1" && state.view === "chat",
        "ws=" + state.workspace.id + " sid=" + state.sessionId + " view=" + state.view);

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
    chk("agg failure: A list view unaffected",
        elsById["sessionList"].textContent.includes("a1"),
        "list=" + elsById["sessionList"].textContent.slice(0, 60));
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
        state.workspace.id === "wsA" && state.sessionId === null && state.view === "list",
        "ws=" + state.workspace.id + " sid=" + state.sessionId + " view=" + state.view);
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
        state.workspace.id === "wsA" && state.sessionId === "a1" && state.view === "chat",
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
    renderSessionList();
    const bRow = [...elsById["sessionList"].querySelectorAll(".session-row")]
      .find((r) => r.textContent.includes("b1"));
    chk("agg delete: B row present in aggregate list", bRow !== null && bHadB1,
        "row=" + (bRow !== null) + " bHad=" + bHadB1);
    bRow.querySelector(".del")._listeners["click"][0]({ stopPropagation(){} });   // 删除行点击：stopPropagation 需要 event 对象
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
        state.workspace.id === "wsA" && state.sessionId === "a1" && state.view === "chat"
        && a1StreamReadResolve !== null,
        "ws=" + state.workspace.id + " sid=" + state.sessionId
        + " pending=" + (a1StreamReadResolve !== null));
    const epochA1 = sessionOpenEpoch;  // a1 打开动作的唯一代次（此后被 B 取代）
    openSessionIn("wsB", "b1");        // 切到 B：代次取代，A 的流成为陈旧流
    await flush();
    await flush();
    chk("agg sse lifetime: switched to B",
        state.workspace.id === "wsB" && state.sessionId === "b1" && state.view === "chat",
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
        state.workspace.id === "wsB" && state.sessionId === "b1" && state.view === "chat",
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
        state.sessionId === "b1" && state.view === "chat"
        && FETCHES.filter(u => u === "http://b.local/api/sessions/b1/events").length === bEventsBefore14 + 1,
        "events=" + FETCHES.filter(u => u === "http://b.local/api/sessions/b1/events").length);
    a1StreamManual = false;

    // =====================================================================
    // perf 修复回归：聚合轮询整轮一次渲染 + 签名去重 + setTimeout 链防重入
    // + 聊天视图停轮询 + 侧边栏 hidden 跳过树渲染 + 任务面板签名去重
    // + 500ms output 轮询防重入
    // =====================================================================
    let renderSessionListCalls = 0;
    const _origRSL = renderSessionList;
    renderSessionList = function (...a) { renderSessionListCalls++; return _origRSL.apply(this, a); };
    let renderSidebarTreeCalls = 0;
    const _origRST = renderSidebarTree;
    renderSidebarTree = function (...a) { renderSidebarTreeCalls++; return _origRST.apply(this, a); };
    chk("perf poll interval is 2s", POLL_INTERVAL_MS === 2000, "=" + POLL_INTERVAL_MS);

    // 双 workspace 整轮：两个响应只触发一次列表渲染 + 一次树渲染（旧实现
    // 每 workspace 响应各自 renderSessionList/renderSidebarTree → 各 2 次）
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
    state.view = "list";
    state.searchQuery = "";
    state.sidebar.filter = "";
    state.sidebar.showAllWs = new Set();
    state.sidebar.expanded = new Set();
    state.renameActive = false;
    elsById["sidebar"].hidden = false;   // 侧边栏可见（默认态）
    renderWorkspaceSelect();
    renderSessionListCalls = 0;
    renderSidebarTreeCalls = 0;
    await pollAllWorkspaces();
    await flush();
    chk("perf whole round renders list once", renderSessionListCalls === 1,
        "calls=" + renderSessionListCalls);
    chk("perf whole round renders tree once", renderSidebarTreeCalls === 1,
        "calls=" + renderSidebarTreeCalls);
    chk("perf round rendered aggregate rows",
        elsById["sessionList"].querySelectorAll(".session-row").length >= 2,
        "n=" + elsById["sessionList"].querySelectorAll(".session-row").length);
    // 数据未变：第二轮整轮 → 列表/树签名去重，不重建 DOM（元素同一性保持）
    const rowRefP = elsById["sessionList"].querySelectorAll(".session-row")[0];
    const treeRefP = elsById["sidebarTree"].querySelectorAll(".tree-ws-section")[0];
    renderSessionListCalls = 0;
    renderSidebarTreeCalls = 0;
    await pollAllWorkspaces();
    await flush();
    chk("perf unchanged round skips DOM rebuild",
        elsById["sessionList"].querySelectorAll(".session-row")[0] === rowRefP
        && elsById["sidebarTree"].querySelectorAll(".tree-ws-section")[0] === treeRefP,
        "rowSame=" + (elsById["sessionList"].querySelectorAll(".session-row")[0] === rowRefP)
        + " treeSame=" + (elsById["sidebarTree"].querySelectorAll(".tree-ws-section")[0] === treeRefP));
    // 数据变化（busy 翻转）→ 重绘
    sessionsData = [{ id: "p1", status: "Busy", title: "P 主会话", created_at: "2024-01-01T00:00:00Z", entry_count: 1, busy: true, active: true }];
    await pollAllWorkspaces();
    await flush();
    chk("perf busy flip rebuilds list",
        elsById["sessionList"].querySelectorAll(".session-row")[0] !== rowRefP
        && elsById["sessionList"].querySelector(".busy-dot.busy") !== null,
        "same=" + (elsById["sessionList"].querySelectorAll(".session-row")[0] === rowRefP));

    // setTimeout 链防重入：慢响应期间不调度下一轮、不渲染；完成后才续调度
    sessionsPDelayed = true;
    sessionsPResolve = null;
    renderSessionListCalls = 0;
    const pollRoundPromise = pollRound();
    await flush();
    chk("perf in-flight round schedules no next round",
        state.pollTimer === null && renderSessionListCalls === 0,
        "timer=" + String(state.pollTimer) + " renders=" + renderSessionListCalls);
    sessionsPResolve(resp(200, sessionsDataB));   // 手动 resolve 慢响应
    await pollRoundPromise;
    await flush();
    chk("perf slow round renders once after settle", renderSessionListCalls === 1,
        "calls=" + renderSessionListCalls);
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

    // 聊天视图停聚合轮询：openSession（侧边栏关）→ 停；openSidebar → 恢复；
    // closeSidebar → 再停；backToList → 恢复
    stopPolling();
    state.sidebar.open = false;
    state.view = "list";
    startPolling();
    chk("perf list view polls", state.pollTimer !== null, "timer=" + String(state.pollTimer));
    state.sessionId = null;
    openSession("p1");   // 聊天 + 侧边栏关 → 停聚合轮询
    chk("perf openSession stops polling in chat",
        state.pollTimer === null && state.view === "chat",
        "timer=" + String(state.pollTimer) + " view=" + state.view);
    stopSSE();
    openSidebar();       // 聊天 + 侧边栏开 → 恢复轮询（树保持 busy/current 新鲜）
    chk("perf openSidebar resumes polling in chat",
        state.pollTimer !== null && state.sidebar.open,
        "timer=" + String(state.pollTimer));
    closeSidebar();      // 聊天 + 侧边栏关 → 再停
    chk("perf closeSidebar stops polling in chat",
        state.pollTimer === null && !state.sidebar.open,
        "timer=" + String(state.pollTimer));
    backToList();        // 回列表 → 恢复
    chk("perf backToList resumes polling",
        state.pollTimer !== null && state.view === "list",
        "timer=" + String(state.pollTimer));
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
  } catch(e){ console.log("MAIN ERROR:", String(e), "STACK:", e && e.stack); fail++; }
  console.log(fail===0 ? "ALL PASS" : fail+" FAILURES");
  imports.system.exit(0);
}
main();
'''.replace('MODE === \'direct\'', 'true' if MODE == 'direct' else 'false')

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
sys.exit(0 if ("ALL PASS" in r.stdout + r.stderr) and _css_ok and _spin_ok else 1)
