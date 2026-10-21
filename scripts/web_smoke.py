#!/usr/bin/env python3
"""Exercise the authenticated web API and real file manager in isolation.

Run after `cargo build -p dawg-daemon`, with Java 21+ and javac on PATH:
    python3 scripts/web_smoke.py

Uses the existing offline Java fixture. Credentials remain in memory and the
temporary private data directory; no provider account or public port is used.
"""

from __future__ import annotations

import argparse
import hashlib
import http.client
import json
from pathlib import Path
import socket
import stat
import sys
import tempfile
import time
from urllib.parse import quote

from smoke import REPOSITORY, Smoke, require


def free_port():
    with socket.socket() as reserved:
        reserved.bind(("127.0.0.1", 0))
        return reserved.getsockname()[1]


class WebSmoke(Smoke):
    def __init__(self, daemon, root):
        super().__init__(daemon, root)
        self.port = free_port()
        self.env["DAWG_WEB_BIND"] = f"127.0.0.1:{self.port}"
        self.env.pop("DAWG_WEB_PASSWORD", None)
        self.env.pop("DAWG_WEB_SECURE_COOKIE", None)
        self.cookie = ""
        self.password = ""
        self.secrets = []

    def http(self, method, path, body=None, *, authenticated=True, csrf=True,
             origin=None, chunked=False, download_hash=False):
        headers = {}
        if isinstance(body, (dict, list, str)):
            body = json.dumps(body).encode()
            headers["Content-Type"] = "application/json"
        if authenticated and self.cookie:
            headers["Cookie"] = self.cookie
        if csrf:
            headers["X-Dawg-Request"] = "1"
        if origin is not None:
            headers["Origin"] = origin
        connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=12)
        try:
            connection.request(method, path, body=body, headers=headers,
                               encode_chunked=chunked)
            response = connection.getresponse()
            response_headers = {name.lower(): value for name, value in response.getheaders()}
            if download_hash:
                digest = hashlib.sha256()
                size = 0
                while chunk := response.read(64 * 1024):
                    size += len(chunk)
                    digest.update(chunk)
                return response.status, response_headers, (size, digest.hexdigest())
            return response.status, response_headers, response.read()
        finally:
            connection.close()

    def json_http(self, method, path, body=None, **options):
        status, headers, payload = self.http(method, path, body, **options)
        return status, headers, json.loads(payload)

    def action(self, request, success=True):
        status, _, response = self.json_http("POST", "/api/action", request)
        require(status == (200 if success else 400), "unexpected manager HTTP status")
        if not success:
            require(isinstance(response, dict) and "Error" in response,
                    "manager did not explain its rejection")
        return response

    def login(self):
        status, headers, _ = self.http("POST", "/api/login",
                                      {"username": "admin", "password": self.password},
                                      authenticated=False)
        require(status == 200, "administrator login failed")
        cookie = headers.get("set-cookie", "")
        require("HttpOnly" in cookie and "SameSite=Strict" in cookie,
                "session cookie is missing browser protections")
        self.cookie = cookie.split(";", 1)[0]
        self.secrets.append(self.cookie)

    def file_uri(self, identifier, path="", content=False):
        suffix = "/content" if content else ""
        return f"/api/servers/{identifier}/files{suffix}?path={quote(path, safe='')}"

    def wait_for_web(self):
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            try:
                status, _, _ = self.http("GET", "/api/session", authenticated=False)
                if status == 200:
                    return
            except OSError:
                pass
            time.sleep(0.05)
        raise AssertionError("web listener did not become ready")

    def run(self):
        java, jar = self.compile_fixture()
        self.start_daemon()
        self.wait_for_web()
        password_file = self.data_path / "web-admin-password"
        self.password = password_file.read_text().strip()
        self.secrets.append(self.password)
        require(len(self.password) >= 32, "bootstrap password has insufficient entropy")
        for path in (password_file, self.data_path / "web-auth.json"):
            require(stat.S_IMODE(path.stat().st_mode) == 0o600,
                    "web authentication file is not private")
        require(self.password not in self.log_path.read_text(),
                "daemon printed its bootstrap credential")
        status, _, session = self.json_http("GET", "/api/session")
        require(status == 200 and session["authenticated"] is False,
                "new browser session unexpectedly authenticated")
        for method, path, body in [
            ("POST", "/api/action", "Snapshot"),
            ("GET", "/api/servers/missing/files?path=", None),
        ]:
            status, _, _ = self.http(method, path, body, authenticated=False)
            require(status == 401, "protected endpoint allowed an anonymous request")
        self.check("private generated credentials and anonymous API protection")

        login_body = {"username": "admin", "password": self.password}
        status, _, _ = self.http("POST", "/api/login", login_body, csrf=False)
        require(status == 403, "login accepted a request without CSRF protection")
        status, _, _ = self.http("POST", "/api/login", login_body,
                                 origin="https://attacker.invalid")
        require(status == 403, "login accepted a cross-origin request")
        status, _, _ = self.http("POST", "/api/login",
                                 {"username": "admin", "password": "incorrect"})
        require(status == 401, "incorrect password was accepted")
        self.login()
        status, _, session = self.json_http("GET", "/api/session")
        require(status == 200 and session["authenticated"] is True,
                "login did not establish a session")
        status, _, _ = self.http("POST", "/api/action", b'"Snapshot"', csrf=False)
        require(status == 403, "authenticated mutation omitted CSRF protection")
        status, _, _ = self.http("POST", "/api/action", b'"Snapshot"',
                                 origin="https://attacker.invalid")
        require(status == 403, "authenticated mutation accepted a cross-origin request")
        self.check("password login, protected session cookie, CSRF and origin checks")

        created = self.action({"CreateServer": {
            "name": "Web file manager fixture", "kind": "Vanilla",
            "minecraft_version": "1.21.4", "memory_mb": 512,
            "port": free_port(), "java_path": java, "eula_accepted": True,
            "import_jar": str(jar),
        }})["Server"]
        identifier = created["config"]["id"]
        directory = Path(created["config"]["directory"])
        base = f"/api/servers/{identifier}/files"
        status, _, listing = self.json_http("GET", self.file_uri(identifier))
        require(status == 200 and listing["writable"], "stopped server files are not writable")
        require({entry["name"] for entry in listing["entries"]} >=
                {"server.jar", "server.properties", "eula.txt"}, "managed files are missing")
        status, _, _ = self.http("POST", base + "/directory", {"path": "uploads"})
        require(status == 200, "directory creation failed")
        payload = self.root / "payload.bin"
        with payload.open("wb") as output:
            for index in range(49):
                output.write(bytes([index]) * (64 * 1024))
        expected_digest = hashlib.sha256(payload.read_bytes()).hexdigest()
        uri = self.file_uri(identifier, "uploads/large.bin", content=True)
        with payload.open("rb") as source:
            status, _, _ = self.http("PUT", uri, source, chunked=True)
        require(status == 200, "chunked upload larger than the JSON body limit failed")
        status, headers, result = self.http("GET", uri, download_hash=True)
        require(status == 200 and result == (payload.stat().st_size, expected_digest),
                "streamed download did not match the uploaded bytes")
        require(headers.get("content-disposition", "").startswith("attachment;"),
                "file download lacks attachment disposition")
        status, _, _ = self.http("POST", base + "/rename",
                                 {"from": "uploads/large.bin", "to": "uploads/renamed.bin"})
        require(status == 200, "file rename failed")
        require(self.http("GET", uri)[0] == 404, "rename left its original path")
        uri = self.file_uri(identifier, "uploads/renamed.bin", content=True)
        require(self.http("GET", uri, download_hash=True)[2][1] == expected_digest,
                "rename changed file contents")
        self.check("real HTTP server creation and streamed file upload, download and rename")

        properties_uri = self.file_uri(identifier, "server.properties", content=True)
        properties = f"server-port={created['config']['port']}\nmotd=Web smoke test\n".encode()
        require(self.http("PUT", properties_uri, properties)[0] == 200,
                "properties editor could not save valid settings")
        saved = self.http("GET", properties_uri)[2]
        require(b"motd=Web smoke test" in saved, "saved properties were not returned")
        require(self.http("PUT", properties_uri, b"server-port=1\n")[0] == 409,
                "generic file editor bypassed manager port validation")
        require(self.http("GET", properties_uri)[2] == saved,
                "rejected properties edit changed the original file")
        require(self.http("DELETE", self.file_uri(identifier, "server.properties"))[0] == 400,
                "file manager deleted protected server.properties")
        require(self.http("POST", base + "/rename",
                          {"from": "eula.txt", "to": "server.properties"})[0] == 400,
                "rename bypassed properties validation")
        self.check("properties editor preserves authoritative port checks")

        for path in ("../web-admin-password", "/etc/passwd", "uploads/../../web-auth.json"):
            require(self.http("GET", self.file_uri(identifier, path, content=True))[0] == 400,
                    "download accepted an escaping path")
            require(self.http("PUT", self.file_uri(identifier, path, content=True), b"bad")[0] == 400,
                    "upload accepted an escaping path")
        outside = self.root / "outside"
        outside.mkdir()
        sentinel = outside / "private.txt"
        sentinel.write_bytes(b"outside sentinel")
        (directory / "outside-dir").symlink_to(outside, target_is_directory=True)
        (directory / "outside-file").symlink_to(sentinel)
        require(self.http("GET", self.file_uri(identifier, "outside-file", content=True))[0] == 403,
                "download followed an external symlink")
        require(self.http("PUT", self.file_uri(identifier, "outside-dir/private.txt", content=True), b"bad")[0] == 403,
                "upload followed a symlink parent")
        require(self.http("DELETE", self.file_uri(identifier, "outside-dir"))[0] == 200,
                "removing a symlink itself failed")
        require(sentinel.read_bytes() == b"outside sentinel", "outside file was changed")
        require(self.http("DELETE", self.file_uri(identifier, "outside-file"))[0] == 200,
                "removing a file symlink itself failed")
        self.check("path traversal and external symlink confinement")

        properties_path = directory / "server.properties"
        original_properties = properties_path.read_bytes()
        properties_path.unlink()
        properties_path.symlink_to(sentinel)
        try:
            self.action({"GetProperties": identifier}, success=False)
            self.action({"UpdateServer": created["config"]}, success=False)
            require(sentinel.read_bytes() == b"outside sentinel",
                    "properties read or settings update followed an external symlink")
            require(properties_path.is_symlink(),
                    "rejected settings update replaced the original properties link")
        finally:
            properties_path.unlink(missing_ok=True)
            properties_path.write_bytes(original_properties)
        self.check("properties reads and settings updates reject external symlinks")

        # Hold an incomplete upload after its temporary file appears. This
        # proves Start cannot race a file write and cancellation preserves data.
        held = http.client.HTTPConnection("127.0.0.1", self.port, timeout=5)
        held.putrequest("PUT", uri)
        held.putheader("Cookie", self.cookie)
        held.putheader("X-Dawg-Request", "1")
        held.putheader("Content-Length", "100")
        held.endheaders()
        held.send(b"partial")
        deadline = time.monotonic() + 3
        while not list((directory / "uploads").glob(".dawg-upload-*")):
            require(time.monotonic() < deadline, "incomplete upload did not reserve its file operation")
            time.sleep(0.02)
        self.action({"StartServer": identifier}, success=False)
        held.close()
        deadline = time.monotonic() + 3
        while list((directory / "uploads").glob(".dawg-upload-*")):
            require(time.monotonic() < deadline, "cancelled upload leaked a partial file")
            time.sleep(0.02)
        require(self.http("GET", uri, download_hash=True)[2][1] == expected_digest,
                "cancelled upload replaced the original file")
        self.check("upload reservation blocks Start and cancellation preserves existing bytes")

        self.action({"StartServer": identifier})
        self.wait_for(identifier, lambda item: item["status"] == "Running", "Java fixture readiness")
        status, _, listing = self.json_http("GET", self.file_uri(identifier))
        require(status == 200 and listing["writable"] is False,
                "running server listing advertises writable files")
        for method, path, body in [
            ("PUT", uri, b"must not replace"),
            ("POST", base + "/directory", {"path": "blocked"}),
            ("POST", base + "/rename", {"from": "uploads/renamed.bin", "to": "blocked.bin"}),
            ("DELETE", self.file_uri(identifier, "uploads/renamed.bin"), None),
            ("PUT", properties_uri, properties),
        ]:
            require(self.http(method, path, body)[0] == 409, "running server allowed a file mutation")
        require(self.http("GET", uri, download_hash=True)[2][1] == expected_digest,
                "running server download stopped working or a rejected edit changed its contents")
        self.action({"StopServer": identifier})
        self.wait_for(identifier, lambda item: item["status"] == "Stopped", "Java fixture shutdown")
        require(self.http("DELETE", self.file_uri(identifier, "uploads/renamed.bin"))[0] == 200,
                "stopped file deletion failed")
        require(self.http("DELETE", self.file_uri(identifier, "uploads"))[0] == 200,
                "directory deletion failed")
        require(not (directory / "uploads").exists(), "deleted directory remains on disk")
        self.check("running server writes are blocked while downloads work; stopped deletion works")

        require(self.http("POST", "/api/logout")[0] == 200, "logout failed")
        require(self.json_http("GET", "/api/session")[2]["authenticated"] is False,
                "logout did not revoke the session")
        require(self.http("POST", "/api/action", b'"Snapshot"')[0] == 401,
                "revoked cookie still authorized an action")
        self.stop_daemon()
        self.start_daemon()
        self.wait_for_web()
        require(password_file.read_text().strip() == self.password,
                "daemon restart changed the bootstrap administrator credential")
        self.login()
        require(self.action("Snapshot")["Snapshot"]["servers"][0]["config"]["id"] == identifier,
                "restart lost the web-managed server")
        self.check("logout revocation and administrator/server persistence after daemon restart")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--daemon", type=Path, default=REPOSITORY / "target/debug/dawg-daemon")
    parser.add_argument("--report", type=Path)
    arguments = parser.parse_args()
    if not arguments.daemon.is_file():
        parser.error(f"build the daemon first: missing {arguments.daemon}")
    with tempfile.TemporaryDirectory(prefix="dawg-web-smoke-") as temporary:
        smoke = WebSmoke(arguments.daemon.resolve(), Path(temporary))
        try:
            smoke.run()
        except Exception as error:
            print(f"FAIL: {error}", file=sys.stderr)
            if smoke.log_path.exists():
                output = smoke.log_path.read_text(errors="replace")[-10000:]
                for secret in smoke.secrets:
                    output = output.replace(secret, "[redacted]")
                print(output, file=sys.stderr)
            return 1
        finally:
            smoke.cleanup()
        report = {"passed_groups": smoke.passed, "real_http": True,
                  "chunked_upload_and_streamed_download": True,
                  "path_and_symlink_confinement": True, "running_write_guards": True,
                  "external_downloads": False, "public_internet_join_tested": False}
        if arguments.report:
            arguments.report.parent.mkdir(parents=True, exist_ok=True)
            arguments.report.write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps(report), flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
