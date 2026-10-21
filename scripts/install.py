#!/usr/bin/env python3
"""Install Dawg for the current Linux user, with a managed user service."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import shutil
import socket
import stat
import sys
import uuid

sys.dont_write_bytecode = True
import uninstall as common


def write_private(path: Path, content: str, mode: int = 0o600) -> None:
    common.no_symlink(path)
    temporary = path.with_name(f".{path.name}.{uuid.uuid4()}.tmp")
    descriptor = os.open(temporary, os.O_CREAT | os.O_EXCL | os.O_WRONLY, mode)
    try:
        with os.fdopen(descriptor, "w") as output:
            output.write(content)
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


def owned_directory(path: Path, identity: str, paths: dict[str, Path], adopt: bool = False) -> None:
    common.safe_root(path, paths)
    if path.exists() and not common.marker(path, identity) and any(path.iterdir()):
        if not adopt:
            raise ValueError(f"Refusing to claim nonempty unowned directory: {path}. Use a new directory; --adopt-data is available only for an existing Dawg data layout.")
        # Never silently adopt a home/project directory or arbitrary files.
        allowed = {"servers", "backups", "servers.json", "network-leases.json", "web-auth.json",
                   "web-admin-password", "tunnel", "run", "daemon.lock"}
        if not (path / "servers.json").is_file() or any(child.name not in allowed for child in path.iterdir()):
            raise ValueError("--adopt-data requires an existing Dawg-only data directory containing servers.json.")
        records = common.read_json(path / "servers.json")
        if not isinstance(records, list):
            raise ValueError("Existing servers.json is not a Dawg registration list.")
        for record in records:
            identity_part = record["id"]
            uuid.UUID(identity_part)
            if Path(record["directory"]) != path / "servers" / identity_part:
                raise ValueError("Existing server paths are outside this Dawg data directory; adoption was refused.")
    path.mkdir(mode=0o700, parents=True, exist_ok=True)
    if path.stat().st_uid != os.getuid():
        raise ValueError(f"Installation directory is owned by another user: {path}")
    path.chmod(0o700)
    write_private(path / common.MARKER, json.dumps({"application": common.APP, "install_id": identity, "version": common.VERSION}) + "\n")


def systemd_quote(value: str | Path, specifiers: bool = False) -> str:
    text = str(value)
    if specifiers:
        text = text.replace("%", "%%")
    return '"' + text.replace("\\", "\\\\").replace('"', '\\"') + '"'


def validate_bind(value: str) -> str:
    if value.startswith("["):
        host, separator, port = value[1:].partition("]:")
    else:
        host, separator, port = value.rpartition(":")
    if not separator or not port.isdigit() or not 1 <= int(port) <= 65535:
        raise ValueError("--web-bind requires an IP address and port, e.g. 127.0.0.1:8080 or [::1]:8080")
    try:
        socket.inet_pton(socket.AF_INET6 if ":" in host else socket.AF_INET, host)
    except OSError:
        raise ValueError("--web-bind requires a numeric IPv4/IPv6 address, not a hostname") from None
    return value


def install(args: argparse.Namespace) -> None:
    if sys.platform != "linux":
        raise ValueError("This installer supports Linux user installations.")
    paths = common.locations(args.prefix, args.data_dir)
    bind = validate_bind(args.web_bind)
    binary_dir = common.absolute(args.binary_dir.absolute())
    names = ["dawg-daemon"] + ([] if args.headless else ["dawg-ui"])
    for name in names:
        source = binary_dir / name
        common.no_symlink(source)
        if not source.is_file() or not os.access(source, os.X_OK):
            raise ValueError(f"Missing executable {source}. Build the release first or pass --binary-dir.")
    for name in ("bin", "unit", "desktop", "autostart"):
        common.no_symlink(paths[name])
    manifest_path = paths["config"] / "install-manifest.json"
    if manifest_path.exists():
        previous = common.validate_manifest(manifest_path, paths)
        identity = previous["install_id"]
        common.terminate_owned(paths)
    else:
        identity = str(uuid.uuid4())
    wants = paths["wants"]
    common.no_symlink(wants.parent)
    if wants.is_symlink() and wants.resolve() != paths["unit"]:
        raise ValueError(f"A foreign service link already exists: {wants}")
    if wants.exists() and not wants.is_symlink():
        raise ValueError(f"A foreign service file already exists: {wants}")
    targets = [paths["bin"] / name for name in ("dawg-daemon", "dawg-ui", "dawg", "dawg-uninstall")]
    targets += [paths["unit"], paths["desktop"], paths["autostart"]]
    previous_files = {record["path"] for record in previous["files"]} if manifest_path.exists() else set()
    for target in targets:
        common.no_symlink(target)
        if target.exists() and str(target) not in previous_files:
            raise ValueError(f"An unrelated file already occupies an install target: {target}")
    # Preflight every root before creating or changing an ownership marker.
    for name in ("config", "data", "cache", "state", "runtime"):
        common.safe_root(paths[name], paths)
        if paths[name].exists() and any(paths[name].iterdir()) and not common.marker(paths[name], identity):
            if not (name == "data" and args.adopt_data):
                raise ValueError(f"Unowned nonempty directory {paths[name]}; refusing to overwrite or claim it.")
    for name in ("config", "data", "cache", "state", "runtime"):
        owned_directory(paths[name], identity, paths, adopt=(name == "data" and args.adopt_data))
    paths["bin"].mkdir(parents=True, exist_ok=True)
    installed_files = []

    def record(path: Path) -> None:
        installed_files.append({"path": str(path), "sha256": common.digest(path)})

    for name in names:
        target = paths["bin"] / name
        temporary = target.with_name(f".{name}.{uuid.uuid4()}.tmp")
        shutil.copyfile(binary_dir / name, temporary)
        temporary.chmod(0o700)
        temporary.replace(target)
        record(target)
    environment = {"DAWG_DATA_DIR": str(paths["data"]),
                   "DAWG_SOCKET": str(paths["runtime"] / "dawg.sock"), "DAWG_WEB_BIND": bind}
    write_private(paths["config"] / "environment.json", json.dumps(environment, indent=2) + "\n")
    write_private(paths["config"] / "daemon.env", "".join(f"{name}={systemd_quote(value)}\n" for name, value in environment.items()))
    runtime_helper = ("#!/usr/bin/env python3\nimport json,os\nfrom pathlib import Path\n"
                      f"root=Path({str(paths['runtime'])!r})\n"
                      "for part in [*reversed(root.parents),root]:\n"
                      " if part.is_symlink(): raise SystemExit('Refusing a symlink in the Dawg runtime path')\n"
                      "root.mkdir(mode=0o700,parents=True,exist_ok=True)\n"
                      "if root.stat().st_uid != os.getuid(): raise SystemExit('Foreign runtime owner')\n"
                      f"owner={{'application':'dawg','install_id':{identity!r},'version':1}}\n"
                      f"marker=root/{common.MARKER!r}\n"
                      "if marker.is_symlink(): raise SystemExit('Foreign runtime ownership marker')\n"
                      "if marker.exists():\n"
                      " if json.loads(marker.read_text()) != owner: raise SystemExit('Foreign runtime ownership marker')\n"
                      "else:\n"
                      " if any(root.iterdir()): raise SystemExit('Unowned nonempty runtime directory')\n"
                      " fd=os.open(marker,os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW,0o600)\n"
                      " with os.fdopen(fd,'w') as f: json.dump(owner,f)\n"
                      "root.chmod(0o700)\n")
    write_private(paths["config"] / "prepare-runtime.py", runtime_helper)
    unit = ("[Unit]\nDescription=Dawg Minecraft server manager\nAfter=network-online.target\nWants=network-online.target\n\n"
            "[Service]\nType=simple\nUMask=0077\n"
            f"EnvironmentFile={systemd_quote(paths['config'] / 'daemon.env', True)}\n"
            f"ExecStart={systemd_quote(paths['bin'] / 'dawg-daemon', True)}\n"
            f"ExecStartPre=/usr/bin/env python3 {systemd_quote(paths['config'] / 'prepare-runtime.py', True)}\n"
            "Restart=on-failure\nRestartSec=3\nKillSignal=SIGTERM\nKillMode=mixed\nTimeoutStopSec=120\n\n"
            "[Install]\nWantedBy=default.target\n")
    paths["unit"].parent.mkdir(parents=True, exist_ok=True)
    write_private(paths["unit"], unit)
    record(paths["unit"])
    if not args.headless:
        launcher = ("#!/usr/bin/env python3\nimport json, os, subprocess\nfrom pathlib import Path\n"
                    f"os.environ.update(json.loads(Path({str(paths['config'] / 'environment.json')!r}).read_text()))\n"
                    f"subprocess.run(['/usr/bin/env','python3',{str(paths['config'] / 'prepare-runtime.py')!r}],check=True)\n"
                    "try:\n subprocess.run(['systemctl', '--user', 'start', 'dawg.service'], check=False, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=15)\n"
                    "except (OSError, subprocess.TimeoutExpired):\n pass\n"
                    f"os.execv({str(paths['bin'] / 'dawg-ui')!r}, [{str(paths['bin'] / 'dawg-ui')!r}])\n")
        write_private(paths["bin"] / "dawg", launcher, 0o700)
        record(paths["bin"] / "dawg")
        paths["desktop"].parent.mkdir(parents=True, exist_ok=True)
        # Desktop Exec reserves % for field codes; quotes/backslashes must also be escaped.
        desktop_exec = systemd_quote(paths["bin"] / "dawg", True).replace("\\", "\\\\")
        desktop = ("[Desktop Entry]\nType=Application\nName=Dawg\nComment=Minecraft server manager\n"
                   f"Exec={desktop_exec}\nIcon=utilities-terminal\nTerminal=false\nCategories=Game;Utility;\n")
        write_private(paths["desktop"], desktop)
        record(paths["desktop"])
    uninstall_source = Path(__file__).with_name("uninstall.py").read_text()
    uninstall_source = uninstall_source.replace("DEFAULT_PREFIX = None", f"DEFAULT_PREFIX = {str(paths['prefix'])!r}").replace("DEFAULT_DATA_DIR = None", f"DEFAULT_DATA_DIR = {str(paths['data'])!r}")
    write_private(paths["bin"] / "dawg-uninstall", uninstall_source, 0o700)
    record(paths["bin"] / "dawg-uninstall")
    manifest = {"application": common.APP, "version": common.VERSION, "install_id": identity,
                "roots": {name: str(paths[name]) for name in ("config", "data", "cache", "state", "runtime")},
                "files": installed_files}
    write_private(manifest_path, json.dumps(manifest, indent=2) + "\n")
    for old in previous["files"] if previous_files else []:
        if old["path"] not in {record["path"] for record in installed_files}:
            common.remove_file(Path(old["path"]))
    common.systemctl("daemon-reload", timeout=15)
    enabled = common.systemctl("enable", "dawg.service", timeout=15)
    # Persist login autostart even if this machine has no running user bus now.
    paths["wants"].parent.mkdir(parents=True, exist_ok=True)
    if not paths["wants"].exists() and not paths["wants"].is_symlink():
        paths["wants"].symlink_to(paths["unit"])
    if enabled.returncode:
        print("User service enabled on disk; systemctl --user was unavailable. It will start at the next user login.", file=sys.stderr)
    if args.enable_linger:
        import subprocess
        result = subprocess.run(["loginctl", "enable-linger", str(os.getuid())], check=False)
        if result.returncode:
            print("Could not enable lingering. An administrator can run loginctl enable-linger for your user.", file=sys.stderr)
    if not args.no_start:
        started = common.systemctl("start", "dawg.service", timeout=30)
        if started.returncode:
            print("Installed, but the user service could not start. Run systemctl --user start dawg.service in your login session.", file=sys.stderr)
    print(f"Installed Dawg in {paths['bin']}. Web listener: {bind}; data: {paths['data']}.")
    print(f"Remove everything with: {paths['bin'] / 'dawg-uninstall'} --purge --yes")
    print("Autostart runs at user login. Use --enable-linger to start the user service at boot before login. Java is installed separately.")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    distribution = Path(__file__).resolve().parent.parent
    default_binaries = distribution if (distribution / "dawg-daemon").is_file() else distribution / "target/release"
    parser.add_argument("--binary-dir", type=Path, default=default_binaries)
    parser.add_argument("--prefix", type=Path, help="Binary prefix, default ~/.local")
    parser.add_argument("--data-dir", type=Path, help="Dedicated custom data directory")
    parser.add_argument("--adopt-data", action="store_true", help="Explicitly take ownership of a preexisting Dawg-only data layout")
    parser.add_argument("--web-bind", default="127.0.0.1:8080", help="Web listener, e.g. 0.0.0.0:8080 for a VPS with a configured firewall/TLS proxy")
    parser.add_argument("--no-start", action="store_true", help="Enable login autostart without starting now")
    parser.add_argument("--headless", action="store_true", help="Install only the daemon and uninstaller for a VPS")
    parser.add_argument("--enable-linger", action="store_true", help="Ask loginctl to start this user's service at boot, before login")
    try:
        install(parser.parse_args(argv))
    except (OSError, ValueError, KeyError) as error:
        print(f"Dawg install: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
