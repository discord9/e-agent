#!/data/data/com.termux/files/usr/bin/bash
# Called by the release installer after payload checksum validation.
set -eu
umask 077
export SVDIR="$PREFIX/var/service" LOGDIR="$PREFIX/var/log"
service="$SVDIR/e-agent-web"
logdir="$LOGDIR/sv/e-agent-web"
for tool in sv svlogd service-daemon; do
  if ! command -v "$tool" >/dev/null; then pkg install -y termux-services; break; fi
done
for tool in sv svlogd service-daemon; do
  command -v "$tool" >/dev/null || { echo "Missing service command: $tool" >&2; exit 1; }
done
fresh=0
if [ ! -e "$service" ]; then
  mkdir -p "$service"
  touch "$service/down"
  fresh=1
fi
mkdir -p "$service/log" "$logdir"
chmod 700 "$logdir"
tmp=$(mktemp -d "$PREFIX/tmp/e-agent-service.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
cat > "$tmp/run" <<'RUN'
#!/data/data/com.termux/files/usr/bin/sh
set -eu
umask 077
exec 2>&1
workspace=${E_AGENT_WORKSPACE:-"$HOME/e-agent-workspace"}
if [ ! -d "$workspace" ]; then mkdir -p "$workspace"; chmod 700 "$workspace"; fi
exec "$PREFIX/bin/e-agent" web --host 127.0.0.1 --port 8766 --workspace "$workspace"
RUN
cat > "$tmp/log-run" <<'LOG'
#!/data/data/com.termux/files/usr/bin/sh
set -eu
umask 077
logdir="$PREFIX/var/log/sv/e-agent-web"
mkdir -p "$logdir"
chmod 700 "$logdir"
exec "$PREFIX/bin/svlogd" -tt "$logdir"
LOG
sed -i "1c#!$PREFIX/bin/sh" "$tmp/run" "$tmp/log-run"
printf 's1048576
n10
t86400
' > "$tmp/config"
# Hash receipts distinguish an older generated file from a user customization.
managed_file() {
  source=$1 destination=$2 mode=$3
  receipt="$destination.e-agent-sha256"
  if [ -e "$destination" ] || [ -L "$destination" ]; then
    if [ ! -L "$destination" ] && [ -f "$destination" ] && [ -f "$receipt" ] &&
       [ "$(sha256sum "$destination" | awk '{print $1}')" = "$(cat "$receipt")" ]; then :
    elif [ ! -L "$destination" ] && cmp -s "$source" "$destination"; then :
    else echo "Note: preserving edited service file: $destination"; return; fi
  fi
  install -m "$mode" "$source" "$destination"
  sha256sum "$destination" | awk '{print $1}' > "$receipt"
}
managed_file "$tmp/run" "$service/run" 700
managed_file "$tmp/log-run" "$service/log/run" 700
managed_file "$tmp/config" "$logdir/config" 600
# Never kill an existing foreground server or silently re-enable a disabled service.
if [ "$fresh" -eq 1 ]; then
  if curl -q --noproxy '*' -sS --max-time 2 http://127.0.0.1:8766/ -o /dev/null 2>/dev/null; then
    echo 'Port 8766 is occupied; service installed but left down. Stop your old server, then run: sv-enable e-agent-web'
  else
    service-daemon start >/dev/null 2>&1 || true
    for _ in 1 2 3 4 5; do [ -p "$service/supervise/ok" ] && break; sleep 1; done
    rm -f "$service/down"
    sv -w 5 up "$service" || { echo 'Service supervisor is not ready; reopen Termux and run: sv up e-agent-web' >&2; exit 1; }
  fi
fi
echo "Service: sv status|up|down|restart e-agent-web"
echo "Rotating service log: $logdir/current (1 MiB, 10 archives, daily rotation)"
