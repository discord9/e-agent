#!/usr/bin/env bash
# setup-searxng.sh - install/repair a local SearXNG instance for e-agent's
# web_search tool (provider = "searxng").
#
# Idempotent: re-running repairs/updates the checkout, venv, settings and unit.
#
#   git checkout (shallow)   -> $INSTALL_DIR/src       ref = $SEARXNG_REF (default: remote HEAD)
#   uv venv                  -> $INSTALL_DIR/.venv
#   uv pip install ...       -> requirements.txt + editable searxng into the venv
#                               (searxng is a setup.py project: no pyproject.toml)
#   settings.yml             -> $INSTALL_DIR/settings.yml (random secret_key,
#                               127.0.0.1:<port>, JSON API, preset engines)
#   systemd --user unit      -> ~/.config/systemd/user/searxng.service
#   self-test                -> GET http://127.0.0.1:<port>/search?q=test&format=json
#
# Version pinning: the default ref is the remote's default branch (`SEARXNG_REF=HEAD`,
# currently `master` upstream). A revision verified with e-agent (2026-09-16) can be
# pinned instead, for example:
#   SEARXNG_REF=461f174b09fc151f49257aa5206417aca8930efa scripts/setup-searxng.sh
#
# Engine availability depends on this machine's egress IP: upstreams CAPTCHA,
# rate-limit (429) or JS-gate requests per host. The installer ends with a curl
# self-test that reports unresponsive engines - edit settings.yml and restart
# the service to adjust.
#
# Usage: scripts/setup-searxng.sh [INSTALL_DIR] [--port PORT] [--ref REF] [--skip-systemd]
# Env:   SEARXNG_DIR, SEARXNG_PORT, SEARXNG_REF, SEARXNG_REPO

set -euo pipefail

log() { printf '[setup-searxng] %s\n' "$*"; }
warn() { printf '[setup-searxng] WARNING: %s\n' "$*" >&2; }
die() {
    printf '[setup-searxng] ERROR: %s\n' "$*" >&2
    exit 1
}

usage() {
    cat <<'EOF'
Usage: setup-searxng.sh [INSTALL_DIR] [options]

Install (or repair) a local SearXNG instance for e-agent's web_search tool.

Options:
  --port PORT       listen port (default: 8088, env SEARXNG_PORT)
  --ref REF         git ref to check out: branch, tag or commit sha
                    (default: HEAD = remote default branch, env SEARXNG_REF)
  --skip-systemd    do not install/start the systemd --user unit
  -h, --help        show this help

Environment:
  SEARXNG_DIR       install directory
                    (default: ${XDG_DATA_HOME:-$HOME/.local/share}/searxng)
  SEARXNG_REPO      git remote
                    (default: https://github.com/searxng/searxng.git)

A positional INSTALL_DIR overrides SEARXNG_DIR.
EOF
}

INSTALL_DIR="${SEARXNG_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/searxng}"
PORT="${SEARXNG_PORT:-8088}"
REF="${SEARXNG_REF:-HEAD}"
REPO_URL="${SEARXNG_REPO:-https://github.com/searxng/searxng.git}"
SKIP_SYSTEMD=0

while [ "$#" -gt 0 ]; do
    case "$1" in
        --port)
            [ "$#" -ge 2 ] || die "--port requires a value"
            PORT="$2"
            shift 2
            ;;
        --port=*)
            PORT="${1#--port=}"
            shift
            ;;
        --ref)
            [ "$#" -ge 2 ] || die "--ref requires a value"
            REF="$2"
            shift 2
            ;;
        --ref=*)
            REF="${1#--ref=}"
            shift
            ;;
        --skip-systemd)
            SKIP_SYSTEMD=1
            shift
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        -*)
            die "unknown option: $1 (try --help)"
            ;;
        *)
            INSTALL_DIR="$1"
            shift
            ;;
    esac
done

case "$PORT" in
    '' | *[!0-9]*) die "invalid port: '$PORT'" ;;
esac
[ -n "$REF" ] || die "empty --ref"

expand_dir() {
    local dir="$1"
    if [ "$dir" = "~" ]; then
        dir="$HOME"
    elif [ "${dir#\~/}" != "$dir" ]; then
        dir="$HOME/${dir#\~/}"
    fi
    mkdir -p "$dir" || die "cannot create directory: $dir"
    (cd "$dir" && pwd)
}

check_deps() {
    local missing=0 tool
    for tool in git uv; do
        if ! command -v "$tool" >/dev/null 2>&1; then
            warn "missing required tool: $tool"
            missing=1
        fi
    done
    [ "$missing" -eq 0 ] || die "install the missing tools above and re-run"
    if ! command -v curl >/dev/null 2>&1; then
        warn "curl not found: the post-install self-test will be skipped"
    fi
    if ! command -v python3 >/dev/null 2>&1; then
        warn "no python3 on PATH: uv can still provide one, see below"
    fi
}

# SearXNG needs Python >= 3.11 (searx/botdetection imports tomllib). Pick the
# first suitable interpreter; otherwise ask uv for a managed CPython, which uv
# downloads automatically when it is not installed yet.
select_python() {
    local candidate
    for candidate in python3.13 python3.12 python3.11 python3; do
        if command -v "$candidate" >/dev/null 2>&1 &&
            "$candidate" -c 'import sys; sys.exit(0 if sys.version_info >= (3, 11) else 1)' 2>/dev/null; then
            PYTHON_SELECT="$candidate"
            log "using interpreter: $candidate ($("$candidate" -c 'import platform; print(platform.python_version())'))"
            return 0
        fi
    done
    PYTHON_SELECT="3.12"
    log "no Python >= 3.11 on PATH: uv will use (and download if needed) a managed CPython $PYTHON_SELECT"
}

install_src() {
    if [ -d "$SRC_DIR/.git" ]; then
        log "updating existing checkout in $SRC_DIR"
        if [ -n "$(git -C "$SRC_DIR" status --porcelain --untracked-files=no)" ]; then
            warn "local changes in $SRC_DIR: leaving the working tree as-is"
            return 0
        fi
    elif [ -e "$SRC_DIR" ]; then
        die "$SRC_DIR exists but is not a git checkout; move it away and re-run"
    else
        log "initializing git checkout in $SRC_DIR"
        mkdir -p "$SRC_DIR"
        git -C "$SRC_DIR" init -q
    fi
    if ! git -C "$SRC_DIR" remote get-url origin >/dev/null 2>&1; then
        git -C "$SRC_DIR" remote add origin "$REPO_URL"
    fi
    log "fetching '$REF' from $REPO_URL (shallow)"
    git -C "$SRC_DIR" fetch --depth 1 origin "$REF" ||
        die "cannot fetch ref '$REF' from $REPO_URL (wrong ref, or offline?)"
    git -C "$SRC_DIR" checkout -q --detach FETCH_HEAD ||
        die "cannot check out '$REF'"
    log "searxng revision: $(git -C "$SRC_DIR" rev-parse HEAD) (fetched ref: $REF)"
    log "pin this revision later with: SEARXNG_REF=<sha> $0"
}

install_env() {
    local have_venv=0
    if [ -x "$VENV_DIR/bin/python" ] &&
        "$VENV_DIR/bin/python" -c 'import sys; sys.exit(0 if sys.version_info >= (3, 11) else 1)' 2>/dev/null; then
        have_venv=1
        log "reusing virtualenv $VENV_DIR ($("$VENV_DIR/bin/python" -c 'import platform; print(platform.python_version())'))"
    elif [ -e "$VENV_DIR" ]; then
        log "removing $VENV_DIR: no Python >= 3.11 environment there"
        rm -rf "$VENV_DIR"
    fi
    if [ "$have_venv" -eq 0 ]; then
        log "creating virtualenv $VENV_DIR with uv (interpreter: $PYTHON_SELECT)"
        uv venv --python "$PYTHON_SELECT" "$VENV_DIR" ||
            die "cannot create the virtualenv: SearXNG needs Python >= 3.11 (uv can download a managed CPython when online)"
    fi
    # SearXNG is a setup.py project (no pyproject.toml) and its setup.py imports
    # searx itself, so the build backend needs the runtime deps plus setuptools
    # preinstalled: install requirements first, then editable without build
    # isolation - the sequence upstream uses in ./manage (pyenv.install).
    log "installing SearXNG runtime requirements into the venv"
    uv pip install --python "$VENV_DIR/bin/python" \
        -r "$SRC_DIR/requirements.txt" setuptools wheel ||
        die "failed to install SearXNG requirements (network?)"
    log "installing searxng (editable) into the venv"
    uv pip install --python "$VENV_DIR/bin/python" --no-build-isolation -e "$SRC_DIR" ||
        die "failed to install the searxng package"
    "$VENV_DIR/bin/python" -c 'import searx; print("searx import OK:", searx.__file__)' ||
        die "searx import check failed"
}

write_settings() {
    if [ -f "$SETTINGS_FILE" ]; then
        log "keeping existing $SETTINGS_FILE (delete it to regenerate, e.g. for a new secret_key)"
        return 0
    fi
    local secret
    secret="$("$VENV_DIR/bin/python" -c 'import secrets; print(secrets.token_hex(32))')"
    log "writing $SETTINGS_FILE"
    cat >"$SETTINGS_FILE" <<EOF
# Local SearXNG for e-agent web_search (generated by scripts/setup-searxng.sh).
# Docs: https://docs.searxng.org/admin/settings/settings.html
#
# Engine availability depends on this machine's egress IP: providers CAPTCHA,
# rate-limit (HTTP 429) or JS-gate requests they dislike. Check the installer's
# "unresponsive engines" output, then enable/disable engines below (engines not
# listed keep SearXNG's defaults) and restart:
#   systemctl --user restart searxng.service
# Re-check with:
#   curl -s 'http://127.0.0.1:$PORT/search?q=test&format=json' | python3 -m json.tool

use_default_settings: true

server:
  bind_address: "127.0.0.1"
  port: $PORT
  secret_key: "$secret"
  limiter: false
  image_proxy: false

search:
  formats:
    - html
    - json

engines:
  # Usually reachable from most egress IPs:
  - name: bing
    disabled: false
  - name: yandex
    disabled: false
  - name: naver
    disabled: false
  - name: mwmbl
    disabled: false
  # Blocked from some IPs (CAPTCHA / 429 / JS gate / no search API): leave them
  # here for the first self-test, then set disabled: true for the dead ones.
  - name: duckduckgo
    disabled: false
  - name: brave
    disabled: false
  - name: google
    disabled: false
  - name: wikipedia
    disabled: false
EOF
}

write_unit() {
    mkdir -p "$(dirname "$UNIT_FILE")"
    log "writing $UNIT_FILE"
    cat >"$UNIT_FILE" <<EOF
[Unit]
Description=Local SearXNG metasearch instance for e-agent web_search
Documentation=https://docs.searxng.org/admin/
After=network.target

[Service]
Type=simple
WorkingDirectory=$SRC_DIR
Environment=SEARXNG_SETTINGS_PATH=$SETTINGS_FILE
ExecStart=$VENV_DIR/bin/python -m searx.webapp
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
EOF
}

start_service() {
    if ! command -v systemctl >/dev/null 2>&1; then
        warn "systemctl not found: unit written but not started"
        return 0
    fi
    if ! systemctl --user show-environment >/dev/null 2>&1; then
        warn "systemd user manager is not reachable in this shell; start the unit later with:"
        warn "  systemctl --user daemon-reload && systemctl --user enable --now searxng.service"
        return 0
    fi
    systemctl --user daemon-reload
    systemctl --user enable searxng.service ||
        die "failed to enable searxng.service"
    systemctl --user restart searxng.service ||
        die "failed to start searxng.service (see 'systemctl --user status searxng.service')"
    RUNNING=1
    log "systemd user unit searxng.service enabled and (re)started"
    log "tip: 'loginctl enable-linger ${USER:-$(id -un)}' keeps it running when you are not logged in"
}

report_json() {
    local body="$1"
    printf '%s' "$body" | "$VENV_DIR/bin/python" -c '
import json, sys

port, settings = sys.argv[1], sys.argv[2]
try:
    data = json.load(sys.stdin)
except Exception as exc:
    print("[setup-searxng] ERROR: invalid JSON from SearXNG: %s" % exc, file=sys.stderr)
    sys.exit(1)
results = data.get("results") or []
unresponsive = data.get("unresponsive_engines") or []
print("[setup-searxng] self-test OK: %d result(s) from 127.0.0.1:%s" % (len(results), port))
if unresponsive:
    parts = []
    for eng in unresponsive:
        if isinstance(eng, (list, tuple)) and eng:
            parts.append(str(eng[0]) + (" (%s)" % eng[1] if len(eng) > 1 else ""))
        else:
            parts.append(str(eng))
    print("[setup-searxng] unresponsive engines: " + ", ".join(parts))
    print("[setup-searxng] adjust the engine list in %s and restart: systemctl --user restart searxng.service" % settings)
else:
    print("[setup-searxng] no unresponsive engines reported")
' "$PORT" "$SETTINGS_FILE"
}

self_test() {
    if ! command -v curl >/dev/null 2>&1; then
        return 0
    fi
    local url="http://127.0.0.1:$PORT/search?q=test&format=json"
    local body="" i
    log "self-test: $url"
    for ((i = 0; i < 15; i++)); do
        if body="$(curl -s --noproxy '*' --max-time 5 "$url" 2>/dev/null)" && [ -n "$body" ]; then
            break
        fi
        body=""
        sleep 1
    done
    if [ -z "$body" ]; then
        if [ "$RUNNING" -eq 1 ]; then
            die "SearXNG is not answering on 127.0.0.1:$PORT; check 'systemctl --user status searxng.service' and 'journalctl --user -u searxng.service -e'"
        fi
        log "nothing is listening on 127.0.0.1:$PORT yet; start SearXNG and re-run for a self-test:"
        log "  (cd '$SRC_DIR' && SEARXNG_SETTINGS_PATH='$SETTINGS_FILE' '$VENV_DIR/bin/python' -m searx.webapp)"
        return 0
    fi
    report_json "$body" || die "self-test response was not valid JSON"
}

print_config_hint() {
    cat <<EOF

[setup-searxng] add this to your e-agent config (~/.config/e-agent/config.toml):

[web_search]
provider = "searxng"
base_url = "http://127.0.0.1:$PORT"

[setup-searxng] verify the instance with:
  curl -s 'http://127.0.0.1:$PORT/search?q=test&format=json' | python3 -m json.tool
EOF
}

RUNNING=0
main() {
    check_deps
    select_python
    INSTALL_DIR="$(expand_dir "$INSTALL_DIR")"
    SRC_DIR="$INSTALL_DIR/src"
    VENV_DIR="$INSTALL_DIR/.venv"
    SETTINGS_FILE="$INSTALL_DIR/settings.yml"
    UNIT_FILE="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/searxng.service"
    log "install directory: $INSTALL_DIR"
    install_src
    install_env
    write_settings
    if [ "$SKIP_SYSTEMD" -eq 1 ]; then
        log "--skip-systemd: unit not installed; start manually with:"
        log "  (cd '$SRC_DIR' && SEARXNG_SETTINGS_PATH='$SETTINGS_FILE' '$VENV_DIR/bin/python' -m searx.webapp)"
    else
        write_unit
        start_service
    fi
    self_test
    print_config_hint
    log "done"
}

main "$@"
