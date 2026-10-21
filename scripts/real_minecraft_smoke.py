#!/usr/bin/env python3
"""Run an explicitly EULA-authorized real Minecraft hosting check in isolation.

The JAR must match its official version metadata. No provider account, public
tunnel, router or existing Dawg installation is modified by this test.
"""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import socket
import struct
import tempfile
import time

from smoke import REPOSITORY, Smoke, require


def varint(value):
    result = bytearray()
    while True:
        byte = value & 127
        value >>= 7
        result.append(byte | (128 if value else 0))
        if not value:
            return bytes(result)


def read_exact(stream, size):
    result = bytearray()
    while len(result) < size:
        chunk = stream.recv(size - len(result))
        require(bool(chunk), "Minecraft closed the status connection")
        result.extend(chunk)
    return bytes(result)


def read_varint(stream):
    value = 0
    for shift in range(0, 35, 7):
        byte = read_exact(stream, 1)[0]
        value |= (byte & 127) << shift
        if not byte & 128:
            return value
    raise AssertionError("Invalid Minecraft VarInt")


def ping(port, protocol):
    with socket.create_connection(("127.0.0.1", port), timeout=10) as stream:
        host = b"localhost"
        handshake = b"\x00" + varint(protocol) + varint(len(host)) + host + struct.pack(">H", port) + b"\x01"
        stream.sendall(varint(len(handshake)) + handshake + b"\x01\x00")
        length = read_varint(stream)
        require(0 < length < 1024 * 1024, "Invalid status packet size")
        require(read_varint(stream) == 0, "Invalid status packet type")
        status = json.loads(read_exact(stream, read_varint(stream)))
        payload = struct.pack(">q", int(time.time() * 1000))
        stream.sendall(b"\x09\x01" + payload)
        require(read_varint(stream) == 9, "Invalid pong length")
        require(read_exact(stream, 9) == b"\x01" + payload, "Minecraft did not return the ping payload")
        return status


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jar", type=Path, required=True)
    parser.add_argument("--metadata", type=Path, required=True)
    parser.add_argument("--accept-eula", action="store_true", required=True)
    parser.add_argument("--daemon", type=Path, default=REPOSITORY / "target/debug/dawg-daemon")
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    metadata = json.loads(args.metadata.read_text())
    require(hashlib.sha1(args.jar.read_bytes()).hexdigest() == metadata["downloads"]["server"]["sha1"], "JAR does not match Mojang's SHA1")
    java = shutil.which("java")
    require(java, "Activate Java before running this check")
    with socket.socket() as reserved:
        reserved.bind(("127.0.0.1", 0))
        port = reserved.getsockname()[1]
    with tempfile.TemporaryDirectory(prefix="dawg-real-minecraft-") as temporary:
        runner = Smoke(args.daemon.resolve(), Path(temporary))
        try:
            runner.start_daemon()
            created = runner.success({"CreateServer": {
                "name": "Real Minecraft hosting verification", "kind": "Vanilla",
                "minecraft_version": metadata["id"], "memory_mb": 1536, "port": port,
                "java_path": java, "eula_accepted": True, "import_jar": str(args.jar.resolve())
            }})["Server"]
            identifier = created["config"]["id"]
            runner.success({"SaveProperties": {"id": identifier, "content":
                "motd=Dawg real hosting test\nonline-mode=true\nview-distance=2\nsimulation-distance=2\nspawn-protection=0\nmax-players=2\n"}})
            runner.success({"StartServer": identifier}, ("Ok",))
            started = time.monotonic()
            runner.wait_for(identifier, lambda s: s["status"] in ("Running", "Crashed"), "real Minecraft startup", timeout=240)
            state = runner.server(identifier)
            require(state["status"] == "Running", f"Minecraft startup failed: {state['last_error']}")
            runner.check("Mojang-verified Minecraft JAR launched by Dawg and reached its ready state")
            # Protocol version is encoded in the actual status packet; modern
            # Minecraft accepts any protocol for status negotiation.
            status = ping(port, 0)
            require(status["version"]["name"] == metadata["id"], f"Wrong Minecraft version: {status['version']}")
            runner.check("real Minecraft Server List Ping handshake, version, JSON status and pong")
            runner.success({"SendCommand": {"id": identifier, "command": "list"}}, ("Ok",))
            runner.wait_for_log(identifier, "players online", timeout=10)
            runner.success({"SendCommand": {"id": identifier, "command": "save-all flush"}}, ("Ok",))
            runner.wait_for_log(identifier, "Saved the game", timeout=20)
            runner.check("real console commands and world save")
            runner.success({"StopServer": identifier}, ("Ok",))
            runner.wait_for(identifier, lambda s: s["status"] == "Stopped", "graceful Minecraft stop", timeout=45)
            world = Path(created["config"]["directory"]) / "world/level.dat"
            require(world.is_file(), "Minecraft did not persist its world")
            runner.stop_daemon()
            runner.start_daemon()
            require(runner.server(identifier)["installed"], "JAR registration lost after daemon restart")
            runner.success({"StartServer": identifier}, ("Ok",))
            runner.wait_for(identifier, lambda s: s["status"] == "Running", "real world restart", timeout=120)
            require(ping(port, 0)["version"]["name"] == metadata["id"], "World restart did not restore Minecraft")
            runner.check("graceful shutdown, persisted world and real Minecraft restart after daemon restart")
            report = {"minecraft_version": metadata["id"], "official_sha1_verified": True,
                      "server_list_ping": True, "console_commands": True, "world_persisted": True,
                      "daemon_and_game_restart": True, "startup_seconds": round(time.monotonic() - started, 1),
                      "public_internet_join_tested": False, "passed_groups": runner.passed}
            if args.report:
                args.report.parent.mkdir(parents=True, exist_ok=True)
                args.report.write_text(json.dumps(report, indent=2) + "\n")
            print(json.dumps(report), flush=True)
        except Exception:
            if runner.log_path.exists():
                print(runner.log_path.read_text()[-10000:])
            if 'identifier' in locals() and runner.process and runner.process.poll() is None:
                for entry in runner.logs(identifier):
                    print(entry["line"])
            raise
        finally:
            runner.cleanup()


if __name__ == "__main__":
    main()
