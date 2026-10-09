#!/data/data/com.termux/files/usr/bin/bash
set -eu
BIN='@PREFIX_BIN@/e-agent'
[ -x "$BIN" ] || { echo "e-agent not installed at $BIN; run the Termux installer first" >&2; exit 1; }
: "${HOME:?HOME is unset}"
export PREFIX="${BIN%/bin/e-agent}"
export SVDIR="$PREFIX/var/service" LOGDIR="$PREFIX/var/log"
service="$SVDIR/e-agent-web"
URL=http://127.0.0.1:8766
state=${XDG_STATE_HOME:-"$HOME/.local/state"}/e-agent/server.token
owned() {
  [ -r "$state" ] || return 1
  token=$(cat "$state") || return 1
  [ -n "$token" ] || return 1
  curl -q --noproxy '*' -fsS --max-time 2 "$URL/" -o "$tmp/root" 2>/dev/null || return 1
  grep -F '<title>e-agent · Web UI</title>' "$tmp/root" >/dev/null || return 1
  printf 'header = "Authorization: Bearer %s"\n' "$token" | curl -q --noproxy '*' --config - -fsS --max-time 2 "$URL/api/models" -o "$tmp/models" 2>/dev/null || return 1
  case "$(cat "$tmp/models")" in \[*\]) return 0;; *) return 1;; esac
}
open_ui() {
  if command -v termux-open-url >/dev/null 2>&1; then termux-open-url "$URL" || echo "Open $URL manually" >&2
  elif command -v termux-open >/dev/null 2>&1; then termux-open "$URL" || echo "Open $URL manually" >&2
  else echo "Open $URL in your browser"; fi
}
tmp=$(mktemp -d "${TMPDIR:-$HOME}/e-agent-web.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
if owned; then open_ui; exit 0; fi
if curl -q --noproxy '*' -sS --max-time 2 "$URL/" -o /dev/null 2>/dev/null; then
  echo 'Port 8766 is occupied by a service that is not an authenticated e-agent; refusing to open it.' >&2; exit 1
fi
if [ ! -x "$service/run" ] || ! command -v sv >/dev/null; then
  echo 'Re-run the Termux installer to configure the e-agent-web service.' >&2; exit 1
fi
service-daemon start >/dev/null 2>&1 || true
for _ in 1 2 3 4 5; do [ -p "$service/supervise/ok" ] && break; sleep 1; done
rm -f "$service/down"
sv -w 5 up "$service"
deadline=$(( $(date +%s) + 30 ))
while [ "$(date +%s)" -lt "$deadline" ]; do
  if owned; then open_ui; exit 0; fi
  sleep 1
done
echo "e-agent did not become ready within approximately 30 seconds. Check sv status e-agent-web and $LOGDIR/sv/e-agent-web/current." >&2
exit 1
