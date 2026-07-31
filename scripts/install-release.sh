#!/usr/bin/env bash
set -euo pipefail

root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

cargo install --path . --locked --features greptime --force

# Optional convenience: symlink `e` → e-agent so `e web -p 8123` works.
# Only installs when ~/.local/bin exists and is on PATH; prints a hint
# otherwise so the user can decide (alias in .bashrc/.zshrc also works).
if command -v e-agent >/dev/null 2>&1; then
  bin_dir="${HOME}/.local/bin"
  if [ -d "${bin_dir}" ] && case ":${PATH}:" in *":${bin_dir}:"*) true;; *) false;; esac; then
    e_agent_path="$(command -v e-agent)"
    ln -sf "${e_agent_path}" "${bin_dir}/e"
    echo "linked ${bin_dir}/e -> ${e_agent_path}"
  else
    echo "hint: add 'alias e=e-agent' to your shell rc, or link \${HOME}/.local/bin/e"
  fi
fi
