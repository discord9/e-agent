#!/usr/bin/env python3
"""Launch configured local e-agent web UIs and open one fragment-only import link.

Child-port preflight prevents adopting an already-listening service.  A bind can
still race another local process between the preflight and child bind.
"""
import argparse
import html
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import threading
import time
import urllib.parse
import urllib.request
import urllib.error
import webbrowser
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PAYLOAD_VERSION = 1
MAX_SERVERS = 32
MAX_PAYLOAD_BYTES = 16 * 1024
STARTUP_SECONDS = 12.0
WAIT_SECONDS = 3.0
TOP_KEYS = {"landing_port", "open_browser", "primary", "command", "servers"}
SERVER_KEYS = {"id", "label", "cwd", "port", "enabled", "profile", "args"}
ID_RE = __import__("re").compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$")
BAD_IDS = {"__proto__", "prototype", "constructor"}


class ConfigError(ValueError):
    pass


def _no_duplicates(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ConfigError("duplicate JSON key: " + key)
        result[key] = value
    return result


def _constant(value):
    raise ConfigError("non-finite JSON value: " + value)


def _string(value, name, *, empty=False, limit=120):
    if type(value) is not str or "\0" in value or len(value) > limit or (not empty and not value):
        raise ConfigError(name + " must be a nonempty safe string")
    return value


def _id(value, name="id"):
    value = _string(value, name, limit=64)
    if value.lower() in BAD_IDS or not ID_RE.fullmatch(value):
        raise ConfigError(name + " is invalid")
    return value


def _label(value):
    value = _string(value, "label")
    if value != value.strip() or any(ord(c) < 32 or ord(c) == 127 for c in value):
        raise ConfigError("label is unsafe")
    return value


def _expect(obj, key, typ, *, required=False, default=None):
    if key not in obj:
        if required:
            raise ConfigError("missing " + key)
        return default
    if type(obj[key]) is not typ:
        raise ConfigError(key + " has wrong type")
    return obj[key]


def _port(value, name):
    if type(value) is not int or not 1 <= value <= 65535:
        raise ConfigError(name + " must be an integer from 1 to 65535")
    if value == 80:
        raise ConfigError(name + " may not be 80 in v1")
    return value


def _resolve_command(command, base):
    command = _string(command, "command", limit=4096)
    if command.lower().endswith((".cmd", ".bat")):
        raise ConfigError("command must be a native executable, not a shell wrapper")
    if os.path.isabs(command) or os.path.dirname(command):
        path = Path(command)
        if not path.is_absolute():
            path = base / path
        path = path.resolve()
        found = str(path) if path.is_file() and os.access(path, os.X_OK) else None
    else:
        found = shutil.which(command)
    if not found:
        raise ConfigError("command was not found or is not executable")
    if str(found).lower().endswith((".cmd", ".bat")):
        raise ConfigError("command must be a native executable, not a shell wrapper")
    return str(Path(found).resolve())


def _validate_args(args):
    if type(args) is not list:
        raise ConfigError("args must be an array")
    # This is intentionally a positive allowlist: launcher owns every endpoint
    # and profile option, so secret-bearing or endpoint-changing arguments cannot enter argv.
    if any(type(arg) is not str or arg != "--read-only" for arg in args):
        raise ConfigError("only --read-only is allowed in args")
    if len(args) != len(set(args)):
        raise ConfigError("duplicate args are not allowed")
    return list(args)


def _payload_bytes(landing_port, primary, servers):
    payload = {"version": PAYLOAD_VERSION, "source": "http://127.0.0.1:%d" % landing_port,
               "primary": primary, "workspaces": [
                   {"id": server["id"], "name": server["label"],
                    "url": "http://127.0.0.1:%d" % server["port"]} for server in servers]}
    return json.dumps(payload, ensure_ascii=False, separators=(",", ":")).encode("utf-8")


def load_config(path):
    """Read and validate the entire JSON schema before spawning anything."""
    config_path = Path(path).resolve()
    try:
        data = json.loads(config_path.read_text(encoding="utf-8"), object_pairs_hook=_no_duplicates,
                          parse_constant=_constant)
    except (OSError, json.JSONDecodeError, UnicodeError) as exc:
        raise ConfigError("cannot read JSON config: " + str(exc)) from exc
    if type(data) is not dict or set(data) - TOP_KEYS:
        raise ConfigError("config must be an object with only known keys")
    base = config_path.parent
    landing_port = _port(_expect(data, "landing_port", int, default=8765), "landing_port")
    open_browser = _expect(data, "open_browser", bool, default=True)
    command = _resolve_command(_expect(data, "command", str, default="e-agent"), base)
    raw_servers = _expect(data, "servers", list, required=True)
    if not raw_servers or len(raw_servers) > MAX_SERVERS:
        raise ConfigError("servers must contain 1 to %d records" % MAX_SERVERS)
    servers, enabled_ids, enabled_ports, enabled_cwds = [], set(), {landing_port}, set()
    for index, raw in enumerate(raw_servers):
        if type(raw) is not dict or set(raw) - SERVER_KEYS:
            raise ConfigError("server %d has unknown keys or is not an object" % index)
        ident = _id(_expect(raw, "id", str, required=True))
        label = _label(_expect(raw, "label", str, required=True))
        cwd_text = _string(_expect(raw, "cwd", str, required=True), "cwd", limit=4096)
        port = _port(_expect(raw, "port", int, required=True), "server port")
        enabled = _expect(raw, "enabled", bool, default=True)
        profile = _string(raw["profile"], "profile") if "profile" in raw else None
        cwd = Path(cwd_text)
        if not cwd.is_absolute():
            cwd = base / cwd
        cwd = cwd.resolve()
        if not cwd.is_dir():
            raise ConfigError("server cwd is not a directory: " + str(cwd))
        args = _validate_args(_expect(raw, "args", list, default=[]))
        if enabled:
            if ident in enabled_ids:
                raise ConfigError("duplicate enabled id: " + ident)
            if port in enabled_ports:
                raise ConfigError("duplicate enabled port or landing-port collision: " + str(port))
            if str(cwd) in enabled_cwds:
                raise ConfigError("duplicate enabled canonical cwd: " + str(cwd))
            enabled_ids.add(ident); enabled_ports.add(port); enabled_cwds.add(str(cwd))
        servers.append({"id": ident, "label": label, "cwd": str(cwd), "port": port,
                        "enabled": enabled, "profile": profile, "args": args})
    enabled = [server for server in servers if server["enabled"]]
    if not enabled:
        raise ConfigError("at least one server must be enabled")
    primary = _id(_expect(data, "primary", str, default=enabled[0]["id"]), "primary")
    if primary not in enabled_ids:
        raise ConfigError("primary must name an enabled server")
    # Landing links can select any ready child as primary; bound the longest
    # enabled ID now so no alternate link can fail only after spawning.
    longest_primary = max((server["id"] for server in enabled), key=len)
    if len(_payload_bytes(landing_port, longest_primary, enabled)) > MAX_PAYLOAD_BYTES:
        raise ConfigError("launcher payload exceeds 16 KiB")
    return {"landing_port": landing_port, "open_browser": open_browser, "command": command,
            "primary": primary, "servers": servers}


def server_argv(config, server):
    argv = [config["command"], "web", "--host", "127.0.0.1", "--port", str(server["port"]),
            "--workspace", server["cwd"]]
    if server["profile"] is not None:
        argv += ["--profile", server["profile"]]
    return argv + server["args"]


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, fp, code, msg, headers, target):
        return None

_READY_OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}), _NoRedirect())

def ready_at(port):
    """Probe only this literal loopback URL; redirects are failures, never followed."""
    try:
        with _READY_OPENER.open("http://127.0.0.1:%d/" % port, timeout=.25) as response:
            return 200 <= response.status < 300 and response.geturl() == "http://127.0.0.1:%d/" % port
    except (OSError, urllib.error.HTTPError):
        return False


def port_is_free(port):
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            sock.bind(("127.0.0.1", port))
        except OSError:
            return False
    return True


class Child:
    def __init__(self, config, spec, popen=subprocess.Popen, probe=ready_at,
                 clock=time.monotonic, pause=time.sleep):
        self.spec, self.status, self.proc = spec, "starting", None
        self.probe, self.clock, self.pause, self.reaped = probe, clock, pause, False
        if not port_is_free(spec["port"]):
            self.status = "failed"
            return
        try:
            self.proc = popen(server_argv(config, spec), cwd=spec["cwd"], stdin=subprocess.DEVNULL,
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, shell=False)
        except OSError:
            self.status = "failed"

    def _reap_exited(self):
        if self.proc and not self.reaped:
            self.proc.wait()  # poll already established exit; collect the child exactly once.
            self.reaped = True

    def wait_ready(self, stop_requested=lambda: False):
        if not self.proc:
            return False
        deadline = self.clock() + STARTUP_SECONDS
        while not stop_requested() and self.clock() < deadline:
            if self.proc.poll() is not None:
                self.status = "failed"; self._reap_exited(); return False
            if self.probe(self.spec["port"]):
                self.status = "ready"; return True
            self.pause(.05)
        self.status = "failed"
        self.stop()  # cancellation and timeout both fully reap before the next child.
        return False

    def update(self):
        if self.proc and self.status == "ready" and self.proc.poll() is not None:
            self.status = "failed"; self._reap_exited()

    def stop(self):
        if not self.proc or self.reaped:
            return
        if self.proc.poll() is not None:
            self._reap_exited(); return
        self.proc.terminate()
        try:
            self.proc.wait(timeout=WAIT_SECONDS)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()
        self.reaped = True


class Launcher:
    def __init__(self, config, popen=subprocess.Popen, browser_open=webbrowser.open,
                 probe=ready_at, clock=time.monotonic, pause=time.sleep):
        self.config, self.popen, self.browser_open = config, popen, browser_open
        self.probe, self.clock, self.pause = probe, clock, pause
        self.children, self.httpd, self.stopping = [], None, False

    def _ready_children(self):
        return [child for child in self.children if child.status == "ready"]

    def import_url(self, primary=None):
        primary = primary or self.config["primary"]
        ready = tuple(child.spec for child in self.children if child.status == "ready")
        chosen = next((spec for spec in ready if spec["id"] == primary), None)
        if chosen is None:
            return None
        encoded = urllib.parse.quote(_payload_bytes(self.config["landing_port"], primary, ready), safe="")
        return "http://127.0.0.1:%d/#eagent-workspaces=%s" % (chosen["port"], encoded)

    def _handler(self):
        launcher = self
        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass
            def _headers(self, code, content_type):
                self.send_response(code); self.send_header("Content-Type", content_type)
                self.send_header("Cache-Control", "no-store")
                self.send_header("Referrer-Policy", "no-referrer")
                self.send_header("X-Frame-Options", "DENY")
                self.end_headers()
            def _allowed_host(self):
                return self.headers.get("Host") == "127.0.0.1:%d" % launcher.config["landing_port"]
            def do_GET(self):
                if not self._allowed_host():
                    self._headers(400, "text/plain; charset=utf-8"); self.wfile.write(b"bad host"); return
                if self.path != "/":
                    self._headers(404, "text/plain; charset=utf-8"); self.wfile.write(b"not found"); return
                rows = []
                for child in launcher.children:
                    label, status = html.escape(child.spec["label"]), html.escape(child.status)
                    link = launcher.import_url(child.spec["id"])
                    rows.append("<li>%s: %s%s</li>" % (label, status,
                        (" <a href=\"%s\">open</a>" % html.escape(link, quote=True)) if link else ""))
                body = ("<!doctype html><meta charset=utf-8><title>e-agent web</title><h1>e-agent web</h1><ul>" + "".join(rows) + "</ul>").encode()
                self._headers(200, "text/html; charset=utf-8"); self.wfile.write(body)
            def do_POST(self):
                self._headers(405, "text/plain; charset=utf-8"); self.wfile.write(b"method not allowed")
            do_PUT = do_POST
            do_DELETE = do_POST
            do_OPTIONS = do_POST
        return Handler

    def start(self):
        self.httpd = ThreadingHTTPServer(("127.0.0.1", self.config["landing_port"]), self._handler())
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()
        print("http://127.0.0.1:%d/" % self.config["landing_port"], flush=True)
        for spec in self.config["servers"]:
            if not spec["enabled"] or self.stopping:
                continue
            child = Child(self.config, spec, self.popen, self.probe, self.clock, self.pause)
            self.children.append(child)
            child.wait_ready(lambda: self.stopping)
        # A primary can exit while later children are starting.  Recheck all
        # children before deciding what, if anything, a browser may import.
        for child in self.children:
            child.update()
        url = self.import_url()
        if self.config["open_browser"] and not self.stopping and url:
            self.browser_open(url)

    def cleanup(self):
        self.stopping = True
        incomplete = False
        for child in self.children:
            try:
                child.stop()
            except OSError:
                try:
                    if child.proc and child.proc.poll() is not None:
                        child._reap_exited()
                    else:
                        incomplete = True
                except OSError:
                    incomplete = True
        if incomplete:
            print("multi-web cleanup incomplete", file=sys.stderr, flush=True)
        if self.httpd:
            self.httpd.shutdown(); self.httpd.server_close(); self.httpd = None

    def run(self):
        try:
            self.start()
            while True:
                for child in self.children: child.update()
                time.sleep(.2)
        except KeyboardInterrupt:
            pass
        finally:
            self.cleanup()


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", required=True)
    args = parser.parse_args(argv)
    try:
        Launcher(load_config(args.config)).run()
    except ConfigError as exc:
        parser.error(str(exc))


if __name__ == "__main__":
    main()
