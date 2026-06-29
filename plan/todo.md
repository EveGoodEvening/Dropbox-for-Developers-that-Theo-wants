# Dropbox for Developers / FS2 — Implementation Todo and Checklist

**Date:** 2026-06-28  
**Status:** step-by-step implementation plan  
**Audience:** autonomous coding agent or engineer implementing the project  
**Related design:** `design.md`

---

## 0. Working rules for the coding agent

- [ ] Treat `design.md` as the source of truth for architecture and semantics.
- [ ] Prefer a working vertical slice over broad incomplete scaffolding.
- [ ] Do not implement blind `.git` directory sync in MVP.
- [ ] Do not log secrets, tokens, decrypted env values, or private keys.
- [ ] Do not silently overwrite local dirty data.
- [ ] Add tests with every feature; sync software without tests is unsafe.
- [ ] Keep all protocol and data-model types in shared crates so backend and client cannot drift.
- [ ] Every network mutation must be idempotent using stable IDs.
- [ ] Every file-write path must preserve a local recoverable copy until the backend acknowledges it.
- [ ] Keep user-facing commands boring and predictable.

---

## 1. Phase 0 — Decisions and repository bootstrap

### 1.1 Confirm MVP boundaries

- [x] Create `docs/mvp.md` with the following MVP decisions:
  - [x] single-user account model
  - [x] macOS and Linux only
  - [x] FUSE mount required for desktop client
  - [x] materialized checkout mode allowed for tests/agents
  - [x] Postgres metadata backend
  - [x] S3-compatible blob store
  - [x] local SQLite cache
  - [x] encrypted file blobs preferred
  - [x] encrypted env var sync required
  - [x] `.git` internals excluded by default
  - [x] no team sharing in MVP
  - [x] no Windows support in MVP
- [x] Create `docs/non-goals.md` and explicitly list deferred work:
  - [x] Git replacement
  - [x] automatic code merge
  - [x] E2EE metadata
  - [x] artifact cache
  - [x] web dashboard
  - [x] editor integrations

Acceptance criteria:

- [x] A new contributor can read `docs/mvp.md` and know exactly what not to build.

### 1.2 Initialize repository

- [x] Create Rust workspace:

```text
fs2-devsync/
  Cargo.toml
  crates/
    fs2-core/
    fs2-cli/
    fs2-daemon/
    fs2-fuse/
    fs2-sync/
    fs2-rules/
    fs2-env/
    fs2-git/
    fs2-crypto/
    fs2-backend/
    fs2-testkit/
  migrations/
    postgres/
    sqlite/
  docs/
  examples/
  scripts/
```

- [x] Add workspace lints:
  - [x] deny unsafe code by default, except FUSE/platform modules if unavoidable
  - [x] deny missing `Debug` where useful
  - [x] enable clippy in CI
- [x] Add `rustfmt.toml`.
- [x] Add `justfile` or `Makefile` with:
  - [x] `just test`
  - [x] `just clippy`
  - [x] `just fmt`
  - [x] `just dev-backend`
  - [x] `just dev-client`
- [x] Add GitHub Actions or equivalent CI:
  - [x] format check
  - [x] clippy
  - [x] unit tests
  - [x] integration tests without FUSE
- [x] Add `README.md` with one-paragraph project summary and warning that it is experimental.
- [x] Add `SECURITY.md` with initial secret-handling policy.

Acceptance criteria:

- [x] `cargo test --workspace` runs with empty placeholder tests.
- [x] CI runs on PRs.

---

## 2. Phase 1 — Core domain model

### 2.1 Implement IDs and shared types in `fs2-core`

- [x] Add typed wrappers:
  - [x] `UserId`
  - [x] `WorkspaceId`
  - [x] `DeviceId`
  - [x] `NodeId`
  - [x] `RevisionId`
  - [x] `OpId`
  - [x] `BlobId`
  - [x] `Cursor`
- [x] Implement serde serialization/deserialization.
- [x] Implement display/from-string parsing.
- [x] Add property tests for ID round trips.

Acceptance criteria:

- [x] IDs do not appear as raw `Uuid` throughout the codebase except at boundaries.

### 2.2 Implement node and revision types

- [x] Add `NodeKind` enum:
  - [x] `Directory`
  - [x] `File`
  - [x] `Symlink`
- [x] Add `Node` struct.
- [x] Add `NodeRevision` struct.
- [x] Add `RevisionContent` enum.
- [x] Add portable file metadata fields:
  - [x] size
  - [x] mtime
  - [x] POSIX mode
  - [x] executable bit
  - [x] symlink target
- [x] Add serialization tests with JSON snapshots.

Acceptance criteria:

- [x] A node tree can be serialized by backend and deserialized by client with identical values.

### 2.3 Implement operations

- [x] Add `Operation` struct.
- [x] Add `OperationKind` enum:
  - [x] `CreateNode`
  - [x] `PutFileRevision`
  - [x] `MoveNode`
  - [x] `DeleteNode`
  - [x] `RestoreNode`
  - [x] `SetRule`
  - [x] `SetEnvVar`
  - [x] `DeleteEnvVar`
- [x] Add idempotency field `op_id`.
- [x] Add `base_cursor` and base revision fields.
- [x] Add validation helpers for operation shape.
- [x] Add snapshot tests for each operation kind.

Acceptance criteria:

- [x] Backend and client can share the same operation JSON contract.

### 2.4 Implement error model

- [x] Define `Fs2Error` enum with stable codes:
  - [x] `Unauthorized`
  - [x] `DeviceRevoked`
  - [x] `WorkspaceNotFound`
  - [x] `NodeNotFound`
  - [x] `PathCollision`
  - [x] `RevisionConflict`
  - [x] `BlobMissing`
  - [x] `InvalidOperation`
  - [x] `QuotaExceeded`
  - [x] `RateLimited`
  - [x] `Offline`
  - [x] `NotHydrated`
  - [x] `SecretUnavailable`
- [x] Implement conversion to HTTP error response.
- [x] Implement conversion to CLI-friendly messages.

Acceptance criteria:

- [x] Error responses are structured and machine-readable.

---

## 3. Phase 2 — Path normalization and collision policy

### 3.1 Implement path utilities

- [x] Add workspace-relative path type.
- [x] Reject:
  - [x] absolute paths
  - [x] `..` traversal
  - [x] null bytes
  - [x] empty path segments except root
- [x] Normalize separators to `/` internally.
- [x] Preserve original filename display string.
- [x] Add tests for malicious paths.

Acceptance criteria:

- [x] No API accepts a path that can escape the workspace root.

### 3.2 Implement portable collision key

- [x] Add Unicode normalization helper.
- [x] Add case-folding helper.
- [x] Compute `normalized_name` for portable workspaces.
- [x] Add tests:
  - [x] `Foo.ts` vs `foo.ts`
  - [x] Unicode composed vs decomposed names
  - [x] names valid on Linux but unsafe on macOS

Acceptance criteria:

- [x] Portable workspaces block known macOS/Linux collision hazards.

---

## 4. Phase 3 — Rule engine

### 4.1 Implement `.fs2ignore` parser in `fs2-rules`

- [x] Support comments.
- [x] Support blank lines.
- [x] Support gitignore-style globs.
- [x] Support action prefixes:
  - [x] `:ignore`
  - [x] `:local-only`
  - [x] `:generated`
  - [x] `:lazy`
  - [x] `:pin`
  - [x] `:normal`
  - [x] `:secret`
  - [x] `:dependency-cache`
- [x] Make no-prefix default to `ignore`.
- [x] Implement last-match-wins behavior.
- [x] Add parser error reporting with line numbers.

Acceptance criteria:

- [x] A `.fs2ignore` file can be parsed into deterministic ordered rules.

### 4.2 Implement `.fs2/config.toml` parser

- [x] Define config schema.
- [x] Parse cache config.
- [x] Parse Git config.
- [x] Parse env config.
- [x] Parse structured rules.
- [x] Validate action names.
- [x] Validate cache size strings.
- [x] Add config snapshot tests.

Acceptance criteria:

- [x] Invalid config fails fast with useful CLI messages.

### 4.3 Implement rule precedence

- [x] Implement precedence order:
  - [x] explicit CLI override
  - [x] `.fs2/config.toml` most-specific rule
  - [x] `.fs2ignore` last match
  - [x] built-in profile
  - [x] workspace default
- [x] Add tests for precedence conflicts.

Acceptance criteria:

- [x] Given a path, the rule engine returns exactly one effective action and explanation.

### 4.4 Built-in profiles

- [x] Add Node profile:
  - [x] `node_modules/` as `dependency-cache`
  - [x] `.next/` as `generated`
  - [x] `.nuxt/` as `generated`
  - [x] `.turbo/` as `generated`
  - [x] `coverage/` as `generated`
  - [x] lockfiles as `pin` or `normal`
- [x] Add Rust profile:
  - [x] `target/` as `generated`
  - [x] `Cargo.lock` as `normal` or `pin`
- [x] Add Python profile:
  - [x] `.venv/` as `generated`
  - [x] `venv/` as `generated`
  - [x] `__pycache__/` as `generated`
  - [x] lockfiles as `normal` or `pin`
- [x] Add Go profile:
  - [x] `go.sum` as `normal` or `pin`
- [x] Do not globally mark `dist/` generated without a prompt or project-specific rule.

Acceptance criteria:

- [x] Creating `node_modules` does not enqueue sync operations under default Node profile.

---

## 5. Phase 4 — Backend skeleton

### 5.1 Create `fs2-backend` service

- [x] Add Axum server.
- [x] Add config loader:
  - [x] bind address
  - [x] database URL
  - [x] object store config
  - [x] JWT/session secrets
- [x] Add health endpoint:

```http
GET /healthz
```

- [x] Add structured logging.
- [x] Add graceful shutdown.

Acceptance criteria:

- [x] `just dev-backend` starts a backend and `GET /healthz` returns OK.

### 5.2 Add Postgres migrations

- [ ] Create users table.
- [ ] Create devices table.
- [ ] Create workspaces table.
- [ ] Create nodes table.
- [ ] Create node_revisions table.
- [ ] Create operations table.
- [ ] Create blobs table.
- [ ] Create env_vars table.
- [ ] Create key_envelopes table.
- [ ] Add indexes from `design.md`.
- [ ] Add migration test that runs migrations on an empty DB.

Acceptance criteria:

- [ ] Backend starts with a fresh migrated database.

Blocked as of 2026-06-29: Phase 4.2 completion requires a real Postgres execution environment to run migrations on an empty database and verify backend startup against the migrated database. This workstation has no `psql`, `postgres`, or Docker, so the migration test and acceptance cannot be run locally. Provide a reachable `DATABASE_URL`, Docker/testcontainers, CI Postgres result, or explicit approval for a real embedded-Postgres test dependency before implementing and marking these tasks complete.

### 5.3 Implement auth stub for local development

- [x] Add dev-only login endpoint that creates a test user.
- [x] Issue signed access token.
- [x] Add middleware that extracts user/device claims.
- [x] Mark dev auth clearly as non-production.

Acceptance criteria:

- [x] CLI can obtain a token from local backend in development.

### 5.4 Implement device enrollment

- [x] Add device registration endpoint.
- [x] Store device name, platform, public key.
- [x] Return `DeviceId`.
- [x] Add device list endpoint.
- [x] Add revoke endpoint.
- [x] Add tests for revoked devices being rejected.

Acceptance criteria:

- [x] A user can register two devices and list both.

---

## 6. Phase 5 — Backend metadata operations

### 6.1 Workspace creation

- [ ] Implement `POST /v1/workspaces`.
- [ ] Create root node transactionally.
- [ ] Create initial cursor.
- [ ] Return workspace ID and root node ID.
- [ ] Add tests.

Acceptance criteria:

- [ ] New workspace always has exactly one live root directory node.

### 6.2 Operation commit transaction

- [ ] Implement `POST /v1/workspaces/{id}/ops`.
- [ ] Lock workspace row or use advisory lock.
- [ ] Validate device belongs to workspace owner.
- [ ] Check idempotency by `(workspace_id, op_id)`.
- [ ] Increment cursor transactionally.
- [ ] Apply operation to nodes/revisions/env/rules.
- [ ] Insert operation row with assigned cursor.
- [ ] Return committed operation and cursor.

Acceptance criteria:

- [ ] Duplicate operation submission returns the original committed cursor without applying twice.

### 6.3 Implement operation validators

- [ ] `CreateNode`:
  - [ ] parent exists
  - [ ] parent is directory
  - [ ] name valid
  - [ ] no live sibling collision
- [ ] `PutFileRevision`:
  - [ ] node exists
  - [ ] node is file
  - [ ] base revision equals current revision, unless initial creation
  - [ ] blob exists or upload reservation exists
- [ ] `MoveNode`:
  - [ ] node exists
  - [ ] new parent exists
  - [ ] no cycle
  - [ ] no path collision
- [ ] `DeleteNode`:
  - [ ] node exists
  - [ ] recursive flag required for non-empty directory
- [ ] `RestoreNode`:
  - [ ] tombstoned node exists
  - [ ] target parent exists
  - [ ] no collision

Acceptance criteria:

- [ ] Invalid operations fail with stable structured error codes.

### 6.4 Implement operation fetch

- [ ] Implement `GET /v1/workspaces/{id}/ops?since=&limit=`.
- [ ] Return operations sorted by cursor.
- [ ] Include `has_more` and `next_cursor`.
- [ ] Add tests for pagination.

Acceptance criteria:

- [ ] Client can reconstruct state by replaying all operations from cursor 0.

### 6.5 Implement manifest fetch

- [ ] Implement `GET /v1/workspaces/{id}/manifest?path=&depth=`.
- [ ] Return subtree metadata without file bytes.
- [ ] Support depth 0, 1, and recursive bounded depth.
- [ ] Add pagination for large directories.

Acceptance criteria:

- [ ] Cold client can fetch workspace tree metadata without downloading content.

### 6.6 Implement WebSocket events

- [ ] Add workspace events endpoint.
- [ ] Authenticate WebSocket connection.
- [ ] Subscribe connection to workspace.
- [ ] Publish event after operation commit.
- [ ] Include cursor range only; clients fetch ops through normal API.
- [ ] Add reconnect test.

Acceptance criteria:

- [ ] Client B receives notification after Client A commits an operation.

---

## 7. Phase 6 — Blob storage

### 7.1 Object store abstraction

- [x] Define trait:

```rust
trait BlobStore {
    async fn put(&self, key: &str, bytes: Bytes) -> Result<()>;
    async fn get(&self, key: &str) -> Result<Bytes>;
    async fn exists(&self, key: &str) -> Result<bool>;
}
```

- [x] Implement local filesystem blob store for tests.
- [ ] Implement S3-compatible blob store.
- [ ] Add MinIO or localstack test option.

Acceptance criteria:

- [x] Backend can run entirely locally without cloud credentials.

### 7.2 Blob registration endpoints

- [ ] Implement `POST /v1/blobs/presign-upload` or direct dev upload endpoint.
- [ ] Implement `POST /v1/blobs/presign-download` or direct dev download endpoint.
- [ ] Implement `GET /v1/blobs/{id}/status`.
- [ ] Store blob metadata in Postgres.
- [ ] Validate declared size and hash where possible.

Acceptance criteria:

- [ ] Client can upload and download a blob by ID.

### 7.3 Content addressing

- [x] Implement hash computation.
- [x] Decide initial `BlobId` format:
  - [x] `sha256:<ciphertext_hash>` preferred
- [x] Verify downloaded blob hash before use.
- [x] Add corrupt blob test.

Acceptance criteria:

- [x] Corrupt object-store bytes are rejected before being served to the user.

### 7.4 Chunking, optional after vertical slice

- [ ] Define chunk threshold, e.g. 32 MiB.
- [ ] Split large files into fixed-size chunks or content-defined chunks.
- [ ] Upload chunks individually.
- [ ] Store chunk IDs in revision.
- [ ] Stream read ranges if feasible.

Acceptance criteria:

- [ ] Large files do not require re-uploading all chunks after a tiny change, if chunking is implemented.

MVP note:

- [x] Whole-file blobs are acceptable for the first vertical slice.

---

## 8. Phase 7 — Crypto and key storage

### 8.1 Implement workspace key model

- [x] Add `WorkspaceContentKey` type.
- [x] Add `WorkspaceSecretKey` type.
- [x] Generate keys with CSPRNG.
- [ ] Store keys locally through OS keychain abstraction.
- [x] For development, allow encrypted file fallback with explicit warning.

Acceptance criteria:

- [ ] Keys are never stored in plaintext config files.

### 8.2 Implement blob encryption

- [x] Choose high-level AEAD primitive.
- [x] Encrypt bytes before upload.
- [x] Decrypt bytes after download.
- [x] Include versioned encryption header.
- [x] Verify authenticated decryption failure on tampering.
- [x] Add tests for round trips and tamper detection.

Acceptance criteria:

- [ ] Object store never receives plaintext file content in normal mode.

### 8.3 Implement secret encryption

- [x] Encrypt env values with workspace secret key.
- [x] Include associated data:
  - [x] workspace ID
  - [x] env var ID
  - [x] env var name
  - [x] environment
- [x] Add decrypt tests.
- [x] Add wrong-key failure tests.

Acceptance criteria:

- [ ] Backend can store env records but cannot decrypt values.

### 8.4 Redaction tests

- [ ] Add test logger.
- [ ] Set a known fake secret.
- [ ] Exercise env set/list/status/error paths.
- [ ] Assert fake secret string does not appear in logs or CLI output.

Acceptance criteria:

- [ ] Secret redaction is enforced by tests, not convention.

---

## 9. Phase 8 — Local SQLite store

### 9.1 SQLite migrations

- [ ] Create local workspace table.
- [ ] Create local nodes table.
- [ ] Create local revisions table.
- [ ] Create local state table.
- [ ] Create pending ops table.
- [ ] Create blob cache table.
- [ ] Create rules table.
- [ ] Create conflicts table.
- [ ] Add indexes for path and parent lookup.

Acceptance criteria:

- [ ] A new daemon can initialize local state root and DB.

### 9.2 Implement local store API

- [ ] `get_node_by_path`
- [ ] `get_node_by_id`
- [ ] `list_children`
- [ ] `apply_operation`
- [ ] `put_pending_op`
- [ ] `list_pending_ops`
- [ ] `mark_blob_cached`
- [ ] `set_hydration_state`
- [ ] `get_effective_rule`
- [ ] Add transactional helpers.

Acceptance criteria:

- [ ] All metadata mutations happen inside explicit transactions.

### 9.3 Implement operation replay locally

- [ ] Apply `CreateNode`.
- [ ] Apply `PutFileRevision`.
- [ ] Apply `MoveNode`.
- [ ] Apply `DeleteNode` with tombstones.
- [ ] Apply `RestoreNode`.
- [ ] Apply rule ops.
- [ ] Apply env ops.
- [ ] Update cursor only after successful apply.
- [ ] Add replay-from-zero test.

Acceptance criteria:

- [ ] Local state reconstructed from backend ops matches backend manifest.

---

## 10. Phase 9 — CLI foundation

### 10.1 Implement config and login commands

- [ ] `fs2 login --backend <url>`.
- [ ] Store backend URL.
- [ ] Store access/refresh token in keychain.
- [ ] Register device during login.
- [ ] `fs2 logout` clears local tokens.
- [ ] `fs2 device list` calls backend.

Acceptance criteria:

- [ ] Developer can login against local backend and see current device.

### 10.2 Workspace commands

- [ ] `fs2 workspace create <name>`.
- [ ] `fs2 workspace list`.
- [ ] `fs2 workspace init <path> --name <name>`.
- [ ] `fs2 mount <workspace> <path>` placeholder command.
- [ ] Write local workspace config.

Acceptance criteria:

- [ ] Workspace can be created from CLI and local metadata DB initialized.

### 10.3 Status command

- [ ] Implement `fs2 status` with JSON and text output.
- [ ] Show:
  - [ ] connection state
  - [ ] cursor lag
  - [ ] pending uploads
  - [ ] pending downloads
  - [ ] cache size
  - [ ] conflicts
  - [ ] env summary
  - [ ] Git warnings
- [ ] Add golden output tests.

Acceptance criteria:

- [ ] `fs2 status --json` is stable enough for tests and editor integrations.

---

## 11. Phase 10 — Sync engine without FUSE

### 11.1 Implement API client

- [ ] Authenticated request middleware.
- [ ] Retry with exponential backoff.
- [ ] Structured error parsing.
- [ ] Operation submit.
- [ ] Operation fetch.
- [ ] Manifest fetch.
- [ ] Blob upload/download.
- [ ] WebSocket event listener.

Acceptance criteria:

- [ ] API client can replay operations from backend in integration tests.

### 11.2 Implement outbound queue

- [ ] Persist pending ops in SQLite.
- [ ] Upload blobs before submitting file revision ops.
- [ ] Submit ops idempotently.
- [ ] Remove pending op only after commit acknowledged.
- [ ] Retry transient failures.
- [ ] Surface permanent failures to status.

Acceptance criteria:

- [ ] Killing process during upload/commit does not lose pending work.

### 11.3 Implement inbound sync loop

- [ ] Connect WebSocket.
- [ ] On notification, fetch ops since local cursor.
- [ ] On startup, fetch ops since local cursor.
- [ ] On WebSocket failure, poll periodically.
- [ ] Apply remote ops locally.
- [ ] Do not hydrate bytes unless pinned/prefetch.

Acceptance criteria:

- [ ] Client B sees Client A metadata changes after event.

### 11.4 Two-client materialized test harness

- [ ] Build `fs2-testkit` with fake/local backend.
- [ ] Create two local client state dirs.
- [ ] Simulate file creation on A by creating operation directly.
- [ ] Sync B.
- [ ] Assert metadata appears.
- [ ] Hydrate B and assert bytes match.

Acceptance criteria:

- [ ] Two-client metadata and blob sync works before FUSE exists.

---

## 12. Phase 11 — FUSE read-only vertical slice

### 12.1 Mount skeleton

- [ ] Implement `fs2-fuse` crate.
- [ ] Mount empty workspace root.
- [ ] Implement `getattr` for root.
- [ ] Implement `readdir` for root.
- [ ] Implement clean unmount.
- [ ] Add manual test instructions for macOS and Linux.

Acceptance criteria:

- [ ] `ls ~/code` works on mounted empty workspace.

### 12.2 Metadata-backed directory listing

- [ ] Resolve path to node.
- [ ] Implement inode mapping for `NodeId`.
- [ ] Implement `lookup`.
- [ ] Implement `getattr` for files/directories/symlinks.
- [ ] Implement `readdir` from local SQLite children.
- [ ] Do not hydrate file content during listing.

Acceptance criteria:

- [ ] A cold workspace tree can be browsed with `find` without downloading bytes, except where `find` stats file metadata only.

### 12.3 Read path with lazy hydration

- [ ] Implement `open` for files.
- [ ] Implement `read` for files.
- [ ] If blob absent locally, call daemon hydration.
- [ ] Verify blob hash.
- [ ] Serve bytes.
- [ ] Return useful error when offline and not hydrated.
- [ ] Update local access timestamp.

Acceptance criteria:

- [ ] `cat ~/code/project/file.txt` downloads bytes on first read and reads from cache on second read.

### 12.4 Symlink read support

- [ ] Implement symlink node metadata.
- [ ] Implement `readlink`.
- [ ] Add tests/manual checks for relative symlink.

Acceptance criteria:

- [ ] Relative symlinks round-trip through sync and FUSE.

---

## 13. Phase 12 — FUSE writes and local change capture

### 13.1 Create file and directory

- [ ] Implement `mkdir`.
- [ ] Implement `create`.
- [ ] Create local node immediately.
- [ ] Create pending `CreateNode` op.
- [ ] For files, track write handle.
- [ ] Add tests with materialized test backend.

Acceptance criteria:

- [ ] `mkdir` and `echo hi > file.txt` in mount produce pending ops.

### 13.2 File write lifecycle

- [ ] Implement write staging files.
- [ ] Implement `write`.
- [ ] Implement `flush`/`release` commit.
- [ ] Compute hash on close.
- [ ] Encrypt blob.
- [ ] Queue upload and `PutFileRevision`.
- [ ] Mark local state dirty until backend ack.

Acceptance criteria:

- [ ] Edited files survive daemon restart before upload completes.

### 13.3 Rename and delete

- [ ] Implement `rename`.
- [ ] Implement `unlink`.
- [ ] Implement `rmdir`.
- [ ] Queue corresponding ops.
- [ ] Apply optimistic local state.
- [ ] Handle backend rejection.

Acceptance criteria:

- [ ] Rename/delete on A converge to B.

### 13.4 chmod/executable bit

- [ ] Implement `setattr` for mode changes.
- [ ] Queue metadata revision or metadata op.
- [ ] Preserve executable bit across machines.

Acceptance criteria:

- [ ] `chmod +x script.sh` on A is reflected on B.

### 13.5 Atomic editor save patterns

- [ ] Test temp-file write + rename.
- [ ] Add default ignore rules for common swap/temp files.
- [ ] Ensure final file revision is uploaded once.

Acceptance criteria:

- [ ] Saving from VS Code or vim does not create noisy synced temp files under default rules.

---

## 14. Phase 13 — Hydration, pinning, and cache eviction

### 14.1 Hydration command

- [ ] Implement `fs2 hydrate <path>`.
- [ ] Add `--recursive`.
- [ ] Add `--pin`.
- [ ] Show progress.
- [ ] Retry failed downloads.
- [ ] Respect generated/local-only rules.

Acceptance criteria:

- [ ] User can hydrate a project before going offline.

### 14.2 Pin/unpin commands

- [ ] Implement `fs2 pin <path> --recursive`.
- [ ] Implement `fs2 unpin <path> --recursive`.
- [ ] Store pin state in local DB and/or remote rule if it should apply across devices.
- [ ] Decide MVP behavior: local pin by default; optional `--sync-rule` for workspace rule.

Acceptance criteria:

- [ ] Pinned files are skipped by cache pruning.

### 14.3 Cache status and prune

- [ ] Implement cache size accounting.
- [ ] Implement LRU eviction.
- [ ] Never evict:
  - [ ] dirty local files
  - [ ] uploading files
  - [ ] conflict files
  - [ ] pinned files
- [ ] Implement `fs2 cache status`.
- [ ] Implement `fs2 cache prune`.
- [ ] Add disk-full simulation test if feasible.

Acceptance criteria:

- [ ] Cache prune cannot cause data loss.

---

## 15. Phase 14 — Conflict handling

### 15.1 Backend conflict detection

- [ ] Enforce base revision on `PutFileRevision`.
- [ ] Return `revision_conflict` with current revision.
- [ ] Add tests for concurrent writes.

Acceptance criteria:

- [ ] Backend rejects stale file update instead of overwriting current content.

### 15.2 Client conflict preservation

- [ ] On rejected stale update, keep local dirty bytes.
- [ ] Hydrate/keep remote current version at canonical path.
- [ ] Write local conflict copy using deterministic conflict filename.
- [ ] Add conflict DB row.
- [ ] Show conflict in `fs2 status`.

Acceptance criteria:

- [ ] Concurrent edits preserve both versions on both machines.

### 15.3 Conflict resolution CLI

- [ ] Implement `fs2 conflicts list`.
- [ ] Implement `fs2 conflicts show <id>`.
- [ ] Implement `fs2 conflicts resolve <id> --use-local`.
- [ ] Implement `fs2 conflicts resolve <id> --use-remote`.
- [ ] Implement `fs2 conflicts resolve <id> --manual <path>`.

Acceptance criteria:

- [ ] User can resolve a conflict without manually editing local DB.

MVP relaxation:

- [ ] `list` and visible conflict files are mandatory; advanced resolve commands may be shortly after MVP.

---

## 16. Phase 15 — Offline mode and restart safety

### 16.1 Offline detection

- [ ] Detect backend unreachable.
- [ ] Mark daemon connection state offline.
- [ ] Continue serving hydrated files.
- [ ] Queue writes.
- [ ] Return clear error for unhydrated reads.

Acceptance criteria:

- [ ] Network outage does not prevent editing already hydrated files.

### 16.2 Pending op replay

- [ ] Persist pending ops before attempting network mutation.
- [ ] On daemon restart, load pending ops.
- [ ] Upload missing blobs.
- [ ] Submit ops in local order.
- [ ] Handle idempotent duplicate success.

Acceptance criteria:

- [ ] Killing daemon during sync and restarting eventually converges.

### 16.3 Rebase after reconnect

- [ ] Fetch remote ops before replaying local pending ops.
- [ ] Apply remote ops.
- [ ] For each pending local op:
  - [ ] submit if still valid
  - [ ] transform move paths if stable node ID allows
  - [ ] create conflict if stale content update
  - [ ] preserve local bytes on failure

Acceptance criteria:

- [ ] Offline concurrent edit produces conflict, not lost update.

---

## 17. Phase 16 — Environment variable sync

### 17.1 Env data model

- [x] Implement `EnvVar` types in `fs2-core` or `fs2-env`.
- [x] Add environment name validation.
- [x] Add variable name validation.
- [x] Add scope model:
  - [x] workspace
  - [x] project
  - [x] machine
  - [x] project-machine

Acceptance criteria:

- [x] Env var records have unambiguous precedence.

### 17.2 Backend env endpoints

- [ ] Add env list endpoint.
- [ ] Add env set endpoint.
- [ ] Add env delete endpoint.
- [ ] Store encrypted payload only.
- [ ] Add tests that backend never receives plaintext in request logs.

Acceptance criteria:

- [ ] Env records sync through operation log or env endpoint consistently.

### 17.3 CLI env commands

- [ ] `fs2 env set`.
- [ ] `fs2 env list`.
- [ ] `fs2 env unset`.
- [ ] `fs2 env import`.
- [ ] `fs2 env materialize`.
- [ ] `fs2 env exec`.
- [ ] Ensure secret prompts do not echo.
- [ ] Ensure list output redacts secrets.

Acceptance criteria:

- [ ] User can set a secret on A and run a command using it on B.

### 17.4 Dotenv parser and materializer

- [x] Parse common dotenv syntax.
- [x] Preserve multiline values if supported.
- [x] Write materialized file with `0600` permissions.
- [x] Add `.env`/materialized file to local-only/secret rule automatically.
- [x] Warn if `.env` is Git-tracked.

Acceptance criteria:

- [ ] `fs2 env import .env` prevents future accidental normal sync of that file.

---

## 18. Phase 17 — Git-aware mode

### 18.1 Git repository detection

- [x] Detect `.git` directory or file.
- [x] Detect worktrees.
- [x] Parse remote URLs.
- [x] Detect current branch.
- [x] Detect HEAD commit.
- [x] Detect dirty status summary.
- [x] Detect `.gitmodules`.

Acceptance criteria:

- [x] `fs2 git status` reports useful state for normal repos and submodule repos.

### 18.2 Exclude `.git` by default

- [x] Add built-in rule for `.git/**` as local-only/ignored internal.
- [ ] Ensure FUSE write path does not upload `.git` internals.
- [ ] `fs2 doctor` warns if user overrides this to normal sync.

Acceptance criteria:

- [ ] No `.git/index` or packfile is uploaded under default config.

### 18.3 Git materialization command

- [ ] Implement `fs2 git materialize <path>`.
- [ ] If remote metadata is known, run safe clone/fetch into temp dir.
- [ ] Move `.git` into place.
- [ ] Overlay synced worktree.
- [ ] Run `git status` and report result.
- [ ] Never auto-merge.

Acceptance criteria:

- [ ] New machine can turn a synced worktree folder into a functional Git repo.

### 18.4 Submodule handling

- [x] Parse `.gitmodules`.
- [x] Record submodule path/URL/commit metadata.
- [x] Treat submodule worktree content as normal nested files.
- [x] Exclude nested `.git` internals.
- [x] Implement `fs2 git submodules status`.

Acceptance criteria:

- [ ] Submodule content appears on another machine without requiring immediate submodule commands.

---

## 19. Phase 18 — Dependency and generated path handling

### 19.1 Package manager detection

- [x] Detect Node package manager from `packageManager` in `package.json`.
- [x] Fallback to lockfile detection.
- [x] Detect monorepo workspace files.
- [x] Detect Rust, Python, and Go dependency roots.

Acceptance criteria:

- [ ] `fs2 doctor` can explain why `node_modules` is generated and which install command to run.

### 19.2 Generated write suppression

- [ ] Ensure generated paths do not produce upload queue entries.
- [ ] Stress test `npm install` creating many files.
- [ ] Stress test Rust `cargo build` creating `target` files.
- [ ] Ensure status reports generated dirs separately.

Acceptance criteria:

- [ ] `npm install` does not attempt to sync thousands of dependency files.

### 19.3 Dependency helper commands

- [ ] Implement `fs2 deps status <path>`.
- [ ] Implement `fs2 deps install <path>` as a safe wrapper that prints and asks before running command, unless `--yes`.
- [ ] Store last installed lockfile hash locally.
- [ ] Warn when lockfile hash changed but dependencies not reinstalled.

Acceptance criteria:

- [ ] User can see dependency state after syncing a project to a new machine.

MVP relaxation:

- [ ] Full `deps install` helper can be deferred, but generated suppression cannot.

---

## 20. Phase 19 — Doctor and diagnostics

### 20.1 Implement `fs2 doctor`

- [ ] Check FUSE availability.
- [ ] Check daemon running.
- [ ] Check backend connection.
- [ ] Check auth token.
- [ ] Check workspace keys.
- [ ] Check cache directory permissions.
- [ ] Check path collisions.
- [ ] Check `.env` sync safety.
- [ ] Check `.git` sync safety.
- [ ] Check generated directories without rules.
- [ ] Check package manager mismatch.

Acceptance criteria:

- [ ] Doctor output gives actionable commands, not vague warnings.

### 20.2 Implement diagnostics bundle

- [ ] `fs2 debug bundle` creates redacted archive.
- [ ] Include logs.
- [ ] Include config.
- [ ] Include status JSON.
- [ ] Exclude secrets/tokens/keys.
- [ ] Add redaction tests.

Acceptance criteria:

- [ ] Debug bundle is safe to attach to an issue.

---

## 21. Phase 20 — Daemon packaging and lifecycle

### 21.1 Daemon command

- [ ] Implement `fs2 daemon run`.
- [ ] Open local RPC socket.
- [ ] Initialize local DB.
- [ ] Start sync loops.
- [ ] Start cache eviction loop.
- [ ] Handle shutdown gracefully.

Acceptance criteria:

- [ ] Daemon can run independently of CLI command lifetime.

### 21.2 macOS LaunchAgent

- [ ] Generate LaunchAgent plist.
- [ ] Implement `fs2 service install`.
- [ ] Implement `fs2 service uninstall`.
- [ ] Implement `fs2 service status`.
- [ ] Document macFUSE requirement.

Acceptance criteria:

- [ ] Daemon starts automatically after login on macOS.

### 21.3 Linux systemd user service

- [ ] Generate systemd user unit.
- [ ] Implement install/uninstall/status commands.
- [ ] Document FUSE3 requirement.

Acceptance criteria:

- [ ] Daemon starts automatically under systemd user session.

---

## 22. Phase 21 — End-to-end test matrix

### 22.1 Two-client happy path

- [ ] Start local backend.
- [ ] Start client A.
- [ ] Start client B.
- [ ] A creates file.
- [ ] B sees metadata.
- [ ] B reads file and hydrates content.
- [ ] B edits file.
- [ ] A receives update.

Acceptance criteria:

- [ ] Test passes repeatedly without sleeps longer than necessary; use event synchronization.

### 22.2 Conflict path

- [ ] A and B hydrate same file.
- [ ] Disconnect B.
- [ ] A edits and syncs.
- [ ] B edits offline.
- [ ] Reconnect B.
- [ ] Assert conflict created.
- [ ] Assert both versions preserved.

Acceptance criteria:

- [ ] No last-writer-wins data loss.

### 22.3 Generated path path

- [ ] A creates `node_modules/pkg/index.js`.
- [ ] A creates `package.json` and lockfile.
- [ ] Sync B.
- [ ] Assert B sees package files.
- [ ] Assert B does not receive `node_modules` metadata/content.

Acceptance criteria:

- [ ] Generated dependencies are suppressed before metadata upload.

### 22.4 Env path

- [ ] A sets env secret.
- [ ] Sync B.
- [ ] B runs `fs2 env exec -- printenv NAME` in controlled test.
- [ ] Assert value exists in child process.
- [ ] Assert value not in logs.

Acceptance criteria:

- [ ] Secret sync works and redaction holds.

### 22.5 Git path

- [ ] Create test Git repo.
- [ ] Sync worktree.
- [ ] Assert `.git` internals are not uploaded.
- [ ] Run `fs2 git status`.
- [ ] Run materialize on second client against local bare remote.

Acceptance criteria:

- [ ] Git-aware mode is useful without unsafe `.git` sync.

---

## 23. Phase 22 — Performance pass

### 23.1 Metadata scale

- [ ] Generate workspace with 100k files.
- [ ] Measure cold manifest sync.
- [ ] Measure `readdir` latency.
- [ ] Add indexes if needed.
- [ ] Avoid loading all nodes into memory in daemon.

Targets:

- [ ] Listing a directory with 1k children should feel interactive.
- [ ] Cold metadata sync should be paginated and resumable.

### 23.2 Hydration performance

- [ ] Measure first-read latency for small files.
- [ ] Measure throughput for large files.
- [ ] Add concurrent download limit.
- [ ] Add per-host backoff.
- [ ] Add progress reporting for explicit hydration.

Targets:

- [ ] Small files hydrate with low overhead after connection is warm.
- [ ] Large files stream or download without blocking unrelated reads.

### 23.3 Upload storms

- [ ] Simulate creating 10k ignored/generated files.
- [ ] Ensure daemon CPU remains bounded.
- [ ] Ensure upload queue remains near zero for ignored/generated files.

Acceptance criteria:

- [ ] Package installs do not overwhelm daemon or backend.

---

## 24. Phase 23 — Security hardening

### 24.1 Token handling

- [ ] Store tokens only in keychain.
- [ ] Redact tokens in logs.
- [ ] Refresh tokens automatically.
- [ ] Reject revoked device tokens.
- [ ] Add logout flow.

Acceptance criteria:

- [ ] Token leakage tests pass.

### 24.2 Key handling

- [ ] Store workspace keys only in keychain or encrypted fallback.
- [ ] Never send private keys to backend.
- [ ] Add device revocation behavior.
- [ ] Add key unavailable status.

Acceptance criteria:

- [ ] A device without workspace key cannot decrypt blobs or env values.

### 24.3 Permission checks

- [ ] Materialized secret files mode `0600`.
- [ ] Cache directory not world-readable where platform supports it.
- [ ] Local RPC socket permissions restricted to user.

Acceptance criteria:

- [ ] Local security checks pass on macOS and Linux.

---

## 25. Phase 24 — User documentation

### 25.1 Install guide

- [ ] macOS install steps.
- [ ] Linux install steps.
- [ ] FUSE prerequisites.
- [ ] Backend configuration for self-hosting/dev.
- [ ] First workspace walkthrough.

Acceptance criteria:

- [ ] New developer can install from docs on a clean machine.

### 25.2 Developer workflow guide

- [ ] Explain metadata-only files.
- [ ] Explain hydration.
- [ ] Explain pinning.
- [ ] Explain generated directories.
- [ ] Explain env sync.
- [ ] Explain Git-aware mode.
- [ ] Explain conflicts.

Acceptance criteria:

- [ ] Docs prevent the most likely misconceptions.

### 25.3 Safety guide

- [ ] How to avoid syncing secrets incorrectly.
- [ ] What encryption protects.
- [ ] What metadata is visible to server in MVP.
- [ ] What happens when a device is revoked.
- [ ] Recovery steps for conflicts.

Acceptance criteria:

- [ ] User can make an informed decision about storing private code.

---

## 26. Phase 25 — Dogfood release

### 26.1 Internal dogfood setup

- [ ] Use FS2 to sync its own repository between two machines/containers.
- [ ] Use a real Node example project.
- [ ] Use real generated directories.
- [ ] Use at least one env secret.
- [ ] Use Git-aware materialization.

Acceptance criteria:

- [ ] Project maintainers can use FS2 daily for one week without data loss.

### 26.2 Dogfood bug categories to track

- [ ] data loss or suspected data loss
- [ ] conflict false positives
- [ ] conflict false negatives
- [ ] hydration latency
- [ ] FUSE incompatibility
- [ ] generated directory upload leakage
- [ ] env materialization issues
- [ ] Git confusion
- [ ] cache eviction bugs
- [ ] offline replay bugs

Acceptance criteria:

- [ ] No known P0/P1 data safety bugs remain before public MVP.

---

## 27. Phase 26 — Public MVP polish

### 27.1 CLI output polish

- [ ] Make errors actionable.
- [ ] Add `--json` for automation.
- [ ] Add progress bars for hydration/upload.
- [ ] Add quiet mode.
- [ ] Add verbose debug mode.

Acceptance criteria:

- [ ] CLI can be used by humans and scripts.

### 27.2 Installer packaging

- [ ] Build release binaries.
- [ ] Add checksums.
- [ ] Add macOS notarization plan if distributing outside developer-only channels.
- [ ] Add Linux tarball or package.
- [ ] Document upgrade process.

Acceptance criteria:

- [ ] A user can install without building from source.

### 27.3 Versioning and migrations

- [ ] Version local DB schema.
- [ ] Version backend API.
- [ ] Version operation payloads.
- [ ] Add migration tests.
- [ ] Add downgrade/unsupported-version error.

Acceptance criteria:

- [ ] Updating client/backend does not corrupt local state.

---

## 28. Implementation order: strict vertical slice

A coding agent should implement in this order unless blocked:

1. [ ] Rust workspace scaffolding.
2. [ ] `fs2-core` IDs/types/operations.
3. [ ] Backend migrations and workspace creation.
4. [ ] Backend operation commit/fetch.
5. [ ] Local SQLite store and operation replay.
6. [ ] Blob store abstraction with local filesystem backend.
7. [ ] API client.
8. [ ] Two-client materialized sync test.
9. [ ] Basic CLI login/workspace/status.
10. [ ] FUSE read-only mount showing metadata.
11. [ ] Lazy hydration on read.
12. [ ] FUSE file creation/write/upload.
13. [ ] Rule engine default suppressing `node_modules`.
14. [ ] Conflict detection/preservation.
15. [ ] Offline queue/replay.
16. [ ] Env secret sync.
17. [ ] Git-aware diagnostics/materialization.
18. [ ] Cache pin/prune.
19. [ ] `doctor`.
20. [ ] Packaging/docs/dogfood.

Do not start with UI, team features, Windows, artifact caching, or Git replacement.

---

## 29. Definition of done for MVP

- [ ] Two clients can sync a workspace through the backend.
- [ ] Directory structure appears on a new client before file bytes download.
- [ ] Reading a metadata-only file hydrates it.
- [ ] Editing a file on one client updates the other.
- [ ] Concurrent edits preserve both versions.
- [ ] Offline edits replay safely.
- [ ] `node_modules` and other generated directories are not synced by default.
- [ ] Env secrets sync encrypted and can be injected into a command.
- [ ] `.git` internals are not synced by default.
- [ ] `fs2 git materialize` can set up a functional Git repo on a new machine.
- [ ] `fs2 status` and `fs2 doctor` expose useful diagnostics.
- [ ] Cache pruning cannot delete dirty, conflict, or pinned data.
- [ ] Tests cover sync, conflicts, generated paths, env secrets, and Git-aware behavior.
- [ ] Docs explain limitations and security model.
- [ ] Dogfood period completes without known data loss bugs.

---

## 30. Post-MVP roadmap checklist

### 30.1 Team workspaces

- [ ] Add workspace member table.
- [ ] Add roles.
- [ ] Add per-project permissions.
- [ ] Add key sharing to users, not only devices.
- [ ] Add audit log.

### 30.2 Cloud agent mode

- [ ] Implement `fs2 agent checkout`.
- [ ] Implement explicit path hydration.
- [ ] Implement agent push.
- [ ] Add ephemeral device credentials.
- [ ] Add workspace cleanup.

### 30.3 Artifact cache

- [ ] Define platform key.
- [ ] Cache package manager artifacts by lockfile hash.
- [ ] Add opt-in rules only.
- [ ] Add quota controls.

### 30.4 Editor integration

- [ ] VS Code extension status indicator.
- [ ] Hydrate-on-open awareness.
- [ ] Conflict UI.
- [ ] Env status warnings.

### 30.5 Windows support

- [ ] Evaluate WinFsp.
- [ ] Implement Windows credential storage.
- [ ] Add Windows path policy.
- [ ] Add CI on Windows.

### 30.6 Source-control replacement research

- [ ] Explore snapshot/tag model.
- [ ] Explore change-level privacy.
- [ ] Explore per-file permissions.
- [ ] Explore Git import/export bridge.
- [ ] Keep separate from sync MVP until proven.
