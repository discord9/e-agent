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
urls=[a for a in args if a.startswith('https://')]
with open(os.environ['CURL_LOG'],'a') as f: f.write(repr(args)+' proxy='+os.environ.get('HTTPS_PROXY','')+'\\n')
if '--noproxy' in args: sys.exit('curl fake: installer must preserve external proxy settings')
if len(urls)!=1: sys.exit('curl fake: expected exactly one URL, got '+repr(urls))
url=urls[0]
if url!='https://github.com/discord9/e-agent/releases/latest' and not url.startswith('https://github.com/discord9/e-agent/releases/download/vtest/'):
 sys.exit('curl fake: unexpected URL '+url)
if url.endswith('/latest'):
 if os.environ.get('LATEST_FAIL'): sys.exit('simulated latest redirect failure')
 if '-w' in args: print(os.environ.get('LATEST_URL','https://github.com/discord9/e-agent/releases/tag/vtest'))
 sys.exit(0)
name=url.rsplit('/',1)[-1]
if name not in ('e-agent-aarch64-linux-android.tar.gz','SHA256SUMS','termux-web.sh'): sys.exit('curl fake: unexpected asset '+name)
out=args[args.index('-o')+1] if '-o' in args else None
if not out: sys.exit('curl fake: expected download output path')
if os.environ.get('DOWNLOAD_FAIL')==name: sys.exit('simulated download failure '+name)
shutil.copyfile(os.path.join(os.environ['ASSETS'],name),out)
''')
        (self.fake / "curl").chmod(0o755)
        (self.fake / "uname").write_text("#!/bin/sh\necho aarch64\n")
        (self.fake / "uname").chmod(0o755)
        env = os.environ.copy()
        env.update(PREFIX=str(self.prefix), HOME=str(self.home), ASSETS=str(self.root), CURL_LOG=str(self.root / "curl.log"), HTTPS_PROXY="http://proxy.fixture:8123", PATH=str(self.fake)+os.pathsep+env["PATH"])
        self.env = env

    def run_installer(self, *args):
        return subprocess.run(["bash", str(ROOT / "install-termux.sh"), *args], env=self.env,
                              text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=20)

    def test_installs_binary_and_expanded_widget_shortcut(self):
        result = self.run_installer("--version", "vtest")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.prefix / "bin/e-agent").read_bytes(), self.bin_payload)
        shortcut = (self.home / ".shortcuts/e-agent-web").read_text()
        self.assertIn(str(self.prefix / "bin") + "/e-agent", shortcut)
        self.assertTrue((self.prefix / "bin/e").is_symlink())

    def test_latest_resolves_once_and_downloads_pinned_assets(self):
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)
        log = list(self.prefix.glob("bin/.e-agent.*"))
        self.assertEqual(log, [])
        self.assertTrue((self.prefix / "bin/e-agent").exists())
        log = (self.root / "curl.log").read_text()
        self.assertIn("proxy=http://proxy.fixture:8123", log)

    def test_download_failure_and_bad_checksums_leave_existing_binary_intact(self):
        target = self.prefix / "bin/e-agent"
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(b"old binary")
        env = self.env.copy(); env["DOWNLOAD_FAIL"] = "termux-web.sh"
        result = subprocess.run(["bash", str(ROOT / "install-termux.sh"), "--version", "vtest"], env=env, text=True, capture_output=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(target.read_bytes(), b"old binary")
        (self.root / "SHA256SUMS").write_text("0" * 64 + "  termux-web.sh\n")
        result = self.run_installer("--version", "vtest")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(target.read_bytes(), b"old binary")

    def test_shortcut_idempotence_edits_dangling_symlinks_and_directories(self):
        result = self.run_installer("--version", "vtest"); self.assertEqual(result.returncode, 0, result.stderr)
        shortcut = self.home / ".shortcuts/e-agent-web"
        result = self.run_installer("--version", "vtest"); self.assertEqual(result.returncode, 0, result.stderr)
        shortcut.write_text("user edit")
        result = self.run_installer("--version", "vtest")
        self.assertNotEqual(result.returncode, 0); self.assertEqual(shortcut.read_text(), "user edit")
        result = self.run_installer("--version", "vtest", "--force-shortcut")
        self.assertEqual(result.returncode, 0, result.stderr)
        shortcut.unlink(); shortcut.symlink_to(self.home / "missing")
        result = self.run_installer("--version", "vtest")
        self.assertNotEqual(result.returncode, 0); self.assertTrue(shortcut.is_symlink())
        shortcut.unlink(); shortcut.mkdir()
        result = self.run_installer("--version", "vtest", "--force-shortcut")
        self.assertNotEqual(result.returncode, 0); self.assertTrue(shortcut.is_dir())

    def test_archive_extra_symlink_and_alias_are_rejected_or_preserved(self):
        alias = self.prefix / "bin/e"
        alias.parent.mkdir(parents=True, exist_ok=True)
        alias.write_text("user agent")
        result = self.run_installer("--version", "vtest")
        self.assertEqual(result.returncode, 0, result.stderr); self.assertEqual(alias.read_text(), "user agent")
        archive = self.root / "e-agent-aarch64-linux-android.tar.gz"
        with tarfile.open(archive, "w:gz") as tf:
            import io
            for name, kind in (("e-agent", tarfile.REGTYPE), ("../escape", tarfile.REGTYPE)):
                info = tarfile.TarInfo(name); info.type=kind; info.size=len(self.bin_payload)
                tf.addfile(info, io.BytesIO(self.bin_payload))
        self.refresh_sums()
        result = self.run_installer("--version", "vtest"); self.assertNotEqual(result.returncode, 0)
        with tarfile.open(archive, "w:gz") as tf:
            info=tarfile.TarInfo("e-agent"); info.type=tarfile.SYMTYPE; info.linkname="/tmp/x"; tf.addfile(info)
        self.refresh_sums()
        result = self.run_installer("--version", "vtest"); self.assertNotEqual(result.returncode, 0)

    def refresh_sums(self):
        names=("e-agent-aarch64-linux-android.tar.gz", "termux-web.sh")
        (self.root / "SHA256SUMS").write_text("".join(hashlib.sha256((self.root/n).read_bytes()).hexdigest()+"  "+n+"\n" for n in names))

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
