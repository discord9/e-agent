#!/usr/bin/env bash
set -euo pipefail

usage() { echo "Usage: $0 ANDROID_BINARY OUTPUT_DIR ANDROID_NDK" >&2; }
if [[ $# -ne 3 ]]; then usage; exit 2; fi
binary=$1
out=$2
ndk=$3
readelf="$ndk/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-readelf"
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
    libc.so|libm.so|libdl.so|liblog.so|libandroid.so|libz.so|libcutils.so|libnativewindow.so|libjnigraphics.so) ;;
    *) echo "Unexpected non-base Android runtime dependency: $lib" >&2; exit 1 ;;
  esac
done <<<"$needed"

mkdir -p "$out"
rm -f "$out/e-agent" "$out/e-agent-aarch64-linux-android.tar.gz" "$out/install-termux.sh" "$out/termux-web.sh" "$out/SHA256SUMS"
install -m 755 "$binary" "$out/e-agent"
install -m 644 scripts/install-termux.sh "$out/install-termux.sh"
install -m 644 scripts/termux-web.sh "$out/termux-web.sh"
tar -C "$out" --owner=0 --group=0 --numeric-owner --mode=755 -czf "$out/e-agent-aarch64-linux-android.tar.gz" e-agent
(cd "$out" && sha256sum e-agent-aarch64-linux-android.tar.gz install-termux.sh termux-web.sh > SHA256SUMS)
echo "Packaged Android ARM64 artifact in $out"
