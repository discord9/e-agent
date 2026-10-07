#!/data/data/com.termux/files/usr/bin/bash
set -eu
BIN='@PREFIX_BIN@/e-agent'
[ -x "$BIN" ] || { echo "e-agent not installed at $BIN; run the Termux installer first" >&2; exit 1; }
: "${HOME:?HOME is unset}"
WORKSPACE=${E_AGENT_WORKSPACE:-"$HOME/e-agent-workspace"}
URL=http://127.0.0.1:8766
state=${XDG_STATE_HOME:-"$HOME/.local/state"}/e-agent/server.token
owned() {
  [ -r "$state" ] || return 1
  token=$(cat "$state") || return 1
  [ -n "$token" ] || return 1
  curl -q --noproxy '*' -fsS --max-time 2 "$URL/" -o "$probe_root" 2>/dev/null || return 1
  grep -F '<title>e-agent · Web UI</title>' "$probe_root" >/dev/null || return 1
  printf 'header = "Authorization: Bearer %s"\n' "$token" | curl -q --noproxy '*' --config - -fsS --max-time 2 "$URL/api/models" -o "$probe_models" 2>/dev/null || return 1
  case "$(cat "$probe_models")" in \[*\]) return 0;; *) return 1;; esac
}
open_ui() {
  if command -v termux-open-url >/dev/null 2>&1; then termux-open-url "$URL" || echo "Open $URL manually" >&2
  elif command -v termux-open >/dev/null 2>&1; then termux-open "$URL" || echo "Open $URL manually" >&2
  else echo "Open $URL in your browser"; fi
}
tmp=$(mktemp -d "${TMPDIR:-$HOME}/e-agent-web.XXXXXX")
pid=
# shellcheck disable=SC2329
cleanup() {
  status=$?
  if [ -n "$pid" ]; then
    kill -0 "$pid" 2>/dev/null && kill -INT "$pid" 2>/dev/null || true
    for _ in 1 2 3; do kill -0 "$pid" 2>/dev/null || break; sleep 1; done
    kill -0 "$pid" 2>/dev/null && kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || status=$?
    pid=
  fi
  rm -rf "$tmp"
  exit "$status"
}
trap 'cleanup' EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
probe_root=$tmp/root
probe_models=$tmp/models
if owned; then open_ui; exit 0; fi
if curl -q --noproxy '*' -sS --max-time 2 "$URL/" -o "$tmp/occupied" 2>/dev/null; then echo "Port 8766 is occupied by a service that is not an authenticated e-agent; refusing to open it." >&2; exit 1; fi
if [ ! -d "$WORKSPACE" ]; then mkdir -p "$WORKSPACE"; chmod 700 "$WORKSPACE"; fi
"$BIN" web --host 127.0.0.1 --port 8766 --workspace "$WORKSPACE" &
pid=$!
ready=0
deadline=$(( $(date +%s) + 30 ))
while [ "$(date +%s)" -lt "$deadline" ]; do
  if ! kill -0 "$pid" 2>/dev/null; then echo 'e-agent web exited before becoming ready; check its configuration and startup output.' >&2; exit 1; fi
  if owned; then
    if ! kill -0 "$pid" 2>/dev/null; then echo 'e-agent web exited during readiness check.' >&2; exit 1; fi
    ready=1; break
  fi
  sleep 1
done
[ "$ready" -eq 1 ] || { echo 'e-agent did not become ready within approximately 30 seconds on 127.0.0.1:8766.' >&2; exit 1; }
echo "e-agent web is ready at $URL (workspace: $WORKSPACE); leave this running in Termux, or press Ctrl-C to stop it."
(open_ui) &
status=0
wait "$pid" || status=$?
pid=
exit "$status"
