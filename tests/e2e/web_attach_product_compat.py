#!/usr/bin/env python3
"""Isolated JSONL product compatibility probe: new -> bca5941 -> new.

Uses actual --serve runners and their authenticated API, never synthetic JSONL.
The local OpenAI-compatible provider is controlled only to make a real delegate
child for the initial parent transcript and deterministic append answers.
"""
import hashlib
import json
import os
import signal
import subprocess
import sys
import threading
import time
from pathlib import Path

import web_attach_product as h

ROOT = Path(__file__).resolve().parents[2]
ART = ROOT / ".e-agent" / "web-product-acceptance"
NEW = h.BINARY
OLD = ART / "bin" / "e-agent-old-bca5941"
OLD_EXPECTED = "ac0cccf63117004110ef4217e1afd969e8d32c28fe42a4b221e10640164b4d59"


def sh(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def start(binary, port, workspace, env, run, label):
    argv = [str(binary), "--serve", "--host", "127.0.0.1", "--port", str(port), "--workspace", str(workspace)]
    proc = subprocess.Popen(argv, cwd=workspace, env=env,
        stdout=open(run / (label + ".stdout.log"), "w"),
        stderr=open(run / (label + ".stderr.log"), "w"))
    # Old server startup does not necessarily recreate an existing token file;
    # prove the socket is listening before any API request.
    import socket
    def listening():
        if proc.poll() is not None:
            raise RuntimeError("%s exited: %s" % (label, (run / (label + ".stderr.log")).read_text()))
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=.2): return True
        except OSError: return False
    h.wait_until(listening, 20, label + " listen")
    return proc, argv


def stop(proc):
    if proc.poll() is None:
        proc.send_signal(signal.SIGINT)
        try: proc.wait(timeout=8)
        except subprocess.TimeoutExpired: proc.kill(); proc.wait()


def count_history(port, token, sid):
    cursor, entries = None, []
    while True:
        q = "?limit=200" + (("&before_seq=%s" % cursor) if cursor is not None else "")
        page = h.api_json(port, token, "GET", "/api/sessions/%s/history%s" % (sid, q))
        entries = page.get("entries", []) + entries
        cursor = page.get("next_before_seq")
        if cursor is None: return entries


def text_entries(entries):
    return json.dumps(entries, ensure_ascii=False)


def message_role_content(entries, role, content):
    needle = {role: {"content": content}}
    return sum(1 for entry in entries if entry.get("type") == "message" and needle.items() <= entry.get("message", {}).items())


def assistant_content(entries, content):
    return sum(1 for entry in entries if entry.get("type") == "message"
               and entry.get("message", {}).get("Assistant", {}).get("content") == content)


def child_session(entries):
    for entry in entries:
        if entry.get("type") == "background_completion":
            output = entry.get("output", "")
            if output.startswith("subagent session: "):
                return output.split("\n", 1)[0].split(": ", 1)[1]
    raise RuntimeError("initial parent has no durable child completion record")


def wait_complete_append(port, token, parent, prompt, answer, label):
    h.api_json(port, token, "POST", "/api/sessions/%s/prompt" % parent, {"text": prompt})
    def complete():
        entries = count_history(port, token, parent)
        return message_role_content(entries, "User", prompt) == 1 and assistant_content(entries, answer) == 1
    h.wait_until(complete, 30, label + " complete user/assistant pair")
    # Session list exposes real runner state; do not stop it until it is idle.
    def idle():
        rows = h.api_json(port, token, "GET", "/api/sessions")
        row = next((r for r in rows if r.get("id") == parent), None)
        return row is not None and row.get("busy") is False and row.get("status") in ("Idle", "Finished")
    h.wait_until(idle, 30, label + " runner idle")
    return count_history(port, token, parent)


def main():
    if not NEW.is_file() or not OLD.is_file():
        raise SystemExit("required new or old binary unavailable")
    if sh(OLD) != OLD_EXPECTED:
        raise SystemExit("old binary hash does not match supplied bca5941 value")
    run = ART / ("compat-new-old-new-" + time.strftime("%Y%m%dT%H%M%SZ", time.gmtime()))
    run.mkdir(parents=True)
    home, cfg, state, workspace = (run / x for x in ("home", "config", "state", "workspace"))
    for p in (home, cfg / "e-agent", state, workspace): p.mkdir(parents=True, exist_ok=True)
    provider_port = h.free_port()
    (cfg / "e-agent" / "config.toml").write_text(
        'default = "mock/product"\n[providers.mock]\nbase_url = "http://127.0.0.1:%d/v1"\napi_key_env = "PRODUCT_DUMMY_KEY"\n[models."mock/product"]\nmodel = "mock-model"\n[session]\nbackend = "jsonl"\n' % provider_port)
    log = open(run / "provider-requests.jsonl", "w", encoding="utf-8")
    h.PROVIDER_STATE = h.ProviderState(log, str(workspace))
    h.Provider.state = h.PROVIDER_STATE
    provider = h.ThreadingHTTPServer(("127.0.0.1", provider_port), h.Provider)
    threading.Thread(target=provider.serve_forever, daemon=True).start()
    env = os.environ.copy()
    env.update({"HOME": str(home), "XDG_CONFIG_HOME": str(cfg), "XDG_STATE_HOME": str(state),
                "PRODUCT_DUMMY_KEY": "dummy-local-only", "RUST_LOG": "info"})
    before = {"new": {"path": str(NEW.resolve()), "sha256_before": sh(NEW)},
              "old": {"path": str(OLD.resolve()), "sha256_before": sh(OLD)},
              "harness": {"path": str(Path(__file__).resolve()), "sha256_before": sh(__file__)},
              "old_expected_sha256": OLD_EXPECTED, "server_isolation_overrides": h.safe_env(env), "stages": []}
    (run / "provenance-before.json").write_text(json.dumps(before, indent=2) + "\n")
    processes = []
    try:
        # New stage: real parent delegate -> child completion transaction.
        port = h.free_port(); proc, argv = start(NEW, port, workspace, env, run, "new-initial"); processes.append(proc)
        token = h.wait_for(state / "e-agent" / "server.token")
        parent = h.api_json(port, token, "POST", "/api/sessions", {"initial_prompt": "PARENT_TRIGGER"})["id"]
        if not h.PROVIDER_STATE.prefix_sent.wait(30): raise RuntimeError("initial parent never reached held completion")
        h.PROVIDER_STATE.release_suffix.set()
        h.wait_until(lambda: h.ANSWER in text_entries(count_history(port, token, parent)), 30, "initial parent answer")
        initial = count_history(port, token, parent)
        child = child_session(initial)
        child_initial = count_history(port, token, child)
        if assistant_content(child_initial, h.CHILD) != 1:
            raise RuntimeError("initial child history lacks exactly one child report")
        before["stages"].append({"binary": "new-initial", "argv": argv, "parent": parent, "child": child, "entry_count": len(initial), "child_entry_count": len(child_initial)})
        stop(proc); processes.pop()

        # Old stage: explicitly resume the same persisted parent and append.
        port = h.free_port(); proc, argv = start(OLD, port, workspace, env, run, "old-append"); processes.append(proc)
        token = h.wait_for(state / "e-agent" / "server.token")
        h.api_json(port, token, "POST", "/api/sessions", {"id": parent})
        old_entries = wait_complete_append(port, token, parent, "COMPAT_OLD_APPEND", "COMPAT_OLD_RESPONSE", "old append")
        child_old = count_history(port, token, child)
        before["stages"].append({"binary": "old-bca5941", "argv": argv, "entry_count": len(old_entries), "child_entry_count": len(child_old)})
        stop(proc); processes.pop()

        # New stage: explicitly resume old-written data, append, then restart
        # once more and use the product history API as final reader.
        port = h.free_port(); proc, argv = start(NEW, port, workspace, env, run, "new-final"); processes.append(proc)
        token = h.wait_for(state / "e-agent" / "server.token")
        h.api_json(port, token, "POST", "/api/sessions", {"id": parent})
        new_entries = wait_complete_append(port, token, parent, "COMPAT_NEW_APPEND", "COMPAT_NEW_RESPONSE", "new append")
        child_new = count_history(port, token, child)
        before["stages"].append({"binary": "new-final", "argv": argv, "entry_count": len(new_entries), "child_entry_count": len(child_new)})
        stop(proc); processes.pop()
        port = h.free_port(); proc, argv_restart = start(NEW, port, workspace, env, run, "new-final-restart"); processes.append(proc)
        token = h.wait_for(state / "e-agent" / "server.token")
        h.api_json(port, token, "POST", "/api/sessions", {"id": parent})
        final = count_history(port, token, parent)
        child_final = count_history(port, token, child)
        final_text = text_entries(final)
        provider_counts = {}
        for record in h.PROVIDER_STATE.requests:
            provider_counts[record["kind"]] = provider_counts.get(record["kind"], 0) + 1
        checks = {"old_hash_expected": sh(OLD) == OLD_EXPECTED,
                  "old_provider_turn_observed": provider_counts.get("compat_old", 0) == 1,
                  "new_provider_turn_observed": provider_counts.get("compat_new", 0) == 1,
                  "initial_parent_full_answer_once": assistant_content(final, h.ANSWER) == 1,
                  "initial_child_history_exact_preserved": child_final == child_initial == child_old == child_new,
                  "initial_parent_prefix_preserved": final[:len(initial)] == initial,
                  "old_complete_pair_once": message_role_content(final, "User", "COMPAT_OLD_APPEND") == 1 and assistant_content(final, "COMPAT_OLD_RESPONSE") == 1,
                  "new_complete_pair_once": message_role_content(final, "User", "COMPAT_NEW_APPEND") == 1 and assistant_content(final, "COMPAT_NEW_RESPONSE") == 1,
                  "parent_child_before_complete_appends": final_text.find(h.CHILD) < final_text.find("COMPAT_OLD_APPEND") < final_text.find("COMPAT_OLD_RESPONSE") < final_text.find("COMPAT_NEW_APPEND") < final_text.find("COMPAT_NEW_RESPONSE")}
        before["stages"].append({"binary": "new-final-restart", "argv": argv_restart, "entry_count": len(final), "child_entry_count": len(child_final)})
        (run / "compat-history.json").write_text(json.dumps({"parent_session_id": parent, "child_session_id": child, "initial_parent": initial, "initial_child": child_initial, "old_parent": old_entries, "old_child": child_old, "new_parent": new_entries, "new_child": child_new, "final_parent": final, "final_child": child_final, "provider_request_counts": provider_counts, "checks": checks}, ensure_ascii=False, indent=2) + "\n")
        if not all(checks.values()): raise RuntimeError("compat checks failed: %r" % checks)
        result = {"status": "passed", "run_dir": str(run), "checks": checks}
    except Exception as error:
        result = {"status": "failed", "run_dir": str(run), "error": str(error)}
        raise
    finally:
        for proc in reversed(processes): stop(proc)
        provider.shutdown(); provider.server_close(); log.close()
        token_path = state / "e-agent" / "server.token"
        if token_path.exists(): token_path.unlink()
        before["new"]["sha256_after"] = sh(NEW); before["old"]["sha256_after"] = sh(OLD); before["harness"]["sha256_after"] = sh(__file__)
        before["new"]["unchanged"] = before["new"]["sha256_before"] == before["new"]["sha256_after"]
        before["old"]["unchanged"] = before["old"]["sha256_before"] == before["old"]["sha256_after"]
        before["harness"]["unchanged"] = before["harness"]["sha256_before"] == before["harness"]["sha256_after"]
        (run / "provenance-after.json").write_text(json.dumps(before, indent=2) + "\n")
        hashes = {str(p.relative_to(run)): sh(p) for p in sorted(run.rglob("*")) if p.is_file() and p.name != "manifest.json"}
        result["provenance"] = {"new_unchanged": before["new"]["unchanged"], "old_unchanged": before["old"]["unchanged"], "harness_unchanged": before["harness"]["unchanged"]}
        result["sha256"] = hashes
        (run / "manifest.json").write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n")
        print(json.dumps(result, ensure_ascii=False, indent=2))
    return 0

if __name__ == "__main__":
    raise SystemExit(main())
