#!/usr/bin/env python3
"""Focused acceptance tests for the Termux installer and Widget launcher."""
import hashlib
import os
import pathlib
import subprocess
import tarfile
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parent


class TermuxInstallTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="termux-test-")
        self.addCleanup(self.tmp.cleanup)
        self.root = pathlib.Path(self.tmp.name)
        self.prefix = self.root / "data/data/com.termux/files/usr"
        self.home = self.root / "home"
        self.prefix.mkdir(parents=True)
        (self.prefix / "tmp").mkdir()
        self.home.mkdir()
        self.fake = self.root / "fake"
        self.fake.mkdir()
        self.bin_payload = b"#!/bin/sh\n[ \"$1\" = web ] && exec sleep 60\nexit 0\n"
        archive = self.root / "e-agent-aarch64-linux-android.tar.gz"
        with tarfile.open(archive, "w:gz") as tf:
            import io
            info = tarfile.TarInfo("e-agent")
            info.mode = 0o755
            info.size = len(self.bin_payload)
            tf.addfile(info, io.BytesIO(self.bin_payload))
        launcher = (ROOT / "termux-web.sh").read_bytes()
        (self.root / "termux-web.sh").write_bytes(launcher)
        sums = "".join(hashlib.sha256((self.root / f).read_bytes()).hexdigest() + "  " + f + "\n"
                       for f in (archive.name, "termux-web.sh"))
        (self.root / "SHA256SUMS").write_text(sums)
        (self.fake / "curl").write_text('''#!/usr/bin/env python3
import os,sys,shutil
args=sys.argv[1:]
if '-w' in args:
 print('https://github.com/discord9/e-agent/releases/tag/vtest'); sys.exit(0)
url=args[-1]
name=url.rsplit('/',1)[-1]
src=os.path.join(os.environ['ASSETS'],name)
out=args[args.index('-o')+1] if '-o' in args else None
if out: shutil.copyfile(src,out)
else: sys.exit(0)
''')
        (self.fake / "curl").chmod(0o755)
        (self.fake / "uname").write_text("#!/bin/sh\necho aarch64\n")
        (self.fake / "uname").chmod(0o755)
        env = os.environ.copy()
        env.update(PREFIX=str(self.prefix), HOME=str(self.home), ASSETS=str(self.root), PATH=str(self.fake)+os.pathsep+env["PATH"])
        self.env = env

    def run_installer(self, *args):
        return subprocess.run(["bash", str(ROOT / "install-termux.sh"), *args], env=self.env,
                              text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=20)

    def test_install_and_preserve_edited_shortcut(self):
        shortcut = self.home / ".shortcuts" / "e-agent-web"
        shortcut.parent.mkdir()
        shortcut.write_text("my edit")
        result = self.run_installer("--version", "vtest")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.prefix / "bin/e-agent").exists())
        self.assertEqual(shortcut.read_text(), "my edit")

    def test_installs_binary_and_expanded_widget_shortcut(self):
        result = self.run_installer("--version", "vtest")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.prefix / "bin/e-agent").read_bytes(), self.bin_payload)
        shortcut = (self.home / ".shortcuts/e-agent-web").read_text()
        self.assertIn(str(self.prefix / "bin") + "/e-agent", shortcut)
        self.assertTrue((self.prefix / "bin/agent").is_symlink())

    def test_bad_checksum_never_installs(self):
        (self.root / "SHA256SUMS").write_text("0" * 64 + "  termux-web.sh\n")
        result = self.run_installer("--version", "vtest")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.prefix / "bin/e-agent").exists())

    def test_rejects_non_termux_and_non_arm(self):
        env = self.env.copy(); env.pop("PREFIX")
        result = subprocess.run(["bash", str(ROOT / "install-termux.sh")], env=env, text=True, capture_output=True)
        self.assertNotEqual(result.returncode, 0)
        env = self.env.copy(); env["PATH"] = str(self.fake) + os.pathsep + os.environ["PATH"]
        (self.fake / "uname").write_text("#!/bin/sh\necho x86_64\n"); (self.fake / "uname").chmod(0o755)
        result = subprocess.run(["bash", str(ROOT / "install-termux.sh"), "--version", "vtest"], env=env, text=True, capture_output=True)
        self.assertNotEqual(result.returncode, 0)


if __name__ == "__main__":
    unittest.main()
