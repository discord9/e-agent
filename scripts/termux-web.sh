#!/data/data/com.termux/files/usr/bin/bash
set -eu
BIN='@PREFIX_BIN@/e-agent'
[ -x "$BIN" ] || { echo "e-agent not installed at $BIN; run the Termux installer first" >&2; exit 1; }
: "${HOME:?HOME is unset}"
WORKSPACE=${E_AGENT_WORKSPACE:-"$HOME/e-agent-workspace"}
URL=http://127.0.0.1:8766
owned() { curl -fsS --max-time 2 "$URL/" 2>/dev/null | grep -Fq '<title>e-agent · Web UI</title>'; }
open_ui() {
  if command -v termux-open-url >/dev/null 2>&1; then termux-open-url "$URL" || echo "Open $URL manually" >&2
  elif command -v termux-open >/dev/null 2>&1; then termux-open "$URL" || echo "Open $URL manually" >&2
  else echo "Open $URL in your browser"; fi
}
if owned; then open_ui; exit 0; fi
if curl -sS --max-time 2 "$URL/" >/dev/null 2>&1; then echo "Port 8766 is occupied by a service that is not e-agent; refusing to open it." >&2; exit 1; fi
mkdir -p "$WORKSPACE"
"$BIN" web --host 127.0.0.1 --port 8766 --workspace "$WORKSPACE" &
pid=$!
cleanup() { kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; }
trap cleanup EXIT HUP INT TERM
ready=0
i=0
while [ "$i" -lt 30 ]; do
  if ! kill -0 "$pid" 2>/dev/null; then echo 'e-agent web exited before becoming ready; check its configuration and startup output.' >&2; exit 1; fi
  if owned; then ready=1; break; fi
  i=$((i+1)); sleep 1
done
[ "$ready" -eq 1 ] || { echo 'e-agent did not become ready on 127.0.0.1:8766 within 30 seconds.' >&2; exit 1; }
echo "e-agent web is ready at $URL (workspace: $WORKSPACE); leave this Widget task running, or press Ctrl-C to stop it."
(open_ui) &
wait "$pid"
