#!/usr/bin/env bash
#
# scripts/smoke.sh — API 级自动冒烟脚本
#
# 只测不依赖真实模型/SSE 的端点（参数校验、registry、undo 栈等）；
# 需要真实模型的端点（prompt/btw 实际效果/SSE/compact 实际效果）不在这里测，
# 见 TESTING.md 的 🖐 手动条目。
#
# 用法: scripts/smoke.sh [--port PORT] [--token TOKEN]
#   默认 http://127.0.0.1:8766；
#   token 自动从 $XDG_STATE_HOME/e-agent/server.token 或
#   ~/.local/state/e-agent/server.token 读取（server 启动时生成，0600），
#   读不到则必须用 --token 显式传入。
#
# 退出码: 全过 0；任一 FAIL 或前置检查失败 1。
#
# 说明（与真实端点的对齐）：
#   * 不存在 GET /api/sessions/{id} 单会话端点 —— 用「GET /api/sessions 列表包含
#     新建 id」验证创建已注册（等价于 id 匹配）。
#   * 不存在 GET /api/sessions/{id}/tasks 按会话任务列表端点 —— 用真实存在的
#     GET /api/sessions/{id}/history 验证会话级只读端点 200；跨会话任务列表
#     由 GET /api/tasks 覆盖。
#   * DELETE /api/sessions/{id} 实际返回 204（NO_CONTENT），不是 200。

set -u

PORT=8766
TOKEN=""
BASE=""

while [ $# -gt 0 ]; do
  case "$1" in
    --port) PORT="${2:-}"; shift 2 ;;
    --token) TOKEN="${2:-}"; shift 2 ;;
    *) echo "[FAIL] 未知参数: $1（用法: $0 [--port PORT] [--token TOKEN]）" >&2; exit 2 ;;
  esac
done

case "$PORT" in
  ''|*[!0-9]*) echo "[FAIL] 非法端口: $PORT" >&2; exit 2 ;;
esac

BASE="http://127.0.0.1:${PORT}"

# 临时目录存响应体/状态码；退出时清理
TMPD="$(mktemp -d /tmp/e-agent-smoke.XXXXXX)"
trap 'rm -rf "$TMPD"' EXIT

# ── token 解析 ────────────────────────────────────────────────────────────
if [ -z "$TOKEN" ]; then
  for f in \
    "${XDG_STATE_HOME:+"${XDG_STATE_HOME}/e-agent/server.token"}" \
    "${HOME:+"${HOME}/.local/state/e-agent/server.token"}"; do
    if [ -n "$f" ] && [ -r "$f" ]; then
      TOKEN="$(cat "$f")"
      break
    fi
  done
fi
if [ -z "$TOKEN" ]; then
  echo "[FAIL] 找不到 server.token（${XDG_STATE_HOME:-~/.local/state}/e-agent/server.token）" >&2
  echo "       请先启动 server 生成 token：cargo run -- --serve（或 e-agent web），或用 --token TOKEN 指定" >&2
  exit 1
fi

# ── 前置：server 是否在跑 / token 是否有效 ────────────────────────────────
BARE="$(curl -s -m 5 -o /dev/null -w '%{http_code}' "$BASE/api/sessions" 2>/dev/null || echo 000)"
if [ "$BARE" = "000" ]; then
  echo "[FAIL] 无法连接 $BASE —— server 未启动？先运行：cargo run -- --serve 或 e-agent web" >&2
  exit 1
fi
if [ "$BARE" != "200" ]; then
  # 无 token 的裸请求预期 401；再用 token 探一次，确认认证可用
  AUTHED="$(curl -s -m 5 -o /dev/null -w '%{http_code}' \
    -H "Authorization: Bearer $TOKEN" "$BASE/api/sessions" 2>/dev/null || echo 000)"
  if [ "$AUTHED" != "200" ]; then
    echo "[FAIL] 认证失败（token 无效或 server 未启动）。先运行：cargo run -- --serve 或 e-agent web；" >&2
    echo "       并确认 --token 与 ${XDG_STATE_HOME:-~/.local/state}/e-agent/server.token 一致" >&2
    exit 1
  fi
fi

# ── 工具函数 ──────────────────────────────────────────────────────────────
PASS=0
FAIL=0

# req METHOD PATH [BODY] —— 发起带认证的请求，状态码存 CODE，响应体存 $TMPD/body
req() {
  local method="$1" path="$2" body="${3:-}"
  local -a args=(-s -m 10 -o "$TMPD/body" -w '%{http_code}'
                -X "$method" -H "Authorization: Bearer $TOKEN" "$BASE$path")
  if [ -n "$body" ]; then
    args+=(-H 'Content-Type: application/json' -d "$body")
  fi
  CODE="$(curl "${args[@]}" 2>/dev/null || echo 000)"
}

# report NAME EXPECTED —— 状态码断言
report() {
  local name="$1" expected="$2"
  if [ "$CODE" = "$expected" ]; then
    echo "[PASS] $name -> $CODE"
    PASS=$((PASS + 1))
  else
    echo "[FAIL] $name -> $CODE（期望 $expected）"
    FAIL=$((FAIL + 1))
  fi
}

# report_body NAME PATTERN —— 响应体 grep 断言（紧随同一步的 req 之后）
report_body() {
  local name="$1" pattern="$2"
  if grep -q "$pattern" "$TMPD/body"; then
    echo "[PASS] $name -> 命中 '$pattern'"
    PASS=$((PASS + 1))
  else
    echo "[FAIL] $name -> 未命中 '$pattern'（body: $(head -c 200 "$TMPD/body" | tr '\n' ' ')）"
    FAIL=$((FAIL + 1))
  fi
}

# ── 1. GET /api/sessions → 200 JSON 数组 ──────────────────────────────────
req GET /api/sessions
report "GET /api/sessions" 200
report_body "GET /api/sessions body 是 JSON 数组" '^\['

# ── 2. POST /api/sessions → 201 + id ──────────────────────────────────────
req POST /api/sessions '{}'
report "POST /api/sessions" 201
SID="$(sed -n 's/.*"id":"\([^"]*\)".*/\1/p' "$TMPD/body" | head -1)"
if [ -n "$SID" ]; then
  echo "[PASS] POST /api/sessions 返回 id -> $SID"
  PASS=$((PASS + 1))
else
  echo "[FAIL] POST /api/sessions 响应中未找到 id（body: $(head -c 200 "$TMPD/body")）"
  FAIL=$((FAIL + 1))
fi

# ── 3. 创建已注册：列表包含新建 id（替代不存在的 GET /api/sessions/{id}）──
req GET /api/sessions
report "GET /api/sessions 包含新建 id（$SID）" 200
report_body "GET /api/sessions 列表命中 id" "$SID"

# ── 4. undo（空栈）→ 409 + 中文错误 ──────────────────────────────────────
# 注意：undo 栈是进程级全局的；若 server 上已发生过文件操作，此处会 200 而非 409，
# 属预期偏差（用刚启动的 server 跑最干净）。
req POST "/api/sessions/${SID}/undo"
report "POST /api/sessions/$SID/undo（空栈）" 409
report_body "undo 空栈返回中文错误" '无法撤销'

# ── 5. 会话级只读端点：history → 200 ────────────────────────────────────
req GET "/api/sessions/${SID}/history"
report "GET /api/sessions/$SID/history" 200

# ── 6. DELETE /api/sessions/{id} → 204（NO_CONTENT）──────────────────────
req DELETE "/api/sessions/${SID}"
report "DELETE /api/sessions/$SID" 204

# ── 7. btw 空 prompt → 400（纯参数校验，不依赖模型）──────────────────────
req POST "/api/sessions/${SID}/btw" '{"prompt":"   "}'
report "POST /api/sessions/$SID/btw 空 prompt" 400

# ── 8. prompt 空 text → 400（参数校验；前端实际用 text 字段）─────────────
req POST "/api/sessions/${SID}/prompt" '{"text":"  "}'
report "POST /api/sessions/$SID/prompt 空 text" 400

# ── 9. 未知 session id → 404 ─────────────────────────────────────────────
req POST /api/sessions/smoke-does-not-exist/cancel
report "POST /api/sessions/smoke-does-not-exist/cancel（未知 id）" 404

# ── 10. GET /api/tasks → 200 ─────────────────────────────────────────────
req GET /api/tasks
report "GET /api/tasks" 200
report_body "GET /api/tasks body 是 JSON 数组" '^\['

# ── 汇总 ─────────────────────────────────────────────────────────────────
echo ""
echo "$PASS passed, $FAIL failed"
if [ "$FAIL" -gt 0 ]; then
  exit 1
fi
exit 0
