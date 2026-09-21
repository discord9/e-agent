#!/bin/sh
# Host-side verification for `[sandbox] gpu = true`.
#
# This is a thin wrapper around the ignored acceptance test
# `tools::tests::gpu_acceptance_amd_rocm_sandbox`, which drives the REAL
# production path (bash_tool -> Bash::execute -> run_bash ->
# wrap_bwrap_command -> build_bwrap_plan) with a temporary config resolved by
# the real `Config` resolver. There is deliberately no second, hand-rolled
# bwrap argv here: a copy of the plan could drift from the builder and pass
# while the product is broken.
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
# Exit status: the cargo test exit status. A host with the toolchain but a
# broken sandbox GPU path FAILS (no SKIP): the acceptance test requires
# rocminfo to enumerate the configured architecture, torch to see a HIP
# device of that architecture, an actual tensor computation to run and
# synchronize, and the result to match the CPU reference.

set -eu

for arg in "$@"; do
    case "$arg" in
        --gpu=false) export E_AGENT_GPU_ACCEPTANCE_GPU=false ;;
        --gpu=true) export E_AGENT_GPU_ACCEPTANCE_GPU=true ;;
        -h|--help)
            sed -n '2,40p' "$0"
            exit 0
            ;;
        *)
            echo "unknown argument: $arg (see --help)" >&2
            exit 2
            ;;
    esac
done

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

exec cargo test --lib -- --ignored --nocapture --exact \
    tools::tests::gpu_acceptance_amd_rocm_sandbox
