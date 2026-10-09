#!/data/data/com.termux/files/usr/bin/bash
# Stop the entire Web/agent service, not merely the browser tab.
set -eu
PREFIX='@PREFIX_BIN@'
PREFIX=${PREFIX%/bin}
export SVDIR="$PREFIX/var/service"
service="$SVDIR/e-agent-web"
[ -d "$service" ] || { echo 'No e-agent-web service is installed.'; exit 0; }
# Keep it disabled even if Termux's supervisor restarts later.
umask 077
touch "$service/down"
if [ -p "$service/supervise/ok" ]; then
  "$PREFIX/bin/sv" -w 5 force-stop "$service" || "$PREFIX/bin/sv" -w 2 down "$service"
fi
echo 'e-agent stopped. Click e-agent-web to start it again.'
