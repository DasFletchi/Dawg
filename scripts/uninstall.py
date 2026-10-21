#!/usr/bin/env python3
"""Remove this Linux user installation. --purge also erases owned server data."""
from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import stat
import subprocess
import sys
import time
from contextlib import contextmanager

APP = "dawg"
MARKER = ".dawg-install-owner"
VERSION = 1
DEFAULT_PREFIX = None
DEFAULT_DATA_DIR = None


def absolute(value: str | Path) -> Path:
    path = Path(value).expanduser()
    if not path.is_absolute() or ".." in path.parts or any(c in str(path) for c in "\0\n\r"):
        raise ValueError(f"An absolute path without '..' or control characters is required: {path}")
    return path


def locations(prefix: str | Path | None = None, data_dir: str | Path | None = None) -> dict[str, Path]:
    home = absolute(os.environ["HOME"])
    config = absolute(os.environ.get("XDG_CONFIG_HOME", str(home / ".config")))
    data = absolute(os.environ.get("XDG_DATA_HOME", str(home / ".local/share")))
    state = absolute(os.environ.get("XDG_STATE_HOME", str(home / ".local/state")))
    cache = absolute(os.environ.get("XDG_CACHE_HOME", str(home / ".cache")))
    prefix_path = absolute(prefix or home / ".local")
    runtime = (absolute(os.environ["XDG_RUNTIME_DIR"]) / APP if os.environ.get("XDG_RUNTIME_DIR") else state / APP / "run")
    return {
        "home": home, "prefix": prefix_path,
        "config_base": config, "data_base": data, "state_base": state, "cache_base": cache,
        "runtime_base": absolute(os.environ["XDG_RUNTIME_DIR"]) if os.environ.get("XDG_RUNTIME_DIR") else state,
        "bin": prefix_path / "bin",
        "config": config / APP,
        "data": absolute(data_dir or os.environ.get("DAWG_DATA_DIR", str(data / APP))),
        "cache": cache / APP,
        "state": state / APP,
        "runtime": runtime,
        "unit": config / "systemd/user/dawg.service",
        "desktop": data / "applications/dawg.desktop",
        "autostart": config / "autostart/dawg.desktop",
        "wants": config / "systemd/user/default.target.wants/dawg.service",
    }


def no_symlink(path: Path) -> None:
    for part in [*reversed(path.parents), path]:
        try:
            info = part.lstat()
        except FileNotFoundError:
            continue
        if stat.S_ISLNK(info.st_mode):
            raise ValueError(f"Refusing a symlink in an installation path: {part}")


def safe_root(path: Path, paths: dict[str, Path]) -> None:
    forbidden = {Path("/"), paths["home"], paths["prefix"], paths["bin"], paths["unit"].parent,
                 paths["desktop"].parent, paths["autostart"].parent,
                 paths["config_base"], paths["data_base"], paths["state_base"], paths["cache_base"], paths["runtime_base"],
                 *paths["home"].parents, *Path.cwd().parents, Path.cwd()}
    if path in forbidden or path in paths["prefix"].parents:
        raise ValueError(f"Refusing a broad or unrelated cleanup root: {path}")
    no_symlink(path)


def digest(path: Path) -> str:
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def read_json(path: Path):
    no_symlink(path)
    info = path.stat()
    if info.st_uid != os.getuid() or not stat.S_ISREG(info.st_mode) or info.st_size > 1024 * 1024:
        raise ValueError(f"Invalid owned metadata file: {path}")
    return json.loads(path.read_text())


def marker(path: Path, identity: str) -> bool:
    try:
        return read_json(path / MARKER) == {"application": APP, "install_id": identity, "version": VERSION}
    except (OSError, ValueError):
        return False


def systemctl(*args: str, timeout: int = 120) -> subprocess.CompletedProcess:
    try:
        return subprocess.run(["systemctl", "--user", *args], text=True,
                              capture_output=True, timeout=timeout, check=False)
    except (FileNotFoundError, subprocess.TimeoutExpired) as error:
        return subprocess.CompletedProcess(["systemctl", *args], 1, "", str(error))


def process_environment(pid: int) -> dict[str, str]:
    try:
        content = Path(f"/proc/{pid}/environ").read_bytes()
        return dict(item.decode(errors="replace").split("=", 1) for item in content.split(b"\0") if b"=" in item)
    except (OSError, ValueError):
        return {}


def owns_process(paths: dict[str, Path], binary: str, pid: int) -> bool:
    expected = paths["bin"] / binary
    entry = Path(f"/proc/{pid}")
    try:
        if entry.stat().st_uid != os.getuid():
            return False
        executable = os.readlink(entry / "exe").removesuffix(" (deleted)")
        env = process_environment(pid)
        return executable == str(expected) and env.get("DAWG_DATA_DIR") == str(paths["data"]) and env.get("DAWG_SOCKET") == str(paths["runtime"] / "dawg.sock")
    except (OSError, ValueError):
        return False


def processes(paths: dict[str, Path], binary: str) -> list[int]:
    found = []
    for entry in Path("/proc").iterdir():
        if entry.name.isdigit() and owns_process(paths, binary, int(entry.name)):
            found.append(int(entry.name))
    return found


def signal_owned(paths: dict[str, Path], binary: str, pid: int) -> None:
    try:
        descriptor = os.pidfd_open(pid)
    except ProcessLookupError:
        return
    except (AttributeError, OSError):
        # Older kernels lack pidfd; recheck the executable and scope immediately.
        if owns_process(paths, binary, pid):
            try:
                os.kill(pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
        return
    try:
        if owns_process(paths, binary, pid):
            try:
                signal.pidfd_send_signal(descriptor, signal.SIGTERM)
            except ProcessLookupError:
                pass
    finally:
        os.close(descriptor)


def terminate_owned(paths: dict[str, Path]) -> None:
    # The UI may otherwise immediately start a new daemon during removal.
    ui = processes(paths, "dawg-ui")
    for pid in ui:
        signal_owned(paths, "dawg-ui", pid)
    stopped = systemctl("stop", "dawg.service")
    daemons = processes(paths, "dawg-daemon")
    for pid in daemons:
        signal_owned(paths, "dawg-daemon", pid)
    deadline = time.monotonic() + 120
    while processes(paths, "dawg-daemon") or processes(paths, "dawg-ui"):
        if time.monotonic() >= deadline:
            raise ValueError("Dawg has not stopped gracefully. No files were deleted. Stop its UI and daemon before retrying.")
        time.sleep(0.1)
    if stopped.returncode:
        print("The user service was unavailable; checked this installation's processes and locks directly.", file=sys.stderr)


@contextmanager
def open_parent(path: Path):
    """Anchor each ancestor using O_NOFOLLOW, including during removal."""
    descriptor = os.open("/", os.O_RDONLY | os.O_DIRECTORY)
    try:
        for component in path.parent.parts[1:]:
            child = os.open(component, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=descriptor)
            os.close(descriptor)
            descriptor = child
        yield descriptor
    finally:
        os.close(descriptor)


def remove_file(path: Path) -> None:
    try:
        with open_parent(path) as parent:
            os.unlink(path.name, dir_fd=parent)
    except FileNotFoundError:
        pass


def remove_tree(path: Path) -> None:
    try:
        with open_parent(path) as parent:
            # Linux's fd-based rmtree unlinks nested symlinks without following them.
            shutil.rmtree(path.name, dir_fd=parent)
    except FileNotFoundError:
        pass


@contextmanager
def hold_daemon_locks(paths: dict[str, Path]):
    descriptors = []
    try:
        for path in [paths["data"] / "daemon.lock", paths["runtime"] / "dawg.lock"]:
            if not path.exists():
                continue
            no_symlink(path)
            if not path.is_file():
                raise ValueError(f"Invalid daemon lock: {path}")
            descriptor = os.open(path, os.O_RDWR | os.O_NOFOLLOW)
            descriptors.append(descriptor)
            try:
                fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                raise ValueError("A daemon still owns this installation's lock. No files were deleted; stop Dawg and retry.") from None
        yield
    finally:
        for descriptor in descriptors:
            os.close(descriptor)


def validate_manifest(path: Path, paths: dict[str, Path]) -> dict:
    no_symlink(path)
    manifest = read_json(path)
    if manifest.get("application") != APP or manifest.get("version") != VERSION:
        raise ValueError("This is not a supported Dawg installation manifest.")
    import uuid
    identity = manifest.get("install_id", "")
    uuid.UUID(identity)
    roots = manifest.get("roots", {})
    for name in ("config", "data", "cache", "state", "runtime"):
        root = absolute(roots[name])
        if root != paths[name]:
            raise ValueError(f"Manifest {name} root does not match this installation. Supply the original --prefix/--data-dir and XDG environment.")
        safe_root(root, paths)
        if root.exists() and root.stat().st_uid != os.getuid():
            raise ValueError(f"Cleanup root is owned by another user: {root}")
        if root.exists() and not marker(root, identity):
            raise ValueError(f"Missing or foreign ownership marker; refusing cleanup: {root}")
    if path != paths["config"] / "install-manifest.json":
        raise ValueError("Manifest must be inside this installation's private config directory.")
    permitted = {paths["bin"] / name for name in ("dawg", "dawg-ui", "dawg-daemon", "dawg-uninstall")}
    permitted |= {paths["unit"], paths["desktop"], paths["autostart"]}
    files = manifest.get("files", [])
    if not files or len(files) > len(permitted):
        raise ValueError("Invalid installation file list.")
    for record in files:
        target = absolute(record["path"])
        if target not in permitted:
            raise ValueError(f"Manifest names an unrelated file: {target}")
        no_symlink(target)
        if target.exists() and (target.stat().st_uid != os.getuid() or digest(target) != record["sha256"]):
            raise ValueError(f"Installed file was replaced or edited; refusing to remove it: {target}")
    wants = paths["wants"]
    no_symlink(wants.parent)
    if wants.is_symlink():
        if wants.resolve() != paths["unit"]:
            raise ValueError(f"Refusing a foreign service link: {wants}")
    elif wants.exists():
        raise ValueError(f"Refusing a foreign service entry: {wants}")
    return manifest


def remaining_external_state(paths: dict[str, Path]) -> None:
    leases = paths["data"] / "network-leases.json"
    if leases.exists():
        try:
            outstanding = read_json(leases)
            if outstanding:
                ports = ", ".join(str(lease.get("external_port", "?")) for lease in outstanding)
                print(f"Warning: router cleanup could not be confirmed for TCP ports {ports}. Remove remaining 'Dawg <UUID>' rules on the router; finite leases expire. Local ownership records will be erased.", file=sys.stderr)
        except (OSError, ValueError):
            print("Warning: router lease metadata could not be read; inspect the router for remaining Dawg rules.", file=sys.stderr)
    tunnel = paths["data"] / "tunnel"
    if tunnel.exists():
        print("Provider tunnel allocations may remain in your Playit account. Remove them in the provider dashboard; local credentials and agent files will be erased.", file=sys.stderr)


def uninstall(args: argparse.Namespace) -> None:
    paths = locations(args.prefix, args.data_dir)
    manifest_path = paths["config"] / "install-manifest.json"
    manifest = validate_manifest(manifest_path, paths)
    if args.purge and not args.yes:
        print(f"Permanently erase {paths['data']} including worlds, backups and credentials.")
        if input("Type DELETE DAWG to continue: ") != "DELETE DAWG":
            raise ValueError("Purge cancelled; nothing was changed.")
    terminate_owned(paths)
    with hold_daemon_locks(paths):
        # Recheck after shutdown, before making any deletion concrete.
        manifest = validate_manifest(manifest_path, paths)
        systemctl("disable", "dawg.service", timeout=15)
        # systemctl creates this one symlink in a shared directory. Only remove
        # a link that resolves to the exact unit installed by this manifest.
        wants = paths["wants"]
        no_symlink(wants.parent)
        if wants.is_symlink():
            if wants.resolve() != paths["unit"]:
                raise ValueError(f"A foreign service link was left untouched: {wants}")
            remove_file(wants)
        elif wants.exists():
            raise ValueError(f"A foreign service entry was left untouched: {wants}")
        if args.purge:
            remaining_external_state(paths)
        for record in manifest["files"]:
            target = Path(record["path"])
            if not args.purge and target.name == "dawg-uninstall":
                continue
            remove_file(target)
        if args.purge:
            for name in ("runtime", "cache", "state", "data", "config"):
                remove_tree(paths[name])
        else:
            remove_file(paths["config"] / "daemon.env")
            remove_file(paths["config"] / "environment.json")
            remove_file(paths["config"] / "prepare-runtime.py")
    systemctl("daemon-reload", timeout=15)
    print("Dawg's owned local files, worlds, backups and credentials were erased." if args.purge else
          "Dawg was uninstalled. Data is retained; run dawg-uninstall --purge --yes to erase it.")
    print("Operating-system journals, browser history, external backups and provider accounts are outside this installation's control.")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--prefix", type=Path, default=DEFAULT_PREFIX, help="Original installation prefix (default ~/.local)")
    parser.add_argument("--data-dir", type=Path, default=DEFAULT_DATA_DIR, help="Original custom DAWG_DATA_DIR")
    parser.add_argument("--purge", action="store_true", help="Permanently erase owned data, worlds, backups, credentials and all installed files")
    parser.add_argument("--yes", action="store_true", help="Skip the DELETE DAWG prompt for an explicit --purge")
    args = parser.parse_args(argv)
    try:
        uninstall(args)
    except (OSError, ValueError, KeyError, EOFError) as error:
        print(f"Dawg uninstall: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
