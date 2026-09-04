#!/usr/bin/env bash
# Real Greptime/process E2E for the production five-minute subagent stall warning.
set -Eeuo pipefail
umask 077

if [[ -v GREPTIME_PG || -v GREPTIME_CONN || -v EAGENT_BASE ]]; then
    echo "refusing inherited GREPTIME_PG, GREPTIME_CONN, or EAGENT_BASE" >&2
    exit 2
fi
: "${EAGENT_BIN:?EAGENT_BIN must be set explicitly}"
: "${GREPTIMEDB_BIN:?GREPTIMEDB_BIN must be set explicitly}"
for name in EAGENT_BIN GREPTIMEDB_BIN; do
    value=${!name}
    if [[ "$value" != /* || ! -f "$value" || ! -x "$value" ]]; then
        echo "$name must be an absolute regular executable: $value" >&2
        exit 2
    fi
done

REPO_ROOT=$(cd "$(dirname "$0")/../.." && pwd -P)
ARTIFACT_PARENT=${EAGENT_STALL_E2E_ARTIFACT_ROOT:-$REPO_ROOT/.e-agent/e2e-artifacts}
mkdir -p "$ARTIFACT_PARENT"
chmod 700 "$ARTIFACT_PARENT"
ROOT=$(mktemp -d "$ARTIFACT_PARENT/subagent-stall.XXXXXX")
CLEANUP_OK=1
MOCK_PID=""; MOCK_STARTTIME=""
GREPTIME_PID=""; GREPTIME_STARTTIME=""
EAGENT_PID=""; EAGENT_STARTTIME=""
SSE_PID=""; SSE_STARTTIME=""
RUN_MARKER="eagent-subagent-stall-$(date +%s)-$$"
WORKSPACE="$ROOT/workspace"
mkdir -p "$WORKSPACE" "$ROOT/greptime-data" "$ROOT/greptime-log" \
    "$ROOT/home" "$ROOT/config/e-agent" "$ROOT/state/e-agent"
chmod 700 "$ROOT" "$WORKSPACE" "$ROOT/greptime-data" "$ROOT/greptime-log" \
    "$ROOT/home" "$ROOT/config" "$ROOT/state"
WORKSPACE=$(cd "$WORKSPACE" && pwd -P)
export HOME="$ROOT/home" XDG_CONFIG_HOME="$ROOT/config" XDG_STATE_HOME="$ROOT/state"
export STALL_E2E_KEY=subagent-stall-e2e
MOCK_CALLS_FILE="$ROOT/mock.calls"
export MOCK_CALLS_FILE

echo "artifact root: $ROOT"

proc_starttime() {
    local pid=$1 stat
    stat=$(cat "/proc/$pid/stat" 2>/dev/null) || return 1
    # comm is parenthesized; starttime is field 22, i.e. token 20 after state.
    awk '{print $22}' <<<"$stat"
}
record_pid() {
    local pid=$1
    proc_starttime "$pid"
}
pid_identity_matches() {
    local pid=$1 expected=$2 actual
    [[ -n "$pid" && -n "$expected" ]] || return 1
    kill -0 "$pid" 2>/dev/null || return 1
    actual=$(proc_starttime "$pid") || return 1
    [[ "$actual" == "$expected" ]]
}
stop_exact() {
    local pid=$1 name=$2 expected=$3
    [[ -n "$pid" ]] || return 0
    if pid_identity_matches "$pid" "$expected"; then
        echo "stopping exact $name PID=$pid"
        kill -TERM "$pid" 2>/dev/null || true
        for _ in {1..100}; do
            pid_identity_matches "$pid" "$expected" || break
            sleep 0.1
        done
        if pid_identity_matches "$pid" "$expected"; then
            echo "exact $name PID=$pid did not exit after TERM; sending KILL" >&2
            kill -KILL "$pid" 2>/dev/null || true
        fi
    else
        echo "refusing to signal $name PID=$pid: process identity changed or is unavailable" >&2
    fi
    wait "$pid" 2>/dev/null || true
}

# The only processes matching this marker are the child command's exact
# process(es).  The mock provider is explicitly excluded because its command
# line contains the marker as an argument but is not owned by the child task.
marker_rows() {
    ps -eo pid=,pgid=,stat=,args= | awk -v marker="$RUN_MARKER" \
        'index($0, marker) && $0 !~ /awk|mock_openai_subagent_stall|greptimedb|greptime|e-agent/ {print}'
}

signal_marker_pid() {
    local pid=$1 signal=$2 identity current
    identity=$(proc_starttime "$pid") || return 0
    current=$(ps -o args= -p "$pid" 2>/dev/null || true)
    [[ "$current" == *"$RUN_MARKER"* && "$current" != *mock_openai_subagent_stall* ]] || return 0
    # Re-check the exact /proc identity after the marker/data-home-style
    # command-line check and immediately before every signal.
    pid_identity_matches "$pid" "$identity" || {
        echo "refusing to signal marker PID=$pid: process identity changed" >&2
        return 0
    }
    kill "$signal" "$pid" 2>/dev/null || true
}

kill_marker_processes() {
    local pid
    # The bash facade gives each child command its own process group. Signal
    # only members whose marker and exact starttime are both still verified;
    # never use pkill/killall or a broad executable match.
    while read -r pid; do
        [[ "$pid" =~ ^[0-9]+$ ]] || continue
        echo "stopping marker-owned PID=$pid"
        signal_marker_pid "$pid" -KILL
    done < <(marker_rows | awk '{print $1}' | sort -u)
}

cleanup() {
    local rc=$?
    trap - EXIT INT TERM HUP
    set +e
    if [[ -n "$SSE_PID" ]]; then
        stop_exact "$SSE_PID" SSE "$SSE_STARTTIME" || CLEANUP_OK=0
    fi
    kill_marker_processes
    for _ in {1..30}; do
        surviving_markers=$(marker_rows | awk '$3 !~ /^Z/ {print}')
        if [[ -z "$surviving_markers" ]]; then
            break
        fi
        sleep 0.1
    done
    surviving_markers=$(marker_rows | awk '$3 !~ /^Z/ {print}')
    if [[ -n "$surviving_markers" ]]; then
        echo "cleanup failure: live non-zombie marker process remains:" >&2
        printf '%s\n' "$surviving_markers" >&2
        CLEANUP_OK=0
    fi
    stop_exact "$EAGENT_PID" e-agent "$EAGENT_STARTTIME" || CLEANUP_OK=0
    stop_exact "$GREPTIME_PID" GreptimeDB "$GREPTIME_STARTTIME" || CLEANUP_OK=0
    stop_exact "$MOCK_PID" mock-provider "$MOCK_STARTTIME" || CLEANUP_OK=0
    if (( CLEANUP_OK && rc == 0 )); then
        rm -rf "$ROOT" || CLEANUP_OK=0
        echo "temp cleanup: removed"
    else
        echo "temp root preserved: $ROOT" >&2
    fi
    (( CLEANUP_OK )) || rc=1
    exit "$rc"
}
trap cleanup EXIT INT TERM HUP

pick_ports() {
    python3 - <<'PY'
import socket
ports = []
for _ in range(6):
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.bind(("127.0.0.1", 0))
    ports.append(sock.getsockname()[1])
    sock.close()
if len(set(ports)) != 6 or 15403 in ports:
    raise SystemExit("port allocator returned duplicate or forbidden port")
print(*ports)
PY
}
read -r MOCK_PORT EAGENT_PORT GT_HTTP GT_GRPC GT_MYSQL GT_PG < <(pick_ports)
PORTS=("$MOCK_PORT" "$EAGENT_PORT" "$GT_HTTP" "$GT_GRPC" "$GT_MYSQL" "$GT_PG")
for port in "${PORTS[@]}"; do
    [[ "$port" =~ ^[0-9]+$ && "$port" != 15403 ]] || {
        echo "invalid allocated localhost port: $port" >&2
        exit 2
    }
done
printf 'safety diagnostics: mock=127.0.0.1:%s e-agent=127.0.0.1:%s greptime-http=127.0.0.1:%s greptime-grpc=127.0.0.1:%s greptime-mysql=127.0.0.1:%s greptime-pg=127.0.0.1:%s\n' \
    "$MOCK_PORT" "$EAGENT_PORT" "$GT_HTTP" "$GT_GRPC" "$GT_MYSQL" "$GT_PG"
echo "safety diagnostics: all six ports are distinct dynamic localhost ports; rejected 15403"
echo "safety diagnostics: database=public, workspace=$WORKSPACE, no inherited endpoint variables"

cat >"$XDG_CONFIG_HOME/e-agent/config.toml" <<EOF
 default = "mock/mock"
[providers.mock]
base_url = "http://127.0.0.1:$MOCK_PORT/v1"
api_key_env = "STALL_E2E_KEY"
[models."mock/mock"]
model = "mock-subagent-stall"
[session]
backend = "greptime"
conn = "host=127.0.0.1 port=$GT_PG dbname=public"
EOF

python3 "$REPO_ROOT/tests/e2e/mock_openai_subagent_stall.py" "$MOCK_PORT" "$RUN_MARKER" \
    >"$ROOT/mock.log" 2>&1 & MOCK_PID=$!
MOCK_STARTTIME=$(record_pid "$MOCK_PID") || { echo "failed to record mock PID identity" >&2; exit 1; }
"$GREPTIMEDB_BIN" standalone start --data-home "$ROOT/greptime-data" --log-dir "$ROOT/greptime-log" \
    --http-addr "127.0.0.1:$GT_HTTP" --grpc-bind-addr "127.0.0.1:$GT_GRPC" \
    --mysql-addr "127.0.0.1:$GT_MYSQL" --postgres-addr "127.0.0.1:$GT_PG" \
    >"$ROOT/greptime.log" 2>&1 & GREPTIME_PID=$!
GREPTIME_STARTTIME=$(record_pid "$GREPTIME_PID") || { echo "failed to record GreptimeDB PID identity" >&2; exit 1; }

DB_CONN="host=127.0.0.1 port=$GT_PG dbname=public"
sql() { psql "$DB_CONN" -v ON_ERROR_STOP=1 -Atqc "$1"; }
wait_for() {
    local seconds=$1; shift; local end=$((SECONDS + seconds))
    while (( SECONDS < end )); do
        "$@" && return 0
        sleep 0.2
    done
    return 1
}
greptime_ready() {
    kill -0 "$GREPTIME_PID" 2>/dev/null &&
        curl -fsS --max-time 1 "http://127.0.0.1:$GT_HTTP/health" >/dev/null 2>&1 &&
        psql "$DB_CONN" -Atqc 'SELECT 1' >/dev/null 2>&1
}
wait_for 90 greptime_ready
"$EAGENT_BIN" --serve --host 127.0.0.1 --port "$EAGENT_PORT" --workspace "$WORKSPACE" \
    >"$ROOT/e-agent.log" 2>&1 & EAGENT_PID=$!
EAGENT_STARTTIME=$(record_pid "$EAGENT_PID") || { echo "failed to record e-agent PID identity" >&2; exit 1; }
mock_ready() { kill -0 "$MOCK_PID" 2>/dev/null; }
wait_for 90 mock_ready
wait_for 90 test -s "$XDG_STATE_HOME/e-agent/server.token"
TOKEN=$(cat "$XDG_STATE_HOME/e-agent/server.token")
AUTH=(-H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json')
BASE="http://127.0.0.1:$EAGENT_PORT"
wait_for 90 curl -fsS "${AUTH[@]}" "$BASE/api/sessions" >/dev/null

api_get() { curl -fsS --max-time 10 "${AUTH[@]}" "$BASE$1"; }
api_post() { curl -fsS --max-time 10 "${AUTH[@]}" -X POST "$BASE$1" -d "$2"; }
provider_count() { python3 - "$MOCK_CALLS_FILE" <<'PY'
import json, sys
try:
    with open(sys.argv[1], encoding="utf-8") as stream:
        print(sum(1 for line in stream if line.strip() and json.loads(line)))
except FileNotFoundError:
    print(0)
PY
}
PARENT_ID="stall-parent"
api_post "/api/sessions" '{"id":"stall-parent","initial_prompt":"Delegate a child that starts the requested silent background command."}' \
    >"$ROOT/create.json"

# Discover both task projections and their isolated Greptime rows.  This is
# observation only; the sole mutation after session creation is natural task
# completion.
parent_task_id=""; child_id=""; child_task_id=""
parent_row=""; child_row=""; child_observed_at_ms=""
end=$((SECONDS + 90))
while (( SECONDS < end )); do
    tasks=$(api_get "/api/tasks" 2>/dev/null || true)
    read -r parent_task_id child_id child_task_id < <(TASKS_JSON="$tasks" PARENT_ID="$PARENT_ID" python3 -c '
import json, os
rows = json.loads(os.environ["TASKS_JSON"])
parent = os.environ["PARENT_ID"]
parents = [x for x in rows if x.get("session_id") == parent and x.get("kind") == "delegate"]
parent_task = parents[0] if parents else {}
child = parent_task.get("subagent_session_id") or ""
children = [x for x in rows if x.get("owner_session") == child and x.get("kind") == "bash"]
child_task = children[0] if children else {}
print(parent_task.get("id", ""), child, child_task.get("id", ""))
')
    parent_row=""; child_row=""
    [[ -n "$parent_task_id" ]] && parent_row=$(sql "SELECT count(*) FROM running_tasks WHERE workspace_id='$WORKSPACE' AND session_id='$PARENT_ID' AND task_id=$parent_task_id" 2>/dev/null || true)
    [[ -n "$child_id" && -n "$child_task_id" ]] && child_row=$(sql "SELECT count(*) FROM running_tasks WHERE workspace_id='$WORKSPACE' AND session_id='$child_id' AND task_id=$child_task_id AND subagent_session_id IS NULL" 2>/dev/null || true)
    if [[ -n "$parent_task_id" && -n "$child_id" && -n "$child_task_id" && "$parent_row" == 1 && "$child_row" == 1 ]]; then
        break
    fi
    sleep 0.25
done
if [[ -n "$parent_task_id" && -n "$child_id" && -n "$child_task_id" && "$parent_row" == 1 && "$child_row" == 1 ]]; then
    child_observed_at_ms=$(date +%s%3N)
else
    echo "ASSERTION FAILED: exact parent delegate and child-owned bash rows were not both observed and validated" >&2
    echo "discovered: parent_task_id=${parent_task_id:-<empty>} child_id=${child_id:-<empty>} child_task_id=${child_task_id:-<empty>} parent_row=${parent_row:-<empty>} child_row=${child_row:-<empty>}" >&2
    exit 1
fi
echo "discovered: parent_task=$parent_task_id child_session=$child_id child_task=$child_task_id child_observed_at_ms=$child_observed_at_ms"
echo "assertion: exact child-owned running_tasks row exists in fresh public database"

# Attach before the five-minute deadline.  The initial snapshot is retained
# for the live-subscriber assertion; the later attach verifies the durable
# snapshot projection independently.
SSE_FILE="$ROOT/child-live.sse"
curl -sS -N --max-time 390 "${AUTH[@]}" "$BASE/api/sessions/$child_id/events" >"$SSE_FILE" 2>&1 & SSE_PID=$!
SSE_STARTTIME=$(record_pid "$SSE_PID") || { echo "failed to record SSE PID identity" >&2; exit 1; }

history_file="$ROOT/child-before-warning.json"
api_get "/api/sessions/$child_id/history" >"$history_file"
python3 - "$history_file" "$child_task_id" <<'PY'
import json, sys
history = json.load(open(sys.argv[1], encoding="utf-8"))
needle = sys.argv[2]
notices = [e for e in history.get("entries", []) if e.get("type") == "notice" and needle in e.get("text", "")]
assert not notices, f"stall Notice appeared before the five-minute threshold: {notices!r}"
PY
echo "assertion: no child stall Notice before the production five-minute threshold"
FULL_COMMAND_MARKER="${RUN_MARKER}-full-command-marker-${RUN_MARKER}"
OUTPUT_TAIL_MARKER="${RUN_MARKER}-child-output"
warning_provider_count=$(provider_count)
polling_deadline=$((SECONDS + 400))

stall_count_in_history() {
    python3 - "$1" "$child_task_id" "$FULL_COMMAND_MARKER" "$OUTPUT_TAIL_MARKER" <<'PY'
import json, sys
h = json.load(open(sys.argv[1], encoding="utf-8"))
n, command_marker, output_marker = sys.argv[2:]
notices = [e.get("text", "") for e in h.get("entries", []) if e.get("type") == "notice"
           and "no output for five minutes" in e.get("text", "") and n in e.get("text", "")]
for text in notices:
    assert "runtime " in text and "silence duration " in text, text
    assert command_marker in text and output_marker in text, text
print(len(notices))
PY
}

# From here until the durable Notice appears, use only observation APIs/SQL.
warning_history=""
while (( SECONDS < polling_deadline )); do
    warning_history="$ROOT/child-warning.json"
    api_get "/api/sessions/$child_id/history" >"$warning_history" 2>/dev/null || true
    [[ -s "$warning_history" ]] && [[ "$(stall_count_in_history "$warning_history" 2>/dev/null || echo 0)" == 1 ]] && break
    sleep 1
done
[[ -s "$warning_history" ]] && [[ "$(stall_count_in_history "$warning_history")" == 1 ]] || {
    echo "ASSERTION FAILED: durable child stall Notice did not appear after the production threshold" >&2
    exit 1
}
warning_wall_ms=$(date +%s%3N)
elapsed_from_child_observed_ms=$((warning_wall_ms - child_observed_at_ms))
(( elapsed_from_child_observed_ms >= 295000 )) || { echo "ASSERTION FAILED: warning arrived too early (${elapsed_from_child_observed_ms}ms from child observation)" >&2; exit 1; }
parent_row=$(sql "SELECT count(*) FROM running_tasks WHERE workspace_id='$WORKSPACE' AND session_id='$PARENT_ID' AND task_id=$parent_task_id")
child_row=$(sql "SELECT count(*) FROM running_tasks WHERE workspace_id='$WORKSPACE' AND session_id='$child_id' AND task_id=$child_task_id AND subagent_session_id IS NULL")
[[ "$child_row" == 1 && "$parent_row" == 1 ]] || {
    echo "ASSERTION FAILED: owner rows changed at warning: parent=$parent_row child=$child_row" >&2
    exit 1
}
[[ "$(provider_count)" == "$warning_provider_count" ]] || {
    echo "ASSERTION FAILED: provider request count changed because of the Notice" >&2
    exit 1
}
warning_history_count=$(stall_count_in_history "$warning_history")
echo "warning: elapsed_from_child_observed_ms=${elapsed_from_child_observed_ms} provider_calls=$warning_provider_count child_notice_count=$warning_history_count"
echo "assertion: child owner row and parent delegate row remain; Notice caused no provider request"

# The already-attached stream must receive exactly one named Notice.  A fresh
# attach's snapshot must independently contain exactly one durable projection.
stall_count_in_live_sse() {
    python3 - "$1" "$child_task_id" "$FULL_COMMAND_MARKER" "$OUTPUT_TAIL_MARKER" <<'PY'
import json, sys
text = open(sys.argv[1], encoding="utf-8", errors="replace").read()
needle, command_marker, output_marker = sys.argv[2:]
count = 0
for block in text.split("\n\n"):
    lines = block.splitlines()
    if "event: Notice" not in lines:
        continue
    for line in lines:
        if line.startswith("data: "):
            try:
                value = json.loads(line[6:])
            except json.JSONDecodeError:
                continue
            notice = value.get("text", "")
            if needle in notice and "no output for five minutes" in notice:
                assert "runtime " in notice and "silence duration " in notice
                assert command_marker in notice and output_marker in notice
                count += 1
print(count)
PY
}

live_count=0
live_deadline=$((SECONDS + 5))
while (( live_count == 0 && SECONDS < live_deadline )); do
    live_count=$(stall_count_in_live_sse "$SSE_FILE" 2>/dev/null || echo 0)
    (( live_count > 1 )) && {
        echo "ASSERTION FAILED: live SSE Notice count=$live_count, expected 1" >&2
        exit 1
    }
    (( live_count == 1 )) && break
    sleep 0.2
done
[[ "$live_count" == 1 ]] || {
    echo "ASSERTION FAILED: live SSE Notice count=$live_count after bounded wait, expected 1" >&2
    exit 1
}
late_sse="$ROOT/child-late.sse"
set +e
curl -sS -N --max-time 3 "${AUTH[@]}" "$BASE/api/sessions/$child_id/events" >"$late_sse" 2>&1
set -e
python3 - "$late_sse" "$child_task_id" "$FULL_COMMAND_MARKER" "$OUTPUT_TAIL_MARKER" <<'PY'
import json, sys
text = open(sys.argv[1], encoding="utf-8", errors="replace").read()
needle, command_marker, output_marker = sys.argv[2:]
count = 0
for block in text.split("\n\n"):
    lines = block.splitlines()
    if "event: snapshot" not in lines:
        continue
    for line in lines:
        if line.startswith("data: "):
            try:
                value = json.loads(line[6:])
            except json.JSONDecodeError:
                continue
            if isinstance(value, list):
                for event in value:
                    notice = event.get("data", "")
                    if (event.get("type") == "notice" and needle in notice
                            and "no output for five minutes" in notice):
                        assert "runtime " in notice and "silence duration " in notice
                        assert command_marker in notice and output_marker in notice
                        count += 1
assert count == 1, f"late-attach durable snapshot Notice count={count}, expected 1"
print(count)
PY
echo "assertion: one live Notice and one independent durable late-attach snapshot Notice"

# Wait for natural sleep(330) completion.  Until the completion entry exists,
# a Finished child is a failure.  These are all observation APIs/SQL polls.
completion_file="$ROOT/child-completion.json"
completion_seen=0
while (( SECONDS < polling_deadline )); do
    api_get "/api/sessions/$child_id/history" >"$completion_file" 2>/dev/null || true
    if [[ -s "$completion_file" ]] && python3 - "$completion_file" <<'PY'
import json, sys
h = json.load(open(sys.argv[1], encoding="utf-8"))
raise SystemExit(0 if any(e.get("type") == "background_completion" for e in h.get("entries", [])) else 1)
PY
    then
        completion_seen=1
        break
    fi
    child_status=$(SESSIONS_JSON="$(api_get "/api/sessions" 2>/dev/null || true)" CHILD_ID="$child_id" python3 -c '
import json, os
try:
    rows = json.loads(os.environ["SESSIONS_JSON"])
except Exception:
    print("unknown")
else:
    print(next((x.get("status", "unknown") for x in rows if x.get("id") == os.environ["CHILD_ID"]), "unknown"))
')
    [[ "$child_status" != Finished ]] || {
        echo "ASSERTION FAILED: child finished before its background completion" >&2
        exit 1
    }
    sleep 1
done
[[ "$completion_seen" == 1 ]] || { echo "ASSERTION FAILED: natural background completion did not appear" >&2; exit 1; }

python3 - "$completion_file" "$child_task_id" "$FULL_COMMAND_MARKER" "$OUTPUT_TAIL_MARKER" <<'PY'
import json, sys
h = json.load(open(sys.argv[1], encoding="utf-8"))
n, command_marker, output_marker = sys.argv[2:]
entries = h.get("entries", [])
notice = [i for i, e in enumerate(entries) if e.get("type") == "notice" and n in e.get("text", "") and "no output for five minutes" in e.get("text", "")]
for index in notice:
    text = entries[index]["text"]
    assert "runtime " in text and "silence duration " in text
    assert command_marker in text and output_marker in text
completion = [i for i, e in enumerate(entries) if e.get("type") == "background_completion"]
assert len(notice) == 1, f"duplicate stall Notice(s): {notice}"
assert completion, "missing background completion"
assert notice[0] < completion[0], f"durable ordering is Notice={notice[0]} completion={completion[0]}"
print(f"notice_index={notice[0]} completion_index={completion[0]}")
PY
child_row=$(sql "SELECT count(*) FROM running_tasks WHERE workspace_id='$WORKSPACE' AND session_id='$child_id' AND task_id=$child_task_id AND subagent_session_id IS NULL")
parent_row=$(sql "SELECT count(*) FROM running_tasks WHERE workspace_id='$WORKSPACE' AND session_id='$PARENT_ID' AND task_id=$parent_task_id")
[[ "$child_row" == 0 && "$parent_row" == 1 ]] || {
    echo "ASSERTION FAILED: completion boundary rows child=$child_row parent_delegate=$parent_row" >&2
    exit 1
}
echo "assertion: child completion follows the warning, clears only the child owner row, and parent remains owned"

# The child owner clear above is the feature-specific durable lifecycle proof;
# generic session convergence and provider-count assertions are covered by
# the shared lifecycle suite.
stop_exact "$SSE_PID" SSE "$SSE_STARTTIME" || CLEANUP_OK=0
SSE_PID=""
SSE_STARTTIME=""

echo "PASS: subagent-owned five-minute stall warning (runtime=$((SECONDS))s; warning_elapsed_from_child_observed_ms=${elapsed_from_child_observed_ms}; event_order=Notice<BackgroundCompletionNotice; live_sse_notice=1; late_attach_notice=1; child_owner_cleared=1)"
