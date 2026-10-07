#!/usr/bin/env python3
"""Focused packaging smoke test using a deterministic fake NDK readelf."""
import hashlib
import os
import pathlib
import subprocess
import tarfile
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[1]
PACKAGER = ROOT / "scripts/package-termux.sh"

with tempfile.TemporaryDirectory(prefix="termux-package-test-") as tmp:
    root = pathlib.Path(tmp)
    ndk = root / "ndk"
    bin_dir = ndk / "toolchains/llvm/prebuilt/linux-x86_64/bin"
    bin_dir.mkdir(parents=True)
    readelf = bin_dir / "llvm-readelf"
    readelf.write_text(
        "#!/usr/bin/env bash\n"
        "case \"$1\" in\n"
        "  -h) echo 'Machine: AArch64' ;;\n"
        "  -l) echo '/system/bin/linker64' ;;\n"
        "  -d) echo 'Shared library: [libc.so]' ;;\n"
        "esac\n"
    )
    readelf.chmod(0o755)
    binary = root / "e-agent"
    binary.write_bytes(b"fixture executable")
    binary.chmod(0o755)
    output = root / "dist"
    subprocess.run([str(PACKAGER), str(binary), str(output), str(ndk)], check=True)

    archive = output / "e-agent-aarch64-linux-android.tar.gz"
    with tarfile.open(archive, "r:gz") as package:
        assert package.getnames() == ["e-agent"]
        assert package.extractfile("e-agent").read() == b"fixture executable"
    checksums = {}
    for line in (output / "SHA256SUMS").read_text().splitlines():
        digest, name = line.split("  ", 1)
        checksums[name] = digest
    assert set(checksums) == {
        "e-agent-aarch64-linux-android.tar.gz", "install-termux.sh", "termux-web.sh"
    }
    for name, digest in checksums.items():
        assert hashlib.sha256((output / name).read_bytes()).hexdigest() == digest
