#!/usr/bin/env bash
# Full isolated GreptimeDB web product acceptance.  It owns every process,
# directory, port, config and credential used by the test.
set -Eeuo pipefail
umask 077
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)
ART="$ROOT/.e-agent/greptime-web-acceptance"
GREPTIME="$ART/bin/greptime"
NEW="$ART/bin/e-agent-new"
OLD="$ROOT/.e-agent/web-product-acceptance/bin/e-agent-old-bca5941"
CHROME=/home/discord9/.cache/ms-playwright/chromium-1234/chrome-linux64/chrome
[[ -x "$GREPTIME" && -x "$NEW" && -x "$OLD" && -x "$CHROME" ]] || { echo 'required supplied binary missing or not executable' >&2; exit 2; }
RUN=$(mktemp -d "$ART/actual-greptime-web-run-XXXXXXXX")
DATA="$RUN/greptime-data"; LOGDIR="$RUN/greptime-log"; HOME_ISO="$RUN/greptime-home"
mkdir -p "$DATA" "$LOGDIR" "$HOME_ISO"
GPID=''
stop() {
  local status=$?
  trap - EXIT
  if [[ -n "$GPID" ]] && kill -0 "$GPID" 2>/dev/null; then
    kill -TERM "$GPID" 2>/dev/null || true
    for _ in {1..100}; do kill -0 "$GPID" 2>/dev/null || break; sleep .1; done
    kill -0 "$GPID" 2>/dev/null && kill -KILL "$GPID" 2>/dev/null || true
    wait "$GPID" 2>/dev/null || true
  fi
  exit "$status"
}
trap stop EXIT
read -r GHTTP GGRPC GMYSQL GPG < <(python3 - <<'PY'
import socket
ports=[]
for _ in range(4):
 s=socket.socket(); s.bind(('127.0.0.1',0)); ports.append(s.getsockname()[1]); s.close()
if len(set(ports)) != 4 or 15403 in ports: raise SystemExit('invalid port allocation')
print(*ports)
PY
)
for p in "$GHTTP" "$GGRPC" "$GMYSQL" "$GPG"; do [[ "$p" != 15403 ]] || exit 2; done
ARGV=("$GREPTIME" standalone start --data-home "$DATA" --http-addr "127.0.0.1:$GHTTP" --grpc-bind-addr "127.0.0.1:$GGRPC" --mysql-addr "127.0.0.1:$GMYSQL" --postgres-addr "127.0.0.1:$GPG" --log-dir "$LOGDIR")
printf '%q ' "${ARGV[@]}" >"$RUN/greptime.argv"; printf '\n' >>"$RUN/greptime.argv"
HOME="$HOME_ISO" XDG_CONFIG_HOME="$HOME_ISO/config" XDG_STATE_HOME="$HOME_ISO/state" "${ARGV[@]}" >"$RUN/greptime.stdout.log" 2>"$RUN/greptime.stderr.log" & GPID=$!
CONN="host=127.0.0.1 port=$GPG user=postgres dbname=public"
for _ in {1..180}; do
  kill -0 "$GPID" 2>/dev/null || { cat "$RUN/greptime.stderr.log" >&2; exit 1; }
  if curl -fsS --max-time 1 "http://127.0.0.1:$GHTTP/health" >"$RUN/greptime-health.json" 2>/dev/null && psql "$CONN" -Atqc 'SELECT 1' >"$RUN/greptime-sql-probe.txt" 2>/dev/null; then break; fi
  sleep .2
done
grep -qx 1 "$RUN/greptime-sql-probe.txt"
printf '%s\n' "$CONN" >"$RUN/isolated-greptime-connection.txt"
sha256sum "$GREPTIME" "$NEW" "$OLD" "$CHROME" >"$RUN/binary-sha256.txt"
printf 'run=%s\ngreptime_http=%s\ngreptime_grpc=%s\ngreptime_mysql=%s\ngreptime_pg=%s\nforbidden_production_port=15403\n' "$RUN" "$GHTTP" "$GGRPC" "$GMYSQL" "$GPG" >"$RUN/isolation-proof.txt"
export EAGENT_PRODUCT_BINARY="$NEW"
export GREPTIME_E2E_CONN="$CONN"
export EAGENT_CHROME="$CHROME"
# Basic is deliberately first: broad cases never run when the live held-stream
# reattach/tool-card product path is not accepted.
uv run --with playwright python3 tests/e2e/web_attach_greptime_product.py --case basic --conn "$CONN" >"$RUN/basic.log" 2>&1
uv run --with playwright python3 tests/e2e/web_attach_greptime_product.py --case gap --conn "$CONN" >"$RUN/gap.log" 2>&1
uv run --with playwright python3 tests/e2e/web_attach_greptime_product.py --case disjoint --conn "$CONN" >"$RUN/disjoint.log" 2>&1
uv run --with playwright python3 tests/e2e/web_attach_greptime_compat.py >"$RUN/compat.log" 2>&1
# This independent physical-key test creates a second owned localhost server;
# TMPDIR keeps all temporary paths under the task artifact root.
TMPDIR="$RUN" GREPTIMEDB_BIN="$GREPTIME" EAGENT_BIN="$NEW" tests/e2e/greptime_history_physical_paging.sh >"$RUN/physical-paging.log" 2>&1
printf 'passed\n' >"$RUN/RESULT"
printf '%s\n' "PASS: $RUN"
