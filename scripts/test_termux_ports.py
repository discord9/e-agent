#!/usr/bin/env python3
"""Actual TCP listeners must not be mistaken for an available HTTP port."""
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parent
with tempfile.TemporaryDirectory(prefix="termux-ports-") as tmp:
    root = Path(tmp)
    prefix = root / "prefix"
    bindir = prefix / "bin"
    bindir.mkdir(parents=True)
    (prefix / "tmp").mkdir()
    for name in ("sv", "svlogd"):
        p = bindir / name
        p.write_text("#!/bin/sh\nexit 0\n")
        p.chmod(0o755)
    daemon = bindir / "service-daemon"
    daemon.write_text('#!/bin/sh\nmkdir -p "$SVDIR/e-agent-web/supervise"\nmkfifo "$SVDIR/e-agent-web/supervise/ok"\n')
    daemon.chmod(0o755)
    env = dict(os.environ, PREFIX=str(prefix), HOME=str(root), PATH=str(bindir) + os.pathsep + os.environ["PATH"])
    service = prefix / "var/service/e-agent-web"
    stop = root / "stop.sh"
    stop.write_text((ROOT / "termux-stop.sh").read_text().replace("@PREFIX_BIN@", str(bindir)))
    # TCP accepts a connection but never speaks HTTP: curl times out, not refused.
    with socket.socket() as listener:
        listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        listener.bind(("127.0.0.1", 8766))
        listener.listen(8)
        installed = subprocess.run(["bash", str(ROOT / "termux-service.sh")], env=env, text=True, capture_output=True, timeout=10)
        assert installed.returncode == 0, installed.stderr
        assert (service / "down").exists(), "non-HTTP listener enabled competing service"
        stopped = subprocess.run(["bash", str(stop)], env=env, text=True, capture_output=True, timeout=10)
        assert stopped.returncode != 0, "stop falsely reported an occupied port as stopped"
        assert "occupied or uncertain" in stopped.stderr
    # A genuinely refused TCP connection allows fresh startup and a clean stop.
    shutil.rmtree(service)
    installed = subprocess.run(["bash", str(ROOT / "termux-service.sh")], env=env, text=True, capture_output=True, timeout=10)
    assert installed.returncode == 0, (installed.stdout, installed.stderr)
    assert not (service / "down").exists(), "connection refusal did not permit startup"
    stopped = subprocess.run(["bash", str(stop)], env=env, text=True, capture_output=True, timeout=10)
    assert stopped.returncode == 0, stopped.stderr
    assert "no listener" in stopped.stdout
print("real TCP: non-HTTP listener remains disabled; explicit refusal permits startup; stop reports both states accurately")
