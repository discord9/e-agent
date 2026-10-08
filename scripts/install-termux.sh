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
: "${HOME:?HOME is unset}"
case "$PREFIX" in /data/data/com.termux/files/usr|/data/user/0/com.termux/files/usr|*/data/data/com.termux/files/usr) ;; *) echo "Install inside Termux only (unexpected PREFIX: $PREFIX)" >&2; exit 1;; esac
arch=$(uname -m)
case "$arch" in aarch64|arm64) ;; *) echo "Unsupported architecture: $arch (ARM64/aarch64 required)" >&2; exit 1;; esac
for tool in curl tar sha256sum mktemp; do command -v "$tool" >/dev/null || { echo "Missing required command: $tool (install with: pkg install curl coreutils tar)" >&2; exit 1; }; done
if [ -z "$version" ]; then
  effective=$(curl -q -fsSL -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest") || { echo 'Could not resolve latest release tag' >&2; exit 1; }
  case "$effective" in "https://github.com/$REPO/releases/tag/"*) version=${effective#"https://github.com/$REPO/releases/tag/"};; *) echo "Latest-release redirect did not identify a tag: $effective" >&2; exit 1;; esac
fi
case "$version" in ''|.|..|*[!A-Za-z0-9._-]*|*//* ) echo "Invalid release tag" >&2; exit 2;; esac
base="https://github.com/$REPO/releases/download/$version"
tmp=$(mktemp -d "${TMPDIR:-$PREFIX/tmp}/e-agent-install.XXXXXX")
stage=
shortcut_stage=
cleanup() { [ -z "$stage" ] || rm -f "$stage"; [ -z "$shortcut_stage" ] || rm -f "$shortcut_stage"; rm -rf "$tmp"; }
trap cleanup EXIT
for file in e-agent-aarch64-linux-android.tar.gz SHA256SUMS termux-web.sh; do curl -q -fsSL "$base/$file" -o "$tmp/$file" || { echo "Download failed: $file ($version)" >&2; exit 1; }; done
for file in e-agent-aarch64-linux-android.tar.gz termux-web.sh; do
  expected=$(awk -v f="$file" '$2==f || $2=="*"f {print $1}' "$tmp/SHA256SUMS")
  [ "$(printf '%s\n' "$expected" | wc -l | tr -d ' ')" = 1 ] && [ "${#expected}" = 64 ] || { echo "Missing or ambiguous checksum for $file" >&2; exit 1; }
  actual=$(sha256sum "$tmp/$file" | awk '{print $1}')
  [ "$actual" = "$expected" ] || { echo "Checksum verification failed: $file" >&2; exit 1; }
done
tar -tzf "$tmp/e-agent-aarch64-linux-android.tar.gz" > "$tmp/list"
[ "$(wc -l < "$tmp/list" | tr -d ' ')" = 1 ] && [ "$(cat "$tmp/list")" = e-agent ] || { echo 'Archive must contain only the regular top-level file e-agent' >&2; exit 1; }
[ "$(tar -tvzf "$tmp/e-agent-aarch64-linux-android.tar.gz" | awk 'NR==1 {print substr($1,1,1)}')" = - ] || { echo 'Archive entry must be a regular file' >&2; exit 1; }
shortcut_dir="$HOME/.shortcuts"
shortcut="$shortcut_dir/e-agent-web"
[ ! -d "$shortcut" ] || { echo "Shortcut path is a directory: $shortcut" >&2; exit 1; }
sed "s|@PREFIX_BIN@|$PREFIX/bin|g" "$tmp/termux-web.sh" > "$tmp/shortcut"
preserve_shortcut=0
if [ -e "$shortcut" ] || [ -L "$shortcut" ]; then
  if [ "$force_shortcut" -ne 1 ] && ! cmp -s "$shortcut" "$tmp/shortcut"; then
    preserve_shortcut=1
  fi
fi
[ ! -d "$PREFIX/bin/e-agent" ] || { echo "Binary path is a directory: $PREFIX/bin/e-agent" >&2; exit 1; }
mkdir -p "$PREFIX/bin"
tar -xOzf "$tmp/e-agent-aarch64-linux-android.tar.gz" e-agent > "$tmp/e-agent"
chmod 755 "$tmp/e-agent"
stage=$(mktemp "$PREFIX/bin/.e-agent.XXXXXX")
cp "$tmp/e-agent" "$stage"
chmod 755 "$stage"
mv -fT "$stage" "$PREFIX/bin/e-agent"
stage=
if [ -e "$PREFIX/bin/e" ] || [ -L "$PREFIX/bin/e" ]; then
  [ -L "$PREFIX/bin/e" ] && [ "$(readlink "$PREFIX/bin/e")" = e-agent ] || echo "Note: leaving existing $PREFIX/bin/e untouched"
else ln -s e-agent "$PREFIX/bin/e"; fi
if [ "$preserve_shortcut" -eq 1 ]; then
  echo "Note: preserving edited Widget shortcut: $shortcut (use --force-shortcut to replace)"
else
  if [ ! -d "$shortcut_dir" ]; then mkdir -m 700 "$shortcut_dir"; fi
  sed "s|@PREFIX_BIN@|$PREFIX/bin|g" "$tmp/termux-web.sh" > "$tmp/shortcut"
  chmod 700 "$tmp/shortcut"
  shortcut_stage=$(mktemp "$shortcut_dir/.e-agent-web.XXXXXX")
  cp "$tmp/shortcut" "$shortcut_stage"
  chmod 700 "$shortcut_stage"
  mv -fT "$shortcut_stage" "$shortcut"
  shortcut_stage=
fi
echo "Installed e-agent $version at $PREFIX/bin/e-agent"
echo "Check the installed release with: $PREFIX/bin/e-agent --version"
echo 'An already-running Web server is not upgraded in place; stop your old server and restart it using the installed release binary or its shortcut.'
echo "Widget shortcut: $shortcut"
echo 'Install Termux:Widget from the same source as Termux, add/refresh the e-agent-web shortcut, then open http://127.0.0.1:8766 and paste your existing token on first visit.'
echo 'Configure a provider in ~/.config/e-agent/config.toml before using model features.'
