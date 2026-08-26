#!/usr/bin/env bash
# End-to-end GreptimeDB physical-row paging test.  This test deliberately
# owns both servers; it never attaches to an existing GreptimeDB/e-agent.
set -Eeuo pipefail
umask 077

if [[ -v GREPTIME_PG || -v GREPTIME_CONN || -v EAGENT_BASE ]]; then
    echo "refusing inherited GREPTIME_PG, GREPTIME_CONN, or EAGENT_BASE" >&2
    exit 2
fi
: "${GREPTIMEDB_BIN:?GREPTIMEDB_BIN must be set to an absolute GreptimeDB executable}"
if [[ "$GREPTIMEDB_BIN" != /* || ! -f "$GREPTIMEDB_BIN" || ! -x "$GREPTIMEDB_BIN" ]]; then
    echo "GREPTIMEDB_BIN must be an absolute regular executable: $GREPTIMEDB_BIN" >&2
    exit 2
fi

ROOT=$(mktemp -d "${TMPDIR:-/tmp}/e-agent-greptime-history.XXXXXX")
EAGENT_PID=""
GREPTIME_PID=""
CLEANUP_OK=1

stop_process() {
    local pid=$1 name=$2
    [[ -n "$pid" ]] || return 0
    if ! kill -0 "$pid" 2>/dev/null; then
        wait "$pid" 2>/dev/null || true
        return 0
    fi
    kill -TERM "$pid" 2>/dev/null || true
    for _ in {1..100}; do
        if ! kill -0 "$pid" 2>/dev/null; then
            wait "$pid" 2>/dev/null || true
            return 0
        fi
        sleep 0.1
    done
    # Only escalate after confirming that this exact PID is still alive.
    if kill -0 "$pid" 2>/dev/null; then
        echo "$name PID $pid did not exit after TERM; sending KILL" >&2
        kill -KILL "$pid" 2>/dev/null || true
    fi
    wait "$pid" 2>/dev/null || true
    if kill -0 "$pid" 2>/dev/null; then
        echo "could not stop $name PID $pid" >&2
        return 1
    fi
}

cleanup() {
    local status=$?
    trap - EXIT
    set +e
    stop_process "$EAGENT_PID" e-agent || CLEANUP_OK=0
    stop_process "$GREPTIME_PID" GreptimeDB || CLEANUP_OK=0
    # Preserve failed runs (including server logs and generated payloads) for
    # diagnosis. Successful runs remove the complete isolated root only after
    # both child processes have exited.
    if (( CLEANUP_OK && status == 0 )); then
        rm -rf "$ROOT" || CLEANUP_OK=0
    fi
    if (( ! CLEANUP_OK )); then
        echo "cleanup failed; preserving temporary root: $ROOT" >&2
        exit 1
    fi
    if (( status != 0 )); then
        echo "test failed; preserving temporary root: $ROOT" >&2
    fi
    exit "$status"
}
trap cleanup EXIT

mkdir -p "$ROOT"/{workspace,greptime-data,greptime-log,config/e-agent,state/e-agent,home}
WORKSPACE=$(cd "$ROOT/workspace" && pwd -P)
export HOME="$ROOT/home"
export XDG_CONFIG_HOME="$ROOT/config"
export XDG_STATE_HOME="$ROOT/state"
export E2E_DUMMY_KEY=dummy-key

# A short-lived bind(0)-then-close allocator.  It avoids hard-coded ports and
# fails closed if any selected address is not available when a server starts.
# The close/start gap is inherently a short TOCTOU window; readiness checks
# below turn a bind collision into a test failure rather than retrying/defaulting.
read -r GREPTIME_HTTP_PORT GREPTIME_GRPC_PORT GREPTIME_MYSQL_PORT GREPTIME_PG_PORT EAGENT_PORT < <(
    python3 - <<'PY'
import socket
ports = []
for _ in range(5):
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.bind(("127.0.0.1", 0))
    ports.append(sock.getsockname()[1])
    sock.close()
if len(set(ports)) != 5 or any(port == 15403 for port in ports):
    raise SystemExit("port allocator returned invalid ports")
print(*ports)
PY
)
for port in "$GREPTIME_HTTP_PORT" "$GREPTIME_GRPC_PORT" "$GREPTIME_MYSQL_PORT" "$GREPTIME_PG_PORT" "$EAGENT_PORT"; do
    [[ "$port" =~ ^[0-9]+$ && "$port" != 15403 ]] || { echo "invalid allocated port: $port" >&2; exit 1; }
done

"$GREPTIMEDB_BIN" standalone start \
    --data-home "$ROOT/greptime-data" \
    --http-addr "127.0.0.1:$GREPTIME_HTTP_PORT" \
    --grpc-bind-addr "127.0.0.1:$GREPTIME_GRPC_PORT" \
    --mysql-addr "127.0.0.1:$GREPTIME_MYSQL_PORT" \
    --postgres-addr "127.0.0.1:$GREPTIME_PG_PORT" \
    --log-dir "$ROOT/greptime-log" >"$ROOT/greptime.log" 2>&1 &
GREPTIME_PID=$!

GREPTIME_CONN="host=127.0.0.1 port=$GREPTIME_PG_PORT user=postgres dbname=public"
for _ in {1..180}; do
    kill -0 "$GREPTIME_PID" 2>/dev/null || { cat "$ROOT/greptime.log" >&2; exit 1; }
    if curl -fsS --max-time 1 "http://127.0.0.1:$GREPTIME_HTTP_PORT/health" >/dev/null 2>&1 \
        && psql "$GREPTIME_CONN" -v ON_ERROR_STOP=1 -Atqc 'SELECT 1' >/dev/null 2>&1; then
        break
    fi
    sleep 0.2
done
kill -0 "$GREPTIME_PID" 2>/dev/null || { cat "$ROOT/greptime.log" >&2; exit 1; }
curl -fsS --max-time 2 "http://127.0.0.1:$GREPTIME_HTTP_PORT/health" >/dev/null
psql "$GREPTIME_CONN" -v ON_ERROR_STOP=1 -Atqc 'SELECT 1' | grep -qx 1

cat >"$XDG_CONFIG_HOME/e-agent/config.toml" <<EOF
# Deliberately unreachable dummy provider: this test creates no prompt/turn.
default = "dummy/model"

[providers.dummy]
base_url = "http://127.0.0.1:9/v1"
api_key_env = "E2E_DUMMY_KEY"

[models."dummy/model"]
model = "dummy"

[session]
backend = "greptime"
conn = "$GREPTIME_CONN"
EOF

if [[ -n "${EAGENT_BIN:-}" ]]; then
    [[ "$EAGENT_BIN" = /* && -f "$EAGENT_BIN" && -x "$EAGENT_BIN" ]] || {
        echo "EAGENT_BIN must be an absolute regular executable" >&2; exit 2;
    }
else
    : "${CARGO_TARGET_DIR:=$ROOT/cargo-target}"
    export CARGO_TARGET_DIR
    cargo build --quiet --bin e-agent
    EAGENT_BIN="$CARGO_TARGET_DIR/debug/e-agent"
fi

"$EAGENT_BIN" --serve --host 127.0.0.1 --port "$EAGENT_PORT" \
    --profile dummy/model --workspace "$WORKSPACE" >"$ROOT/e-agent.log" 2>&1 &
EAGENT_PID=$!
TOKEN_FILE="$XDG_STATE_HOME/e-agent/server.token"
EAGENT_BASE="http://127.0.0.1:$EAGENT_PORT"
for _ in {1..150}; do
    kill -0 "$EAGENT_PID" 2>/dev/null || { cat "$ROOT/e-agent.log" >&2; exit 1; }
    if [[ -s "$TOKEN_FILE" ]] && curl -fsS --max-time 1 \
        -H "Authorization: Bearer $(cat "$TOKEN_FILE")" "$EAGENT_BASE/api/sessions" >/dev/null 2>&1; then
        break
    fi
    sleep 0.2
done
kill -0 "$EAGENT_PID" 2>/dev/null || { cat "$ROOT/e-agent.log" >&2; exit 1; }
[[ -s "$TOKEN_FILE" ]]
TOKEN=$(cat "$TOKEN_FILE")
[[ -n "$TOKEN" ]]

CREATE_BODY="$ROOT/create.json"
CREATE_STATUS=$(curl -sS --max-time 5 -o "$CREATE_BODY" -w '%{http_code}' \
    -X POST -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
    --data '{}' "$EAGENT_BASE/api/sessions")
[[ "$CREATE_STATUS" == 201 ]]
SESSION_ID=$(python3 - "$CREATE_BODY" <<'PY'
import json, sys
value = json.load(open(sys.argv[1], encoding="utf-8"))
sid = value.get("id")
if not isinstance(sid, str) or not sid:
    raise SystemExit("POST /api/sessions returned no non-empty id")
print(sid)
PY
)
HISTORICAL_ID=historical-physical-paging
# Fixture writes are the only SQL in this test.  The payloads are the actual
# externally-tagged SessionEntry serde representation; all behavior checks
# below use only the public HTTP history API.
psql "$GREPTIME_CONN" -v ON_ERROR_STOP=1 -v workspace="$WORKSPACE" -v session="$HISTORICAL_ID" <<'SQL'
INSERT INTO session_entries
  (workspace_id, session_id, seq, event_time, entry_kind, payload, schema_version, is_error)
VALUES
  (:'workspace', :'session', 0, '2024-01-01 00:00:00'::timestamp, 'message',
   $$ {"type":"message","message":{"User":{"content":"seq-0-old","images":[]}}} $$, 1, false),
  (:'workspace', :'session', 1, '2024-01-01 00:00:01'::timestamp, 'message',
   $$ {"type":"message","message":{"User":{"content":"seq-1-old","images":[]}}} $$, 1, false),
  (:'workspace', :'session', 2, '2024-01-01 00:00:02'::timestamp, 'compaction',
   $$ {"type":"compaction","summary":"compaction-seq-2","retained":[],"current_prompt_at":null,"no_current_prompt":false} $$, 1, false),
  (:'workspace', :'session', 3, '2024-01-01 00:00:03'::timestamp, 'message',
   $$ {"type":"message","message":{"User":{"content":"seq-3-middle","images":[]}}} $$, 1, false),
  (:'workspace', :'session', 4, '2024-01-01 00:00:04'::timestamp, 'message',
   $$ {"type":"message","message":{"User":{"content":"seq-4-middle","images":[]}}} $$, 1, false),
  (:'workspace', :'session', 5, '2024-01-01 00:00:05'::timestamp, 'compaction',
   $$ {"type":"compaction","summary":"compaction-seq-5","retained":[],"current_prompt_at":null,"no_current_prompt":false} $$, 1, false),
  (:'workspace', :'session', 6, '2024-01-01 00:00:06'::timestamp, 'message',
   $$ {"type":"message","message":{"User":{"content":"seq-6-head","images":[]}}} $$, 1, false),
  (:'workspace', :'session', 7, '2024-01-01 00:00:07'::timestamp, 'message',
   $$ {"type":"message","message":{"User":{"content":"seq-7-head","images":[]}}} $$, 1, false),
  -- Two physical rows at seq 8: the later event_time is the winner.  Keep
  -- these a full second apart because this engine's wire timestamp decode
  -- may not preserve sub-millisecond fixture literals.
  (:'workspace', :'session', 8, '2024-01-01 00:00:08'::timestamp, 'message',
   $$ {"type":"message","message":{"User":{"content":"seq-8-old-retry","images":[]}}} $$, 1, false),
  (:'workspace', :'session', 8, '2024-01-01 00:00:09'::timestamp, 'message',
   $$ {"type":"message","message":{"User":{"content":"seq-8-latest","images":[]}}} $$, 1, false);
SQL

# Deliberately restart the independent e-agent after fixture creation.  The
# persisted session is now a historical registry miss, not the live session
# created during setup; the restarted process still uses the same isolated DB.
stop_process "$EAGENT_PID" e-agent
EAGENT_PID=""
"$EAGENT_BIN" --serve --host 127.0.0.1 --port "$EAGENT_PORT" \
    --profile dummy/model --workspace "$WORKSPACE" >"$ROOT/e-agent-restarted.log" 2>&1 &
EAGENT_PID=$!
for _ in {1..150}; do
    kill -0 "$EAGENT_PID" 2>/dev/null || { cat "$ROOT/e-agent-restarted.log" >&2; exit 1; }
    if [[ -s "$TOKEN_FILE" ]] && curl -fsS --max-time 1 \
        -H "Authorization: Bearer $(cat "$TOKEN_FILE")" "$EAGENT_BASE/api/sessions" >/dev/null 2>&1; then
        break
    fi
    sleep 0.2
done
kill -0 "$EAGENT_PID" 2>/dev/null || { cat "$ROOT/e-agent-restarted.log" >&2; exit 1; }

# HTTP-only functional assertions.  Entries do not carry seq on the wire, so
# the fixture's content labels provide the seq oracle while cursors are checked
# from next_before_seq.  The first physical page intentionally returns one
# logical entry from two rows (best-effort page-local dedup).
python3 - "$EAGENT_BASE" "$TOKEN" "$HISTORICAL_ID" "$EAGENT_PORT" "$GREPTIME_HTTP_PORT" "$GREPTIME_GRPC_PORT" "$GREPTIME_MYSQL_PORT" "$GREPTIME_PG_PORT" <<'PY'
import json
import sys
import urllib.error
import urllib.parse
import urllib.request

base, token, historical = sys.argv[1:4]
ports = list(map(int, sys.argv[4:]))
assertions = 0

def check(condition, message):
    global assertions
    if not condition:
        raise AssertionError(message)
    assertions += 1

def request(method, path, expected):
    req = urllib.request.Request(
        base + path,
        method=method,
        headers={"Authorization": "Bearer " + token},
    )
    try:
        with urllib.request.urlopen(req, timeout=5) as response:
            status = response.status
            body = response.read()
    except urllib.error.HTTPError as exc:
        status = exc.code
        body = exc.read()
    if status != expected:
        detail = body.decode("utf-8", "replace")
        raise AssertionError(f"{method} {path}: HTTP {status}, expected {expected}; body={detail}")
    if expected != 200:
        return None
    return json.loads(body)

# Authorization and the API status are also asserted against this isolated server.
req = urllib.request.Request(base + "/api/sessions")
try:
    urllib.request.urlopen(req, timeout=5)
    raise AssertionError("unauthorized /api/sessions unexpectedly succeeded")
except urllib.error.HTTPError as exc:
    check(exc.code == 401, "unauthorized /api/sessions must be 401")

sessions = request("GET", "/api/sessions", 200)
check(not any(item.get("id") == historical for item in sessions),
      "fixture must be a historical registry miss after restart")

# Historical resolution must not probe with an unbounded head load.  The
# restarted server has no registry entry for this fixture, so this is a real
# historical registry-miss path.  Unknown and known empty sessions are
# distinguished by the bounded page result.
unknown = "unknown-history-session"
check(request("GET", f"/api/sessions/{unknown}/history?limit=2", 404) is None,
      "unknown valid id initial history must be 404")
check(request("GET", f"/api/sessions/{unknown}/history?before_seq=0&limit=2", 404) is None,
      "unknown valid id before_seq=0 history must be 404")
known_empty = request("GET", f"/api/sessions/{historical}/history?before_seq=0&limit=2", 200)
check(known_empty == {"entries": [], "next_before_seq": None},
      "known historical session before_seq=0 must be an empty terminal page")
# The route returns entries in ascending order inside each physical page.
def page(before=None):
    query = "?limit=2"
    if before is not None:
        query += "&before_seq=" + urllib.parse.quote(str(before))
    value = request("GET", f"/api/sessions/{historical}/history{query}", 200)
    check(isinstance(value.get("entries"), list), "history entries must be an array")
    return value

def logical_names(value):
    result = []
    for entry in value["entries"]:
        if entry["type"] == "compaction":
            result.append(entry["summary"])
        else:
            result.append(entry["message"]["User"]["content"])
    return result

known_initial = request("GET", f"/api/sessions/{historical}/history?limit=2", 200)
check(logical_names(known_initial) == ["seq-8-latest"],
      "known historical initial page must expose the physical head")
check(request("GET", f"/api/sessions/{historical}/history?limit=0", 400) is None,
      "explicit limit=0 must be 400")
check(request("GET", f"/api/sessions/{historical}/history?limit=9223372036854775808", 400) is None,
      "limit above i64::MAX must be 400")

expected = [
    (None, ["seq-8-latest"], 8),
    (8, ["seq-6-head", "seq-7-head"], 6),
    # A cursor at 6 enters the one-row [compaction-5, 6) segment.  The
    # following cursor then enters [compaction-2, 5), so segment boundaries
    # are traversed explicitly rather than filling a page across them.
    (6, ["compaction-seq-5"], 5),
    (5, ["seq-3-middle", "seq-4-middle"], 3),
    (3, ["compaction-seq-2"], 2),
    (2, ["seq-0-old", "seq-1-old"], None),
]
seen = []
for before, names, next_cursor in expected:
    value = page(before)
    actual = logical_names(value)
    check(actual == names, f"history page before={before}: {actual!r} != {names!r}")
    check(value.get("next_before_seq") == next_cursor,
          f"history cursor before={before}: {value.get('next_before_seq')} != {next_cursor}")
    seen.extend(actual)

check(len(expected[0][1]) == 1, "duplicate physical rows must be allowed to shorten a page")
check("seq-8-old-retry" not in seen, "superseded same-seq retry was returned")
check(len(seen) == len(set(seen)), "logical entries repeated across HTTP pages")
check(seen.count("compaction-seq-2") == 1 and seen.count("compaction-seq-5") == 1,
      "both compaction segment boundaries must be reachable")
check(ports and all(port != 15403 for port in ports), "test used the forbidden default port")
check(assertions > 0, "must execute at least one assertion")
print(f"ASSERTIONS={assertions}")
PY

echo "PASS: Greptime HTTP history physical paging (Greptime HTTP=$GREPTIME_HTTP_PORT gRPC=$GREPTIME_GRPC_PORT MySQL=$GREPTIME_MYSQL_PORT PG=$GREPTIME_PG_PORT; e-agent HTTP=$EAGENT_PORT)"
