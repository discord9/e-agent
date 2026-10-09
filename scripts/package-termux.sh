#!/usr/bin/env bash
set -euo pipefail

usage() { echo "Usage: $0 ANDROID_BINARY OUTPUT_DIR ANDROID_NDK" >&2; }
if [[ $# -ne 3 ]]; then usage; exit 2; fi
binary=$1
out=$2
ndk=$3
readelf="$ndk/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-readelf"
clang="$ndk/toolchains/llvm/prebuilt/linux-x86_64/bin/clang"
props="$ndk/source.properties"
if [[ -f "$props" ]]; then
  revision=$(sed -n 's/^Pkg.Revision[[:space:]]*=[[:space:]]*//p' "$props")
  echo "Android NDK revision: ${revision:-unknown}"
fi
if [[ -x "$clang" ]]; then "$clang" --version | sed -n '1p'; fi
if command -v rustc >/dev/null 2>&1; then rustc --version; fi
if [[ ! -x "$readelf" ]]; then
  echo "Android NDK llvm-readelf not found or not executable: $readelf" >&2
  exit 1
fi
if [[ ! -f "$binary" || ! -x "$binary" ]]; then
  echo "Built binary must be an executable regular file: $binary" >&2
  exit 1
fi
header=$("$readelf" -h "$binary")
grep -q 'Machine:.*AArch64' <<<"$header" || { echo 'Binary is not an AArch64 ELF' >&2; exit 1; }
programs=$("$readelf" -l "$binary")
grep -q '/system/bin/linker64' <<<"$programs" || { echo 'Binary does not use Android ARM64 loader /system/bin/linker64' >&2; exit 1; }
needed=$("$readelf" -d "$binary" | sed -n 's/.*Shared library: \[\([^]]*\)\].*/\1/p')
while IFS= read -r lib; do
  [[ -z "$lib" ]] && continue
  case "$lib" in
    libc.so|libm.so|libdl.so|liblog.so|libandroid.so|libz.so) ;;
    *) echo "Unexpected non-base Android runtime dependency: $lib" >&2; exit 1 ;;
  esac
done <<<"$needed"

mkdir -p "$out"
archive="$out/e-agent-aarch64-linux-android.tar.gz"
rm -f "$out/e-agent"
tmp_binary="$out/.e-agent-package-tmp"
trap 'rm -f "$tmp_binary"' EXIT
install -m 755 "$binary" "$tmp_binary"
install -m 644 "$(dirname "$0")/install-termux.sh" "$out/install-termux.sh"
install -m 644 "$(dirname "$0")/termux-web.sh" "$out/termux-web.sh"
install -m 644 "$(dirname "$0")/termux-service.sh" "$out/termux-service.sh"
install -m 644 "$(dirname "$0")/termux-stop.sh" "$out/termux-stop.sh"
install -m 644 "$(dirname "$0")/termux-icons/e-agent-web.png" "$out/e-agent-web.png"
install -m 644 "$(dirname "$0")/termux-icons/e-agent-stop.png" "$out/e-agent-stop.png"
# Keep only the executable in the archive, under the installer-expected name.
tar --transform='s|^\.e-agent-package-tmp$|e-agent|' --owner=0 --group=0 --numeric-owner -czf "$archive" -C "$out" .e-agent-package-tmp
rm -f "$tmp_binary"
(cd "$out" && sha256sum e-agent-aarch64-linux-android.tar.gz install-termux.sh termux-web.sh termux-service.sh termux-stop.sh e-agent-web.png e-agent-stop.png > SHA256SUMS)
echo "Packaged Android ARM64 artifact in $out"
