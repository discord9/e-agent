#!/usr/bin/env bash
# Process-level Greptime E2E for ownerless stale-row delegate resume.
set -Eeuo pipefail
umask 077

if [[ -v GREPTIME_PG || -v GREPTIME_CONN || -v EAGENT_BASE ]]; then
    echo "refusing inherited GREPTIME_PG, GREPTIME_CONN, or EAGENT_BASE" >&2
    exit 2
fi
: "${EAGENT_BIN:?EAGENT_BIN must be an absolute prebuilt e-agent executable}"
: "${GREPTIMEDB_BIN:?GREPTIMEDB_BIN must be an absolute prebuilt GreptimeDB executable}"
for binary in EAGENT_BIN GREPTIMEDB_BIN; do
    value=${!binary}
    [[ "$value" == /* && -f "$value" && -x "$value" ]] || {
        echo "$binary must be an absolute regular executable: $value" >&2
        exit 2
    }
done
command -v curl >/dev/null || { echo "curl is required" >&2; exit 2; }
command -v psql >/dev/null || { echo "psql is required" >&2; exit 2; }
command -v python3 >/dev/null || { echo "python3 is required" >&2; exit 2; }

REPO=$(cd "$(dirname "$0")/../.." && pwd -P)
ARTIFACT_ROOT="$REPO/.e2e-run"
mkdir -p "$ARTIFACT_ROOT"
ROOT=$(mktemp -d "$ARTIFACT_ROOT/resume-stale-owner.XXXXXX")
RUN_MARKER="resume-stale-owner-${$}-$(date +%s%N)"
WORKSPACE="$ROOT/workspace"
DATA_HOME="$ROOT/greptime-data"
LOG_DIR="$ROOT/greptime-log"
export HOME="$ROOT/home"
export XDG_CONFIG_HOME="$ROOT/config"
export XDG_STATE_HOME="$ROOT/state"
mkdir -p "$WORKSPACE" "$DATA_HOME" "$LOG_DIR" "$HOME" \
    "$XDG_CONFIG_HOME/e-agent" "$XDG_STATE_HOME"

MOCK_PID=""; GREPTIME_PID=""; SERVER_PID=""; KEEP_ARTIFACTS=1
say() { printf '[resume-stale-owner] %s\n' "$*"; }
fail() { echo "ASSERTION FAILED: $*" >&2; exit 1; }

# Marker matching is limited to this run's unique artifact paths/argument and
# is supplementary to the three saved service PIDs; no process-name killing.
marker_pids() {
    local pid cmd
    for proc in /proc/[0-9]*; do
        pid=${proc##*/}
        [[ -r "$proc/cmdline" ]] || continue
        cmd=$(tr '\0' ' ' <"$proc/cmdline" 2>/dev/null || true)
        [[ "$cmd" == *"$RUN_MARKER"* ]] && printf '%s\n' "$pid"
    done
}
pid_contains() {
    local pid=$1 expected=$2 cmd
    [[ -r "/proc/$pid/cmdline" ]] || return 1
    cmd=$(tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null || true)
    [[ "$cmd" == *"$expected"* ]]
}
stop_exact() {
    local pid=$1 name=$2 expected=$3
    [[ -n "$pid" ]] || return 0
    if kill -0 "$pid" 2>/dev/null; then
        if ! pid_contains "$pid" "$expected"; then
            echo "containment failure: $name PID $pid does not contain expected argument $expected; refusing to signal" >&2
            return 1
        fi
        kill -TERM "$pid" 2>/dev/null || true
        for _ in {1..100}; do
            kill -0 "$pid" 2>/dev/null || break
            sleep 0.1
        done
        if kill -0 "$pid" 2>/dev/null; then
            if ! pid_contains "$pid" "$expected"; then
                echo "containment failure: $name PID $pid changed before KILL; refusing to signal" >&2
                return 1
            fi
            say "$name PID $pid did not stop after TERM; sending KILL"
            kill -KILL "$pid" 2>/dev/null || true
        fi
    fi
    wait "$pid" 2>/dev/null || true
}
cleanup() {
    local status=$?
    trap - EXIT
    set +e
    # The marker is an exact per-run containment check, not a PID ownership
    # probe and not a broad pattern.
    for marker_pid in $(marker_pids); do
        [[ "$marker_pid" != "$$" ]] && kill -TERM "$marker_pid" 2>/dev/null || true
    done
    # Saved PIDs are signaled only after checking their unique per-run argv.
    stop_exact "$SERVER_PID" e-agent "$WORKSPACE" || status=1
    stop_exact "$MOCK_PID" mock "$RUN_MARKER" || status=1
    stop_exact "$GREPTIME_PID" GreptimeDB "$DATA_HOME" || status=1
    for _ in {1..30}; do
        marker_pids | grep -q . || break
        sleep 0.1
    done
    if marker_pids | grep -q .; then
        echo "cleanup failed: marker processes remain: $(marker_pids | tr '\n' ' ')" >&2
        status=1
    fi
    if (( status == 0 && KEEP_ARTIFACTS == 0 )); then
        rm -rf "$ROOT"
        say "cleanup: exact rows/services stopped; removed $ROOT"
    else
        echo "failure artifacts retained: $ROOT" >&2
    fi
    exit "$status"
}
trap cleanup EXIT

read -r MOCK_PORT SERVER_PORT GT_HTTP GT_GRPC GT_MYSQL GT_PG < <(
    python3 - <<'PY'
import socket
sockets = []
try:
    for _ in range(6):
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.bind(("127.0.0.1", 0))
        sockets.append(sock)
    ports = [sock.getsockname()[1] for sock in sockets]
    if len(set(ports)) != 6 or 15403 in ports:
        raise SystemExit("dynamic allocator returned duplicate or forbidden port")
    print(*ports)
finally:
    for sock in sockets:
        sock.close()
PY
)
for port in "$MOCK_PORT" "$SERVER_PORT" "$GT_HTTP" "$GT_GRPC" "$GT_MYSQL" "$GT_PG"; do
    [[ "$port" =~ ^[0-9]+$ && "$port" != 15403 ]] || fail "invalid dynamic port $port"
done
say "ports: mock=$MOCK_PORT server=$SERVER_PORT greptime_http=$GT_HTTP greptime_grpc=$GT_GRPC greptime_mysql=$GT_MYSQL greptime_pg=$GT_PG"
say "assertion: all listeners are distinct loopback ports and none is 15403"

cat >"$XDG_CONFIG_HOME/e-agent/config.toml" <<EOF
 default = "mock/resume-stale-owner"
[providers.mock]
base_url = "http://127.0.0.1:$MOCK_PORT/v1"
api_key_env = "RESUME_STALE_OWNER_KEY"
[models."mock/resume-stale-owner"]
model = "mock-resume-stale-owner"
[session]
backend = "greptime"
conn = "host=127.0.0.1 port=$GT_PG dbname=public"
EOF
export RESUME_STALE_OWNER_KEY=isolated-resume-stale-owner
PG_CONN="host=127.0.0.1 port=$GT_PG user=postgres dbname=public"
pg() { psql "$PG_CONN" -v ON_ERROR_STOP=1 -Atqc "$1"; }
wait_for() {
    local end=$((SECONDS + 90))
    while (( SECONDS < end )); do
        "$@" && return 0
        sleep 0.2
    done
    return 1
}

python3 "$REPO/tests/e2e/mock_openai_resume_stale_owner.py" "$MOCK_PORT" "$RUN_MARKER" \
    >"$ROOT/mock.log" 2>&1 & MOCK_PID=$!
"$GREPTIMEDB_BIN" standalone start --data-home "$DATA_HOME" --log-dir "$LOG_DIR" \
    --http-addr "127.0.0.1:$GT_HTTP" --grpc-bind-addr "127.0.0.1:$GT_GRPC" \
    --mysql-addr "127.0.0.1:$GT_MYSQL" --postgres-addr "127.0.0.1:$GT_PG" \
    >"$ROOT/greptime.log" 2>&1 & GREPTIME_PID=$!
wait_for curl -fsS "http://127.0.0.1:$MOCK_PORT/health" >/dev/null
wait_for psql "$PG_CONN" -Atqc 'SELECT 1' >/dev/null
say "A: isolated Greptime public database and deterministic mock are ready"

"$EAGENT_BIN" --serve --host 127.0.0.1 --port "$SERVER_PORT" --workspace "$WORKSPACE" \
    >"$ROOT/e-agent.log" 2>&1 & SERVER_PID=$!
TOKEN_FILE="$XDG_STATE_HOME/e-agent/server.token"
BASE="http://127.0.0.1:$SERVER_PORT"
wait_for test -s "$TOKEN_FILE"
TOKEN=$(cat "$TOKEN_FILE")
AUTH=(-H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json')
wait_for curl -fsS "${AUTH[@]}" "$BASE/api/sessions" >/dev/null
say "A: real e-agent --serve is ready (PID=$SERVER_PID)"

PARENT=resume-stale-owner-parent
curl -fsS "${AUTH[@]}" -X POST "$BASE/api/sessions" \
    -d '{"id":"resume-stale-owner-parent","initial_prompt":"Create and finish a child transcript for the stale-owner E2E."}' \
    >"$ROOT/create-parent.json"
PARENT_HISTORY="$ROOT/parent-create-history.json"
CHILD_ID=""
for _ in {1..360}; do
    curl -fsS "${AUTH[@]}" "$BASE/api/sessions/$PARENT/history" >"$PARENT_HISTORY" 2>/dev/null || true
    CHILD_ID=$(python3 - "$PARENT_HISTORY" <<'PY'
import json, re, sys
try:
    text = json.dumps(json.load(open(sys.argv[1], encoding="utf-8")))
except (OSError, ValueError):
    raise SystemExit
found = re.findall(r"subagent session: (sub-[A-Za-z0-9_-]+)", text)
print(found[-1] if found else "")
PY
)
    tasks=$(curl -fsS "${AUTH[@]}" "$BASE/api/tasks" 2>/dev/null || true)
    if [[ -n "$CHILD_ID" ]] && ! grep -Fq '"subagent_session_id":"'"$CHILD_ID"'"' <<<"$tasks"; then
        break
    fi
    sleep 0.25
done
[[ -n "$CHILD_ID" ]] || fail "delegate did not create a persisted child session"
# The completed delegate wrapper has removed its local SessionHandle; the
# absence of the child task in the public registry is the handle-free boundary.
tasks=$(curl -fsS "${AUTH[@]}" "$BASE/api/tasks")
! grep -Fq '"subagent_session_id":"'"$CHILD_ID"'"' <<<"$tasks" || fail "child SessionHandle remains live"
say "B: child transcript persisted as $CHILD_ID and no live child handle remains"

# The server is deliberately still alive. Insert ownerless stale rows using
# only the application columns; the rows describe interrupted work, not a PID.
kill -0 "$SERVER_PID" || fail "server stopped before stale-row fixture"
OWN_TASK=910001
INBOUND_TASK=910002
# Application task timestamps are generated from a strictly monotonic
# microsecond clock; use ISO literals with exactly six fractional digits for
# manually inserted stale rows. The values are numeric/date output only.
OWN_STARTED_AT=$(date -u '+%Y-%m-%dT%H:%M:%S.%6N')
INBOUND_STARTED_AT=$(date -u '+%Y-%m-%dT%H:%M:%S.%6N')
while [[ "$INBOUND_STARTED_AT" == "$OWN_STARTED_AT" ]]; do
    INBOUND_STARTED_AT=$(date -u '+%Y-%m-%dT%H:%M:%S.%6N')
done
pg "INSERT INTO running_tasks (workspace_id, session_id, task_id, label, full_command, subagent_session_id, started_at) VALUES ('$WORKSPACE', '$CHILD_ID', $OWN_TASK, 'stale-row-own-task', NULL, NULL, TIMESTAMP '$OWN_STARTED_AT')"
pg "INSERT INTO running_tasks (workspace_id, session_id, task_id, label, full_command, subagent_session_id, started_at) VALUES ('$WORKSPACE', '$PARENT', $INBOUND_TASK, 'stale-row-inbound-parent', NULL, '$CHILD_ID', TIMESTAMP '$INBOUND_STARTED_AT')"
[[ "$(pg "SELECT count(*) FROM running_tasks WHERE workspace_id='$WORKSPACE' AND session_id='$CHILD_ID' AND task_id=$OWN_TASK AND subagent_session_id IS NULL")" == 1 ]] || fail "own stale row fixture missing"
[[ "$(pg "SELECT count(*) FROM running_tasks WHERE workspace_id='$WORKSPACE' AND session_id='$PARENT' AND task_id=$INBOUND_TASK AND subagent_session_id='$CHILD_ID'")" == 1 ]] || fail "inbound stale row fixture missing"
say "C: exact own stale row and distinct inbound parent row inserted"

RESUME_PROMPT="Resume child session $CHILD_ID through delegate and report success."
curl -fsS "${AUTH[@]}" -X POST "$BASE/api/sessions/$PARENT/prompt" \
    -d "$(python3 -c 'import json,sys; print(json.dumps({"text":sys.argv[1]}))' "$RESUME_PROMPT")" \
    >"$ROOT/resume-prompt.json"
RESUME_HISTORY="$ROOT/parent-resume-history.json"
for _ in {1..360}; do
    curl -fsS "${AUTH[@]}" "$BASE/api/sessions/$PARENT/history" >"$RESUME_HISTORY" 2>/dev/null || true
    grep -Fq 'parent resume completed successfully' "$RESUME_HISTORY" && break
    sleep 0.25
done
grep -Fq 'parent resume completed successfully' "$RESUME_HISTORY" || fail "actual parent/model delegate resume did not complete"
! grep -Eiq 'unfinished task owners block resume|PID alive' "$RESUME_HISTORY" || fail "old stale-owner resume error was emitted"
python3 - "$RESUME_HISTORY" <<'PY'
import json, sys
history = json.load(open(sys.argv[1], encoding="utf-8"))
resume_tools = [
    entry["message"]["Tool"]
    for entry in history.get("entries", [])
    if entry.get("type") == "message"
    and "Tool" in entry.get("message", {})
    and entry["message"]["Tool"].get("call_id") == "resume-stale-owner-resume"
]
if len(resume_tools) != 1 or resume_tools[0].get("is_error") is not False:
    raise SystemExit("delegate resume tool call was not a single successful call")
if "subagent session:" not in resume_tools[0].get("content", ""):
    raise SystemExit("successful delegate resume returned no subagent session")
PY
OWN_NOTICE_COUNT=$(python3 - "$CHILD_ID" "$BASE" "$TOKEN" <<'PY'
import json, sys, urllib.request
sid, base, token = sys.argv[1:]
request = urllib.request.Request(
    f"{base}/api/sessions/{sid}/history",
    headers={"Authorization": f"Bearer {token}"},
)
with urllib.request.urlopen(request) as response:
    history = json.load(response)
entries = history.get("entries", [])
notices = [
    entry for entry in entries
    if entry.get("type") == "notice"
    and "stale-row-own-task" in json.dumps(entry)
]
text = json.dumps(history)
if "unfinished task owners block resume" in text or "PID alive" in text:
    raise SystemExit("old stale-owner error in child history")
print(len(notices))
PY
)
[[ "$OWN_NOTICE_COUNT" == 1 ]] || fail "expected exactly one durable own-row recovery notice, got $OWN_NOTICE_COUNT"
[[ "$(pg "SELECT count(*) FROM running_tasks WHERE workspace_id='$WORKSPACE' AND session_id='$CHILD_ID' AND task_id=$OWN_TASK")" == 0 ]] || fail "exact own stale row was not consumed"
[[ "$(pg "SELECT count(*) FROM running_tasks WHERE workspace_id='$WORKSPACE' AND session_id='$PARENT' AND task_id=$INBOUND_TASK AND subagent_session_id='$CHILD_ID'")" == 1 ]] || fail "inbound parent row was consumed by child resume"
[[ "$OWN_NOTICE_COUNT" == 1 ]] || fail "duplicate own recovery notice"
say "D: delegate resume succeeded; no old error; exactly one durable notice; own row=0; inbound row=1"

# The focused Rust coverage already proves a live local child handle rejects
# concurrent resume. This process E2E intentionally does not build a second
# long-lived orchestration around that unit-level admission check.
say "E: concurrent live-child block covered by focused Rust test; omitted here by design"

# The inbound stale row is still present, but there is no local child handle.
WEB_STATUS=$(curl -sS "${AUTH[@]}" -X POST "$BASE/api/sessions" \
    -d "{\"id\":\"$CHILD_ID\"}" -o "$ROOT/web-resume.json" -w '%{http_code}')
[[ "$WEB_STATUS" == 201 ]] || fail "web explicit resume returned HTTP $WEB_STATUS: $(cat "$ROOT/web-resume.json")"
[[ "$(pg "SELECT count(*) FROM running_tasks WHERE workspace_id='$WORKSPACE' AND session_id='$PARENT' AND task_id=$INBOUND_TASK AND subagent_session_id='$CHILD_ID'")" == 1 ]] || fail "web resume consumed inbound parent row"
WEB_HISTORY="$ROOT/web-resume-history.json"
curl -fsS "${AUTH[@]}" "$BASE/api/sessions/$CHILD_ID/history" >"$WEB_HISTORY"
WEB_NOTICE_COUNT=$(python3 - "$WEB_HISTORY" <<'PY'
import json, sys
history = json.load(open(sys.argv[1], encoding="utf-8"))
print(sum(
    1 for entry in history.get("entries", [])
    if entry.get("type") == "notice"
    and "stale-row-own-task" in json.dumps(entry)
))
PY
)
[[ "$WEB_NOTICE_COUNT" == 1 ]] || fail "web resume duplicated or lost own recovery notice: $WEB_NOTICE_COUNT"
say "F: web POST /api/sessions child returned HTTP 201 despite stale inbound row; inbound row remains; durable notice count stays 1"

# Final fixture cleanup is exact and restricted to this run's workspace/ids.
pg "DELETE FROM running_tasks WHERE workspace_id='$WORKSPACE' AND session_id='$CHILD_ID' AND task_id=$OWN_TASK"
pg "DELETE FROM running_tasks WHERE workspace_id='$WORKSPACE' AND session_id='$PARENT' AND task_id=$INBOUND_TASK"
[[ "$(pg "SELECT count(*) FROM running_tasks WHERE workspace_id='$WORKSPACE' AND (session_id='$CHILD_ID' AND task_id=$OWN_TASK OR session_id='$PARENT' AND task_id=$INBOUND_TASK)")" == 0 ]] || fail "exact fixture rows remain"
curl -fsS "${AUTH[@]}" -X DELETE "$BASE/api/sessions/$CHILD_ID" >/dev/null
curl -fsS "${AUTH[@]}" -X DELETE "$BASE/api/sessions/$PARENT" >/dev/null
[[ "$(pg "SELECT count(*) FROM running_tasks WHERE workspace_id='$WORKSPACE'")" == 0 ]] || fail "isolated workspace still has running_tasks rows"
KEEP_ARTIFACTS=0
say "PASS: stale-owner Greptime delegate/web resume E2E; exact fixture rows cleared"
