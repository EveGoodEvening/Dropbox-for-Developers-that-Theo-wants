# FS2 MVP scope

This document defines what is in scope for the first usable release of FS2
(`fs2-devsync`). It is the authoritative answer to "should I build this now?"
during MVP work. Anything not listed here is either deferred (see
`docs/non-goals.md`) or post-MVP.

## Account model

- **Single-user account model.** One user owns workspaces and enrolls multiple
  devices. There is no team sharing, no per-user ACLs, and no multi-tenant
  isolation in the MVP.
- Devices belong to a single user. Device revocation is supported.

## Platforms

- **macOS and Linux only.**
- FUSE is required for the desktop client (macFUSE on macOS, FUSE3 on Linux).
- A materialized checkout mode is allowed for tests and cloud agents that
  cannot use FUSE.
- **No Windows support in MVP.** WinFsp/credential storage/path policy are
  deferred.

## Backend

- **Postgres** for authoritative metadata (users, devices, workspaces, nodes,
  revisions, operations, blobs, env vars, key envelopes).
- **S3-compatible blob store** (Cloudflare R2, MinIO, or local filesystem
  backend for development/tests).
- **Local SQLite cache** on each client for metadata, hydration state, pending
  ops, blob cache index, rules, and conflicts.
- WebSocket for live metadata invalidation; REST/JSON for ordinary requests.

## Encryption

- **Encrypted file blobs preferred.** File content is encrypted client-side
  before upload using a workspace content key. Blob IDs are ciphertext hashes.
- **Encrypted env var sync required.** Env values are encrypted with a
  workspace secret key. The backend stores only encrypted envelopes.
- **Metadata is NOT end-to-end encrypted in MVP.** Filenames, paths, sizes,
  mtimes, and directory structure are visible to the service operator. This is
  documented in `SECURITY.md`.

## Git

- **`.git` internals are excluded by default.** Blind sync of `.git/index`,
  lockfiles, packfiles, and refs is forbidden.
- Git-aware mode detects repos, records remote/branch/HEAD metadata, and
  provides `fs2 git materialize` to set up a functional repo on a new machine.
- Git is not replaced in MVP.

## Explicit non-features for MVP

- No team sharing.
- No Windows.
- No Git replacement.
- No automatic semantic code merge.
- No E2EE metadata.
- No remote artifact cache.
- No web dashboard.
- No editor integrations.

See `docs/non-goals.md` for the full deferred-work list.
