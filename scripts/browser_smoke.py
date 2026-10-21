#!/usr/bin/env python3
"""Exercise the embedded web UI against an isolated, real Dawg daemon.

Build first: cargo build -p dawg-daemon
Requires Python Playwright, Chromium, and Java/javac on PATH. The lifecycle
fixture is a real JVM process, not a Minecraft server or a provider download.
Screenshots are saved outside the checkout (default: /tmp/dawg-browser-smoke).
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import socket
import tempfile
import time
import urllib.request

from playwright.sync_api import sync_playwright, expect
from smoke import REPOSITORY, Smoke, require


def free_port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def run(daemon, artifacts, chromium):
    artifacts.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="dawg-browser-") as temporary:
        fixture = Smoke(daemon, Path(temporary))
        password = os.urandom(24).hex()
        port = free_port()
        fixture.env.update(DAWG_WEB_BIND=f"127.0.0.1:{port}", DAWG_WEB_PASSWORD=password)
        base = f"http://127.0.0.1:{port}"
        try:
            java, jar = fixture.compile_fixture()
            fixture.start_daemon()
            deadline = time.monotonic() + 15
            while True:
                try:
                    urllib.request.urlopen(f"{base}/api/session", timeout=1).close()
                    break
                except OSError:
                    if time.monotonic() >= deadline:
                        raise AssertionError("Web listener did not become ready")
                    time.sleep(.1)
            with sync_playwright() as playwright:
                browser = playwright.chromium.launch(executable_path=chromium, args=["--no-sandbox"])
                context = browser.new_context(viewport={"width": 1440, "height": 960}, accept_downloads=True)
                page = context.new_page()
                failures = []
                page.on("pageerror", lambda error: failures.append(str(error)))
                page.goto(base)
                expect(page.locator("#login-panel")).to_be_visible()
                expect(page.locator("#app-shell")).to_be_hidden()
                page.locator("#login-password").fill("incorrect-password")
                page.locator("#login-submit").click()
                expect(page.locator("#login-error")).to_contain_text("Invalid username or password")
                page.locator("#login-password").fill(password)
                page.locator("#login-submit").click()
                expect(page.locator("#overview-empty")).to_be_visible()
                fixture.check("administrator login, invalid-password feedback, real empty state")

                page.locator("#first-server").click()
                expect(page.locator("#create-form [name=kind]")).to_have_value("Fabric")
                expect(page.locator("#create-form [name=version]")).to_be_enabled(timeout=30000)
                require(page.locator("#create-form [name=version] option").count() > 1, "Official release catalog was not populated")
                page.locator("#create-form [name=channel]").select_option("Snapshot")
                require(page.locator("#create-form [name=version] option").count() > 1, "Official snapshot catalog was not populated")
                page.locator("#create-form [name=channel]").select_option("Release")
                page.locator("#create-form [name=version]").select_option("1.21.4")
                page.locator("#create-form [name=name]").fill("Survival")
                page.locator("#create-form [name=memory]").fill("512")
                page.locator("#create-form [name=java]").fill(java)
                page.locator("#create-form [name=jar]").set_input_files(str(jar))
                expect(page.locator("#create-submit")).to_be_disabled()
                page.screenshot(path=str(artifacts / "desktop-create.png"))
                page.set_viewport_size({"width": 390, "height": 844})
                page.locator("#create-panel").evaluate("element => { element.scrollTop = 0; }")
                page.screenshot(path=str(artifacts / "mobile-create.png"))
                require(page.evaluate("document.documentElement.scrollWidth <= innerWidth"), "Mobile create form overflows horizontally")
                page.locator("#create-submit").scroll_into_view_if_needed()
                expect(page.locator("#create-submit")).to_be_in_viewport()
                page.set_viewport_size({"width": 1440, "height": 960})
                page.locator("#create-form [name=eula]").check()
                page.locator("#create-submit").click()
                expect(page.locator("#server-workspace")).to_be_visible()
                expect(page.locator("#server-name")).to_have_text("Survival")
                expect(page.locator("#console-panel")).to_be_visible()
                expect(page.locator("#lifecycle-actions")).to_contain_text("Start", timeout=10000)
                expect(page.locator("#join-address")).to_have_text("Public access not configured")
                require("localhost" not in page.locator("#join-address").inner_text(), "Loopback exposed as public address")
                require("Local manager connected" not in page.locator("body").inner_text(), "Unwanted connection footer remains")
                fixture.check("Fabric default, official release/snapshot catalog, explicit EULA and browser JAR import")

                page.get_by_role("button", name="Start", exact=True).click()
                expect(page.locator("#server-status")).to_have_text("Online", timeout=12000)
                expect(page.locator("#console-output")).to_contain_text("Done (0.001s)")
                page.locator("#console-command").fill("status")
                page.locator("#send-command").click()
                expect(page.locator("#console-output")).to_contain_text("Fixture status: running")
                page.locator("#console-command").fill("say <img src=x onerror=alert(1)>")
                page.locator("#send-command").click()
                expect(page.locator("#console-output")).to_contain_text("<img src=x onerror=alert(1)>")
                require(page.locator("#console-output img").count() == 0, "Console output parsed as HTML")
                page.screenshot(path=str(artifacts / "desktop-console.png"))
                fixture.check("console-first workspace, live process status and command output rendered as text")

                page.get_by_role("button", name="Files", exact=True).click()
                expect(page.get_by_role("button", name="New folder", exact=True)).to_be_disabled()
                expect(page.get_by_role("button", name="world", exact=True)).to_be_visible()
                page.screenshot(path=str(artifacts / "desktop-files-running.png"))
                page.get_by_role("button", name="Settings", exact=True).click()
                expect(page.locator("#settings-form [name=name]")).to_be_disabled()
                page.get_by_role("button", name="Stop", exact=True).click()
                expect(page.locator("#server-status")).to_have_text("Offline", timeout=12000)
                page.locator("#settings-form [name=name]").fill("Friends survival")
                time.sleep(1.2)
                expect(page.locator("#settings-form [name=name]")).to_have_value("Friends survival")
                page.locator("#save-settings").click()
                expect(page.locator("#server-name")).to_have_text("Friends survival")
                page.screenshot(path=str(artifacts / "desktop-settings.png"))
                page.set_viewport_size({"width": 390, "height": 844})
                page.locator("#settings-panel").evaluate("element => { element.scrollTop = 0; }")
                page.screenshot(path=str(artifacts / "mobile-settings.png"))
                require(page.evaluate("document.documentElement.scrollWidth <= innerWidth"), "Mobile settings overflow horizontally")
                page.locator("#remove-server").scroll_into_view_if_needed()
                expect(page.locator("#remove-server")).to_be_in_viewport()
                page.set_viewport_size({"width": 1440, "height": 960})
                fixture.check("running file/settings guards, stopped settings save, edits survive polling")

                page.get_by_role("button", name="Files", exact=True).click()
                expect(page.get_by_role("button", name="New folder", exact=True)).to_be_enabled()
                page.once("dialog", lambda dialog: dialog.accept("config"))
                page.get_by_role("button", name="New folder", exact=True).click()
                page.get_by_role("button", name="config", exact=True).click()
                upload = Path(temporary) / "notes.txt"
                upload.write_text("first line\n", encoding="utf-8")
                page.locator("#files-panel input[type=file]").set_input_files(str(upload))
                page.get_by_role("button", name="notes.txt", exact=True).click()
                editor = page.get_by_role("textbox", name="Contents of config/notes.txt", exact=True)
                expect(editor).to_have_value("first line\n")
                editor.fill("saved through browser\n")
                page.get_by_role("button", name="Save file", exact=True).click()
                server = fixture.snapshot()["servers"][0]
                saved = Path(server["config"]["directory"]) / "config/notes.txt"
                deadline = time.monotonic() + 5
                while saved.read_text() != "saved through browser\n" and time.monotonic() < deadline:
                    time.sleep(.1)
                require(saved.read_text() == "saved through browser\n", "Editor did not persist bytes")
                expect(page.get_by_role("button", name="Save file", exact=True)).to_be_disabled()
                editor.fill("unsaved draft\n")
                page.once("dialog", lambda dialog: dialog.dismiss())
                page.get_by_role("button", name="Console", exact=True).click()
                expect(page.locator("#files-panel")).to_be_visible()
                expect(editor).to_have_value("unsaved draft\n")
                page.once("dialog", lambda dialog: dialog.accept())
                page.get_by_role("button", name="Console", exact=True).click()
                expect(page.locator("#console-panel")).to_be_visible()
                page.get_by_role("button", name="Files", exact=True).click()
                editor = page.get_by_role("textbox", name="Contents of config/notes.txt", exact=True)
                expect(editor).to_have_value("saved through browser\n")
                page.screenshot(path=str(artifacts / "desktop-editor.png"))
                with page.expect_download() as download:
                    page.get_by_role("link", name="Download notes.txt", exact=True).click()
                output = Path(temporary) / "downloaded.txt"
                download.value.save_as(output)
                require(output.read_text() == "saved through browser\n", "Download bytes differ")
                page.once("dialog", lambda dialog: dialog.accept("config/renamed.txt"))
                page.get_by_role("button", name="Rename notes.txt", exact=True).click()
                expect(page.get_by_role("button", name="renamed.txt", exact=True)).to_be_visible()
                page.get_by_role("button", name="Server files", exact=True).click()
                messages = []
                def confirm_delete(dialog):
                    messages.append(dialog.message)
                    dialog.accept()
                page.once("dialog", confirm_delete)
                page.get_by_role("button", name="Delete config", exact=True).click()
                expect(page.get_by_role("button", name="config", exact=True)).to_have_count(0)
                require(messages and "contents" in messages[0], "Recursive delete consequence omitted")
                fixture.check("real file browse/create/upload/edit/save/download/rename/recursive delete")

                page.get_by_role("button", name="Properties", exact=True).click()
                expect(page.locator("#properties-editor")).to_be_editable()
                current = page.locator("#properties-editor").input_value()
                page.locator("#properties-editor").fill(current + "\nmotd=Browser smoke\n")
                page.locator("#save-properties").click()
                expect(page.locator("#properties-dirty")).to_be_hidden()
                page.get_by_role("button", name="Network", exact=True).click()
                page.locator("#network-form [name=mode]").select_option("tunnel")
                expect(page.locator("#network-form [name=host]")).to_be_disabled()
                expect(page.locator("#network-form [name=external_port]")).to_be_disabled()
                expect(page.locator("#network-mode-help")).to_contain_text("Playit")
                page.locator("#network-form [name=mode]").select_option("manual")
                page.locator("#network-form [name=host]").fill("play.example.net")
                page.locator("#network-form [name=external_port]").fill("25591")
                page.locator("#save-network").click()
                expect(page.locator("#network-dirty")).to_be_hidden()
                require(fixture.snapshot()["servers"][0]["config"]["network"]["public_host"] == "play.example.net", "Network method did not persist")
                page.screenshot(path=str(artifacts / "desktop-network.png"))
                fixture.check("properties and public address configuration saved through real API")

                page.get_by_role("button", name="Console", exact=True).click()
                page.reload()
                expect(page.locator("#console-panel")).to_be_visible()
                expect(page.locator("#server-name")).to_have_text("Friends survival")
                page.set_viewport_size({"width": 390, "height": 844})
                page.screenshot(path=str(artifacts / "mobile-console.png"))
                require(page.evaluate("document.documentElement.scrollWidth <= innerWidth"), "Mobile page overflows horizontally")
                page.locator("#open-sidebar").click()
                expect(page.locator("#sidebar")).to_have_class("sidebar open")
                page.locator("#all-servers").click()
                expect(page.locator("#sidebar")).not_to_have_class("sidebar open")
                page.locator("#server-table button").click()
                page.get_by_role("button", name="Files", exact=True).click()
                expect(page.get_by_role("button", name="New folder", exact=True)).to_be_visible()
                expect(page.get_by_role("button", name="Rename server.properties", exact=True)).to_be_in_viewport()
                page.screenshot(path=str(artifacts / "mobile-files.png"))
                require(page.evaluate("document.documentElement.scrollWidth <= innerWidth"), "Mobile files overflow page")
                fixture.check("session reload opens console, narrow console/files and server drawer")

                page.locator("#open-sidebar").click()
                page.locator("#logout").click()
                expect(page.locator("#login-panel")).to_be_visible()
                require(not context.request.get(f"{base}/api/session").json()["authenticated"], "Logout did not revoke session")
                require(not failures, f"Browser JavaScript errors: {failures}")
                fixture.check("session logout and no browser JavaScript errors")
                browser.close()
        finally:
            fixture.cleanup()
            if fixture.log_path.exists():
                (artifacts / "daemon.log").write_bytes(fixture.log_path.read_bytes())
    print(f"Screenshots: {artifacts}", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--daemon", type=Path, default=REPOSITORY / "target/debug/dawg-daemon")
    parser.add_argument("--artifacts", type=Path, default=Path("/tmp/dawg-browser-smoke"))
    parser.add_argument("--chromium", default="/usr/bin/chromium")
    args = parser.parse_args()
    run(args.daemon.resolve(), args.artifacts.resolve(), args.chromium)
