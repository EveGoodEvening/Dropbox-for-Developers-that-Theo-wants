# FS2 MVP Boundaries

FS2 is an experimental developer sync system for a single user with multiple devices. The MVP keeps the scope intentionally narrow so the first vertical slice can protect local work, synchronize metadata before bytes, and avoid unsafe assumptions around Git, generated directories, and secrets.

## Build for MVP

- Single-user account model: one account can enroll multiple personal devices. Team sharing and ACLs are out of scope.
- macOS and Linux desktop support only. Windows support is deferred.
- FUSE mount for the desktop client: the normal user experience is a mounted `~/code` tree backed by the daemon.
- Materialized checkout mode for tests and cloud/agent workflows where FUSE is unavailable or unnecessary.
- Postgres metadata backend for authoritative workspaces, nodes, revisions, operations, devices, blobs, env records, and key envelopes.
- S3-compatible blob store for encrypted file bytes. Local filesystem blob storage is allowed for development and tests.
- Local SQLite cache for workspace metadata, hydration state, pending operations, blob cache state, rules, and conflicts.
- Encrypted file blobs are preferred by default before upload.
- Encrypted environment variable sync is required. The service must not receive plaintext env values in normal operation.
- `.git` internals are excluded by default; FS2 is Git-aware, not a blind `.git` synchronizer.

## Do not build for MVP

- Team sharing, organization accounts, roles, permissions, or audit logs.
- Windows filesystem or credential-manager support.
- Git replacement, automatic code merges, or automatic branch switching.
- End-to-end encrypted metadata. Filenames, sizes, mtimes, and directory structure are visible to the service operator in MVP.
- Remote artifact caching for generated outputs such as `node_modules` or `target`.
- Web dashboard and editor integrations.

## Contributor rule

If a proposed change requires expanding the MVP boundary above, record it as deferred instead of implementing it in the first vertical slice.
