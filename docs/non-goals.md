# Non-Goals — fs2-devsync

Explicitly deferred work. These items are intentionally **not** part of the MVP. They are recorded here so contributors do not accidentally build them or assume they are missing bugs.

## 1. Git replacement

FS2 does not replace Git in MVP. Git is treated as a tool that projects may use inside the synced workspace. A future source-control model (snapshots, tags, per-file permissions, change-level privacy) is research-only until proven.

## 2. Automatic code merge

FS2 is a sync system, not a merge engine. Concurrent writes produce explicit conflict artifacts or a conflict state. Future LLM-assisted or tool-based merge resolution is out of scope.

## 3. End-to-end encrypted metadata

MVP stores plaintext metadata on the server: filenames, paths, sizes, mtimes, directory structure. Content and secrets are encrypted; metadata is not private from the service operator in MVP. E2EE metadata complicates conflict detection, listing, and web management and is deferred.

## 4. Artifact cache

A remote artifact cache for generated dependencies (`node_modules/.pnpm/**`, `target/`, etc.) keyed by platform/lockfile hash is deferred. It substantially increases complexity and storage cost. MVP treats generated outputs as local cache only.

## 5. Web dashboard

No web UI in MVP. Management is via CLI.

## 6. Editor integrations

No VS Code extension or other editor integration in MVP. The local RPC API is defined to enable this later.

## 7. Windows support

Out of MVP. Future options: WinFsp, Windows Credential Manager, NTFS path policy. Do not let Windows delay macOS/Linux MVP.

## 8. Team workspaces

No multi-user teams in MVP. Single-user account model with multiple devices. The data model is designed so team ACLs can be added later, but they are not implemented.

## 9. Cloud agent mode as a first-class surface

The protocol supports ephemeral Linux workers, but no dedicated `fs2 agent` CLI is shipped in MVP. Agent-style usage is possible through materialized checkout and env exec.

## 10. Hiding filenames/structure from the server

Not solved in MVP. Metadata privacy is deferred (see E2EE metadata above).

## 11. Malicious local root user

Not fully solved in MVP. Treat local root as trusted.

## 12. Compromised enrolled device

Not fully solved in MVP. Device revocation stops new key envelopes but does not guarantee a revoked device cannot read previously cached files or old keys.

## 13. Malicious package install scripts reading materialized secrets

Not solved in MVP. Materialized `.env` files are `0600` but a process running as the same user can read them.

## 14. Automatic `git merge` / `git pull --rebase` / branch switching

Never automatic. The user remains in control of commit/merge semantics. FS2 may run safe `git fetch` but never merge, rebase, reset, or switch branches without an explicit user command.
