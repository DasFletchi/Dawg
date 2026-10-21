#!/usr/bin/env python3
"""Isolated install/uninstall tests. No real HOME, service or router is touched."""
from __future__ import annotations

import fcntl
import http.client
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import unittest

SCRIPTS = Path(__file__).resolve().parent
REAL_BINARY_DIR = None


class InstallationTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="dawg-installer-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.home = self.root / "home with space %"
        self.home.mkdir()
        self.prefix = self.home / ".local"
        self.data = self.home / "data/dawg"
        self.config = self.home / "config/dawg"
        self.runtime = self.home / "runtime/dawg"
        self.source = self.root / "release"
        self.source.mkdir()
        shutil.copyfile(shutil.which("sleep"), self.source / "dawg-daemon")
        shutil.copyfile(shutil.which("true"), self.source / "dawg-ui")
        for path in self.source.iterdir():
            path.chmod(0o700)
        self.tools = self.root / "tools"
        self.tools.mkdir()
        fake_service = self.tools / "systemctl"
        fake_service.write_text(f"#!{sys.executable}\nimport os,sys\nwith open(os.environ['FAKE_SERVICE_LOG'], 'a') as f: f.write(' '.join(sys.argv[1:])+'\\n')\n")
        fake_service.chmod(0o700)
        self.environment = dict(os.environ)
        # The development environment exports DAWG_*; never inherit those paths.
        for key in list(self.environment):
            if key.startswith("DAWG_") or key.startswith("XDG_"):
                del self.environment[key]
        self.environment.update({"HOME": str(self.home), "XDG_DATA_HOME": str(self.home / "data"),
                                 "XDG_CONFIG_HOME": str(self.home / "config"), "XDG_CACHE_HOME": str(self.home / "cache"),
                                 "XDG_STATE_HOME": str(self.home / "state"), "XDG_RUNTIME_DIR": str(self.home / "runtime"),
                                 "PATH": str(self.tools) + os.pathsep + os.defpath,
                                 "FAKE_SERVICE_LOG": str(self.root / "service.log"), "PYTHONDONTWRITEBYTECODE": "1"})
        self.uninstaller = self.prefix / "bin/dawg-uninstall"

    def command(self, script: Path, *arguments: str, success: bool = True, input: str | None = None):
        result = subprocess.run([sys.executable, str(script), *map(str, arguments)],
                                env=self.environment, text=True, input=input,
                                capture_output=True, timeout=20)
        if success:
            self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        else:
            self.assertNotEqual(result.returncode, 0, result.stderr + result.stdout)
        return result

    def install(self, *arguments: str):
        return self.command(SCRIPTS / "install.py", "--binary-dir", str(self.source), "--no-start", *arguments)

    def seed_data(self):
        for relative, content in [("servers/one/world/level.dat", b"world"), ("backups/world.tar.gz", b"backup"),
                                  ("web-admin-password", b"secret"), ("web-auth.json", b"{}"),
                                  ("tunnel/bin/agent", b"agent"), ("tunnel/one/state.json", b"secret"),
                                  ("network-leases.json", b"[]"), ("daemon.lock", b"")]:
            path = self.data / relative
            path.parent.mkdir(exist_ok=True, parents=True)
            if not path.exists():
                path.write_bytes(content)
        (self.runtime / "dawg.lock").touch()
        (self.home / "cache/dawg/cache-file").write_text("cache")
        (self.home / "state/dawg/state-file").write_text("state")

    def test_purge_removes_owned_worlds_credentials_runtime_and_itself(self):
        self.install()
        self.seed_data()
        sentinel = self.home / "data/unrelated"
        sentinel.write_text("keep")
        other_binary = self.prefix / "bin/other-app"
        other_binary.write_text("keep")
        result = self.command(self.uninstaller, "--purge", "--yes")
        self.assertIn("provider", result.stderr.lower())
        self.assertFalse(self.uninstaller.exists())
        for path in [self.data, self.config, self.runtime, self.home / "cache/dawg", self.home / "state/dawg",
                     self.home / "data/applications/dawg.desktop", self.home / "config/systemd/user/dawg.service"]:
            self.assertFalse(path.exists(), str(path))
        self.assertFalse((self.home / "config/systemd/user/default.target.wants/dawg.service").is_symlink())
        self.assertEqual(sentinel.read_text(), "keep")
        self.assertEqual(other_binary.read_text(), "keep")
        self.assertFalse(any(path.name.startswith("dawg") for path in self.home.rglob("*")))
        service_log = (self.root / "service.log").read_text()
        self.assertIn("--user stop dawg.service", service_log)
        self.assertIn("--user disable dawg.service", service_log)

    def test_nested_symlink_is_unlinked_without_touching_target(self):
        self.install()
        outside = self.root / "unrelated-world"
        outside.mkdir()
        (outside / "keep").write_text("keep")
        (self.data / "world-link").symlink_to(outside, target_is_directory=True)
        self.command(self.uninstaller, "--purge", "--yes")
        self.assertEqual((outside / "keep").read_text(), "keep")

    def test_symlink_root_refuses_cleanup_before_any_deletion(self):
        self.install()
        moved = self.home / "moved-data"
        self.data.rename(moved)
        self.data.symlink_to(moved, target_is_directory=True)
        result = self.command(self.uninstaller, "--purge", "--yes", success=False)
        self.assertIn("symlink", result.stderr)
        self.assertTrue(self.uninstaller.exists())
        self.assertTrue((moved / ".dawg-install-owner").exists())

    def test_forged_manifest_cannot_delete_unrelated_roots_or_files(self):
        self.install()
        manifest_path = self.config / "install-manifest.json"
        original = manifest_path.read_text()
        manifest = json.loads(original)
        target = self.home / "precious"
        target.mkdir()
        (target / "keep").write_text("keep")
        manifest["roots"]["data"] = str(target)
        manifest_path.write_text(json.dumps(manifest))
        self.command(self.uninstaller, "--purge", "--yes", success=False)
        self.assertEqual((target / "keep").read_text(), "keep")
        manifest = json.loads(original)
        manifest["files"][0]["path"] = str(target / "keep")
        manifest_path.write_text(json.dumps(manifest))
        self.command(self.uninstaller, "--purge", "--yes", success=False)
        self.assertEqual((target / "keep").read_text(), "keep")
        self.assertTrue((self.prefix / "bin/dawg-daemon").exists())

    def test_replaced_binary_and_foreign_service_link_are_preserved(self):
        self.install()
        daemon = self.prefix / "bin/dawg-daemon"
        original = daemon.read_bytes()
        daemon.write_bytes(b"foreign")
        self.command(self.uninstaller, "--purge", "--yes", success=False)
        self.assertEqual(daemon.read_bytes(), b"foreign")
        daemon.write_bytes(original)
        link = self.home / "config/systemd/user/default.target.wants/dawg.service"
        link.unlink()
        foreign = self.home / "foreign.service"
        foreign.write_text("foreign")
        link.symlink_to(foreign)
        self.command(self.uninstaller, "--purge", "--yes", success=False)
        self.assertTrue(link.is_symlink())
        self.assertTrue(self.uninstaller.exists())

    def test_busy_daemon_lock_prevents_any_file_deletion(self):
        self.install()
        self.seed_data()
        with (self.data / "daemon.lock").open("r+") as held:
            fcntl.flock(held, fcntl.LOCK_EX | fcntl.LOCK_NB)
            result = self.command(self.uninstaller, "--purge", "--yes", success=False)
        self.assertIn("still owns", result.stderr)
        self.assertTrue((self.data / "servers/one/world/level.dat").exists())
        self.assertTrue(self.uninstaller.exists())

    def test_default_uninstall_retains_data_and_allows_later_full_purge(self):
        self.install()
        self.seed_data()
        self.command(self.uninstaller)
        self.assertTrue((self.data / "web-admin-password").exists())
        self.assertTrue(self.uninstaller.exists())
        self.assertFalse((self.prefix / "bin/dawg-daemon").exists())
        self.command(self.uninstaller, "--purge", "--yes")
        self.assertFalse(self.data.exists())
        self.assertFalse(self.uninstaller.exists())

    def test_confirmation_cancel_changes_nothing(self):
        self.install()
        self.command(self.uninstaller, "--purge", success=False, input="no\n")
        self.assertTrue(self.uninstaller.exists())
        self.assertTrue((self.prefix / "bin/dawg-daemon").exists())

    def test_custom_prefix_data_and_headless_install_have_self_contained_purge(self):
        prefix = self.home / "custom prefix"
        custom_data = self.root / "custom data"
        self.install("--prefix", str(prefix), "--data-dir", str(custom_data), "--headless", "--web-bind", "0.0.0.0:8088")
        self.assertFalse((prefix / "bin/dawg-ui").exists())
        environment = json.loads((self.config / "environment.json").read_text())
        self.assertEqual(environment["DAWG_WEB_BIND"], "0.0.0.0:8088")
        self.command(prefix / "bin/dawg-uninstall", "--purge", "--yes")
        self.assertFalse(custom_data.exists())
        self.assertFalse(any((prefix / "bin").iterdir()))

    def test_unowned_existing_data_is_never_silently_adopted(self):
        self.data.mkdir(parents=True)
        (self.data / "precious").write_text("keep")
        result = self.command(SCRIPTS / "install.py", "--binary-dir", str(self.source), "--no-start", success=False)
        self.assertIn("Unowned", result.stderr)
        self.assertFalse((self.config / "install-manifest.json").exists())
        self.assertEqual((self.data / "precious").read_text(), "keep")

    def test_graceful_process_fallback_only_signals_this_installation(self):
        self.install()
        environment = dict(self.environment)
        environment.update(json.loads((self.config / "environment.json").read_text()))
        daemon = subprocess.Popen([str(self.prefix / "bin/dawg-daemon"), "60"], env=environment)
        unrelated = subprocess.Popen([shutil.which("sleep"), "60"], env=environment)
        self.addCleanup(lambda: unrelated.poll() is None and unrelated.terminate())
        try:
            self.command(self.uninstaller, "--purge", "--yes")
            self.assertEqual(daemon.wait(timeout=5), -signal.SIGTERM)
            self.assertIsNone(unrelated.poll())
        finally:
            if daemon.poll() is None:
                daemon.terminate()
            daemon.wait(timeout=5)
            unrelated.terminate()
            unrelated.wait(timeout=5)

    def test_upgrade_to_headless_does_not_leave_old_desktop_binaries(self):
        self.install()
        self.install("--headless")
        self.assertFalse((self.prefix / "bin/dawg-ui").exists())
        self.assertFalse((self.prefix / "bin/dawg").exists())
        self.assertFalse((self.home / "data/applications/dawg.desktop").exists())
        self.command(self.uninstaller, "--purge", "--yes")

    def test_runtime_is_owned_again_after_reboot_clears_the_runtime_directory(self):
        self.install()
        shutil.rmtree(self.runtime)
        self.command(self.config / "prepare-runtime.py")
        self.assertTrue((self.runtime / ".dawg-install-owner").exists())
        self.command(self.prefix / "bin/dawg")
        self.command(self.uninstaller, "--purge", "--yes")
        self.assertFalse(self.runtime.exists())

    def test_existing_dawg_data_can_be_explicitly_adopted(self):
        self.data.mkdir(parents=True)
        (self.data / "servers.json").write_text("[]")
        (self.data / "backups").mkdir()
        (self.data / "backups/old-world.tar.gz").write_text("world")
        self.install("--adopt-data")
        self.command(self.uninstaller, "--purge", "--yes")
        self.assertFalse(self.data.exists())

    def test_release_archive_finds_neighboring_binaries_without_flags(self):
        archive = self.root / "archive"
        (archive / "scripts").mkdir(parents=True)
        for name in ("install.py", "uninstall.py"):
            shutil.copyfile(SCRIPTS / name, archive / "scripts" / name)
        for path in self.source.iterdir():
            shutil.copy2(path, archive / path.name)
        self.command(archive / "scripts/install.py", "--no-start")
        self.command(self.uninstaller, "--purge", "--yes")
        self.assertFalse((archive / "scripts/__pycache__").exists())

    def test_real_daemon_auth_bootstrap_and_complete_purge(self):
        if REAL_BINARY_DIR is None:
            self.skipTest("Pass --real-binary-dir target/debug to exercise the actual daemon")
        shutil.copyfile(REAL_BINARY_DIR / "dawg-daemon", self.source / "dawg-daemon")
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            web_port = listener.getsockname()[1]
        self.install("--headless", "--web-bind", f"127.0.0.1:{web_port}")
        environment = dict(self.environment)
        environment.update(json.loads((self.config / "environment.json").read_text()))
        manifest = json.loads((self.config / "install-manifest.json").read_text())
        log_path = self.root / "real-daemon.log"
        with log_path.open("w+") as log:
            daemon = subprocess.Popen([str(self.prefix / "bin/dawg-daemon")], env=environment,
                                      stdout=log, stderr=subprocess.STDOUT)
            try:
                deadline = time.monotonic() + 15
                while not ((self.runtime / "dawg.sock").exists() and (self.data / "web-auth.json").exists()):
                    if daemon.poll() is not None or time.monotonic() >= deadline:
                        self.fail("Actual daemon did not initialize: " + log_path.read_text())
                    time.sleep(0.05)
                with socket.socket(socket.AF_UNIX) as ipc:
                    ipc.settimeout(3)
                    ipc.connect(str(self.runtime / "dawg.sock"))
                    ipc.sendall(b'"Snapshot"\n')
                    with ipc.makefile("r") as response:
                        snapshot = json.loads(response.readline())["Snapshot"]
                self.assertGreaterEqual(snapshot["protocol_version"], 2)
                self.assertEqual(snapshot["data_directory"], str(self.data))
                self.assertEqual(snapshot["servers"], [])
                password_path = self.data / "web-admin-password"
                password = password_path.read_text().strip()
                self.assertGreaterEqual(len(password), 12)
                self.assertEqual(password_path.stat().st_mode & 0o777, 0o600)
                connection = http.client.HTTPConnection("127.0.0.1", web_port, timeout=5)
                try:
                    headers = {"Content-Type": "application/json", "X-Dawg-Request": "1",
                               "Origin": f"http://127.0.0.1:{web_port}"}
                    connection.request("POST", "/api/login", json.dumps({"username": "admin", "password": password}), headers)
                    response = connection.getresponse()
                    self.assertEqual(response.status, 200, response.read())
                    cookie = response.getheader("Set-Cookie")
                    self.assertIn("HttpOnly", cookie)
                    headers["Cookie"] = cookie.split(";")[0]
                    response.read()
                    connection.request("POST", "/api/action", '"Snapshot"', headers)
                    response = connection.getresponse()
                    self.assertEqual(response.status, 200)
                    self.assertEqual(json.loads(response.read())["Snapshot"]["servers"], [])
                finally:
                    connection.close()
                self.seed_data()
                sentinel = self.home / "data/unrelated"
                sentinel.write_text("keep")
                self.command(self.uninstaller, "--purge", "--yes")
                self.assertEqual(daemon.wait(timeout=10), 0, log_path.read_text())
                self.assertIn("Dawg daemon stopped", log_path.read_text())
                self.assertNotIn(password, log_path.read_text())
                for root in manifest["roots"].values():
                    self.assertFalse(Path(root).exists(), root)
                for record in manifest["files"]:
                    self.assertFalse(Path(record["path"]).exists(), record["path"])
                self.assertEqual(sentinel.read_text(), "keep")
            finally:
                if daemon.poll() is None:
                    daemon.terminate()
                    daemon.wait(timeout=15)


if __name__ == "__main__":
    import argparse
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument("--real-binary-dir", type=Path)
    options, remaining = parser.parse_known_args()
    REAL_BINARY_DIR = options.real_binary_dir.absolute() if options.real_binary_dir else None
    unittest.main(argv=[sys.argv[0], *remaining])
