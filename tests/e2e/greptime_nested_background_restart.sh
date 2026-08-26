#!/usr/bin/env bash
# Nested subagent restart reproducer. running_tasks is observed with SELECT only.
set -Eeuo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
EAGENT_BIN=${EAGENT_BIN:-$ROOT/target/debug/e-agent}
TARGET=${CARGO_TARGET_DIR:-/mnt/nvme_rust/rust-targets-2/e-agent-nested-restart}
GREPTIMEDB_BIN=${GREPTIMEDB_BIN:-/home/discord9/.local/share/e-agent/greptimedb/greptime}
if [[ ! -x "$EAGENT_BIN" ]]; then
  echo "EAGENT_BIN missing; building with CARGO_TARGET_DIR=$TARGET"
  CARGO_TARGET_DIR="$TARGET" cargo build --features greptime --bin e-agent
  EAGENT_BIN="$TARGET/debug/e-agent"
fi
[[ -x "$EAGENT_BIN" ]] || { echo "missing EAGENT_BIN=$EAGENT_BIN" >&2; exit 2; }
[[ -x "$GREPTIMEDB_BIN" ]] || { echo "missing GREPTIMEDB_BIN=$GREPTIMEDB_BIN" >&2; exit 2; }

ARTIFACT_PARENT=${TMPDIR:-/tmp}
mkdir -p "$ARTIFACT_PARENT"
TMP=$(mktemp -d "$ARTIFACT_PARENT/e-agent-nested-restart.XXXXXX")
echo "artifact root: $TMP"
WORKSPACE="$TMP/workspace"; XDG_CONFIG_HOME="$TMP/xdg-config"; XDG_STATE_HOME="$TMP/xdg-state"
mkdir -p "$WORKSPACE" "$XDG_CONFIG_HOME/e-agent" "$XDG_STATE_HOME" "$TMP/home"
export HOME="$TMP/home" XDG_CONFIG_HOME XDG_STATE_HOME NESTED_E2E_KEY=nested-e2e-only
RUN_MARKER="eagent-nested-bg-$(date +%s)-$$"
CHILD_LABEL="exec -a ${RUN_MARKER}-child-own-background sleep 600"
pick_port() { python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()'; }
MOCK_PORT=$(pick_port); SERVER_A_PORT=$(pick_port); SERVER_B_PORT=$(pick_port)
GT_HTTP=$(pick_port); GT_GRPC=$(pick_port); GT_MYSQL=$(pick_port); GT_PG=$(pick_port)
for p in "$MOCK_PORT" "$SERVER_A_PORT" "$SERVER_B_PORT" "$GT_HTTP" "$GT_GRPC" "$GT_MYSQL" "$GT_PG"; do
  [[ "$p" != 15403 ]] || { echo "forbidden port 15403" >&2; exit 2; }
done
cat >"$XDG_CONFIG_HOME/e-agent/config.toml" <<EOF
 default = "mock/mock"
[providers.mock]
base_url = "http://127.0.0.1:$MOCK_PORT/v1"
api_key_env = "NESTED_E2E_KEY"
[models."mock/mock"]
model = "mock-nested"
[session]
backend = "greptime"
conn = "host=127.0.0.1 port=$GT_PG dbname=public"
EOF

cleanup() {
  rc=$?; set +e
  cleanup_failed=0
  # Kill only the exact per-run marker process groups. Do not use a broad
  # process pattern: unrelated services must never be touched.
  marker_pids_now() { ps -eo pid=,pgid=,stat=,args= | awk -v marker="$RUN_MARKER" 'index($0, marker) && $0 !~ /awk|mock_openai_nested_background|greptimedb|greptime|e-agent/ {print}'; }
  marker_groups=$(marker_pids_now | awk '{print $2}' | sort -u)
  for pgid in $marker_groups; do
    [[ "$pgid" =~ ^[0-9]+$ ]] && kill -KILL -- -"$pgid" 2>/dev/null
  done
  for pid in $(marker_pids_now | awk '{print $1}'); do kill -KILL "$pid" 2>/dev/null; done
  for _ in 1 2 3 4 5; do
    marker_pids_now | awk '$3 !~ /^Z/ {found=1} END {exit found}' || break
    sleep .1
  done
  if marker_pids_now | awk '$3 !~ /^Z/ {print; found=1} END {exit found}'; then :; else
    echo "cleanup failure: live non-zombie marker process remains" >&2
    cleanup_failed=1
  fi
  for p in "${SERVER_A_PID:-}" "${SERVER_B_PID:-}" "$GREPTIME_PID" "$MOCK_PID"; do [[ -n "$p" ]] && kill "$p" 2>/dev/null; done
  wait 2>/dev/null || true
  if [[ "$cleanup_failed" -ne 0 ]]; then rc=1; fi
  if [[ "$rc" -eq 0 ]]; then rm -rf "$TMP"; else echo "temp root preserved: $TMP" >&2; fi
  exit "$rc"
}
trap cleanup EXIT
python3 "$ROOT/tests/e2e/mock_openai_nested_background.py" "$MOCK_PORT" "$RUN_MARKER" >"$TMP/mock.log" 2>&1 & MOCK_PID=$!
"$GREPTIMEDB_BIN" standalone start --data-home "$TMP/greptime-data" --log-dir "$TMP/greptime-log" \
  --http-addr "127.0.0.1:$GT_HTTP" --grpc-bind-addr "127.0.0.1:$GT_GRPC" \
  --mysql-addr "127.0.0.1:$GT_MYSQL" --postgres-addr "127.0.0.1:$GT_PG" >"$TMP/greptime.log" 2>&1 & GREPTIME_PID=$!
pg() { psql "host=127.0.0.1 port=$GT_PG dbname=public" -v ON_ERROR_STOP=1 -Atqc "$1"; }
wait_for() { local end=$((SECONDS+90)); while ((SECONDS<end)); do "$@" && return 0; sleep .25; done; return 1; }
wait_for psql "host=127.0.0.1 port=$GT_PG dbname=public" -Atqc 'select 1'
start_server() {
  local port=$1 log=$2
  "$EAGENT_BIN" --serve --host 127.0.0.1 --port "$port" --workspace "$WORKSPACE" >"$log" 2>&1 &
  STARTED_SERVER_PID=$!
}
start_server "$SERVER_A_PORT" "$TMP/server-a.log"
SERVER_A_PID=$STARTED_SERVER_PID
wait_for test -s "$XDG_STATE_HOME/e-agent/server.token"
TOKEN=$(cat "$XDG_STATE_HOME/e-agent/server.token"); AUTH=(-H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json')
BASE_A="http://127.0.0.1:$SERVER_A_PORT"; BASE_B="http://127.0.0.1:$SERVER_B_PORT"
wait_for curl -fsS "${AUTH[@]}" "$BASE_A/api/sessions"
curl -fsS "${AUTH[@]}" -X POST "$BASE_A/api/sessions" -d '{"id":"parent-e2e","initial_prompt":"Delegate a child that starts a long-lived background bash task."}' >"$TMP/create.json"
workspace_id="$WORKSPACE"; child_id=""; tasks_json=""
end=$((SECONDS+90))
while ((SECONDS<end)); do
  tasks_json=$(curl -fsS "${AUTH[@]}" "$BASE_A/api/tasks" 2>/dev/null || true)
  child_id=$(pg "SELECT subagent_session_id FROM running_tasks WHERE workspace_id='$workspace_id' AND session_id='parent-e2e' AND subagent_session_id IS NOT NULL LIMIT 1" || true)
  own=$(pg "SELECT session_id FROM running_tasks WHERE workspace_id='$workspace_id' AND session_id <> 'parent-e2e' AND subagent_session_id IS NULL LIMIT 1" || true)
  parent_live=$(python3 -c 'import json,sys; a=json.load(sys.stdin); print(any(x.get("kind")=="delegate" and x.get("session_id")=="parent-e2e" for x in a))' <<<"$tasks_json" 2>/dev/null || true)
  child_live=$(python3 -c 'import json,sys; a=json.load(sys.stdin); print(any(x.get("kind")=="bash" and x.get("owner_session")==sys.argv[1] for x in a))' "$own" <<<"$tasks_json" 2>/dev/null || true)
  [[ -n "$child_id" && "$own" == "$child_id" && "$parent_live" == true && "$child_live" == true ]] && break
  sleep .25
done
[[ -n "$child_id" && "$own" == "$child_id" ]] || { echo "assertion failed: both DB rows did not appear" >&2; exit 1; }
parent_scope=$(pg "SELECT session_id||' | '||task_id||' | '||subagent_session_id||' | '||label FROM running_tasks WHERE workspace_id='$workspace_id' AND session_id='parent-e2e'")
child_scope=$(pg "SELECT session_id||' | '||task_id||' | NULL | '||label FROM running_tasks WHERE workspace_id='$workspace_id' AND session_id='$child_id' AND subagent_session_id IS NULL")
[[ -n "$parent_scope" && -n "$child_scope" ]] || { echo "assertion failed: exact row scopes" >&2; exit 1; }
echo "ports: mock=$MOCK_PORT serverA=$SERVER_A_PORT serverB=$SERVER_B_PORT greptime_http=$GT_HTTP greptime_grpc=$GT_GRPC greptime_mysql=$GT_MYSQL greptime_pg=$GT_PG"
echo "assertion reached: /api/tasks parent delegate live=true child background live=true"
echo "parent row scope before SIGKILL: $parent_scope"
echo "child row scope before SIGKILL: $child_scope"
# Locate the real child sleep through the server-A process tree. Do not use SQL
# or cancel/delete to alter the crash scene.
marker_pids() { ps -eo pid=,ppid=,pgid=,stat=,args= | awk -v marker="$RUN_MARKER" 'index($0, marker) && $0 !~ /awk|mock_openai_nested_background|greptimedb|greptime|e-agent/ {print}'; }
background_pid=$(marker_pids | awk '{print $1; exit}')
[[ -n "$background_pid" ]] || { echo "assertion failed: unique marker child background PID not found; evidence:" >&2; marker_pids >&2; exit 1; }
background_pgid=$(ps -o pgid= -p "$background_pid" | tr -d ' ')
[[ -n "$background_pgid" ]] || { echo "assertion failed: child background PGID not found for PID=$background_pid" >&2; exit 1; }
echo "background process before SIGKILL: $(ps -o pid=,ppid=,pgid=,stat=,args= -p "$background_pid" | sed 's/^ *//')"
# Required crash: no DELETE/cancel call appears before this point.
echo "SIGKILL server A pid=$SERVER_A_PID"
killed_server_a_pid=$SERVER_A_PID
kill -KILL "$killed_server_a_pid"; set +e; wait "$killed_server_a_pid"; server_a_exit=$?; set -e; SERVER_A_PID=
echo "server A actual exit=$server_a_exit; child background pid=$background_pid pgid=$background_pgid"
[[ "$server_a_exit" -ne 0 && "$server_a_exit" -ne 127 ]] || { echo "assertion failed: server A wait/exit was invalid: $server_a_exit" >&2; exit 1; }
[[ ! -e "/proc/$killed_server_a_pid" ]] || { echo "assertion failed: server A PID still exists: $killed_server_a_pid" >&2; exit 1; }
echo "assertion reached: server A PID $killed_server_a_pid exited and /proc entry is gone"
if marker_pids | awk '$4 !~ /^Z/ {print; found=1} END {exit found}'; then :; else
  echo "ASSERTION FAILED: marker process survived server A crash:" >&2
  marker_pids >&2
  exit 1
fi
echo "assertion reached: no live marker process after server A crash"

start_server "$SERVER_B_PORT" "$TMP/server-b.log"
SERVER_B_PID=$STARTED_SERVER_PID
wait_for curl -fsS "${AUTH[@]}" "$BASE_B/api/sessions"
resume_status=$(curl -sS "${AUTH[@]}" -X POST "$BASE_B/api/sessions" -d '{"id":"parent-e2e"}' -o "$TMP/resume.json" -w '%{http_code}')
echo "resume HTTP=$resume_status body=$(cat "$TMP/resume.json")"
after_resume=$(pg "SELECT session_id||' | '||task_id||' | '||COALESCE(subagent_session_id,'NULL')||' | '||label FROM running_tasks WHERE workspace_id='$workspace_id' ORDER BY session_id,task_id")
echo "row scopes after resume: ${after_resume:-<none>}"
if [[ "$resume_status" != 201 ]]; then echo "ASSERTION FAILED: resume must succeed; HTTP/body above; rows above" >&2; exit 1; fi
parent_after=$(pg "SELECT 1 FROM running_tasks WHERE workspace_id='$workspace_id' AND session_id='parent-e2e' LIMIT 1")
[[ -z "$parent_after" ]] || { echo "ASSERTION FAILED: parent rows were not consumed after successful resume" >&2; exit 1; }
parent_history=$(curl -fsS "${AUTH[@]}" "$BASE_B/api/sessions/parent-e2e/history")
parent_notice_text=$(python3 -c 'import json,sys; h=json.load(sys.stdin); print("\n".join(e.get("text","") for e in h.get("entries",[]) if e.get("type")=="notice" and "parent-delegate-live" in e.get("text","") and sys.argv[1] not in e.get("text","")))' "$CHILD_LABEL" <<<"$parent_history")
parent_notice_count=$(python3 -c 'import json,sys; h=json.load(sys.stdin); print(sum(1 for e in h.get("entries",[]) if e.get("type")=="notice" and "parent-delegate-live" in e.get("text","") and sys.argv[1] not in e.get("text","")))' "$CHILD_LABEL" <<<"$parent_history")
echo "parent notice count=$parent_notice_count text=${parent_notice_text:-<none>}"
[[ "$parent_notice_count" == 1 ]] || { echo "ASSERTION FAILED: parent must have exactly one parent-owned killed notice, got $parent_notice_count" >&2; exit 1; }
echo "assertion reached: parent resume consumed/notified only parent delegate row"

# The child is resumed explicitly on the same server B; its own session scope must produce exactly
# one notice and consume its own row. No SQL lifecycle writes are performed.
child_resume_status=$(curl -sS "${AUTH[@]}" -X POST "$BASE_B/api/sessions" -d "{\"id\":\"$child_id\"}" -o "$TMP/resume-child.json" -w '%{http_code}')
echo "child resume HTTP=$child_resume_status body=$(cat "$TMP/resume-child.json")"
[[ "$child_resume_status" == 201 ]] || { echo "ASSERTION FAILED: child resume must succeed" >&2; exit 1; }
child_history=$(curl -fsS "${AUTH[@]}" "$BASE_B/api/sessions/$child_id/history")
child_notice_text=$(python3 -c 'import json,sys; h=json.load(sys.stdin); print("\n".join(e.get("text","") for e in h.get("entries",[]) if e.get("type")=="notice" and sys.argv[1] in e.get("text","") and "parent-delegate-live" not in e.get("text","")))' "$CHILD_LABEL" <<<"$child_history")
child_notice_count=$(python3 -c 'import json,sys; h=json.load(sys.stdin); print(sum(1 for e in h.get("entries",[]) if e.get("type")=="notice" and sys.argv[1] in e.get("text","") and "parent-delegate-live" not in e.get("text","")))' "$CHILD_LABEL" <<<"$child_history")
echo "child notice count=$child_notice_count text=${child_notice_text:-<none>}"
child_rows=$(pg "SELECT session_id||' | '||task_id||' | '||COALESCE(subagent_session_id,'NULL')||' | '||label FROM running_tasks WHERE workspace_id='$workspace_id' AND (session_id='$child_id' OR subagent_session_id='$child_id') ORDER BY session_id,task_id")
if [[ "$child_notice_count" != 1 ]]; then
  echo "ASSERTION FAILED: child history must contain exactly one child-owned killed notice for label $CHILD_LABEL, got $child_notice_count" >&2
  echo "full child history: $child_history" >&2
  echo "child rows: ${child_rows:-<none>}" >&2
  exit 1
fi
[[ -z "$child_rows" ]] || { echo "ASSERTION FAILED: child-owned rows were not consumed after child resume; rows: $child_rows" >&2; echo "full child history: $child_history" >&2; exit 1; }
echo "assertion reached: child resume produced exactly one child-owned notice and consumed child-own row"
