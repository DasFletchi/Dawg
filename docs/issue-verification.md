# Reported daemon and desktop regressions

The following checks use isolated temporary directories and local fixtures. They
do not need provider downloads or acceptance of the Minecraft EULA.

| Issue | Behavior | Verification |
| --- | --- | --- |
| [#4](https://github.com/DasFletchi/Dawg/issues/4) | `Snapshot` lists actual registrations and reports status and PID for each requested server. Unknown server IDs return an error. | `scripts/smoke.py` keeps one real JVM running while another registered server is stopped, checks both IDs and different states in the same snapshot, and rejects an unknown log ID. |
| [#5](https://github.com/DasFletchi/Dawg/issues/5) | A fresh installation has no fabricated default server. Creation assigns a managed directory before a game can start. | The smoke suite checks the empty registry, rejects invalid creation, imports a fixture into the real managed directory and starts it successfully. |
| [#6](https://github.com/DasFletchi/Dawg/issues/6) | IPC uses bounded newline-delimited JSON frames in both directions. | The smoke suite fragments and batches requests and checks malformed-frame recovery. UI transport tests assemble a fragmented response larger than 4 KiB and reject incomplete or oversized replies. |
| [#7](https://github.com/DasFletchi/Dawg/issues/7) | Lifetime socket/data locks reject duplicate daemons. The desktop auto-starts only when the socket path is absent; stale sockets, ordinary files and dangling links are preserved. | The smoke suite verifies duplicate rejection, continued access and an unchanged socket inode. Five UI startup tests check live reuse, stale sockets, ordinary files, dangling links and exactly one launch for an absent socket. |
| [#8](https://github.com/DasFletchi/Dawg/issues/8) | Daemon shutdown sends `stop` to every managed game, waits for exit and reaps its process before the runtime ends. IPC listener errors also use this cleanup path. | The smoke suite sends SIGTERM while one JVM is Running and another is Starting. Both must write a save marker exclusively from their `stop` command handler and both PIDs must disappear before daemon exit. |

Run from the repository root with the Rust toolchain, native build dependencies
and a JDK activated:

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo build --locked --workspace --all-targets
python3 scripts/smoke.py
python3 scripts/web_smoke.py
python3 scripts/test_uninstall.py --real-binary-dir target/debug
```

These tests validate management and shutdown behavior. They do not establish
external Internet reachability or compatibility with every Minecraft modpack.
