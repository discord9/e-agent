#!/usr/bin/env python3
"""Exercise generated services with real runsvdir/svlogd, not supervisor mocks."""
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parent

def wait_for(predicate, timeout=8):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.1)
    raise AssertionError("service did not reach expected state")

with tempfile.TemporaryDirectory(prefix="termux-sv-test-") as tmp:
    root = Path(tmp)
    prefix = root / "data/data/com.termux/files/usr"
    home = root / "home"
    bindir = prefix / "bin"
    svdir = prefix / "var/service"
    for path in (bindir, svdir, prefix / "tmp", home):
        path.mkdir(parents=True, exist_ok=True)
    for name in ("sh", "sv", "svlogd"):
        (bindir / name).symlink_to(shutil.which(name))
    pidfile = root / "pid"
    (bindir / "e-agent").write_text('#!/bin/sh\necho $$ > "$PIDFILE"\necho fixture-started\necho fixture-stderr >&2\n[ ! -e "$STUBBORN" ] || trap "" TERM\nexec sleep 60\n')
    (bindir / "service-daemon").write_text("#!/bin/sh\nexit 0\n")
    (bindir / "curl").write_text("#!/bin/sh\necho 'Connection refused' >&2\nexit 7\n")
    for name in ("e-agent", "service-daemon", "curl"):
        (bindir / name).chmod(0o755)
    env = dict(os.environ, PREFIX=str(prefix), HOME=str(home), SVDIR=str(svdir),
               LOGDIR=str(prefix / "var/log"), PIDFILE=str(pidfile), STUBBORN=str(root / "stubborn"), PATH=str(bindir)+os.pathsep+os.environ["PATH"])
    service = svdir / "e-agent-web"
    logfile = prefix / "var/log/sv/e-agent-web/current"
    with (root / "supervisor.log").open("wb") as output:
        supervisor = subprocess.Popen(["runsvdir", str(svdir)], env=env, stdout=output, stderr=output)
        try:
            install = subprocess.run(["bash", str(ROOT / "termux-service.sh")], env=env,
                                     text=True, capture_output=True, timeout=15)
            assert install.returncode == 0, (install.stdout, install.stderr)
            wait_for(pidfile.exists)
            oldpid = int(pidfile.read_text())
            wait_for(lambda: logfile.exists() and "fixture-stderr" in logfile.read_text())
            os.kill(oldpid, signal.SIGTERM)
            wait_for(lambda: int(pidfile.read_text()) != oldpid)
            wait_for(lambda: logfile.read_text().count("fixture-started") >= 2)
            # The stop icon must escalate when the server ignores TERM.
            (root / "stubborn").touch()
            oldpid = int(pidfile.read_text())
            subprocess.run(["sv", "-w", "5", "restart", str(service)], env=env, check=True)
            wait_for(lambda: int(pidfile.read_text()) != oldpid)
            stop = root / "stop.sh"
            stop.write_text((ROOT / "termux-stop.sh").read_text().replace("@PREFIX_BIN@", str(bindir)))
            subprocess.run(["bash", str(stop)], env=env, check=True)
            assert (service / "down").exists(), "stop shortcut did not persist disabled state"
            current = pidfile.read_text()
            time.sleep(1.2)
            assert pidfile.read_text() == current, "disabled service restarted"
            assert (logfile.parent.stat().st_mode & 0o077) == 0, "service log directory is not private"
            assert (logfile.parent / "config").read_text() == "s1048576\nn10\nt86400\n"
            # A click after stop re-enables the service, opens the UI, then exits.
            token = home / ".local/state/e-agent/server.token"
            token.parent.mkdir(parents=True)
            token.write_text("fixture-token")
            (bindir / "curl").write_text("""#!/bin/sh
[ -f "$PIDFILE" ] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null || { echo 'Connection refused' >&2; exit 7; }
case "$*" in
  */api/models*) data='["fixture"]' ;;
  *) data='<title>e-agent · Web UI</title>' ;;
esac
while [ "$#" -gt 0 ]; do
  if [ "$1" = -o ]; then printf '%s' "$data" > "$2"; exit 0; fi
  shift
done
printf '%s' "$data"
""")
            opened = root / "opened"
            env["OPENED_FILE"] = str(opened)
            (bindir / "termux-open-url").write_text('#!/bin/sh\n echo "$1" >> "$OPENED_FILE"\n')
            (bindir / "termux-open-url").chmod(0o755)
            start = root / "start.sh"
            start.write_text((ROOT / "termux-web.sh").read_text().replace("@PREFIX_BIN@", str(bindir)))
            launched = subprocess.run(["bash", str(start)], env=env, text=True, capture_output=True, timeout=15)
            assert launched.returncode == 0, (launched.stdout, launched.stderr)
            assert not (service / "down").exists(), "start shortcut did not re-enable service"
            assert opened.read_text().strip() == "http://127.0.0.1:8766"
            running_pid = int(pidfile.read_text())
            again = subprocess.run(["bash", str(start)], env=env, capture_output=True, timeout=5)
            assert again.returncode == 0
            assert int(pidfile.read_text()) == running_pid, "repeated click spawned another server"
            subprocess.run(["bash", str(stop)], env=env, check=True, timeout=10)
            print("real runit: stdout/stderr logs, crash restart, forced stop, persistent disable, start/reopen shortcuts passed")
        finally:
            if service.exists():
                subprocess.run(["sv", "-w", "2", "force-stop", str(service)], env=env, capture_output=True)
                subprocess.run(["sv", "-w", "5", "exit", str(service)], env=env, capture_output=True)
            supervisor.terminate()
            supervisor.wait(timeout=8)
