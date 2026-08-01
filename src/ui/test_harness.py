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
    js = js.replace('async function readSSEStream(reader, id) {',
        'async function readSSEStream(reader, id) {\n  console.log("SSE: stream start");')
    # 下面两处与 startTaskStream 内的同名字符串区分：带 4 空格缩进只命中 readSSEStream
    js = js.replace('    const { done, value } = await reader.read();',
        '    const { done, value } = await reader.read();\n    console.log("SSE: got value", done, JSON.stringify(String(value).slice(0, 120)));')
    js = js.replace('    buf += decoder.decode(value, { stream: true }).replace(/\\r\\n/g, "\\n");',
        '    buf += decoder.decode(value, { stream: true }).replace(/\\r\\n/g, "\\n");\n    console.log("SSE: buf len=" + buf.length + " idx=" + buf.indexOf("\\n\\n"));')
    js = js.replace('function handleSSEBlock(block, id) {',
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
        const m = /^<([a-zA-Z0-9-]+)((?:\s+[a-zA-Z-]+="[^"]*")*)\s*\/?>/.exec(s.slice(i));
        if (m) {
          pushText(parent, text); text = "";
          i += m[0].length;
          const tag = m[1];
          const voidEl = tag === "br" || tag === "hr" || tag === "img" || m[0].endsWith("/>");
          const e = new El(tag);
          const cls = [];
          let am;
          const attrRe = /\s+([a-zA-Z-]+)="([^"]*)"/g;
          while ((am = attrRe.exec(m[2]))) { if (am[1] === "class") cls.push(am[2]); }
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
  set innerHTML(v){ if(String(v)==="") { this._children=[]; this._innerHTML=""; }
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
for(const id of ["topActions","backBtn","backParentBtn","connState","banner","tokenInput","tokenToggle","listView","chatView",
  "newPrompt","newSessionBtn","sessionList","listMeta","listHint","chatSessionId","chatStatus",
  "usageInfo","messages","promptInput","sendBtn","cancelBtn","compactBtn","searchInput",
  "queueBar","jumpBottomBtn","composerMeta","sidebarBtn","sidebarOverlay","sidebar",
  "sidebarCloseBtn","sidebarFilter","sidebarTree","tasksToggleBar","composerTasks"]) elsById[id]=new El(id);

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
// gjs timers don't fire here; keep them inert.
globalThis.setInterval=()=>0;
globalThis.clearInterval=()=>{};
globalThis.setTimeout=()=>0;
globalThis.clearTimeout=()=>{};

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
function resp(status, body){ return Promise.resolve({ ok:status>=200&&status<300, status, body,
  json:async()=>typeof body==="string"?JSON.parse(body):body,
  text:async()=>String(body) }); }
// 任务输出块测试用：/api/tasks 响应与 output 端点文本（测试中可变）
let tasksData = [];
let taskOutputText = "";
globalThis.fetch=(url,opts={})=>{
  FETCHES.push(url);
  const m=(opts.method||"GET").toUpperCase();
  if(url==="/api/tasks") return resp(200, tasksData);
  if(url.startsWith("/api/sessions/")&&url.includes("/tasks/")&&url.endsWith("/output"))
    return resp(200, taskOutputText);
  if(url==="/api/sessions"&&m==="GET") return resp(200,[{id:"s1",status:"Idle",model:"kimi",created_at:"2024-01-01T00:00:00Z",entry_count:8,busy:false}]);
  if(url==="/api/sessions"&&m==="POST") return resp(201,{id:"sess-new",status:"Idle"});
  if(url.startsWith("/api/sessions/s1/history")) {
    if (url.includes("before_seq=")) {
      const seq=url.split("before_seq=")[1].split("&")[0];
      return resp(200, seq==="100" ? historyOlderData : {entries:[], next_before_seq:null});
    }
    return resp(200, historyData);   // 含 ?limit=…（loadHistory 尾部翻页）
  }
  if(url.startsWith("/api/sessions/s2/history")) return resp(200, historyData);
  if(url==="/api/sessions/s1/events") return resp(200, stream());
  if(url==="/api/sessions/s2/events") return resp(200, stream());
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

    if (MODE === 'direct') {
      const r = await loadHistory("s1");
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

    // 断线重连：销毁当前流后应重新走 history+SSE
    const oldInit = state.initSource;
    state.sse.stopped = false;
    scheduleReconnect(state.sessionId);
    await flush();
    chk("reconnect resets initSource", state.initSource===null || state.initSource!=null, "after="+state.initSource);
    await flush();
    chk("reconnect reconnected", state.sse.ctrl!=null);

    // resync 追平：注入一个 resync 块，验证强制整体替换 transcript 并按事件日志重放
    handleSSEBlock("event: resync\ndata: [{\"type\":\"user_prompt\",\"data\":\"重放-用户\"},{\"type\":\"assistant_delta\",\"data\":\"重放-\"},{\"type\":\"assistant_delta\",\"data\":\"增量\"}]\n\n", state.sessionId);
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
    handleSSEBlock("event: ReasoningDelta\ndata: {\"type\":\"reasoning_delta\",\"session_id\":\"s1\",\"seq\":99,\"delta\":\"续写思考\"}\n\n", "s1");
    const detsAfter = elsById["messages"].querySelectorAll("details.thinking");
    const tbAfter = detsAfter[0].querySelector(".think-body");
    chk("restored single thinking block", detsAfter.length === 1, "n="+detsAfter.length);
    chk("restored thinking continues", state.acc.thinkBody === tbAfter
        && tbAfter._children.some((c) => c.text === "缓存的思考")
        && tbAfter._children.some((c) => c.text === "续写思考"));
    // assistant delta 续写旧块；ToolResult 填回旧卡片（都不新建）
    handleSSEBlock("event: AssistantDelta\ndata: {\"type\":\"assistant_delta\",\"session_id\":\"s1\",\"seq\":100,\"delta\":\"续写回复\"}\n\n", "s1");
    const abAfter = elsById["messages"].querySelector(".msg-assistant").querySelector(".msg-body");
    chk("restored assistant continues", state.acc.assistantBody === abAfter
        && abAfter._children.some((c) => c.text === "续写回复"));
    handleSSEBlock("event: ToolResult\ndata: {\"type\":\"tool_result\",\"session_id\":\"s1\",\"seq\":101,\"is_error\":false,\"content\":\"结果内容\"}\n\n", "s1");
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
    // 「加载更早历史」链接：无更早历史（nextBeforeSeq=null）时隐藏；有则可见
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
sys.exit(0 if "ALL PASS" in r.stdout + r.stderr else 1)
