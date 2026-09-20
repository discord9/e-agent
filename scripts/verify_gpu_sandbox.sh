#!/bin/sh
# Host-side verification for `[sandbox] gpu = true` (bwrap GPU device binds).
#
# RUN ON THE HOST, not inside an e-agent sandbox: bwrap cannot create a new
# user namespace from within an existing sandbox, so this script always fails
# in a nested context. It is read-only and stateless: it creates no files,
# changes nothing on the host, and can be re-run freely.
#
# Usage:
#   sh scripts/verify_gpu_sandbox.sh
#
# Exit status: 0 = every applicable check passed; 1 = a real failure.
# Absent GPU nodes are SKIPs, not failures (a host without an AMD/NVIDIA GPU
# is a valid configuration; the -try binds must simply be inert there).

set -u

pass=0
fail=0
skip=0

ok()   { pass=$((pass + 1)); printf 'PASS  %s\n' "$1"; }
bad()  { fail=$((fail + 1)); printf 'FAIL  %s\n' "$1"; }
skip() { skip=$((skip + 1)); printf 'SKIP  %s\n' "$1"; }

if ! command -v bwrap >/dev/null 2>&1; then
    echo "FATAL: bwrap not found in PATH" >&2
    exit 1
fi

# Minimal argv mirroring build_bwrap_plan's fixed prologue. The GPU binds
# under test are appended per-case; everything else stays identical so a
# failure localizes to the device binds.
BASE="--dev /dev --proc /proc --ro-bind /usr /usr --ro-bind /bin /bin \
--ro-bind /lib /lib --ro-bind /lib64 /lib64 --ro-bind-try /etc /etc"

# Nested-context guard: inside an existing e-agent/bwrap sandbox the kernel
# refuses a new user namespace and EVERY bwrap call fails, which this script
# would misreport as real failures. Probe with the full BASE argv (a bare
# `--dev /dev -- /bin/true` cannot exec anything: no /bin is bound). Detect
# a genuine namespace refusal and refuse up front.
# shellcheck disable=SC2086
if ! bwrap $BASE -- /bin/true 2>/dev/null; then
    echo "FATAL: cannot create a user namespace from here." >&2
    echo "This shell is itself sandboxed (or userns is disabled). Run this" >&2
    echo "script on the host, outside e-agent." >&2
    exit 1
fi

# --- Check 1: -try with a nonexistent source must be inert -----------------
# shellcheck disable=SC2086
if bwrap $BASE --dev-bind-try /nonexistent-e-agent-gpu-test /dev/nope \
    -- /bin/sh -c 'test ! -e /dev/nope' 2>/dev/null; then
    ok "missing-source --dev-bind-try is skipped and creates no node"
else
    bad "bwrap aborted or materialized a node for a nonexistent --dev-bind-try source"
fi

# --- Check 2: ordering — binds placed BEFORE --dev /dev must be hidden -----
# Guards the plan-builder invariant: GPU binds must come after `--dev /dev`
# in argv, or the fresh devtmpfs shadows them. Only meaningful with a real
# node to bind.
if [ -e /dev/kfd ]; then
    # shellcheck disable=SC2086
    if bwrap --dev-bind-try /dev/kfd /dev/kfd $BASE -- /bin/sh -c \
        'test ! -e /dev/kfd' 2>/dev/null; then
        ok "control: a bind placed before --dev /dev is shadowed (ordering matters)"
    else
        bad "control broken: pre---dev bind still visible; ordering assumption is wrong"
    fi
else
    skip "ordering control (no /dev/kfd on this host)"
fi

# --- Check 3: the gpu=true prologue exposes exactly the GPU nodes ----------
GPUARGS=""
[ -e /dev/kfd ] && GPUARGS="$GPUARGS --dev-bind-try /dev/kfd /dev/kfd"
[ -d /dev/dri ] && GPUARGS="$GPUARGS --dev-bind-try /dev/dri /dev/dri"
for n in /dev/nvidia*; do
    [ -e "$n" ] && GPUARGS="$GPUARGS --dev-bind-try $n $n"
done

if [ -z "$GPUARGS" ]; then
    skip "no GPU nodes found (/dev/kfd, /dev/dri, /dev/nvidia* all absent)"
else
    # shellcheck disable=SC2086
    bwrap $BASE $GPUARGS -- /bin/sh -c '
        rc=0
        for n in /dev/kfd /dev/dri/card* /dev/dri/renderD* /dev/nvidia*; do
            [ -e "$n" ] || continue
            if [ -c "$n" ] || [ -d "$n" ]; then
                echo "NODE  $n"
            else
                echo "BADTYPE  $n (not char device/dir)"; rc=1
            fi
        done
        # The actual open test: MS_NODEV leaks would fail here with ENXIO/EACCES.
        for n in /dev/kfd /dev/dri/renderD* /dev/nvidiactl; do
            [ -c "$n" ] || continue
            if (exec 3<"$n") 2>/dev/null; then
                echo "OPEN  $n"
            else
                echo "OPENFAIL  $n"; rc=1
            fi
        done
        exit "$rc"
    '
    if [ $? -eq 0 ]; then
        ok "gpu=true prologue exposes GPU nodes as openable char devices"
    else
        bad "nodes visible but wrong type or not openable inside the sandbox"
    fi
fi

# --- Check 4: gpu=false keeps the nodes out (default posture) --------------
if [ -e /dev/kfd ] || [ -d /dev/dri ]; then
    # shellcheck disable=SC2086
    if bwrap $BASE -- /bin/sh -c \
        'test ! -e /dev/kfd && test ! -e /dev/dri' 2>/dev/null; then
        ok "gpu=false (no GPU binds): /dev/kfd and /dev/dri absent inside"
    else
        bad "default sandbox leaks /dev/kfd or /dev/dri"
    fi
else
    skip "gpu=false posture (no AMD nodes on this host)"
fi

# --- Check 5: end-to-end compute smoke test, if tooling exists -------------
# Device open success is necessary but not sufficient for ROCm: topology
# enumeration may also need /sys (not mounted by the sandbox). Report, do
# not fail, so the output tells the maintainer whether a /sys follow-up is
# needed.
# shellcheck disable=SC2086
if [ -n "$GPUARGS" ] && command -v rocminfo >/dev/null 2>&1; then
    if bwrap $BASE $GPUARGS -- rocminfo >/dev/null 2>&1; then
        ok "rocminfo works inside the sandbox (no /sys follow-up needed)"
    else
        skip "rocminfo FAILS inside the sandbox despite openable nodes — likely needs /sys (e.g. /sys/class/kfd); report this output"
    fi
elif [ -n "$GPUARGS" ] && command -v nvidia-smi >/dev/null 2>&1; then
    if bwrap $BASE $GPUARGS -- nvidia-smi >/dev/null 2>&1; then
        ok "nvidia-smi works inside the sandbox"
    else
        skip "nvidia-smi FAILS inside the sandbox despite openable nodes — report this output"
    fi
else
    skip "end-to-end compute smoke test (no rocminfo/nvidia-smi in PATH, or no GPU)"
fi

echo
echo "result: $pass passed, $fail failed, $skip skipped"
[ "$fail" -eq 0 ]
