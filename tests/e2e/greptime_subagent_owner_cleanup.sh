#!/usr/bin/env bash
# Real Greptime/process E2E for nested subagent owner cleanup.
set -Eeuo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
: "${GREPTIMEDB_BIN:?GREPTIMEDB_BIN must be set explicitly}"
: "${EAGENT_BIN:?EAGENT_BIN must be set explicitly}"
[[ -x "$GREPTIMEDB_BIN" ]] || { echo "missing GREPTIMEDB_BIN=$GREPTIMEDB_BIN" >&2; exit 2; }
[[ -x "$EAGENT_BIN" ]] || { echo "missing EAGENT_BIN=$EAGENT_BIN" >&2; exit 2; }

TMP=$(mktemp -d "${TMPDIR:-/tmp}/e-agent-owner-cleanup.XXXXXX")
echo "artifact root: $TMP"
WORKSPACE="$TMP/workspace"; XDG_CONFIG_HOME="$TMP/xdg-config"; XDG_STATE_HOME="$TMP/xdg-state"
mkdir -p "$WORKSPACE" "$XDG_CONFIG_HOME/e-agent" "$XDG_STATE_HOME" "$TMP/home"
export HOME="$TMP/home" XDG_CONFIG_HOME XDG_STATE_HOME NESTED_E2E_KEY=owner-cleanup-e2e
RUN_MARKER="eagent-owner-cleanup-$(date +%s)-$$"

pick_port() { python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()'; }
MOCK_PORT=$(pick_port); SERVER_PORT=$(pick_port); GT_HTTP=$(pick_port); GT_GRPC=$(pick_port); GT_MYSQL=$(pick_port); GT_PG=$(pick_port)
PORTS=("$MOCK_PORT" "$SERVER_PORT" "$GT_HTTP" "$GT_GRPC" "$GT_MYSQL" "$GT_PG")
for p in "${PORTS[@]}"; do [[ "$p" != 15403 ]] || { echo "forbidden port 15403" >&2; exit 2; }; done
echo "ports: mock=$MOCK_PORT server=$SERVER_PORT greptime_http=$GT_HTTP greptime_grpc=$GT_GRPC greptime_mysql=$GT_MYSQL greptime_pg=$GT_PG"
echo "assertion: no allocated port is 15403"

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

marker_pids() { ps -eo pid=,stat=,args= | awk -v marker="$RUN_MARKER" 'index($0, marker) && $0 !~ /awk|mock_openai_nested_background|greptimedb|greptime|e-agent/ {print}'; }
cleanup() {
  rc=$?; set +e
  for pid in $(marker_pids | awk '{print $1}'); do kill -KILL "$pid" 2>/dev/null; done
  for pid in "${SERVER_PID:-}" "${GREPTIME_PID:-}" "${MOCK_PID:-}"; do [[ -n "$pid" ]] && kill "$pid" 2>/dev/null; done
  wait 2>/dev/null || true
  if [[ "$rc" -eq 0 ]]; then
    rm -rf "$TMP"
    echo "temp cleanup: removed"
  else
    echo "temp root preserved: $TMP" >&2
  fi
  exit "$rc"
}
trap cleanup EXIT

EAGENT_OWNER_CLEANUP_E2E=1 python3 "$ROOT/tests/e2e/mock_openai_nested_background.py" "$MOCK_PORT" "$RUN_MARKER" >"$TMP/mock.log" 2>&1 & MOCK_PID=$!
"$GREPTIMEDB_BIN" standalone start --data-home "$TMP/greptime-data" --log-dir "$TMP/greptime-log" \
  --http-addr "127.0.0.1:$GT_HTTP" --grpc-bind-addr "127.0.0.1:$GT_GRPC" \
  --mysql-addr "127.0.0.1:$GT_MYSQL" --postgres-addr "127.0.0.1:$GT_PG" >"$TMP/greptime.log" 2>&1 & GREPTIME_PID=$!
pg() { psql "host=127.0.0.1 port=$GT_PG dbname=public" -v ON_ERROR_STOP=1 -Atqc "$1"; }
wait_for() { local end=$((SECONDS+90)); while ((SECONDS<end)); do "$@" && return 0; sleep .25; done; return 1; }
wait_for psql "host=127.0.0.1 port=$GT_PG dbname=public" -Atqc 'select 1'
"$EAGENT_BIN" --serve --host 127.0.0.1 --port "$SERVER_PORT" --workspace "$WORKSPACE" >"$TMP/server.log" 2>&1 & SERVER_PID=$!
wait_for test -s "$XDG_STATE_HOME/e-agent/server.token"
TOKEN=$(cat "$XDG_STATE_HOME/e-agent/server.token"); AUTH=(-H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json')
BASE="http://127.0.0.1:$SERVER_PORT"
wait_for curl -fsS "${AUTH[@]}" "$BASE/api/sessions"
workspace_id="$WORKSPACE"

run_case() {
  local case_name=$1 parent="owner-cleanup-$1" prompt
  prompt="Run the $case_name owner-cleanup case: delegate a child which starts its requested background process."
  local body
  body=$(CASE_PROMPT="$prompt" CASE_ID="$parent" python3 -c 'import json,os; print(json.dumps({"id":os.environ["CASE_ID"],"initial_prompt":os.environ["CASE_PROMPT"]}))')
  curl -fsS "${AUTH[@]}" -X POST "$BASE/api/sessions" -d "$body" >"$TMP/$case_name-create.json"
  local parent_task_id="" child_id="" child_task_id="" tasks="" own="" child_row=""
  local end=$((SECONDS+90))
  while ((SECONDS<end)); do
    tasks=$(curl -fsS "${AUTH[@]}" "$BASE/api/tasks" 2>/dev/null || true)
    parent_task_id=$(python3 -c 'import json,sys; a=json.load(sys.stdin); x=[x for x in a if x.get("session_id")==sys.argv[1] and x.get("kind")=="delegate"]; print(x[0]["id"] if x else "")' "$parent" <<<"$tasks" 2>/dev/null || true)
    child_id=$(python3 -c 'import json,sys; a=json.load(sys.stdin); x=[x for x in a if x.get("session_id")==sys.argv[1] and x.get("kind")=="delegate"]; print(x[0].get("subagent_session_id") or "" if x else "")' "$parent" <<<"$tasks" 2>/dev/null || true)
    child_task_id=$(python3 -c 'import json,sys; a=json.load(sys.stdin); x=[x for x in a if x.get("owner_session")==sys.argv[1] and x.get("kind")=="bash"]; print(x[0]["id"] if x else "")' "$child_id" <<<"$tasks" 2>/dev/null || true)
    own=$(pg "SELECT count(*) FROM running_tasks WHERE workspace_id='$workspace_id' AND session_id='$parent' AND task_id='$parent_task_id'" 2>/dev/null || true)
    child_row=""
    [[ -n "$child_id" && -n "$child_task_id" ]] && child_row=$(pg "SELECT count(*) FROM running_tasks WHERE workspace_id='$workspace_id' AND session_id='$child_id' AND task_id='$child_task_id'" 2>/dev/null || true)
    if [[ -n "$parent_task_id" && -n "$child_id" && -n "$child_task_id" && "$own" == 1 && "$child_row" == 1 ]] && marker_pids | grep -q .; then break; fi
    sleep .25
  done
  [[ -n "$parent_task_id" && -n "$child_id" && -n "$child_task_id" && "$own" == 1 && "$child_row" == 1 ]] || { echo "ASSERTION FAILED [$case_name]: parent/child IDs and exact rows did not appear" >&2; exit 1; }
  marker_pids | grep -q . || { echo "ASSERTION FAILED [$case_name]: real child marker process did not appear" >&2; exit 1; }
  echo "[$case_name] live IDs: parent_task=$parent_task_id child_session=$child_id child_task=$child_task_id"
  echo "[$case_name] assertion: exact parent+child Greptime rows and marker process live"

  if [[ "$case_name" == cancel ]]; then
    local status
    status=$(curl -sS "${AUTH[@]}" -X DELETE "$BASE/api/sessions/$parent/tasks/$parent_task_id" -o "$TMP/$case_name-cancel.json" -w '%{http_code}')
    [[ "$status" == 204 ]] || { echo "ASSERTION FAILED [cancel]: DELETE returned HTTP $status" >&2; exit 1; }
    echo "[cancel] DELETE parent delegate task=$parent_task_id HTTP=204"
  fi

  local history="" completion_count=0
  end=$((SECONDS+90))
  while ((SECONDS<end)); do
    history=$(curl -fsS "${AUTH[@]}" "$BASE/api/sessions/$parent/history" 2>/dev/null || true)
    completion_count=$(python3 -c 'import json,sys; h=json.load(sys.stdin); print(sum(1 for e in h.get("entries",[]) if e.get("type")=="background_completion"))' <<<"$history" 2>/dev/null || echo 0)
    [[ "$completion_count" == 1 ]] && break
    sleep .25
  done
  [[ "$completion_count" == 1 ]] || { echo "ASSERTION FAILED [$case_name]: parent background_completion was not observed" >&2; exit 1; }
  if [[ "$case_name" == cancel ]]; then
    grep -Fq 'subagent cancelled' < <(python3 -c 'import json,sys; h=json.load(sys.stdin); print("\n".join(e.get("output","") for e in h.get("entries",[]) if e.get("type")=="background_completion"))' <<<"$history") || { echo "ASSERTION FAILED [cancel]: completion did not say subagent cancelled" >&2; exit 1; }
  fi
  # This is deliberately the first check after observing the parent completion.
  marker_pids | grep -q . && { echo "ASSERTION FAILED [$case_name]: marker process still live at completion" >&2; exit 1; }
  [[ "$(pg "SELECT count(*) FROM running_tasks WHERE workspace_id='$workspace_id' AND session_id='$parent' AND task_id='$parent_task_id'")" == 0 ]] || { echo "ASSERTION FAILED [$case_name]: parent row remains" >&2; exit 1; }
  [[ "$(pg "SELECT count(*) FROM running_tasks WHERE workspace_id='$workspace_id' AND session_id='$child_id' AND task_id='$child_task_id'")" == 0 ]] || { echo "ASSERTION FAILED [$case_name]: child row remains" >&2; exit 1; }
  tasks=$(curl -fsS "${AUTH[@]}" "$BASE/api/tasks")
  python3 -c 'import json,sys; a=json.load(sys.stdin); p,c,ct=sys.argv[1:]; assert not any((x.get("session_id")==p and str(x.get("id"))==c) or (x.get("owner_session")==ct and x.get("kind")=="bash") for x in a)' "$parent" "$parent_task_id" "$child_id" <<<"$tasks"
  echo "[$case_name] assertions: completion=$completion_count, marker dead, exact parent row=0, exact child row=0, no matching /api/tasks wrapper/owner"
}

run_case normal
run_case cancel
echo "assertion summary: 2 isolated cases passed (normal completion; explicit DELETE cancel)"
