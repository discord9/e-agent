#!/bin/sh
# Host-side verification for `[sandbox] gpu = true`.
#
# This is a thin wrapper around the ignored acceptance test
# `tools::tests::gpu_acceptance_amd_rocm_sandbox`, which drives the REAL
# production path (bash_tool -> Bash::execute -> run_bash ->
# wrap_bash_command -> build_bwrap_plan) with a temporary config resolved by
# the real `Config` resolver. There is deliberately no second, hand-rolled
# bwrap argv here: a copy of the plan could drift from the builder and pass
# while the product is broken.
#
# The script always runs cargo in ITS OWN repository — the parent of the
# directory holding this script — never in the caller's current directory: a
# checkout that does not contain the test must FAIL, not exit 0 with zero
# matching tests.
#
# RUN ON THE HOST, not inside an e-agent sandbox: bwrap cannot create a new
# user namespace from within an existing sandbox, so the test fails there.
#
# Usage:
#   scripts/verify_gpu_sandbox.sh [--gpu=false]
#
# Environment (all optional here except the two runtime paths on an AMD host):
#   E_AGENT_GPU_ACCEPTANCE_PYTHON   host Python with ROCm-built PyTorch
#                                   (default: python3 from PATH)
#   E_AGENT_GPU_ACCEPTANCE_ROCMINFO host rocminfo binary
#                                   (default: rocminfo from PATH)
#   E_AGENT_GPU_ACCEPTANCE_READABLE extra `:`-separated read-only roots to
#                                   grant inside the sandbox (optional)
#   E_AGENT_GPU_ACCEPTANCE_WORKSPACE
#                                   workspace directory for the sandboxed
#                                   command (default: a fresh temp dir)
#   E_AGENT_GPU_ACCEPTANCE_ARCH     required GPU architecture, default gfx1201
#   E_AGENT_GPU_ACCEPTANCE_GPU      true (default) = GPU must work;
#                                   false = negative control: the same path
#                                   with `gpu = false` must expose no GPU
#
# Exit status: cargo's exit status when cargo fails; otherwise 0 only when the
# named test itself ran and passed. A host with the toolchain but a broken
# sandbox GPU path FAILS (no SKIP): the acceptance test requires rocminfo to
# enumerate the configured architecture, torch to see a HIP device of that
# architecture, an actual tensor computation to run and synchronize, and the
# result to match the CPU reference. The exact test identity line and its
# `test result: ok. 1 passed; 0 failed` summary are checked explicitly, so a
# checkout where the test does not exist cannot report success.
#
# Cargo's full output is captured to a temporary file and printed afterwards,
# so nothing is hidden on failure and cargo's exit status is never masked by a
# pipeline.

set -eu

test_name="tools::tests::gpu_acceptance_amd_rocm_sandbox"

for arg in "$@"; do
    case "$arg" in
        --gpu=false) export E_AGENT_GPU_ACCEPTANCE_GPU=false ;;
        --gpu=true) export E_AGENT_GPU_ACCEPTANCE_GPU=true ;;
        -h|--help)
            # The whole comment header (everything before `set -eu`).
            sed -n '2,/^set -eu$/p' "$0" | sed '$d'
            exit 0
            ;;
        *)
            echo "unknown argument: $arg (see --help)" >&2
            exit 2
            ;;
    esac
done

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(dirname -- "$script_dir")

if [ -z "${E_AGENT_GPU_ACCEPTANCE_PYTHON:-}" ]; then
    E_AGENT_GPU_ACCEPTANCE_PYTHON="$(command -v python3 || true)"
fi
if [ -z "${E_AGENT_GPU_ACCEPTANCE_ROCMINFO:-}" ]; then
    E_AGENT_GPU_ACCEPTANCE_ROCMINFO="$(command -v rocminfo || true)"
fi
export E_AGENT_GPU_ACCEPTANCE_PYTHON E_AGENT_GPU_ACCEPTANCE_ROCMINFO
if [ -z "$E_AGENT_GPU_ACCEPTANCE_PYTHON" ] || [ -z "$E_AGENT_GPU_ACCEPTANCE_ROCMINFO" ]; then
    echo "FATAL: set E_AGENT_GPU_ACCEPTANCE_PYTHON and E_AGENT_GPU_ACCEPTANCE_ROCMINFO" >&2
    echo "(or have python3 and rocminfo on PATH)" >&2
    exit 1
fi

output=$(mktemp "${TMPDIR:-/tmp}/e-agent-gpu-acceptance.XXXXXX") || exit 1
trap 'rm -f "$output"' EXIT HUP INT TERM

# Run from the repository that owns this script. `$?` on the `||` arm is
# cargo's exact exit status: no pipe, no tee.
status=0
(cd "$repo_root" && cargo test --lib -- --ignored --nocapture --exact "$test_name") \
    >"$output" 2>&1 || status=$?
cat "$output"

if [ "$status" -ne 0 ]; then
    exit "$status"
fi

# cargo exits 0 with "0 passed; 0 filtered out" when --exact matches nothing,
# so a successful exit alone is not evidence the acceptance test ran. Require
# the harness summary for the exact selector. With --nocapture, single-thread
# libtest output can split the test name and its final `ok` across lines.
if ! grep -q "^test result: ok[.] 1 passed; 0 failed;" "$output"; then
    echo "FATAL: unexpected test result summary for $test_name in $repo_root" >&2
    exit 1
fi

exit 0
