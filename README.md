# Dawg

A Minecraft server manager for Linux home PCs and VPS hosts. A native Iced desktop and an authenticated web interface use the same Rust daemon, which owns the real server processes. Closing the desktop window leaves your servers running; stopping the daemon shuts them down gracefully.

The daemon serves browser administration on 127.0.0.1:8080 by default. Game servers run on your machine. Public join addresses and browser administration have separate settings.

## Server management

- Create named Fabric (default), Vanilla, Paper and modern Forge servers, or import a local server JAR. Search official version catalogs and include supported snapshots and pre-releases.
- Download an exact Minecraft version from the official provider. Vanilla and Paper downloads are checked against the provider's published digest. Fabric's launcher endpoint uses verified HTTPS and JAR validation; that endpoint does not publish a digest.
- Start, stop, and restart real Java processes; inspect their status, uptime, CPU, and memory usage.
- Read live stdout/stderr and send console commands. Logs are bounded in memory.
- Edit memory, Java executable, port, and `server.properties` while the server is stopped.
- Make an offline backup and open the server's actual directory.
- Keep server registrations across daemon restarts. Removing a registration retains its files.

With an existing server, both interfaces open its console directly. There are no sample servers or fabricated metrics. Accept the [Minecraft EULA](https://www.minecraft.net/eula) in the create dialog only if you agree to it. Download and start errors appear in the interface and server console.

## Build and run

Use an x86-64 Linux machine with Rust stable, a C/C++ compiler, CMake, pkg-config, Wayland/X11 libraries, libxkbcommon, and a working Vulkan driver. The repository currently targets `x86-64-v3`, so the CPU must support AVX2. Install `rustfmt` and `clippy` for the development checks.

The default Java setting is `auto`: official installation records the Java major required by Mojang metadata. Dawg reuses a matching installed runtime or downloads a SHA-256-verified Eclipse Temurin JRE into its private managed data directory. Minecraft 1.21.x needs Java 21; 26.3 needs Java 25. You can select an explicit Java executable instead. Imported JARs without runtime metadata use system Java when `auto` is selected. `tar` is required for backups, and `xdg-open` is used by the folder button.

```bash
cargo build --locked --workspace --all-targets
./target/debug/dawg-ui
```

With a registered server, the desktop opens its console directly. Console, Settings, Properties and Network share a compact server workspace; the list of all servers is secondary navigation. The interface embeds Source Sans 3 so its typography is consistent across Linux installations. Design decisions and the installed design skills are documented in [DESIGN.md](DESIGN.md).

The desktop starts the neighboring `dawg-daemon` binary when its socket is absent. An existing but unresponsive socket produces a recovery message instead of launching another daemon. Retry if the daemon is starting; to recover a stale socket or inspect startup failures, run the daemon directly in a terminal:

```bash
./target/debug/dawg-daemon
```

In the desktop, create a server, choose **Install**, then **Start**. Imported JARs do not need the download step. A new server can take time to download its libraries and generate its world; Dawg shows it as starting until Minecraft emits its ready message. A stopped or crashed server's console remains available for diagnosis.

## Install, boot and remove

After `cargo build --release --workspace --bins`, install for the current Linux user:

```bash
python3 scripts/install.py --binary-dir target/release
```

A release archive includes both binaries and the installer scripts; there, run `python3 scripts/install.py`. Use `--headless` on a VPS and choose `--web-bind` when needed. The installer adds a user service, desktop entry and login autostart. `--enable-linger` requests running the user service before login; system permissions must allow it. The service restores the daemon and persistent tunnel allocation after boot. Game registrations remain saved; games are started from their console.

To remove this installation and **all its managed worlds, backups, settings and credentials**:

```bash
~/.local/bin/dawg-uninstall --purge --yes
```

Without `--purge`, game data remains. Purge stops the owned daemon and games first, removes owned service/autostart entries, programs, configuration, state, cache, runtime, managed Java and tunnel files, then removes its own executable and manifest. Ownership markers and exact executable checks constrain cleanup; unrelated files/processes and symlink targets remain untouched. Customized paths are recorded at installation. An existing unmarked data directory is not silently adopted or erased.

Dawg cannot erase OS journal entries, browser history or a provider account. The daemon removes its own router leases during graceful shutdown; an unreachable router or provider allocation may require cleanup at that service. The uninstaller reports those external remnants instead of claiming success for them.

## Data and local IPC

Server registrations and managed files live in `$XDG_DATA_HOME/dawg`, or `~/.local/share/dawg`. Back up that directory if you want to move the installation. The socket is `$XDG_RUNTIME_DIR/dawg.sock`, with a fallback inside the data directory.

For an isolated installation or a test, set both paths explicitly:

```bash
export DAWG_DATA_DIR=/absolute/path/to/dawg-data
export DAWG_SOCKET=/absolute/path/to/private-runtime/dawg.sock
./target/debug/dawg-ui
```

The UI and daemon must use the same environment variables. Do not start another daemon over a live instance. The daemon locks its socket and rejects a second owner. Keep the data directory on a local filesystem and do not run multiple daemons against it.

The shared `dawg-core` crate defines versioned, newline-delimited JSON requests and responses. Requests and responses have size limits; malformed frames return an error instead of terminating the daemon.

## Browser administration and files

Start `dawg-daemon`, then access its HTTP listener. Sign in as `admin` using the generated password in `$DAWG_DATA_DIR/web-admin-password` (mode 0600). The password is never printed in application logs. Authentication hashes are stored separately; sessions expire after 12 hours and are invalidated when the daemon restarts. To set or reset a password, provide `DAWG_WEB_PASSWORD` (at least 12 characters) through a private service environment file and restart the daemon. Do not commit credentials.

For a VPS, put an HTTPS reverse proxy in front of the loopback listener, preserve the external Host header (for nginx: `proxy_set_header Host $http_host;`), and set `DAWG_WEB_SECURE_COOKIE=1`. To bind explicitly, set `DAWG_WEB_BIND=0.0.0.0:8080`; restrict access with the host firewall and use HTTPS for remote logins. The desktop and web app show the same server state and console. Closing either interface leaves games running.

The Files tab browses real files, uploads/downloads, edits text, creates folders, renames and deletes. Downloads stream; uploads are atomic and limited to 2 GiB. Editing and other writes require a stopped server. Paths are constrained to that server's managed root, with traversal and symlink escapes rejected. Directory deletion removes its contents after an explicit confirmation; the managed server root cannot be deleted from the file manager. The root `server.properties` editor uses the manager's port validation.

## Internet joining

Open **Network** and choose the connection method, then save while the game is stopped:

- **Free stable address with Playit** links a free provider account and keeps its assigned Minecraft hostname and credentials across daemon/PC restarts. No purchased domain is needed. The official pinned agent is installed only with a published SHA-256 digest; provider approval, availability and free quota still apply. This tunnel carries Minecraft TCP traffic.
- **UPnP** requests the exact configured external TCP port from a compatible router, renews the owned mapping and recreates it after a router restart. It refuses conflicting mappings and detects private/CGNAT WAN addresses. A changing public IP still needs DDNS for a stable hostname.
- **Manual public hostname/IP** is appropriate for a VPS with an assigned address, or existing DDNS/forwarding. Configure the VPS/provider firewall or router to allow the Minecraft TCP port. A configured hostname is shown as unverified; saving it cannot establish Internet reachability.

LAN addresses are labeled separately. Dawg never substitutes `localhost` for an Internet join address, never puts a WAN IP into Minecraft's `server-ip`, and never claims an unassigned tunnel is connected. Keeping the same tunnel allocation preserves its join hostname; deleting it at the provider can change that address. Browser administration is configured independently.

## Development checks

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo build --locked --workspace --all-targets
python3 scripts/smoke.py
python3 scripts/web_smoke.py
python3 scripts/test_uninstall.py
```

The smoke suite requires a JDK (`java`, `javac`, and `jar`) and `tar`. It builds a tiny local Java fixture and exercises the real daemon with temporary directories. It tests process control and IPC without downloading Minecraft or accepting its EULA on your behalf. Download provider tests use metadata and checksum fixtures; a live provider check additionally needs network access to Mojang, Paper, or Fabric.

Regression checks for reported startup, IPC, status and shutdown problems are mapped in [docs/issue-verification.md](docs/issue-verification.md).

For headless GUI development, provide Xvfb and a Mesa software Vulkan driver:

```bash
xvfb-run -a -s '-screen 0 1440x960x24 -nolisten tcp' ./target/debug/dawg-ui
```

The native desktop needs a display. The daemon also provides the browser interface described below.

## Scope

The daemon manages game processes on its own host. Browser access lets you administer that host remotely. Remote execution agents, scheduled backups, player administration and a mod/plugin marketplace are not implemented. Use the web file manager or the server directory to manage mods, plugins and worlds. A backup is a local archive, not an off-machine disaster recovery system.

Server installation depends on the provider's supported versions and availability. Fabric and Paper may download additional libraries on their first launch. Forge installation currently supports its modern argument-file layout (Minecraft 1.17 onward) and verifies the official installer digest. Each loader lists its own published game versions; a Fabric snapshot does not imply a Forge or Paper build exists. A successful build or process-fixture test does not establish that every Minecraft version and modpack is compatible.

## Releases

The GitHub Actions workflow checks formatting, Clippy, tests, and a workspace build. Pushing a version tag such as `v1.0.0` builds release binaries and publishes an archive and checksum to that GitHub release.

## Real Minecraft verification

`python3 scripts/real_minecraft_smoke.py --jar /path/server.jar --metadata /path/version.json --accept-eula` runs an isolated game test using an explicitly accepted EULA and an official SHA1-verified JAR. It checks Minecraft startup, status handshake and pong, console commands, world persistence and restart. The test has passed against Minecraft 1.21.11. It does not establish public Internet reachability or test player authentication; those require an external client and working Mojang authentication endpoints.
