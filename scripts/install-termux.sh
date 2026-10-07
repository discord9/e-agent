#!/data/data/com.termux/files/usr/bin/bash
set -eu
REPO=discord9/e-agent
usage() { echo "Usage: bash install-termux.sh [--version TAG] [--force-shortcut]"; }
version=
force_shortcut=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --version) [ "$#" -ge 2 ] || { usage >&2; exit 2; }; version=$2; shift 2 ;;
    --force-shortcut) force_shortcut=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) usage >&2; exit 2 ;;
  esac
done
: "${PREFIX:?This installer requires Termux (PREFIX is unset)}"
case "$PREFIX" in /data/data/com.termux/files/usr|/data/user/0/com.termux/files/usr|*/data/data/com.termux/files/usr) ;; *) echo "Install inside Termux only (unexpected PREFIX: $PREFIX)" >&2; exit 1;; esac
arch=$(uname -m)
case "$arch" in aarch64|arm64) ;; *) echo "Unsupported architecture: $arch (ARM64/aarch64 required)" >&2; exit 1;; esac
for tool in curl tar sha256sum mktemp; do command -v "$tool" >/dev/null || { echo "Missing required command: $tool (install with: pkg install curl coreutils tar)" >&2; exit 1; }; done
if [ -z "$version" ]; then
  version=$(curl -fsSL -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest" | sed 's#.*/tag/##')
  [ -n "$version" ] || { echo 'Could not resolve latest release tag' >&2; exit 1; }
fi
case "$version" in *[!A-Za-z0-9._-]*|'') echo "Invalid release tag" >&2; exit 2;; esac
base="https://github.com/$REPO/releases/download/$version"
tmp=$(mktemp -d "${TMPDIR:-$PREFIX/tmp}/e-agent-install.XXXXXX")
trap 'rm -rf "$tmp"' EXIT HUP INT TERM
for file in e-agent-aarch64-linux-android.tar.gz SHA256SUMS termux-web.sh; do curl -fsSL "$base/$file" -o "$tmp/$file" || { echo "Download failed: $file ($version)" >&2; exit 1; }; done
for file in e-agent-aarch64-linux-android.tar.gz termux-web.sh; do
  expected=$(awk -v f="$file" '$2==f || $2=="*"f {print $1}' "$tmp/SHA256SUMS")
  [ "$(printf '%s\n' "$expected" | wc -l | tr -d ' ')" = 1 ] && [ "${#expected}" = 64 ] || { echo "Missing or ambiguous checksum for $file" >&2; exit 1; }
  actual=$(sha256sum "$tmp/$file" | awk '{print $1}')
  [ "$actual" = "$expected" ] || { echo "Checksum verification failed: $file" >&2; exit 1; }
done
# Accept exactly one top-level regular binary; reject links, directories and extra entries.
tar -tzf "$tmp/e-agent-aarch64-linux-android.tar.gz" > "$tmp/list"
[ "$(wc -l < "$tmp/list" | tr -d ' ')" = 1 ] && [ "$(cat "$tmp/list")" = e-agent ] || { echo 'Archive must contain only the regular top-level file e-agent' >&2; exit 1; }
[ "$(tar -tvzf "$tmp/e-agent-aarch64-linux-android.tar.gz" | awk 'NR==1 {print substr($1,1,1)}')" = - ] || { echo 'Archive entry must be a regular file' >&2; exit 1; }
shortcut_dir="$HOME/.shortcuts"
shortcut="$shortcut_dir/e-agent-web"
if [ -e "$shortcut" ] && [ "$force_shortcut" -ne 1 ]; then
  echo "Refusing to overwrite existing shortcut: $shortcut (use --force-shortcut to replace)" >&2
  exit 1
fi
mkdir -p "$PREFIX/bin"
tar -xOzf "$tmp/e-agent-aarch64-linux-android.tar.gz" e-agent > "$tmp/e-agent"
chmod 755 "$tmp/e-agent"
install_tmp="$PREFIX/bin/.e-agent.$$"
cp "$tmp/e-agent" "$install_tmp"
chmod 755 "$install_tmp"
mv -f "$install_tmp" "$PREFIX/bin/e-agent"
# Add conventional alias only if absent or already points at our installed binary.
if [ -e "$PREFIX/bin/agent" ] || [ -L "$PREFIX/bin/agent" ]; then
  [ -L "$PREFIX/bin/agent" ] && [ "$(readlink "$PREFIX/bin/agent")" = e-agent ] || echo "Note: leaving existing $PREFIX/bin/agent untouched"
else ln -s e-agent "$PREFIX/bin/agent"; fi
shortcut_dir="$HOME/.shortcuts"
shortcut="$shortcut_dir/e-agent-web"
mkdir -p "$shortcut_dir"
if [ -e "$shortcut" ] && [ "$force_shortcut" -ne 1 ]; then
  echo "Refusing to overwrite existing shortcut: $shortcut (use --force-shortcut to replace)" >&2
  exit 1
fi
sed "s|@PREFIX_BIN@|$PREFIX/bin|g" "$tmp/termux-web.sh" > "$tmp/shortcut"
chmod 700 "$tmp/shortcut"
mv -f "$tmp/shortcut" "$shortcut"
echo "Installed e-agent $version at $PREFIX/bin/e-agent"
echo "Widget shortcut: $shortcut"
echo 'Install Termux:Widget from the same source as Termux, add/refresh the e-agent-web shortcut, then open http://127.0.0.1:8766 and paste your existing token on first visit.'
echo 'Configure a provider in ~/.config/e-agent/config.toml before using model features.'
