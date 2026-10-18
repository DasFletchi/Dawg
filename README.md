# Dawg

Dawg is a fast, native, memory-safe Minecraft Server Manager.
It uses a decoupled architecture of a background Daemon and an Iced-based UI frontend for blazing fast desktop performance without electron.

## Known Limitations (MVP Demo status)
Due to extensive development time and environment boundaries:
- The UI is currently hardcoded to a mock server list.
- Live console log streaming from daemon to UI is incomplete.
- You must manually create server configuration objects inside the Daemon state to add more servers currently.

Future updates will patch dynamic management into the IPC endpoints.

## Releases

Push a version tag like `v1.0.0` to trigger a release build:

```bash
git tag v1.0.0
git push origin v1.0.0
```

When the workflow succeeds, the generated release archive is uploaded to the matching GitHub Release as an asset.
