#!/usr/bin/env python3
"""Exercise the real daemon and Java process lifecycle without network downloads.

Run after `cargo build --workspace --all-targets` with Java 21+ and javac on PATH:
    python3 scripts/smoke.py

This compiles a tiny Java process fixture. It validates Dawg's management stack,
not Minecraft gameplay or the external download providers. All server data and
child processes belong to an isolated temporary directory and are cleaned up.
"""

from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import stat
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import zipfile


REPOSITORY = Path(__file__).resolve().parents[1]


def require(condition, message):
    if not condition:
        raise AssertionError(message)


class Smoke:
    def __init__(self, daemon, root):
        self.daemon = daemon
        self.root = root
        self.socket_path = root / "daemon.sock"
        self.data_path = root / "data"
        self.env = dict(os.environ, DAWG_SOCKET=str(self.socket_path), DAWG_DATA_DIR=str(self.data_path), DAWG_WEB_BIND="127.0.0.1:0")
        self.process = None
        self.log_file = None
        self.log_path = root / "daemon.log"
        self.passed = 0
        self.process_groups = set()

    def check(self, label):
        self.passed += 1
        print(f"PASS {self.passed:02d}  {label}", flush=True)

    def start_daemon(self):
        self.log_file = self.log_path.open("ab")
        self.process = subprocess.Popen([str(self.daemon)], env=self.env, stdout=self.log_file, stderr=subprocess.STDOUT, start_new_session=True)
        self.process_groups.add(self.process.pid)
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                raise AssertionError(f"daemon exited during startup ({self.process.returncode})")
            try:
                self.snapshot()
                return
            except (OSError, ValueError):
                time.sleep(0.05)
        raise AssertionError("daemon did not answer Snapshot within 10 seconds")

    def stop_daemon(self):
        if self.process is not None and self.process.poll() is None:
            self.process.send_signal(signal.SIGTERM)
            try:
                self.process.wait(timeout=12)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=3)
                raise AssertionError("daemon did not shut down gracefully")
        if self.log_file is not None:
            self.log_file.close()
            self.log_file = None

    def cleanup(self):
        try:
            self.stop_daemon()
        finally:
            # Own process groups only; this also cleans up a fixture JVM if a
            # broken daemon failed the explicit orphan-detection assertion.
            for group in self.process_groups:
                try:
                    os.killpg(group, signal.SIGKILL)
                except ProcessLookupError:
                    pass

    def connect(self):
        connection = socket.socket(socket.AF_UNIX)
        connection.settimeout(8)
        connection.connect(str(self.socket_path))
        return connection

    @staticmethod
    def read_frame(stream):
        line = stream.readline(1024 * 1024 + 1)
        require(bool(line), "daemon closed connection without a response")
        require(len(line) <= 1024 * 1024, "daemon response exceeded frame limit")
        return json.loads(line)

    def request(self, request):
        with self.connect() as connection:
            connection.sendall(json.dumps(request).encode() + b"\n")
            with connection.makefile("rb") as stream:
                return self.read_frame(stream)

    def success(self, request, variants=("Ok", "Server")):
        response = self.request(request)
        valid = response in variants if isinstance(response, str) else any(key in response for key in variants)
        require(valid, f"{request!r} failed: {response!r}")
        return response

    def error(self, request):
        response = self.request(request)
        require(isinstance(response, dict) and "Error" in response, f"expected rejection of {request!r}, got {response!r}")
        require(bool(response["Error"]), "error lacked an explanation")
        return response["Error"]

    def snapshot(self):
        response = self.request("Snapshot")
        require("Snapshot" in response, f"invalid Snapshot response: {response!r}")
        return response["Snapshot"]

    def server(self, identifier):
        return next(item for item in self.snapshot()["servers"] if item["config"]["id"] == identifier)

    def wait_for(self, identifier, predicate, description, timeout=10):
        deadline = time.monotonic() + timeout
        current = None
        while time.monotonic() < deadline:
            current = self.server(identifier)
            if predicate(current):
                return current
            time.sleep(0.08)
        raise AssertionError(f"timed out waiting for {description}: {current!r}")

    def logs(self, identifier, after=0):
        entries = []
        while len(entries) < 2000:
            result = self.request({"Logs": {"id": identifier, "after": after}})
            require("Logs" in result, f"invalid logs response: {result!r}")
            batch = result["Logs"]["entries"]
            entries.extend(batch)
            if len(batch) < 64:
                break
            after = batch[-1]["seq"]
        return entries

    def wait_for_log(self, identifier, fragment, timeout=5):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            entries = self.logs(identifier)
            if any(fragment in entry["line"] for entry in entries):
                return entries
            time.sleep(0.08)
        raise AssertionError(f"console never received {fragment!r}: {entries!r}")

    def compile_fixture(self):
        javac = shutil.which("javac")
        java = shutil.which("java")
        require(javac and java, "Java 21+ and javac are required; activate the development environment first")
        classes = self.root / "classes"
        classes.mkdir()
        subprocess.run([javac, "-d", str(classes), str(REPOSITORY / "scripts/fixtures/DawgTestServer.java")], check=True, timeout=15)
        jar = self.root / "fixture.jar"
        with zipfile.ZipFile(jar, "w", zipfile.ZIP_DEFLATED) as archive:
            archive.writestr("META-INF/MANIFEST.MF", "Manifest-Version: 1.0\r\nMain-Class: DawgTestServer\r\n\r\n")
            for path in classes.glob("*.class"):
                archive.write(path, path.name)
        return java, jar

    def run(self):
        java, jar = self.compile_fixture()
        self.start_daemon()
        snapshot = self.snapshot()
        require(snapshot["protocol_version"] == 2, "protocol version mismatch")
        require(snapshot["servers"] == [], "new data directory contains fabricated servers")
        require(snapshot["host"]["memory_total_bytes"] > 0, "host metrics are missing")
        self.check("empty persisted dashboard and real host metrics")

        socket_identity = (self.socket_path.stat().st_dev, self.socket_path.stat().st_ino)
        duplicate = subprocess.run([str(self.daemon)], env=self.env, capture_output=True, text=True, timeout=5)
        require(duplicate.returncode != 0, "a second daemon claimed the same socket")
        other_socket = dict(self.env, DAWG_SOCKET=str(self.root / "other.sock"))
        duplicate_data = subprocess.run([str(self.daemon)], env=other_socket, capture_output=True, text=True, timeout=5)
        require(duplicate_data.returncode != 0, "two daemons claimed one data directory through different sockets")
        require(self.snapshot()["servers"] == [], "a second daemon disrupted the first")
        require((self.socket_path.stat().st_dev, self.socket_path.stat().st_ino) == socket_identity,
                "a second daemon replaced the first daemon's socket")
        require(stat.S_IMODE(self.socket_path.stat().st_mode) == 0o600, "IPC socket is not private to its user")
        self.check("duplicate-daemon protection and private IPC socket permissions")

        with self.connect() as connection, connection.makefile("rb") as stream:
            connection.sendall(b'"Snap')
            connection.sendall(b'shot"\n"Snapshot"\n')
            require("Snapshot" in self.read_frame(stream), "fragmented request failed")
            require("Snapshot" in self.read_frame(stream), "batched request failed")
            connection.sendall(b'not json\n"Snapshot"\n')
            require("Error" in self.read_frame(stream), "malformed request accepted")
            require("Snapshot" in self.read_frame(stream), "malformed frame poisoned connection")
        self.check("fragmented, batched, and malformed newline-framed requests")

        create = {"name": "Integration survival", "kind": "Vanilla", "minecraft_version": "1.21.4", "memory_mb": 512, "port": 25591, "java_path": java, "eula_accepted": True, "import_jar": str(jar)}
        for changes in [{"name": ""}, {"memory_mb": 0}, {"port": 0}, {"minecraft_version": "../escape"}]:
            self.error({"CreateServer": dict(create, **changes)})
        require(not self.snapshot()["servers"], "invalid create leaked a registered server")
        unaccepted = self.success({"CreateServer": dict(create, eula_accepted=False)}, ("Server",))["Server"]
        self.error({"StartServer": unaccepted["config"]["id"]})
        self.error({"InstallServer": unaccepted["config"]["id"]})
        self.success({"RemoveServer": unaccepted["config"]["id"]}, ("Removed",))
        created = self.success({"CreateServer": create}, ("Server",))["Server"]
        identifier = created["config"]["id"]
        directory = Path(created["config"]["directory"])
        require(created["installed"], "imported JAR is not installed")
        require(directory.is_relative_to(self.data_path), "managed directory escaped data root")
        require((directory / "server.jar").read_bytes() == jar.read_bytes(), "import modified JAR bytes")
        self.error({"CreateServer": dict(create, name="Port collision")})
        self.error({"UpdateServer": dict(created["config"], directory=str(self.root / "escape"))})
        self.check("validated creation, explicit EULA, unique ports, and immutable managed JAR directory")

        config = dict(created["config"], name="Renamed survival", memory_mb=640)
        self.success({"UpdateServer": config})
        require(self.server(identifier)["config"]["name"] == "Renamed survival", "settings were not updated")
        properties = self.success({"GetProperties": identifier}, ("Properties",))["Properties"]["content"]
        require("server-port=25591" in properties, "configured port missing from properties")
        properties += "\nmotd=Dawg integration test\n"
        self.success({"SaveProperties": {"id": identifier, "content": properties}})
        readback = self.success({"GetProperties": identifier}, ("Properties",))["Properties"]["content"]
        require("motd=Dawg integration test" in readback, "properties did not persist")
        self.check("editable server configuration and server.properties read/write")

        registry = self.data_path / "servers.json"
        saved_registry = self.root / "registry-before-failure.json"
        original_properties = (directory / "server.properties").read_bytes()
        original_eula = (directory / "eula.txt").read_bytes()
        original_config = self.server(identifier)["config"]
        registry.rename(saved_registry)
        registry.mkdir()
        try:
            self.error({"UpdateServer": dict(original_config, name="Must roll back", port=25594, eula_accepted=False)})
            require(self.server(identifier)["config"] == original_config, "failed persistence changed the live configuration")
            require((directory / "server.properties").read_bytes() == original_properties, "failed persistence changed server.properties")
            require((directory / "eula.txt").read_bytes() == original_eula, "failed persistence changed eula.txt")
        finally:
            registry.rmdir()
            saved_registry.rename(registry)
        self.check("failed registry writes roll back configuration and runtime files")

        self.stop_daemon()
        require(not self.socket_path.exists(), "daemon did not remove its socket on shutdown")
        self.start_daemon()
        restored = self.server(identifier)
        require(restored["config"]["name"] == "Renamed survival", "server registration lost on daemon restart")
        require(restored["config"]["memory_mb"] == 640, "settings lost on daemon restart")
        require(restored["status"] == "Stopped", "restart fabricated a live process")
        self.check("registration/settings persistence and graceful daemon socket cleanup")

        self.success({"StartServer": identifier})
        running = self.wait_for(identifier, lambda item: item["status"] == "Running", "real Java process readiness")
        require(isinstance(running["pid"], int), "running Java process has no PID")
        other = self.success({"CreateServer": dict(create, name="Offline status fixture", port=25593)}, ("Server",))["Server"]
        listed = {server["config"]["id"]: server for server in self.snapshot()["servers"]}
        require(set(listed) == {identifier, other["config"]["id"]}, "snapshot did not list the actual registrations")
        require(listed[identifier]["status"] == "Running" and listed[other["config"]["id"]]["status"] == "Stopped",
                "per-server status was hardcoded or looked up using the wrong ID")
        require(listed[other["config"]["id"]]["pid"] is None, "stopped server fabricated a process")
        self.error({"Logs": {"id": "missing-server", "after": 0}})
        self.success({"RemoveServer": other["config"]["id"]}, ("Removed",))
        self.wait_for_log(identifier, "Fixture stderr is connected")
        self.success({"SendCommand": {"id": identifier, "command": "status"}})
        entries = self.wait_for_log(identifier, "Fixture status: running")
        require([item["seq"] for item in entries] == sorted(set(item["seq"] for item in entries)), "log sequence is not ordered and unique")
        last_sequence = entries[-1]["seq"]
        require(all(entry["seq"] > last_sequence for entry in self.logs(identifier, last_sequence)), "log cursor replayed older lines")
        self.wait_for(identifier, lambda item: item["memory_bytes"] > 0, "actual Java memory metrics")
        self.error({"StartServer": identifier})
        self.error({"SaveProperties": {"id": identifier, "content": properties}})
        self.error({"SendCommand": {"id": identifier, "command": "status\ncrash"}})
        self.error({"BackupServer": identifier})
        self.error({"RemoveServer": identifier})
        self.check("real Java startup, stdout/stderr, console commands, metrics, and live-state guards")

        self.success({"SendCommand": {"id": identifier, "command": "slow-stop"}})
        self.wait_for_log(identifier, "Fixture slow stop enabled")
        barrier = threading.Barrier(2)

        def restart_at_once():
            barrier.wait(timeout=3)
            return self.request({"RestartServer": identifier})

        with ThreadPoolExecutor(max_workers=2) as executor:
            futures = [executor.submit(restart_at_once) for _ in range(2)]
            replies = [future.result(timeout=5) for future in futures]
        require(replies.count("Ok") == 1 and sum(isinstance(reply, dict) and "Error" in reply for reply in replies) == 1, f"concurrent restarts were not serialized: {replies!r}")
        restarted = self.wait_for(identifier, lambda item: item["status"] == "Running" and item["pid"] != running["pid"], "new process after restart")
        require(restarted["pid"] != running["pid"], "restart kept old Java process")
        self.success({"StopServer": identifier})
        self.wait_for(identifier, lambda item: item["status"] == "Stopped", "graceful server stop")
        self.error({"SendCommand": {"id": identifier, "command": "status"}})
        self.check("serialized concurrent restart, a different Java PID, and graceful stop")

        self.success({"StartServer": identifier})
        self.wait_for(identifier, lambda item: item["status"] == "Running", "readiness before crash")
        self.success({"SendCommand": {"id": identifier, "command": "crash"}})
        crashed = self.wait_for(identifier, lambda item: item["status"] == "Crashed", "crash detection")
        require(crashed["last_error"], "crash lacked an error message")
        self.wait_for_log(identifier, "Fixture intentional crash")
        self.success({"StartServer": identifier})
        self.wait_for(identifier, lambda item: item["status"] == "Running", "recovery after crash")
        self.success({"StopServer": identifier})
        self.wait_for(identifier, lambda item: item["status"] == "Stopped", "stop before backup")
        self.check("nonzero-exit crash reporting and successful recovery")

        last_sequence = max(entry["seq"] for entry in self.logs(identifier))
        config = dict(self.server(identifier)["config"], name="Updated after running")
        self.success({"UpdateServer": config})
        self.success({"StartServer": identifier})
        self.wait_for(identifier, lambda item: item["status"] == "Running", "readiness after settings update")
        self.success({"SendCommand": {"id": identifier, "command": "cursor-regression"}})
        self.wait_for_log(identifier, "Fixture received: cursor-regression")
        newer = self.logs(identifier, last_sequence)
        require(any("cursor-regression" in entry["line"] for entry in newer), "settings update reset console sequence numbers")
        self.success({"StopServer": identifier})
        self.wait_for(identifier, lambda item: item["status"] == "Stopped", "stop after cursor test")
        self.check("console cursors remain monotonic across saved settings and process restarts")

        response = self.success({"BackupServer": identifier}, ("BackupCreated",))
        backup = Path(response["BackupCreated"]["path"])
        require(backup.is_file(), "backup response did not name an archive")
        with tarfile.open(backup, "r:gz") as archive:
            world = next((member for member in archive.getmembers() if member.name.endswith("world/level.dat")), None)
            require(world is not None, "backup omitted saved world data")
            with archive.extractfile(world) as stream:
                require(stream.read() == b"Dawg integration fixture\n", "backup corrupted saved world data")
        self.check("real compressed backup containing verified world bytes")

        self.success({"RemoveServer": identifier}, ("Removed",))
        require(not self.snapshot()["servers"], "remove left server registered")
        require((directory / "world/level.dat").is_file(), "remove deleted world files")
        self.error({"StartServer": identifier})
        self.check("unregister preserves server files and rejects unknown IDs")

        second = self.success({"CreateServer": dict(create, name="Shutdown fixture", port=25592)}, ("Server",))["Server"]
        second_id = second["config"]["id"]
        self.success({"StartServer": second_id})
        active = self.wait_for(second_id, lambda item: item["status"] == "Running", "process before daemon shutdown")
        starting = self.success({"CreateServer": dict(create, name="Starting shutdown fixture", port=25593)}, ("Server",))["Server"]
        starting_id = starting["config"]["id"]
        Path(starting["config"]["directory"], "startup-delay-ms").write_text("5000\n")
        self.success({"StartServer": starting_id})
        self.wait_for_log(starting_id, "Fixture startup paused")
        preparing = self.server(starting_id)
        require(preparing["status"] == "Starting", "fixture did not remain in Starting for the shutdown regression")
        self.stop_daemon()
        require(not self.socket_path.exists(), "socket survived final daemon shutdown")
        for process in (active, preparing):
            try:
                os.kill(process["pid"], 0)
            except ProcessLookupError:
                pass
            else:
                raise AssertionError("daemon shutdown orphaned a managed Java process")
            saved = Path(process["config"]["directory"], "world/clean-shutdown.txt")
            require(saved.read_text() == "Stopped through console\n", "shutdown killed Java without processing stop and flushing world data")
        self.check("daemon shutdown gracefully stops and reaps both Running and Starting Java processes")

        relocated = self.root / "relocated-data"
        self.data_path.rename(relocated)
        self.data_path = relocated
        self.env["DAWG_DATA_DIR"] = str(relocated)
        self.start_daemon()
        moved = self.server(second_id)
        moved_directory = Path(moved["config"]["directory"])
        require(moved_directory.is_relative_to(relocated), "restored server retained its obsolete absolute path")
        require((moved_directory / "world/level.dat").read_bytes() == b"Dawg integration fixture\n", "relocation lost server data")
        self.success({"StartServer": second_id})
        self.wait_for(second_id, lambda item: item["status"] == "Running", "real Java readiness after data relocation")
        self.stop_daemon()
        self.check("persisted server data remains runnable after moving the data directory")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--daemon", type=Path, default=REPOSITORY / "target/debug/dawg-daemon")
    arguments = parser.parse_args()
    if not arguments.daemon.is_file():
        parser.error(f"build the daemon first: missing {arguments.daemon}")
    with tempfile.TemporaryDirectory(prefix="dawg-smoke-") as temporary:
        smoke = Smoke(arguments.daemon.resolve(), Path(temporary))
        try:
            smoke.run()
        except Exception as error:
            print(f"FAIL: {error}", file=sys.stderr)
            if smoke.log_path.exists():
                print("--- daemon output ---", file=sys.stderr)
                print(smoke.log_path.read_text(errors="replace")[-10000:], file=sys.stderr)
            return 1
        finally:
            smoke.cleanup()
        print(f"\n{smoke.passed} end-to-end checks passed (real daemon + Java fixture; no external downloads).")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
