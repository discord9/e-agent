#!/data/data/com.termux/files/usr/bin/bash
# Disable the supervised service; never claim an unmanaged server was stopped.
set -eu
PREFIX='@PREFIX_BIN@'
PREFIX=${PREFIX%/bin}
export SVDIR="$PREFIX/var/service"
service="$SVDIR/e-agent-web"
umask 077
if [ -d "$service" ]; then
  touch "$service/down"
  if [ -p "$service/supervise/ok" ]; then
    "$PREFIX/bin/sv" -w 5 force-stop "$service" || "$PREFIX/bin/sv" -w 2 down "$service"
  fi
else
  echo 'No e-agent-web service is installed.'
fi
tmp=$(mktemp -d "$PREFIX/tmp/e-agent-stop.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
probe=0
LC_ALL=C curl -q --noproxy '*' --verbose --max-time 2 http://127.0.0.1:8766/ -o /dev/null 2> "$tmp/port-error" || probe=$?
if [ "$probe" -ne 7 ] || ! grep -F 'Connection refused' "$tmp/port-error" >/dev/null; then
  echo 'Supervised service disabled, but port 8766 is still occupied or uncertain. An old foreground launcher may still be running; stop it separately. No unmanaged process was killed.' >&2
  exit 1
fi
echo 'Supervised service is down and port 8766 has no listener. Click e-agent-web to start it again.'
