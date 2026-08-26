#!/usr/bin/env bash
set -Eeuo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
TMP=$(mktemp -d "${TMPDIR:-/tmp}/eagent-bg-restart.XXXXXX")
KEEP=1; A_PID=; B_PID=; DB_PID=; MOCK_PID=; CHILD_PID=; CHILD_PGID=; TASK_MARKER=; ASSERTIONS=0; FAIL_POINT=; CLEANUP_FAILED=0
say(){ printf '[e2e] %s\n' "$*"; }
pass(){ ASSERTIONS=$((ASSERTIONS+1)); say "PASS: $*"; }
fail(){ FAIL_POINT=$1; printf '[e2e] FAIL: %s\n' "$*" >&2; exit 1; }
proc_state(){ local p=$1; [ -r "/proc/$p/stat" ] && awk '{print $3}' "/proc/$p/stat" 2>/dev/null || printf gone; }
marker_pids(){ local m=$1 p cmd; for d in /proc/[0-9]*; do p=${d##*/}; [ -r "$d/cmdline" ] || continue; cmd=$(tr '\0' ' ' <"$d/cmdline" 2>/dev/null || true); [[ "$cmd" == *"$m"* ]] && printf '%s\n' "$p"; done; }
marker_live(){ local m=$1 p s; while read -r p; do s=$(proc_state "$p"); [ "$s" != Z ] && [ "$s" != gone ] && return 0; done < <(marker_pids "$m"); return 1; }
marker_snapshot(){ local m=$1 p s cmd; while read -r p; do s=$(proc_state "$p"); cmd=$(tr '\0' ' ' <"/proc/$p/cmdline" 2>/dev/null || true); printf '%s:%s:%s ' "$p" "$s" "$cmd"; done < <(marker_pids "$m"); }
kill_marker(){ local end=$((SECONDS+10)) p; while read -r p; do kill -KILL "$p" 2>/dev/null || true; done < <(marker_pids "$TASK_MARKER"); while ((SECONDS<end)); do marker_live "$TASK_MARKER" || return 0; sleep .1; done; return 1; }
cleanup(){ local rc=0 p; set +e; kill_marker || rc=1; for p in "$A_PID" "$B_PID" "$MOCK_PID" "$DB_PID"; do [ -n "$p" ] && kill -KILL "$p" 2>/dev/null || true; done; for p in "$A_PID" "$B_PID" "$MOCK_PID" "$DB_PID"; do [ -n "$p" ] && wait "$p" 2>/dev/null || true; done; marker_live "$TASK_MARKER" && rc=1; if ((rc)); then printf '[e2e] CLEANUP FAIL marker=%s states=%s\n' "$TASK_MARKER" "$(marker_snapshot "$TASK_MARKER")" >&2; CLEANUP_FAILED=1; else say "cleanup containment passed: marker=$TASK_MARKER states=$(marker_snapshot "$TASK_MARKER")"; fi; if ((KEEP)); then say "failure artifacts retained: $TMP"; else rm -rf "$TMP"; fi; }
trap 's=$?; trap - EXIT; cleanup; [ "$CLEANUP_FAILED" -eq 0 ] || s=1; exit "$s"' EXIT
: "${GREPTIMEDB_BIN:?set GREPTIMEDB_BIN}"
EAGENT_BIN=${EAGENT_BIN:-$ROOT/target/debug/e-agent}; [ -x "$GREPTIMEDB_BIN" ] || fail "GREPTIMEDB_BIN not executable"; [ -x "$EAGENT_BIN" ] || fail "EAGENT_BIN not executable"; command -v curl >/dev/null; command -v psql >/dev/null; command -v ps >/dev/null
ports=$(python3 - <<'PY'
import socket
x=[]
for _ in range(7):
 s=socket.socket(); s.bind(('127.0.0.1',0)); x.append(s)
print(*[s.getsockname()[1] for s in x])
for s in x:s.close()
PY
); read -r DB_HTTP DB_GRPC DB_MYSQL DB_PG A_PORT B_PORT MOCK_PORT <<<"$ports"; [ "$DB_PG" != 15403 ] || fail forbidden-port
say "dynamic ports: db http=$DB_HTTP grpc=$DB_GRPC mysql=$DB_MYSQL pg=$DB_PG serverA=$A_PORT serverB=$B_PORT mock=$MOCK_PORT"
DATA_HOME=$TMP/greptime-data; mkdir -p "$DATA_HOME" "$TMP/config-a/e-agent" "$TMP/config-b/e-agent" "$TMP/state-a" "$TMP/state-b" "$TMP/ws/.e-agent"
for c in "$TMP/config-a/e-agent/config.toml" "$TMP/config-b/e-agent/config.toml"; do cat >"$c" <<EOF
default = "mock/background"
[providers.mock]
base_url = "http://127.0.0.1:$MOCK_PORT/v1"
api_key_env = "E2E_MOCK_KEY"
[models."mock/background"]
model = "mock-background"
[session]
backend = "greptime"
conn = "host=127.0.0.1 port=$DB_PG dbname=public"
EOF
done
TASK_MARKER="eagent-bg-${RANDOM}-$(date +%s%N)-$RANDOM"; export E2E_TASK_MARKER=$TASK_MARKER E2E_MOCK_KEY=e2e-local-only E2E_MOCK_PORT=$MOCK_PORT XDG_CONFIG_HOME=$TMP/config-a XDG_STATE_HOME=$TMP/state-a HOME=$TMP/home-a; mkdir -p "$HOME"
"$GREPTIMEDB_BIN" standalone start --data-home "$DATA_HOME" --http-addr "127.0.0.1:$DB_HTTP" --grpc-bind-addr "127.0.0.1:$DB_GRPC" --mysql-addr "127.0.0.1:$DB_MYSQL" --postgres-addr "127.0.0.1:$DB_PG" >"$TMP/greptime.log" 2>&1 & DB_PID=$!
PG_ARGS=(-h 127.0.0.1 -p "$DB_PG" -U postgres -d public)
for _ in $(seq 1 225); do psql "${PG_ARGS[@]}" -Atqc 'select 1' >/dev/null 2>&1 && break; kill -0 "$DB_PID" 2>/dev/null || fail greptime-exited; sleep .2; done
psql "${PG_ARGS[@]}" -Atqc 'select 1' >/dev/null || fail greptime-not-ready; pass isolated-greptime
python3 "$ROOT/tests/e2e/mock_openai_background.py" >"$TMP/mock.log" 2>&1 & MOCK_PID=$!
for _ in $(seq 1 150); do curl -fsS "http://127.0.0.1:$MOCK_PORT/health" >/dev/null 2>&1 && break; sleep .2; done; curl -fsS "http://127.0.0.1:$MOCK_PORT/health" >/dev/null || fail mock-not-ready; pass local-mock
start_server(){ setsid env XDG_CONFIG_HOME="$1" XDG_STATE_HOME="$2" HOME="$TMP/home-$3" "$EAGENT_BIN" --serve --host 127.0.0.1 --port "$4" --workspace "$TMP/ws" >"$5" 2>&1 & STARTED_PID=$!; }
ready(){ local port=$1 tok=$2 pid=$3; for _ in $(seq 1 225); do kill -0 "$pid" 2>/dev/null || fail server-exited; [ -s "$tok" ] && curl -fsS -H "Authorization: Bearer $(cat "$tok")" "http://127.0.0.1:$port/api/sessions" >/dev/null 2>&1 && { cat "$tok"; return; }; sleep .2; done; fail server-not-ready; }
start_server "$TMP/config-a" "$TMP/state-a" a "$A_PORT" "$TMP/server-a.log"; A_PID=$STARTED_PID; TOKEN_A=$(ready "$A_PORT" "$TMP/state-a/e-agent/server.token" "$A_PID"); pass server-A
SID=e2e-bg-restart; r=$(curl -sS -w '\n%{http_code}' -H "Authorization: Bearer $TOKEN_A" -H content-type:application/json -d "{\"id\":\"$SID\"}" "http://127.0.0.1:$A_PORT/api/sessions"); [ "${r##*$'\n'}" = 201 ] || fail create-session; pass create-http
r=$(curl -sS -w '\n%{http_code}' -H "Authorization: Bearer $TOKEN_A" -H content-type:application/json -d '{"text":"Start the deterministic long-running background task."}' "http://127.0.0.1:$A_PORT/api/sessions/$SID/prompt"); [ "${r##*$'\n'}" = 202 ] || fail prompt-http; pass prompt-http
for _ in $(seq 1 225); do tasks=$(curl -sS -H "Authorization: Bearer $TOKEN_A" "http://127.0.0.1:$A_PORT/api/tasks" || true); grep -q 'sleep 300' <<<"$tasks" && marker_pids "$TASK_MARKER" | grep -q . && break; sleep .2; done
marker_pids "$TASK_MARKER" | grep -q . || fail task-marker-not-visible; CHILD_PID=$(marker_pids "$TASK_MARKER" | head -1); CHILD_PGID=$(ps -o pgid= -p "$CHILD_PID" | tr -d ' '); before=$(proc_state "$CHILD_PID"); [ "$before" != Z ] || fail child-zombie-before; say "host child before crash: pid=$CHILD_PID state=$before pgid=$CHILD_PGID argv=$(tr '\0' ' ' <"/proc/$CHILD_PID/cmdline")"; pass task-running-http
for _ in $(seq 1 150); do row=$(psql "${PG_ARGS[@]}" -Atqc "select count(*) from running_tasks where session_id='$SID'" 2>/dev/null || true); [ "$row" = 1 ] && break; sleep .2; done; [ "${row:-}" = 1 ] || fail durable-row; pass durable-row
kill -KILL "$A_PID"; wait "$A_PID" 2>/dev/null || true; A_PID=; after=$(proc_state "$CHILD_PID"); say "host child after crash: pid=$CHILD_PID state=$after pgid=$CHILD_PGID marker-processes=$(marker_snapshot "$TASK_MARKER")"; end=$((SECONDS+10)); while ((SECONDS<end)) && marker_live "$TASK_MARKER"; do sleep .1; done; if marker_live "$TASK_MARKER"; then fail "background marker process survived server A termination"; fi; pass crash-contained
XDG_CONFIG_HOME="$TMP/config-b" XDG_STATE_HOME="$TMP/state-b" HOME="$TMP/home-b" "$EAGENT_BIN" --serve --host 127.0.0.1 --port "$B_PORT" --workspace "$TMP/ws" >"$TMP/server-b.log" 2>&1 & B_PID=$!; TOKEN_B=$(ready "$B_PORT" "$TMP/state-b/e-agent/server.token" "$B_PID"); pass server-B
r=$(curl -sS -w '\n%{http_code}' -H "Authorization: Bearer $TOKEN_B" -H content-type:application/json -d "{\"id\":\"$SID\"}" "http://127.0.0.1:$B_PORT/api/sessions"); [ "${r##*$'\n'}" = 201 ] || fail "resume HTTP/body: $r"; pass resume-http
for _ in $(seq 1 225); do h=$(curl -sS -H "Authorization: Bearer $TOKEN_B" "http://127.0.0.1:$B_PORT/api/sessions/$SID/history" || true); grep -Fq 'killed with the process' <<<"$h" && break; sleep .2; done; grep -Fq 'killed with the process' <<<"${h:-}" || fail "history notice HTTP/body: $h"; pass history-notice
curl -sS --max-time 5 -N -H "Authorization: Bearer $TOKEN_B" "http://127.0.0.1:$B_PORT/api/sessions/$SID/events" >"$TMP/events.sse" || true; grep -Fq 'killed with the process' "$TMP/events.sse" || fail "SSE notice HTTP/body: $(cat "$TMP/events.sse")"; pass sse-notice
t=$(curl -sS -H "Authorization: Bearer $TOKEN_B" "http://127.0.0.1:$B_PORT/api/tasks"); ! grep -Fq '"session_id":"'$SID'"' <<<"$t" || fail stale-task; pass no-stale-task
row=$(psql "${PG_ARGS[@]}" -Atqc "select count(*) from running_tasks where session_id='$SID'" 2>/dev/null || true); [ "$row" = 0 ] || fail row-not-consumed; pass row-consumed
[ "$ASSERTIONS" -gt 0 ] || fail zero-assertions; KEEP=0; say "PASS background restart recovery ($ASSERTIONS assertions)"
