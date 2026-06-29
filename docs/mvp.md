# MVP Scope — fs2-devsync

**Status:** authoritative MVP boundary. Anything not listed here is out of scope for the first implementation.

## In scope

- **Single-user account model.** One user, multiple devices. No team sharing, no ACLs, no member roles.
- **macOS and Linux only.** Windows is explicitly out of scope for MVP.
- **FUSE mount required for the desktop client.** macFUSE on macOS, FUSE3 on Linux.
- **Materialized checkout mode allowed** for tests and cloud-agent style workflows where FUSE is unavailable.
- **Postgres metadata backend.** Authoritative metadata lives in Postgres.
- **S3-compatible blob store.** File content lives in an S3/R2-compatible object store; a local filesystem implementation is provided for development and tests.
- **Local SQLite cache.** Each client keeps metadata, hydration state, pending ops, blob cache, rules, and conflicts in a local SQLite database.
- **Encrypted file blobs preferred.** File content is encrypted client-side before upload by default.
- **Encrypted env var sync required.** Environment variable values are encrypted client-side; the backend never sees plaintext values.
- **`.git` internals excluded by default.** Git-aware metadata is recorded; `.git` internals are never uploaded as ordinary files.
- **Metadata-first sync.** Directory structure syncs before file bytes; file content is hydrated on demand.
- **Developer-specific ignore semantics** via `.fs2ignore` and `.fs2/config.toml` with actions: `ignore`, `local-only`, `generated`, `lazy`, `pin`, `normal`, `secret`, `dependency-cache`.
- **Safe conflict handling.** Concurrent writes produce explicit conflict artifacts; deletions use tombstones.
- **On-demand hydration and pinning.** Read triggers download; pinning keeps files offline-available.
- **Cache eviction.** LRU eviction that never removes dirty, uploading, conflict, or pinned data.
- **Operation log sync primitive.** Clients submit operations; the backend assigns an ordering cursor and broadcasts to other devices.
- **Idempotent network mutations** using stable IDs (`op_id`, `blob_id`).
- **Local recoverable copy** of every file write until the backend acknowledges it.

## Out of scope for MVP (see `non-goals.md` for the full deferred list)

- Git replacement / post-Git source-control model.
- Automatic semantic code merges.
- End-to-end encrypted metadata (filenames, paths, sizes remain plaintext to the service operator in MVP).
- Artifact cache for generated dependencies.
- Web dashboard.
- Editor integrations.
- Windows support.
- Team workspaces, roles, per-project permissions, audit log.
- Cloud agent mode as a first-class product surface (the protocol supports it, but no dedicated agent CLI in MVP).

## What a contributor should NOT build in MVP

- Do not implement blind `.git` directory sync.
- Do not log secrets, tokens, decrypted env values, or private keys.
- Do not silently overwrite local dirty data.
- Do not implement multi-user/team features.
- Do not implement Windows support.
- Do not implement E2EE metadata.
- Do not implement automatic code merges.
- Do not implement an artifact cache for `node_modules`/`target`/etc.
