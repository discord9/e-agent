#!/usr/bin/env python3
"""Installer acceptance fixtures; launcher execution is verified separately."""
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
        service_helper = (ROOT / "termux-service.sh").read_bytes()
        (self.root / "termux-service.sh").write_bytes(service_helper)
        stop_launcher = (ROOT / "termux-stop.sh").read_bytes()
        (self.root / "termux-stop.sh").write_bytes(stop_launcher)
        sums = "".join(hashlib.sha256((self.root / f).read_bytes()).hexdigest() + "  " + f + "\n"
                       for f in (archive.name, "termux-web.sh", "termux-service.sh", "termux-stop.sh"))
        (self.root / "SHA256SUMS").write_text(sums)
        self.releases = self.root / "releases"
        for tag, payload in (("vtest-old", self.bin_payload), ("vtest-new", b"new release payload\n")):
            release = self.releases / tag
            release.mkdir(parents=True)
            with tarfile.open(release / archive.name, "w:gz") as tf:
                import io
                info = tarfile.TarInfo("e-agent")
                info.mode = 0o755
                info.size = len(payload)
                tf.addfile(info, io.BytesIO(payload))
            (release / "termux-web.sh").write_bytes(launcher)
            (release / "termux-service.sh").write_bytes(service_helper)
            (release / "termux-stop.sh").write_bytes(stop_launcher)
            release_sums = "".join(hashlib.sha256((release / f).read_bytes()).hexdigest() + "  " + f + "\n"
                                   for f in (archive.name, "termux-web.sh", "termux-service.sh", "termux-stop.sh"))
            (release / "SHA256SUMS").write_text(release_sums)
        (self.fake / "curl").write_text('''#!/usr/bin/env python3
import os,sys,shutil
args=sys.argv[1:]
urls=[a for a in args if a.startswith('https://')]
with open(os.environ['CURL_LOG'],'a') as f: f.write(repr(args)+' proxy='+os.environ.get('HTTPS_PROXY','')+'\\n')
if '--noproxy' in args:
 if os.environ.get('PORT_OCCUPIED'): sys.exit(0)
 sys.exit(7)
if len(urls)!=1: sys.exit('curl fake: expected exactly one URL, got '+repr(urls))
url=urls[0]
if url!='https://github.com/discord9/e-agent/releases/latest' and not any(url.startswith('https://github.com/discord9/e-agent/releases/download/'+tag+'/') for tag in ('vtest','vtest-old','vtest-new')):
 sys.exit('curl fake: unexpected URL '+url)
if url.endswith('/latest'):
 if os.environ.get('LATEST_FAIL'): sys.exit('simulated latest redirect failure')
 if '-w' in args: print(os.environ.get('LATEST_URL','https://github.com/discord9/e-agent/releases/tag/vtest'))
 sys.exit(0)
name=url.rsplit('/',1)[-1]
if name not in ('e-agent-aarch64-linux-android.tar.gz','SHA256SUMS','termux-web.sh','termux-service.sh','termux-stop.sh'): sys.exit('curl fake: unexpected asset '+name)
out=args[args.index('-o')+1] if '-o' in args else None
if not out: sys.exit('curl fake: expected download output path')
if os.environ.get('DOWNLOAD_FAIL')==name: sys.exit('simulated download failure '+name)
tag=url.split('/download/',1)[1].split('/',1)[0]
source=os.path.join(os.environ['ASSETS'],'releases',tag,name)
if not os.path.exists(source):
 if tag!='vtest': sys.exit('missing fixture asset '+tag+'/'+name)
 source=os.path.join(os.environ['ASSETS'],name)
shutil.copyfile(source,out)
''')
        (self.fake / "curl").chmod(0o755)
        (self.fake / "uname").write_text("#!/bin/sh\necho aarch64\n")
        (self.fake / "uname").chmod(0o755)
        for name in ("sv", "svlogd", "pkg", "am"):
            (self.fake / name).write_text("#!/bin/sh\necho \"$0 $*\" >> \"$SERVICE_LOG\"\n")
            (self.fake / name).chmod(0o755)
        (self.fake / "service-daemon").write_text("#!/bin/sh\nmkdir -p \"$SVDIR/e-agent-web/supervise\"\n[ -p \"$SVDIR/e-agent-web/supervise/ok\" ] || mkfifo \"$SVDIR/e-agent-web/supervise/ok\"\n")
        (self.fake / "service-daemon").chmod(0o755)
        env = os.environ.copy()
        env.update(PREFIX=str(self.prefix), HOME=str(self.home), ASSETS=str(self.root), CURL_LOG=str(self.root / "curl.log"), SERVICE_LOG=str(self.root / "service.log"), HTTPS_PROXY="http://proxy.fixture:8123", PATH=str(self.fake)+os.pathsep+env["PATH"])
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
        self.assertIn("force-stop", (self.home / ".shortcuts/e-agent-stop").read_text())

    def test_latest_resolves_once_and_downloads_pinned_assets(self):
        env = self.env.copy(); env["LATEST_URL"] = "https://github.com/discord9/e-agent/releases/tag/vtest-new"
        result = subprocess.run(["bash", str(ROOT / "install-termux.sh")], env=env, text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.prefix / "bin/e-agent").read_bytes(), b"new release payload\n")
        self.assertEqual(list(self.prefix.glob("bin/.e-agent.*")), [])
        calls = (self.root / "curl.log").read_text().splitlines()
        self.assertEqual(sum("releases/latest" in line for line in calls), 1)
        self.assertEqual(sum("/download/vtest-new/" in line for line in calls), 5)
        self.assertFalse(any("/download/vtest/" in line for line in calls))
        self.assertIn("proxy=http://proxy.fixture:8123", "\\n".join(calls))

    def test_repeat_install_upgrades_binary_and_preserves_user_state(self):
        result = self.run_installer("--version", "vtest-old")
        self.assertEqual(result.returncode, 0, result.stderr)
        alias = self.prefix / "bin/e"
        alias.unlink(); alias.write_bytes(b"Cargo-installed alias")
        state = {
            self.home / ".config/e-agent/config.toml": b"user config",
            self.home / ".config/e-agent/credentials": b"credentials",
            self.home / "e-agent-workspace/.e-agent/sessions/fixture.jsonl": b"workspace",
            self.home / ".local/state/e-agent/server.token": b"token",
            self.home / ".cargo/bin/e-agent": b"Cargo binary sentinel",
            alias: b"Cargo-installed alias",
        }
        for path, content in state.items():
            path.parent.mkdir(parents=True, exist_ok=True); path.write_bytes(content)
        cargo_binary = self.home / ".cargo/bin/e-agent"
        cargo_target = self.home / ".cargo/bin/cargo-target-sentinel"
        cargo_target.write_bytes(b"Cargo executable target")
        cargo_binary.unlink(); cargo_binary.symlink_to(cargo_target)
        alias.unlink(); alias.symlink_to(cargo_binary)
        state[cargo_target] = b"Cargo executable target"
        state[cargo_binary] = b"Cargo executable target"
        state[alias] = b"Cargo executable target"
        env = self.env.copy(); env["LATEST_URL"] = "https://github.com/discord9/e-agent/releases/tag/vtest-new"
        result = subprocess.run(["bash", str(ROOT / "install-termux.sh")], env=env, text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.prefix / "bin/e-agent").read_bytes(), b"new release payload\n")
        self.assertIn("leaving existing", result.stdout)
        self.assertEqual((self.prefix / "bin/e-agent").is_symlink(), False)
        for path, content in state.items(): self.assertEqual(path.read_bytes(), content)

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
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("preserving edited Widget shortcut", result.stdout)
        self.assertEqual(shortcut.read_text(), "user edit")
        result = self.run_installer("--version", "vtest", "--force-shortcut")
        self.assertEqual(result.returncode, 0, result.stderr)
        shortcut.unlink(); shortcut.symlink_to(self.home / "missing")
        result = self.run_installer("--version", "vtest")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("preserving edited Widget shortcut", result.stdout)
        self.assertTrue(shortcut.is_symlink())
        shortcut.unlink(); shortcut.mkdir()
        result = self.run_installer("--version", "vtest", "--force-shortcut")
        self.assertNotEqual(result.returncode, 0); self.assertTrue(shortcut.is_dir())

    def test_edited_shortcut_is_preserved_while_binary_upgrades_or_force_replaced(self):
        result = self.run_installer("--version", "vtest-old")
        self.assertEqual(result.returncode, 0, result.stderr)
        shortcut = self.home / ".shortcuts/e-agent-web"
        shortcut.write_bytes(b"custom launcher")
        env = self.env.copy(); env["LATEST_URL"] = "https://github.com/discord9/e-agent/releases/tag/vtest-new"
        result = subprocess.run(["bash", str(ROOT / "install-termux.sh")], env=env, text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("preserving edited Widget shortcut", result.stdout)
        self.assertEqual(shortcut.read_bytes(), b"custom launcher")
        self.assertEqual((self.prefix / "bin/e-agent").read_bytes(), b"new release payload\n")
        result = subprocess.run(["bash", str(ROOT / "install-termux.sh"), "--force-shortcut"], env=env, text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotEqual(shortcut.read_bytes(), b"custom launcher")
        self.assertIn(str(self.prefix / "bin/e-agent"), shortcut.read_text())

    def test_shortcut_directory_preflight_does_not_replace_binary(self):
        target = self.prefix / "bin/e-agent"
        target.parent.mkdir(parents=True, exist_ok=True); target.write_bytes(b"existing binary")
        shortcut = self.home / ".shortcuts/e-agent-web"
        shortcut.mkdir(parents=True)
        result = self.run_installer("--version", "vtest-new")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Shortcut path is a directory", result.stderr)
        self.assertEqual(target.read_bytes(), b"existing binary")

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
        names=("e-agent-aarch64-linux-android.tar.gz", "termux-web.sh", "termux-service.sh", "termux-stop.sh")
        (self.root / "SHA256SUMS").write_text("".join(hashlib.sha256((self.root/n).read_bytes()).hexdigest()+"  "+n+"\n" for n in names))

    def test_service_log_install_upgrade_and_user_edits(self):
        result = self.run_installer("--version", "vtest")
        self.assertEqual(result.returncode, 0, result.stderr)
        service = self.prefix / "var/service/e-agent-web"
        logdir = self.prefix / "var/log/sv/e-agent-web"
        self.assertIn('exec 2>&1', (service / "run").read_text())
        self.assertIn('exec "$PREFIX/bin/e-agent" web', (service / "run").read_text())
        self.assertIn('svlogd', (service / "log/run").read_text())
        self.assertEqual((logdir / "config").read_text(), "s1048576\nn10\nt86400\n")
        self.assertFalse((service / "down").exists())
        self.assertIn('intent.action.CREATE_SHORTCUT', (self.root / "service.log").read_text())
        (service / "run").write_text("custom run\n")
        (logdir / "config").write_text("s2048\nn3\n")
        (service / "down").touch()
        result = self.run_installer("--version", "vtest")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((service / "run").read_text(), "custom run\n")
        self.assertEqual((logdir / "config").read_text(), "s2048\nn3\n")
        self.assertTrue((service / "down").exists())

    def test_occupied_port_leaves_new_service_disabled(self):
        self.env["PORT_OCCUPIED"] = "1"
        result = self.run_installer("--version", "vtest")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((self.prefix / "var/service/e-agent-web/down").exists())
        self.assertIn("left down", result.stdout)

    def test_bad_service_helper_checksum_does_not_replace_binary(self):
        target = self.prefix / "bin/e-agent"
        target.parent.mkdir(parents=True)
        target.write_bytes(b"old binary")
        (self.root / "termux-service.sh").write_bytes(b"corrupt")
        result = self.run_installer("--version", "vtest")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Checksum verification failed: termux-service.sh", result.stderr)
        self.assertEqual(target.read_bytes(), b"old binary")

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
