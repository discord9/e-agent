#!/usr/bin/env python3
"""Packaging validation tests with deterministic fake NDK llvm-readelf output."""
import hashlib
import pathlib
import subprocess
import tarfile
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[1]
PACKAGER = ROOT / "scripts/package-termux.sh"
EXPECTED = {
    "e-agent-aarch64-linux-android.tar.gz", "install-termux.sh", "termux-web.sh", "SHA256SUMS"
}

with tempfile.TemporaryDirectory(prefix="termux-package-test-") as tmp:
    root = pathlib.Path(tmp)
    ndk = root / "ndk"
    bin_dir = ndk / "toolchains/llvm/prebuilt/linux-x86_64/bin"
    bin_dir.mkdir(parents=True)
    (ndk / "source.properties").write_text("Pkg.Revision = 28.0.13004108\n")
    readelf = bin_dir / "llvm-readelf"
    fixture = root / "fixture"
    fixture.write_text("valid")
    readelf.write_text(
        "#!/usr/bin/env bash\n"
        "case \"$1\" in\n"
        "  -h) printf 'Machine: %s\\n' \"$(cat '" + str(fixture) + "')\" ;;\n"
        "  -l) printf '%s\\n' \"$(sed -n '2p' '" + str(fixture) + "')\" ;;\n"
        "  -d) printf '%s\\n' \"$(sed -n '3,$p' '" + str(fixture) + "')\" ;;\n"
        "esac\n"
    )
    readelf.chmod(0o755)
    binary = root / "e-agent"
    binary.write_bytes(b"fixture executable")
    binary.chmod(0o755)
    output = root / "dist"

    def run_case(name, contents, succeeds):
        fixture.write_text(contents)
        if output.exists():
            for item in output.iterdir():
                item.unlink()
        result = subprocess.run(
            [str(PACKAGER), str(binary), str(output), str(ndk)],
            text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
        assert (result.returncode == 0) is succeeds, (name, result.stdout, result.stderr)
        if succeeds:
            assert {item.name for item in output.iterdir()} == EXPECTED
            archive = output / "e-agent-aarch64-linux-android.tar.gz"
            with tarfile.open(archive, "r:gz") as package:
                assert package.getnames() == ["e-agent"]
                assert package.extractfile("e-agent").read() == b"fixture executable"
            checksums = {}
            for line in (output / "SHA256SUMS").read_text().splitlines():
                digest, filename = line.split("  ", 1)
                checksums[filename] = digest
            assert set(checksums) == EXPECTED - {"SHA256SUMS"}
            for filename, digest in checksums.items():
                assert hashlib.sha256((output / filename).read_bytes()).hexdigest() == digest
        else:
            assert not output.exists() or not list(output.iterdir()), (name, list(output.iterdir()))

    run_case("valid", "AArch64\n/system/bin/linker64\nShared library: [libc.so]\n", True)
    run_case("wrong architecture", "x86-64\n/system/bin/linker64\n", False)
    run_case("wrong loader", "AArch64\n/lib/ld-linux-aarch64.so.1\n", False)
    run_case("unsupported runtime", "AArch64\n/system/bin/linker64\nShared library: [libc++.so]\n", False)
print("4 package cases passed (1 valid fixture, 3 rejected fixtures)")
