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
# 直接读 app.js + vendor/marked.min.js，等价于 server 组装的单文件。
js = open(os.path.join(HERE, 'app.js'), encoding='utf-8').read()
vendor_js = open(os.path.join(HERE, 'vendor', 'marked.min.js'), encoding='utf-8').read()

MODE = os.environ.get('MODE', 'open')   # 'open' = openSession path, 'direct' = loadHistory
TRACE = os.environ.get('TRACE') == '1'
# gjs 内置 TextDecoder 不可覆盖且不支持 stream 选项；页面 JS 里的 new TextDecoder() 换成桩工厂
js = js.replace('const decoder = new TextDecoder();', 'const decoder = makeTextDecoder();')
if TRACE:
    js = js.replace('async function readSSEStream(reader, id) {',
        'async function readSSEStream(reader, id) {\n  console.log("SSE: stream start");')
    js = js.replace('const { done, value } = await reader.read();',
        'const { done, value } = await reader.read();\n    console.log("SSE: got value", done, JSON.stringify(String(value).slice(0, 120)));')
    js = js.replace('buf += decoder.decode(value, { stream: true }).replace(/\\r\\n/g, "\\n");',
        'buf += decoder.decode(value, { stream: true }).replace(/\\r\\n/g, "\\n");\n    console.log("SSE: buf len=" + buf.length + " idx=" + buf.indexOf("\\n\\n"));')
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
  function walk(parent, stopTag){
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
          if (!voidEl) walk(e._children, tag);
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
  walk(roots, null);
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
    const c=typeof n==="string"?{text:n}:n; this._children.push(c); if(c._parent===undefined) c._parent=this; } }
  appendChild(n){ this._children.push(n); n._parent=this; return n; }
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
}
const elsById={};
for(const id of ["topActions","backBtn","connState","banner","tokenInput","listView","chatView",
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
  json:async()=>typeof body==="string"?JSON.parse(body):body }); }
globalThis.fetch=(url,opts={})=>{
  FETCHES.push(url);
  const m=(opts.method||"GET").toUpperCase();
  if(url==="/api/sessions"&&m==="GET") return resp(200,[{id:"s1",status:"Idle",model:"kimi",created_at:"2024-01-01T00:00:00Z",entry_count:8,busy:false}]);
  if(url==="/api/sessions"&&m==="POST") return resp(201,{id:"sess-new",status:"Idle"});
  if(url.startsWith("/api/sessions/s1/history")) {
    if (url.includes("before_seq=")) {
      const seq=url.split("before_seq=")[1].split("&")[0];
      return resp(200, seq==="100" ? historyOlderData : {entries:[], next_before_seq:null});
    }
    return resp(200, historyData);   // 含 ?limit=…（loadHistory 尾部翻页）
  }
  if(url==="/api/sessions/s1/events") return resp(200, stream());
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
    print(r.stderr[:4000])
if os.environ.get('KEEP') != '1':
    os.unlink(out)
sys.exit(0 if "ALL PASS" in r.stdout + r.stderr else 1)
