# Project design skills

Installed for the user's request to replace the generated-looking UI with a more deliberate native interface.

- `frontend-design`: upstream https://github.com/anthropics/skills, commit `dbd4588f9e1033efb41dad4bef2f7947c8993d44`, source `skills/frontend-design`. Original Apache-2.0 license included.
- `impeccable`: upstream https://github.com/pbakaus/impeccable, commit `dad8dbb3aac21ad1fa236c110315ef4abece23e8`, source `.agents/skills/impeccable`. Original license and third-party notices included. Its versioned engine launcher checks the release checksum before execution. No design hooks were installed.

Read PRODUCT.md and DESIGN.md for this project's actual native Linux constraints and the user's console-first preference. For native UI work, apply Impeccable's Operate, quieter, distill and craft-floor guidance; the web-markup detector does not validate Rust/Iced. Upstream skill copies are unmodified; project-specific decisions belong in the root context documents.

In this cloud workspace, use a writable engine cache when invoking the optional launcher:

```bash
IMPECCABLE_HOME=/workspace/.dawg-env/impeccable .agents/skills/impeccable/scripts/impeccable context --target dawg-ui/src/main.rs
```
