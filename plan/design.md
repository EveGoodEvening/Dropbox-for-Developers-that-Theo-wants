# Dropbox for Developers / FS2 — Detailed Design

**Date:** 2026-06-28  
**Status:** implementation design for an MVP plus v1 roadmap  
**Working codename:** `fs2-devsync`  
**Primary audience:** coding agent or engineer implementing the system  
**Source context:** user-provided product brief, Theo/t3.gg public context, and public third-party episode summaries. Theo has not endorsed this project.

---

## 1. Executive summary

Build a developer-focused sync layer that makes a `~/code` directory behave like Dropbox across machines while respecting the realities of software projects: Git repositories, secrets, generated directories, platform-specific dependency folders, large workspaces, and offline edits.

The core product is a local daemon plus virtual filesystem mount. Every machine sees the same workspace tree immediately because directory metadata is synchronized first. File content is fetched lazily on first access, then cached locally. Generated or platform-specific directories such as `node_modules` are not synchronized as ordinary files; instead, rules decide whether they are ignored, rebuilt locally, pinned, or handled through a dependency profile. Environment variables are synchronized through an encrypted secret store and can be materialized into `.env` files or injected into commands.

For the MVP, do **not** replace Git. Treat Git as a tool that projects may use inside the synced workspace. The product should solve the “my code folder is stale on this machine” problem by syncing the working tree and project layout, while avoiding unsafe blind synchronization of `.git` internals. A later version can introduce a post-Git snapshot/source-control model, but that is outside the first implementation target.

---

## 2. Product goals

### 2.1 Primary goals

1. **Same project tree everywhere.**
   - A user installs the client on macOS and Linux machines.
   - Each machine mounts or materializes the same `code/` workspace.
   - New projects, renamed directories, and deleted files propagate automatically.

2. **Metadata-first sync.**
   - Directory names, file names, sizes, mtimes, executable bits, symlink targets, and content hashes sync before file bytes.
   - A cold machine can show the entire tree quickly without downloading every file.

3. **On-demand file hydration.**
   - Reading a file that is not present locally triggers download of its content.
   - Frequently used files stay cached.
   - Users can pin directories for full offline availability.

4. **Developer-specific ignore semantics.**
   - Provide a `.fs2ignore` and `.fs2/config.toml` policy model.
   - The rules are not merely “do not upload.” They can express `ignore`, `local-only`, `generated`, `lazy`, `pinned`, `secret`, and `dependency-cache` behavior.

5. **Environment variable synchronization.**
   - Sync `.env`-style values without committing them to Git.
   - Encrypt secret values client-side.
   - Support per-project, per-environment, and per-machine overrides.

6. **Special handling for generated and platform-specific content.**
   - `node_modules`, `.next`, `.turbo`, `target`, `.venv`, and similar directories should not behave like normal shared source files by default.
   - Lockfiles and manifests should sync; generated outputs should usually be rebuilt locally.

7. **Safe conflict handling.**
   - Never silently discard developer edits.
   - Concurrent writes produce explicit conflict artifacts or a conflict state.
   - Deletions use tombstones so offline machines can converge.

8. **Works with cloud coding agents.**
   - The same daemon/protocol should support ephemeral Linux workers.
   - Agents should be able to hydrate only the paths needed for a task, run commands with synced env vars, and push resulting changes back through the same sync engine.

### 2.2 Non-goals for MVP

1. **Do not replace Git in v0.**
   - Git integration is required.
   - Git replacement is not required.

2. **Do not support multi-user teams in the first implementation.**
   - Use a single-user account model with multiple devices.
   - Design the data model so team ACLs can be added later.

3. **Do not sync `.git` internals as ordinary files by default.**
   - Blind sync of `.git/index`, lockfiles, packfiles, and refs can corrupt repositories or produce confusing states.
   - Use Git-aware metadata instead.

4. **Do not attempt automatic semantic merges for code.**
   - This is a sync system, not a merge engine.
   - A future product may integrate merge tools or LLM-assisted conflict resolution.

5. **Do not require users to move secrets into a third-party vault.**
   - The product owns a lightweight encrypted secret sync primitive.
   - External vault integrations can come later.

---

## 3. System overview

### 3.1 Major components

```text
+------------------------------+            +----------------------------------+
| Machine A                    |            | Machine B                        |
|                              |            |                                  |
|  ~/code  -> FUSE mount       |            |  ~/code  -> FUSE mount           |
|             fs2fs            |            |             fs2fs                |
|       |                      |            |       |                          |
|       v                      |            |       v                          |
|  fs2d local daemon           |<---------->|  fs2d local daemon               |
|       |                      | metadata   |       |                          |
|       v                      | events     |       v                          |
|  SQLite metadata/cache DB    |            |  SQLite metadata/cache DB        |
|  Blob cache                  |            |  Blob cache                      |
|  OS keychain                 |            |  OS keychain                     |
+------------------------------+            +----------------------------------+
             |                                           |
             | HTTPS/WebSocket                           | HTTPS/WebSocket
             v                                           v
+------------------------------------------------------------------------------+
| Hosted control plane                                                         |
|                                                                              |
|  API service        Auth/device service        Metadata service              |
|      |                    |                         |                         |
|      +--------------------+-------------------------+                         |
|                           |                                                   |
|                           v                                                   |
|                    Postgres / metadata DB                                     |
|                           |                                                   |
|                           v                                                   |
|                    S3/R2-compatible blob store                                |
+------------------------------------------------------------------------------+
```

### 3.2 Local process model

The local client consists of three logical modules. They can ship as one binary with subcommands.

1. **`fs2` CLI**
   - User-facing commands: login, init, mount, status, hydrate, pin, env, doctor.
   - Talks to `fs2d` over a Unix domain socket.
   - Can start the daemon if needed.

2. **`fs2d` daemon**
   - Owns local metadata DB, content cache, upload/download queues, WebSocket session, ignore/rule engine, and secret materialization.
   - Runs continuously as a launch agent/systemd user service.
   - Exposes a local RPC API for CLI, mount process, and future editor integrations.

3. **`fs2fs` filesystem adapter**
   - FUSE filesystem for macOS and Linux.
   - Handles `lookup`, `readdir`, `getattr`, `open`, `read`, `write`, `rename`, `unlink`, `mkdir`, `symlink`, and `flush/release`.
   - Delegates sync and hydration decisions to `fs2d`.

For the MVP, `fs2d` and `fs2fs` may run in the same process to reduce IPC complexity. Keep module boundaries clean so they can be split later.

### 3.3 Backend services

1. **Auth/device service**
   - User login.
   - Device enrollment.
   - Device public key registration.
   - Token issuance and refresh.

2. **Metadata service**
   - Stores workspace manifests, node revisions, operation logs, tombstones, conflict markers, and rule metadata.
   - Provides incremental sync via `since_cursor`.
   - Publishes change notifications over WebSocket.

3. **Blob service**
   - Uses object storage for encrypted file chunks/blobs.
   - Supports presigned upload/download URLs.
   - Verifies content hash and declared size.

4. **Secret service**
   - Stores encrypted secret envelopes.
   - Does not see plaintext values.
   - Initially shares the same API service and database as metadata.

---

## 4. Technology choices

### 4.1 Recommended implementation stack

Use **Rust** for the local client and backend service.

Reasons:

- Mature async runtime with Tokio.
- Good filesystem and FUSE ecosystem.
- Safe systems language for long-running daemons.
- Good SQLite, HTTP, TLS, and crypto libraries.
- Easy static binaries for Linux.
- Better fit than Node for filesystem mounts, cache management, and low-level IO.

Recommended crates/libraries:

- CLI: `clap`
- Async runtime: `tokio`
- HTTP client/server: `reqwest`, `axum`
- Serialization: `serde`, `serde_json`, `toml`
- SQLite: `sqlx` or `rusqlite`
- Postgres backend: `sqlx`
- FUSE:
  - Linux: `fuser` or `polyfuse`
  - macOS: validate against macFUSE; abstract the FUSE layer because macOS behavior differs.
- File watching fallback: `notify`
- Crypto: `ring`, `age`, or `libsodium` bindings. Prefer a well-reviewed high-level library for secret boxes.
- Keychain:
  - macOS Keychain through `security-framework` or `keyring`
  - Linux Secret Service through `keyring`; fallback to encrypted file with clear warning.
- Logging/tracing: `tracing`, `tracing-subscriber`
- Tests: `insta`, `proptest`, `tempfile`, testcontainers for Postgres/MinIO.

### 4.2 Backend storage

Use:

- **Postgres** for authoritative metadata.
- **S3-compatible object storage** for blobs. Cloudflare R2 is a good default because the product premise is sync-heavy and egress matters.
- **Redis** is optional for WebSocket fanout and rate limits; avoid it in the first backend if a single API process can publish events from Postgres notifications.

### 4.3 Protocol choices

- HTTPS REST or JSON-RPC for ordinary requests.
- WebSocket for live metadata invalidation and cursors.
- Presigned URLs for blob transfer.
- Content-addressed blobs/chunks for deduplication.

Do not introduce gRPC in the MVP unless the implementation agent strongly prefers it. JSON over HTTP is easier to inspect and debug.

---

## 5. Data model

### 5.1 Core identifiers

Use opaque stable IDs.

```rust
type UserId = Uuid;
type WorkspaceId = Uuid;
type DeviceId = Uuid;
type NodeId = Uuid;
type RevisionId = Uuid;
type BlobId = String;     // e.g. "sha256:<hex>" or "b3:<hex>"
type OpId = Uuid;
type Cursor = i64;        // monotonically increasing per workspace
```

### 5.2 Node model

A node is a file, directory, symlink, or special virtual object in a workspace tree.

```rust
struct Node {
    node_id: NodeId,
    workspace_id: WorkspaceId,
    parent_id: Option<NodeId>,
    name: String,
    kind: NodeKind,
    current_rev: RevisionId,
    created_at: DateTime,
    updated_at: DateTime,
    deleted_at: Option<DateTime>,
    tombstone_version: Option<i64>,
}

enum NodeKind {
    Directory,
    File,
    Symlink,
}
```

### 5.3 Revision model

A revision captures metadata and content identity for a node.

```rust
struct NodeRevision {
    revision_id: RevisionId,
    node_id: NodeId,
    workspace_id: WorkspaceId,
    device_id: DeviceId,
    base_revision_id: Option<RevisionId>,
    content: RevisionContent,
    posix_mode: u32,
    mtime: DateTime,
    size: u64,
    executable: bool,
    created_at: DateTime,
}

enum RevisionContent {
    Directory,
    File {
        blob_id: BlobId,
        chunk_ids: Vec<BlobId>,
        content_hash: String,
        encryption_header: Option<String>,
    },
    Symlink {
        target: String,
    },
}
```

### 5.4 Operation log

The operation log is the canonical sync primitive. Clients submit operations; the backend validates them, assigns an ordering cursor, and broadcasts them to other devices.

```rust
struct Operation {
    op_id: OpId,
    workspace_id: WorkspaceId,
    device_id: DeviceId,
    base_cursor: Cursor,
    kind: OperationKind,
    created_at: DateTime,
}

enum OperationKind {
    CreateNode {
        parent_id: NodeId,
        name: String,
        kind: NodeKind,
        initial_revision: Option<NodeRevision>,
    },
    PutFileRevision {
        node_id: NodeId,
        base_revision_id: Option<RevisionId>,
        revision: NodeRevision,
    },
    MoveNode {
        node_id: NodeId,
        old_parent_id: NodeId,
        old_name: String,
        new_parent_id: NodeId,
        new_name: String,
    },
    DeleteNode {
        node_id: NodeId,
        recursive: bool,
    },
    RestoreNode {
        node_id: NodeId,
        parent_id: NodeId,
        name: String,
    },
    SetRule {
        path_pattern: String,
        rule: FsRule,
    },
    SetEnvVar {
        env_var_id: Uuid,
        encrypted_payload: String,
        metadata: EnvVarMetadata,
    },
    DeleteEnvVar {
        env_var_id: Uuid,
    },
}
```

### 5.5 Path model

Paths are derived from `parent_id + name`, not primary keys.

Rules:

- `node_id` is stable across renames and moves.
- `path` is a cached derived field for lookup speed.
- Avoid using path strings as authoritative IDs.
- Maintain a uniqueness constraint on `(workspace_id, parent_id, normalized_name, deleted_at IS NULL)`.

Path normalization policy:

- Store the exact displayed name.
- Store a normalized comparison key.
- Detect and block unsafe path collisions.
- Default policy: no two live siblings may differ only by Unicode normalization or case-folding unless the workspace is explicitly set to `case_sensitive_only` and all enrolled devices support it.

This avoids a common macOS/Linux failure mode where Linux allows both `Foo.ts` and `foo.ts`, while a default macOS filesystem may not.

### 5.6 Local hydration state

Local state is not authoritative and lives only in the client SQLite DB.

```rust
enum HydrationState {
    MetadataOnly,        // visible in tree, bytes not cached
    Hydrating,           // download in progress
    Hydrated,            // bytes present and verified
    DirtyLocal,          // local bytes changed, upload pending
    Uploading,           // upload in progress
    Pinned,              // must remain available offline
    Evictable,           // cache can remove bytes
    Conflict,            // local and remote diverged
    Unavailable,         // known remote blob cannot currently be fetched
    LocalOnly,           // never uploaded because rule says local-only
    Generated,           // owned by build/dependency system, not synced
}
```

Local DB tables should include:

- `workspaces`
- `nodes`
- `node_revisions`
- `path_index`
- `local_node_state`
- `pending_ops`
- `blob_cache`
- `download_queue`
- `upload_queue`
- `rules`
- `env_vars`
- `device_keys`
- `sync_cursors`
- `conflicts`

---

## 6. Filesystem behavior

### 6.1 Mount model

The user should experience this:

```bash
fs2 login
fs2 workspace create personal-code
fs2 mount personal-code ~/code
cd ~/code
ls
cd apps/my-app
cat package.json   # hydrates package.json if needed
```

The mount should show all known directory entries even if content bytes are absent locally.

### 6.2 File read path

Algorithm for `open/read` on a file:

1. Resolve path to `node_id` through local DB.
2. If node is deleted or missing, return `ENOENT`.
3. If rule says `generated` or `local-only`, read from local materialized cache only.
4. If hydration state is `Hydrated`, serve bytes from local content cache.
5. If hydration state is `MetadataOnly`:
   - Acquire a per-node hydration lock.
   - Check whether another thread hydrated it while waiting.
   - Request blob/chunks from backend.
   - Stream to a temp file in the blob cache.
   - Verify hash and size.
   - Atomically move into the cache.
   - Mark `Hydrated` or `Pinned` as appropriate.
   - Serve bytes.
6. If offline and bytes are absent, return `EIO` and log a clear local event. The CLI status should show “read failed: not hydrated and offline.”

### 6.3 Directory listing path

Algorithm for `readdir`:

1. Resolve directory node.
2. Return all live child nodes from local DB.
3. Include generated/local-only children if present locally and allowed by rule.
4. Do not hydrate child file contents merely because a directory is listed.

For very large directories, implement paginated metadata sync internally, but FUSE still needs to present complete results. The daemon may block briefly while fetching missing child metadata if the directory was not yet enumerated.

### 6.4 File write path

Support common write patterns:

- atomic save through temp file + rename
- truncate + write
- append
- editor swap files
- chmod executable bit
- symlink creation

Algorithm for write close/release:

1. Mark node as `DirtyLocal`.
2. Compute hash and size from the local temp/materialized file.
3. Apply rules:
   - `ignore`: no upload; hide or leave local depending on rule.
   - `local-only`: keep local, do not upload.
   - `generated`: keep local, do not upload unless explicitly overridden.
   - normal/pinned/lazy: upload.
4. Upload encrypted blob/chunks if not already present in object store.
5. Submit `PutFileRevision` with `base_revision_id` equal to the remote revision known when the local write started.
6. If backend accepts, mark hydrated and clean.
7. If backend detects base revision mismatch, create conflict.

### 6.5 Rename and move

Use stable `node_id` so renames are true moves, not delete+create.

On rename:

1. Validate path collision rules.
2. Submit `MoveNode` op.
3. Optimistically apply locally.
4. If backend rejects due to conflict, rollback or create conflict marker.

### 6.6 Delete behavior

Deletes create tombstones.

- Tombstones must sync to offline devices when they reconnect.
- Keep tombstones long enough to avoid deleted files reappearing from stale clients.
- Default tombstone retention: 90 days for MVP.
- Provide `fs2 trash list` and `fs2 restore` later; MVP can support restore through internal API only.

### 6.7 Symlinks

Preserve symlink targets as strings.

Rules:

- Relative symlinks are allowed.
- Absolute symlinks are allowed but flagged by `fs2 doctor` because they are machine-specific.
- Symlinks escaping the workspace are allowed only if the rule permits `local-only` behavior; otherwise warn.

### 6.8 File modes and ownership

Sync:

- regular file vs directory vs symlink
- executable bit
- basic POSIX mode bits where portable

Do not sync:

- numeric UID/GID
- platform-specific ACLs
- extended attributes, except optional internal xattrs where available

---

## 7. On-demand downloading and cache policy

### 7.1 Cache layout

Local state root:

```text
~/.fs2/
  config.toml
  devices/
  workspaces/
    <workspace-id>/
      metadata.sqlite
      blobs/
        sha256/
          ab/
            cd/<hash>
      materialized/
        <node-id>        # temp/write staging, not user-facing path
      logs/
      secrets/
```

The user-facing `~/code` is a FUSE mount. The bytes live in `~/.fs2/workspaces/<id>/blobs` or write staging.

### 7.2 Cache state machine

```text
MetadataOnly
    | read/open
    v
Hydrating --> Hydrated --> Evictable --> MetadataOnly
    |             |              ^
    | error       | pin          |
    v             v              |
Unavailable     Pinned ---------+

Hydrated -- local write --> DirtyLocal --> Uploading --> Hydrated
                              |              |
                              | conflict     | conflict
                              v              v
                            Conflict       Conflict
```

### 7.3 Eviction

Eviction runs when:

- cache exceeds configured size
- disk free space falls below threshold
- user runs `fs2 cache prune`

Eviction rules:

- Never evict `DirtyLocal`, `Uploading`, or `Conflict` bytes.
- Never evict `Pinned` bytes.
- Prefer evicting largest and least recently accessed files.
- Keep small project manifest files longer: `package.json`, lockfiles, `README`, config files, `Cargo.toml`, `go.mod`, etc.
- Store access timestamps in SQLite rather than relying on filesystem atime.

### 7.4 Pinning

CLI:

```bash
fs2 hydrate apps/web --pin
fs2 unpin apps/web
fs2 pin list
```

Pinning a directory means:

- all current children hydrate recursively
- future files under the path hydrate automatically
- eviction skips those blobs

### 7.5 Prefetching

Use conservative heuristics:

- On `cd` into a directory, shell integration can notify `fs2d`; prefetch small metadata-adjacent files.
- On reading `package.json`, prefetch lockfiles and common config files in the same directory.
- On opening a file in an editor, prefetch siblings in the same directory if the total size is below a budget.
- For cloud agents, expose explicit prefetch APIs instead of relying on heuristics.

Do not prefetch `node_modules`, build outputs, or ignored/generated directories unless explicitly configured.

---

## 8. Rule engine: `.fs2ignore` and `.fs2/config.toml`

### 8.1 Why `.gitignore` is insufficient

`.gitignore` answers one question: “Should Git track this path?” This product needs to answer several different questions:

- Should the path appear on other machines?
- Should metadata sync but bytes stay lazy?
- Should content sync normally?
- Should content be generated locally from a manifest?
- Should a value be treated as an encrypted secret?
- Should a path be pinned for offline use?
- Should a path exist only on this machine?

### 8.2 `.fs2ignore` syntax

Use gitignore-compatible glob syntax plus optional action prefixes.

Example:

```gitignore
# Default action is ignore
.DS_Store
*.swp
*.tmp

# Generated dependencies: never upload/download contents
:generated node_modules/
:generated .next/
:generated .turbo/
:generated target/
:generated .venv/

# Local-only machine files
:local-only .env.local
:local-only .vscode/settings.json

# Lazy large assets: sync metadata, hydrate bytes on access
:lazy fixtures/**
:lazy datasets/**

# Always available offline
:pin README.md
:pin package.json
:pin pnpm-lock.yaml

# Secret materialization target
:secret .env
```

Supported actions:

| Action | Meaning |
|---|---|
| `ignore` | Do not sync metadata or content. For FUSE mount, hide remote ignored paths. |
| `local-only` | Allow path to exist locally, never upload it. Other machines do not see it. |
| `generated` | Path is produced from source/lockfiles; do not sync content. May be rebuilt. |
| `lazy` | Sync metadata immediately, hydrate content on access. |
| `pin` | Sync metadata and proactively hydrate content. Do not evict. |
| `normal` | Sync metadata and content normally, but content may still be evicted unless pinned. |
| `secret` | Treat path as a materialized secret file. Contents come from secret store. |
| `dependency-cache` | Special generated dependency directory with package-manager integration. |

### 8.3 `.fs2/config.toml`

Use TOML for structured config.

```toml
version = 1
workspace_name = "personal-code"
default_file_policy = "lazy"
default_new_file_policy = "normal"
case_policy = "portable" # portable | case-sensitive-only

[cache]
max_bytes = "50GiB"
min_free_bytes = "20GiB"
eviction = "lru"

[git]
mode = "aware" # ignore | aware | experimental-sync-git-dir
auto_fetch = true
auto_merge = false
sync_git_dir = false

[env]
mode = "encrypted"
materialize = "on-command" # never | on-command | on-mount
materialize_filename = ".env.fs2"

[[rules]]
pattern = "node_modules/**"
action = "dependency-cache"
manager = "node"

[[rules]]
pattern = "apps/*/.env"
action = "secret"
scope = "project"

[[rules]]
pattern = "datasets/**"
action = "lazy"
```

### 8.4 Rule precedence

Order of precedence:

1. Explicit CLI override for path.
2. `.fs2/config.toml` rule with the most specific pattern.
3. `.fs2ignore` rule with the last matching line, gitignore-style.
4. Built-in default profiles.
5. Workspace default.

### 8.5 Built-in profiles

The daemon should ship language/tool profiles. Users can disable them.

Node profile:

```text
node_modules/          dependency-cache
dist/                  generated? prompt before applying
.next/                 generated
.nuxt/                 generated
.turbo/                generated
.vercel/               local-only or generated
coverage/              generated
package-lock.json      normal/pin
pnpm-lock.yaml         normal/pin
yarn.lock              normal/pin
bun.lockb              normal/pin
```

Rust profile:

```text
target/                generated
Cargo.lock             normal/pin for apps, normal for libraries
```

Python profile:

```text
.venv/                 generated
venv/                  generated
__pycache__/           generated
.pytest_cache/         generated
requirements*.txt      normal/pin
uv.lock                normal/pin
poetry.lock            normal/pin
```

Go profile:

```text
bin/                   generated? prompt
go.sum                 normal/pin
go.work.sum            normal/pin
```

Do not over-aggressively mark `dist/` generated globally. Some repositories intentionally commit or publish built artifacts. Default to prompting or project-level detection.

---

## 9. Dependency and generated-directory handling

### 9.1 `node_modules`

Default behavior:

- Do not upload `node_modules` contents.
- Do not download `node_modules` contents.
- Show `node_modules` only if it exists locally, unless user enables a virtual generated marker.
- Sync lockfiles and package manager metadata.
- Provide status guidance when dependencies are missing.

Example UX:

```bash
cd ~/code/apps/web
pnpm dev
# if node_modules absent, shell hook or fs2 doctor can say:
# "Dependencies are not installed on this machine. Run: pnpm install"
```

Optional v1 behavior:

```bash
fs2 deps install apps/web
fs2 deps status apps/web
fs2 deps rebuild apps/web
```

The daemon detects package manager in priority order:

1. `packageManager` field in `package.json`
2. lockfile: `pnpm-lock.yaml`, `yarn.lock`, `package-lock.json`, `bun.lockb`
3. workspace files: `pnpm-workspace.yaml`, `turbo.json`, `nx.json`

### 9.2 Dependency profile model

```rust
struct DependencyProfile {
    project_root: NodeId,
    manager: PackageManager,
    lockfile_nodes: Vec<NodeId>,
    generated_paths: Vec<String>,
    install_command: Vec<String>,
    last_installed_lock_hash: Option<String>,
    platform_key: PlatformKey,
}

struct PlatformKey {
    os: String,          // darwin, linux
    arch: String,        // arm64, x86_64
    libc: Option<String>,// glibc, musl
    node_version: Option<String>,
}
```

### 9.3 Platform-specific generated outputs

Generated paths can differ across:

- OS
- CPU architecture
- libc
- package manager version
- Node/Python/Rust/Go version
- environment variables

Therefore, treat generated outputs as local cache by default. A future artifact-cache feature can store generated outputs per `PlatformKey`, but the MVP should not sync them across incompatible machines.

### 9.4 Artifact cache, future

For v1/v2, add an opt-in remote artifact cache:

```toml
[[rules]]
pattern = "node_modules/.pnpm/**"
action = "artifact-cache"
key = ["os", "arch", "node", "lockfile_hash"]
```

This is not required for MVP because it substantially increases complexity and storage cost.

---

## 10. Git integration

### 10.1 Default Git policy

Default:

```toml
[git]
mode = "aware"
sync_git_dir = false
auto_fetch = true
auto_merge = false
```

Meaning:

- The product detects Git repositories.
- It does not upload `.git` internals as normal files.
- It syncs the working tree as files, subject to `.fs2ignore` and `.gitignore` imports.
- It records Git metadata for setup and diagnostics:
  - remote URLs
  - branch name
  - HEAD commit
  - dirty status summary
  - submodule paths
- It may run safe `git fetch` on a machine with an existing clone if enabled.
- It never runs automatic `git merge`, `git pull --rebase`, `reset`, or branch switching without user command.

### 10.2 New machine flow for Git project

When a Git project appears on a new machine:

1. The tree metadata appears in `~/code/project`.
2. If `.git` is absent locally, the daemon marks project as `git-uninitialized`.
3. `fs2 status` shows:

```text
project: apps/web
  git: remote known, local clone absent
  suggested: fs2 git materialize apps/web
```

4. User runs:

```bash
fs2 git materialize apps/web
```

5. Daemon:
   - reads recorded remote URL and branch
   - runs `git clone --no-checkout <remote> <temp>` or `git init + fetch`
   - moves `.git` into place
   - checks out expected branch if safe
   - overlays synced working tree content from FS2
   - validates `git status`

### 10.3 Existing machine stale branch flow

If project exists and Git metadata is stale:

- `fs2d` can run `git fetch --all --prune`.
- It reports “remote branch advanced” but does not merge.
- The synced working tree may already have file changes from another machine; Git status may show them as local modifications.
- User remains in control of commit/merge semantics.

### 10.4 Why not sync `.git` blindly

`.git` contains files that represent process-local and repo-local state:

- `index`
- `*.lock`
- `refs`
- `logs`
- `packed-refs`
- packfiles being written
- worktree metadata
- hooks that may be machine-specific

Blind multi-machine sync can create invalid intermediate states. The product should be Git-aware rather than Git-naive.

### 10.5 Submodules

Submodules are treated as nested projects.

MVP behavior:

- Parse `.gitmodules` when present.
- Do not recursively sync nested `.git` internals.
- Sync submodule working tree files like ordinary files.
- Record submodule URL/path/commit metadata.
- Provide diagnostics:

```bash
fs2 git submodules status
fs2 git submodules materialize
```

This reduces “submodule hell” because the folder content still appears everywhere, while preserving enough Git metadata to rehydrate proper submodule state when needed.

### 10.6 Future: source-control replacement

A later version can implement source control as snapshots, tags, per-file permissions, and change-level privacy. The MVP data model already has node revisions and operation logs, so it can evolve toward that. Do not make the first implementation depend on solving that entire problem.

---

## 11. Environment variable sync

### 11.1 Goals

- Make project environment configuration available on every machine.
- Avoid committing `.env` files to Git.
- Avoid plaintext secret storage on the server.
- Allow per-machine overrides for values that must differ.
- Make it easy for agents and scripts to run commands with the right environment.

### 11.2 Core concepts

Environment variables are modeled as encrypted records scoped to a workspace/project/environment/machine.

```rust
struct EnvVar {
    env_var_id: Uuid,
    workspace_id: WorkspaceId,
    project_path: Option<String>,
    env_name: String,          // e.g. STRIPE_SECRET_KEY
    environment: String,       // dev | test | staging | prod | custom
    scope: EnvScope,
    secret_kind: SecretKind,
    encrypted_value: String,
    metadata: EnvVarMetadata,
}

enum EnvScope {
    Workspace,
    Project,
    Machine(DeviceId),
    ProjectMachine { project_path: String, device_id: DeviceId },
}

enum SecretKind {
    Secret,       // encrypted and redacted
    PlainConfig,  // synced as config but still encrypted at rest if content encryption is enabled
}
```

### 11.3 Key hierarchy

Recommended MVP hierarchy:

```text
Account recovery key or passphrase
  -> Workspace Key Encryption Key (WKEK)
       -> Workspace Content Key (WCK)
       -> Workspace Secret Key (WSK)
            -> Env var value encryption
```

Device enrollment:

1. Device generates local keypair.
2. User authenticates.
3. Existing device or recovery key encrypts workspace keys to new device public key.
4. New device stores decrypted keys in OS keychain.

Server stores:

- device public keys
- encrypted workspace keys per device
- encrypted secret envelopes

Server must not receive plaintext secret values.

### 11.4 CLI UX

```bash
fs2 env set STRIPE_SECRET_KEY --project apps/web --env dev --secret
fs2 env set NEXT_PUBLIC_API_URL https://api.local --project apps/web --env dev --plain
fs2 env list --project apps/web --env dev
fs2 env pull --project apps/web --env dev --materialize .env.fs2
fs2 env exec --project apps/web --env dev -- pnpm dev
fs2 env unset STRIPE_SECRET_KEY --project apps/web --env dev
```

Interactive setting should not echo secret values.

### 11.5 Materialization modes

Supported modes:

1. `never`
   - Secrets only injected into subprocesses through `fs2 env exec`.

2. `on-command`
   - Materialize `.env.fs2` or configured filename only when user asks.
   - File mode `0600`.
   - Path is automatically `local-only` or `secret` by rule.

3. `on-mount`
   - Materialize at mount time.
   - Good for compatibility, less secure.
   - Should be opt-in.

### 11.6 `.env` file import

Command:

```bash
fs2 env import apps/web/.env --project apps/web --env dev
```

Behavior:

- Parse dotenv format.
- Ask whether each value is secret or plain config unless `--all-secret` or `--all-plain` is passed.
- Upload encrypted envelopes.
- Add a rule so the original `.env` is not synced as ordinary content.
- Optionally rewrite the file into `.env.example` with values redacted.

### 11.7 Redaction

Never print secret values in:

- logs
- panic messages
- telemetry
- `fs2 status`
- conflict files
- backend request traces

Show values as:

```text
STRIPE_SECRET_KEY = ******** (set, updated 2026-06-28, scope project:apps/web/dev)
```

---

## 12. Encryption and security

### 12.1 Threat model

Protect against:

- accidental secret leakage through Git
- server-side plaintext secret storage
- lost access tokens
- network interception
- unauthorized device enrollment
- accidental logs containing secrets
- object-store compromise revealing file bytes, if content encryption is enabled

Not fully solved in MVP:

- hiding filenames and directory structure from the server
- malicious local root user
- compromised enrolled device
- malicious package install scripts reading materialized secrets

### 12.2 File content encryption

Recommended default: encrypt file content client-side before blob upload.

Trade-off:

- Pros: object store cannot read code bytes.
- Cons: server-side previews/search/dedup across users are impossible.

Because this product is for source code and secrets-adjacent developer files, default to privacy. Use workspace-level content encryption, with per-blob nonces and authenticated encryption.

Blob ID options:

1. Hash plaintext, then encrypt.
   - Enables client-side dedup by plaintext.
   - Reveals equality if server can observe blob IDs.

2. Hash ciphertext.
   - Better privacy.
   - Less useful dedup.

For MVP, use ciphertext hash as `BlobId` and store plaintext hash encrypted in metadata if needed for verification. Simpler acceptable alternative: use plaintext hash in single-user workspaces only and document the leakage.

### 12.3 Metadata privacy

MVP stores plaintext metadata on the server:

- filenames
- paths
- sizes
- mtimes
- directory structure

Reason: encrypted metadata complicates conflict detection, listing, and web management. Add a clear `Security.md` note that content and secrets are protected, while metadata is not private from the service operator in MVP.

### 12.4 Auth tokens

- Access tokens short-lived.
- Refresh tokens bound to device.
- Store tokens in OS keychain.
- Support `fs2 logout --device` and remote device revoke.

### 12.5 Device revocation

On device revoke:

- Server stops accepting its tokens.
- Server stops sending new key envelopes to it.
- Future key rotation should re-encrypt workspace keys excluding revoked devices.

MVP does not guarantee revoked devices cannot read previously cached files or old keys.

---

## 13. Sync algorithm

### 13.1 High-level loop

Each client runs two loops:

1. **Outbound loop**
   - watches local changes from FUSE operations
   - uploads missing blobs
   - submits operations
   - retries pending operations

2. **Inbound loop**
   - maintains WebSocket to backend
   - fetches operations since last cursor
   - applies remote metadata changes
   - hydrates content only if pinned or prefetch policy says so

### 13.2 Operation acceptance

Backend accepts an operation if:

- auth token is valid
- device belongs to workspace
- referenced nodes exist and are not tombstoned unless restore is intended
- parent directory exists
- path collision constraints pass
- base revision rules pass for file content updates

### 13.3 File update conflict rule

For `PutFileRevision`:

- If submitted `base_revision_id == current_rev`, accept.
- If `current_rev` changed but the submitting device authored the current rev and the op is a retry, dedupe by `op_id` and accept idempotently.
- Otherwise reject with conflict.

Client conflict handling:

1. Keep remote version at original path.
2. Save local version under a conflict name:

```text
<filename>.conflict.<device-name>.<timestamp>.<ext>
```

Example:

```text
src/app.conflict.mac-mini-2.2026-06-28T13-04-11.ts
```

3. Mark conflict in local DB and metadata.
4. Show in `fs2 status`.

Do not auto-merge in MVP.

### 13.4 Directory operation conflicts

Cases:

- Two devices create same path: keep one canonical name, rename the other with conflict suffix.
- Device edits file while another deletes it: keep edited file as conflict/restored file.
- Device moves directory while another edits child: stable node IDs allow both operations to converge if parent still exists.
- Device deletes directory while another edits child: deletion wins for canonical tree, edited child is restored under conflict path or `.fs2-conflicts/`.

### 13.5 Offline behavior

Offline machine should allow:

- reading hydrated files
- editing hydrated files
- creating new files
- renaming local nodes
- deleting local nodes

Offline machine cannot:

- read metadata-only file bytes absent from cache
- enroll device
- receive remote changes
- upload new blobs

Pending operations remain in `pending_ops`. On reconnect:

1. Fetch remote ops since cursor.
2. Apply them.
3. Rebase pending local ops where safe.
4. Upload blobs.
5. Submit ops in original local order.
6. Create conflicts when base revisions changed.

### 13.6 Idempotency

Every client-generated operation has a stable `op_id`.

Backend must be idempotent:

- duplicate `op_id` returns same result
- duplicate blob upload is accepted if content hash matches
- interrupted multipart upload can resume or restart safely

---

## 14. Backend API design

### 14.1 Auth and devices

```http
POST /v1/auth/login/start
POST /v1/auth/login/complete
POST /v1/devices/enroll
GET  /v1/devices
POST /v1/devices/{device_id}/revoke
```

### 14.2 Workspaces

```http
POST /v1/workspaces
GET  /v1/workspaces
GET  /v1/workspaces/{workspace_id}
PATCH /v1/workspaces/{workspace_id}
```

### 14.3 Metadata sync

```http
GET  /v1/workspaces/{workspace_id}/ops?since=<cursor>&limit=<n>
POST /v1/workspaces/{workspace_id}/ops
GET  /v1/workspaces/{workspace_id}/manifest?path=<path>&depth=<n>
GET  /v1/workspaces/{workspace_id}/nodes/{node_id}
```

WebSocket:

```http
GET /v1/workspaces/{workspace_id}/events/ws
```

Event payload:

```json
{
  "type": "workspace_ops_available",
  "workspace_id": "...",
  "from_cursor": 1042,
  "to_cursor": 1048
}
```

### 14.4 Blob upload/download

```http
POST /v1/blobs/presign-upload
POST /v1/blobs/presign-download
GET  /v1/blobs/{blob_id}/status
```

Upload request:

```json
{
  "workspace_id": "...",
  "blob_id": "sha256:...",
  "size": 123456,
  "chunked": false,
  "encryption": {
    "algorithm": "xchacha20poly1305",
    "header": "..."
  }
}
```

### 14.5 Env vars

```http
GET  /v1/workspaces/{workspace_id}/env?project_path=&environment=
POST /v1/workspaces/{workspace_id}/env
DELETE /v1/workspaces/{workspace_id}/env/{env_var_id}
```

### 14.6 Error model

Use structured errors.

```json
{
  "error": {
    "code": "revision_conflict",
    "message": "Node revision changed before operation was applied.",
    "details": {
      "node_id": "...",
      "client_base_revision": "...",
      "server_current_revision": "..."
    }
  }
}
```

Important error codes:

- `unauthorized`
- `device_revoked`
- `workspace_not_found`
- `node_not_found`
- `path_collision`
- `revision_conflict`
- `blob_missing`
- `invalid_operation`
- `quota_exceeded`
- `rate_limited`

---

## 15. Backend database schema sketch

### 15.1 Tables

```sql
CREATE TABLE users (
  id UUID PRIMARY KEY,
  email TEXT UNIQUE NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE devices (
  id UUID PRIMARY KEY,
  user_id UUID NOT NULL REFERENCES users(id),
  name TEXT NOT NULL,
  public_key BYTEA NOT NULL,
  platform JSONB NOT NULL,
  revoked_at TIMESTAMPTZ,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE workspaces (
  id UUID PRIMARY KEY,
  user_id UUID NOT NULL REFERENCES users(id),
  name TEXT NOT NULL,
  root_node_id UUID NOT NULL,
  current_cursor BIGINT NOT NULL DEFAULT 0,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE nodes (
  id UUID PRIMARY KEY,
  workspace_id UUID NOT NULL REFERENCES workspaces(id),
  parent_id UUID NULL,
  name TEXT NOT NULL,
  normalized_name TEXT NOT NULL,
  kind TEXT NOT NULL,
  current_revision_id UUID NULL,
  deleted_at TIMESTAMPTZ,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX nodes_live_name_idx
ON nodes(workspace_id, parent_id, normalized_name)
WHERE deleted_at IS NULL;

CREATE TABLE node_revisions (
  id UUID PRIMARY KEY,
  workspace_id UUID NOT NULL REFERENCES workspaces(id),
  node_id UUID NOT NULL REFERENCES nodes(id),
  device_id UUID NOT NULL REFERENCES devices(id),
  base_revision_id UUID NULL,
  kind TEXT NOT NULL,
  blob_id TEXT NULL,
  chunk_ids JSONB NULL,
  symlink_target TEXT NULL,
  size BIGINT NOT NULL DEFAULT 0,
  content_hash TEXT NULL,
  encryption_header TEXT NULL,
  posix_mode INTEGER NOT NULL DEFAULT 420,
  mtime TIMESTAMPTZ NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE operations (
  id UUID PRIMARY KEY,
  workspace_id UUID NOT NULL REFERENCES workspaces(id),
  device_id UUID NOT NULL REFERENCES devices(id),
  cursor BIGINT NOT NULL,
  kind TEXT NOT NULL,
  payload JSONB NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  UNIQUE(workspace_id, cursor),
  UNIQUE(workspace_id, id)
);

CREATE TABLE blobs (
  id TEXT PRIMARY KEY,
  workspace_id UUID NOT NULL REFERENCES workspaces(id),
  size BIGINT NOT NULL,
  encryption_header TEXT NULL,
  object_key TEXT NOT NULL,
  uploaded_at TIMESTAMPTZ,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE env_vars (
  id UUID PRIMARY KEY,
  workspace_id UUID NOT NULL REFERENCES workspaces(id),
  project_path TEXT NULL,
  environment TEXT NOT NULL,
  name TEXT NOT NULL,
  scope JSONB NOT NULL,
  secret_kind TEXT NOT NULL,
  encrypted_value TEXT NOT NULL,
  metadata JSONB NOT NULL,
  deleted_at TIMESTAMPTZ,
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE key_envelopes (
  id UUID PRIMARY KEY,
  workspace_id UUID NOT NULL REFERENCES workspaces(id),
  device_id UUID NOT NULL REFERENCES devices(id),
  encrypted_workspace_key TEXT NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  UNIQUE(workspace_id, device_id)
);
```

### 15.2 Cursor allocation

Operation commit must be transactional:

1. Lock workspace row.
2. Increment `current_cursor`.
3. Validate op.
4. Apply node/revision changes.
5. Insert operation with assigned cursor.
6. Commit.
7. Publish notification.

Use `SELECT ... FOR UPDATE` on the workspace row or a Postgres advisory lock keyed by workspace ID.

---

## 16. Local SQLite schema sketch

```sql
CREATE TABLE local_workspaces (
  workspace_id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  mount_path TEXT,
  root_node_id TEXT NOT NULL,
  last_cursor INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL
);

CREATE TABLE local_nodes (
  node_id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL,
  parent_id TEXT,
  name TEXT NOT NULL,
  normalized_name TEXT NOT NULL,
  path TEXT NOT NULL,
  kind TEXT NOT NULL,
  current_revision_id TEXT,
  deleted_at TEXT,
  updated_at TEXT NOT NULL
);

CREATE INDEX local_nodes_path_idx ON local_nodes(workspace_id, path);
CREATE INDEX local_nodes_parent_idx ON local_nodes(workspace_id, parent_id);

CREATE TABLE local_revisions (
  revision_id TEXT PRIMARY KEY,
  node_id TEXT NOT NULL,
  blob_id TEXT,
  chunk_ids TEXT,
  symlink_target TEXT,
  size INTEGER NOT NULL,
  content_hash TEXT,
  encryption_header TEXT,
  posix_mode INTEGER NOT NULL,
  mtime TEXT NOT NULL,
  created_at TEXT NOT NULL
);

CREATE TABLE local_state (
  node_id TEXT PRIMARY KEY,
  hydration_state TEXT NOT NULL,
  local_blob_path TEXT,
  dirty_base_revision_id TEXT,
  last_accessed_at TEXT,
  pinned INTEGER NOT NULL DEFAULT 0,
  error_code TEXT,
  error_message TEXT
);

CREATE TABLE pending_ops (
  op_id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL,
  kind TEXT NOT NULL,
  payload TEXT NOT NULL,
  created_at TEXT NOT NULL,
  retry_count INTEGER NOT NULL DEFAULT 0,
  last_error TEXT
);

CREATE TABLE blob_cache (
  blob_id TEXT PRIMARY KEY,
  path TEXT NOT NULL,
  size INTEGER NOT NULL,
  verified INTEGER NOT NULL DEFAULT 0,
  last_accessed_at TEXT,
  pinned_ref_count INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE rules (
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL,
  pattern TEXT NOT NULL,
  action TEXT NOT NULL,
  source TEXT NOT NULL,
  priority INTEGER NOT NULL,
  metadata TEXT
);

CREATE TABLE conflicts (
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL,
  node_id TEXT NOT NULL,
  conflict_path TEXT NOT NULL,
  remote_revision_id TEXT,
  local_revision_id TEXT,
  status TEXT NOT NULL,
  created_at TEXT NOT NULL
);
```

---

## 17. Local RPC API

Use Unix domain socket:

```text
~/.fs2/fs2d.sock
```

Methods:

```text
status(workspace_id?)
hydrate(path, recursive, pin)
unpin(path, recursive)
evict(path, recursive)
get_node(path)
read_file(node_id, offset, length)
prepare_write(path, flags)
commit_write(write_handle)
apply_rule(pattern, action)
list_conflicts()
resolve_conflict(conflict_id, strategy)
env_list(project, env)
env_set(project, env, name, value, secret_kind, scope)
env_exec(project, env, command)
git_status(path)
git_materialize(path)
```

For the MVP, FUSE can call internal Rust functions directly. Still define the RPC interface early because it stabilizes boundaries.

---

## 18. CLI design

### 18.1 Commands

```bash
fs2 login
fs2 logout
fs2 device list
fs2 device revoke <device-id>

fs2 workspace create <name>
fs2 workspace list
fs2 workspace init <path> --name <name>
fs2 mount <workspace> <path>
fs2 unmount <path>

fs2 status [path]
fs2 sync now [path]
fs2 hydrate <path> [--recursive] [--pin]
fs2 pin <path> [--recursive]
fs2 unpin <path> [--recursive]
fs2 cache status
fs2 cache prune [--max-bytes <size>]

fs2 rules list [path]
fs2 rules add <pattern> --action <action>
fs2 doctor [path]

fs2 env set <NAME> [value] --project <path> --env <env> [--secret|--plain]
fs2 env get <NAME> --project <path> --env <env>
fs2 env list --project <path> --env <env>
fs2 env import <file> --project <path> --env <env>
fs2 env materialize --project <path> --env <env> --output .env.fs2
fs2 env exec --project <path> --env <env> -- <command...>

fs2 git status [path]
fs2 git materialize <path>
fs2 git submodules status [path]
```

### 18.2 `fs2 status` output

Example:

```text
Workspace: personal-code
Mount: /Users/theo/code
Cursor: local 12048 / remote 12048
Connection: online
Cache: 18.2 GiB used / 50 GiB limit, 312 pinned files

Sync:
  clean

Hydration:
  metadata-only files: 12,934
  hydrated files: 2,812
  pinned files: 312

Generated/local-only:
  node_modules/: local generated, not synced
  .next/: generated, not synced

Git:
  apps/web: branch main, remote advanced by 2 commits, working tree has synced changes

Env:
  apps/web dev: 18 variables set, 3 machine overrides

Warnings:
  - path collision risk: packages/Foo and packages/foo cannot both sync to case-insensitive macOS
```

### 18.3 `fs2 doctor`

Checks:

- FUSE installed and mountable.
- Daemon reachable.
- Backend reachable.
- Device token valid.
- Workspace keys available.
- Cache writable.
- Path collision hazards.
- `.env` files not accidentally synced as normal files.
- `.git` directories not configured for blind sync.
- Large generated directories missing rules.
- Package manager/lockfile mismatch.

---

## 19. Platform notes

### 19.1 macOS

- Requires macFUSE or compatible FUSE layer.
- Default APFS may be case-insensitive; path collision policy matters.
- File provider APIs could be a future native replacement, but FUSE is faster to prototype.
- Use LaunchAgent for daemon.
- Store secrets and tokens in Keychain.

### 19.2 Linux

- Requires FUSE3.
- Use systemd user service for daemon.
- Store secrets through Secret Service when available.
- Inotify is available for fallback materialized mode.

### 19.3 Windows

Out of MVP.

Future options:

- WinFsp filesystem adapter.
- Windows Credential Manager.
- NTFS case behavior and path length issues.

Do not let Windows support delay macOS/Linux MVP.

---

## 20. Cloud agent mode

Cloud coding agents should not need a full desktop mount.

Provide:

```bash
fs2 agent checkout <workspace> --path apps/web --dest /workspace
fs2 agent hydrate --paths src package.json pnpm-lock.yaml
fs2 env exec --project apps/web --env dev -- pnpm test
fs2 agent push --message "agent changes"
```

Agent mode can use materialized checkout rather than FUSE:

1. Fetch metadata for requested subtree.
2. Hydrate selected files.
3. Let agent edit local files.
4. Use file watcher or explicit `fs2 agent push` to submit ops.

This is safer in containers where FUSE may not be available.

---

## 21. Observability

### 21.1 Local logs

Use structured logs with redaction.

```text
~/.fs2/logs/fs2d.log
~/.fs2/workspaces/<id>/logs/sync.log
```

Include:

- operation IDs
- node IDs
- paths where safe
- cursor numbers
- error codes
- transfer sizes
- retry counts

Never include:

- secret values
- decrypted file content
- auth tokens
- private keys

### 21.2 Metrics

Local `fs2 status --json` should expose:

- pending upload count
- pending download count
- last successful sync time
- cache size
- conflict count
- hydrated bytes
- evicted bytes
- WebSocket connected/disconnected

Backend metrics:

- operation commit latency
- manifest fetch latency
- blob presign latency
- WebSocket connections
- cursor lag by device
- blob upload/download bytes
- conflict rate
- auth failures

---

## 22. Testing strategy

### 22.1 Unit tests

- path normalization
- rule matching
- ignore precedence
- operation validation
- conflict detection
- encryption/decryption round trips
- dotenv parsing and redaction
- cache eviction
- dependency profile detection

### 22.2 Integration tests

Use temporary directories and a local test backend.

Scenarios:

1. Create file on machine A; appears on machine B as metadata-only; reading on B hydrates content.
2. Rename directory on A; B observes same node IDs under new path.
3. Delete on A while B offline; B reconnects and tombstone applies.
4. Concurrent edit on A and B; conflict file created.
5. `node_modules` created on A; B does not receive it.
6. `.env` imported on A; B can run `fs2 env exec` with decrypted value after key enrollment.
7. Cache eviction removes unpinned hydrated file; next read rehydrates.
8. Pinned directory remains hydrated after prune.
9. Path collision blocked for portable workspace.
10. Git project materializes on new machine without syncing `.git` internals.

### 22.3 FUSE behavior tests

Test common syscalls:

- `stat`
- `ls -la`
- `cat`
- `sed -i`
- editor atomic save pattern
- `mv`
- `rm -rf`
- `chmod +x`
- symlink create/read
- large sequential read
- partial read
- interrupted read

### 22.4 Chaos tests

- Kill daemon during upload.
- Kill daemon during blob hydration.
- Drop network mid-read.
- Backend returns duplicate events.
- Backend delays operation commit.
- Local disk full.
- Object store returns corrupt blob; client must hash-fail.

### 22.5 Dogfood tests

Use the product on its own repository.

Minimum dogfood gate:

- two machines or containers
- real Node app
- real `.env` values
- real Git repo
- dependency folder excluded
- edit/build/test loop works for at least a week without data loss

---

## 23. MVP scope

### 23.1 Must ship in MVP

1. Rust CLI and daemon.
2. Single-user auth and device enrollment.
3. Workspace creation and mounting.
4. macOS and Linux FUSE support.
5. Metadata-first tree sync.
6. Lazy file hydration on read.
7. Content-addressed blob storage.
8. Local SQLite metadata/cache.
9. Postgres metadata backend.
10. S3/R2 blob backend.
11. `.fs2ignore` with `ignore`, `local-only`, `generated`, `lazy`, `pin`.
12. Default generated rules for `node_modules`, `.next`, `.turbo`, `.venv`, `target`, cache directories.
13. Conflict detection with conflict files.
14. Offline edit queue.
15. Encrypted env var sync.
16. `fs2 env exec`.
17. Git-aware mode excluding `.git` by default.
18. `fs2 status` and `fs2 doctor`.
19. End-to-end tests for two-client sync.
20. Install docs.

### 23.2 Should ship soon after MVP

1. Native package installers.
2. Rich conflict resolution CLI.
3. Dependency install helpers.
4. Cloud agent materialized checkout mode.
5. Team workspaces and sharing.
6. Windows support.
7. Remote artifact cache.
8. Editor integrations.
9. Web dashboard.

### 23.3 Explicitly defer

1. Full Git replacement.
2. Automatic code merging.
3. End-to-end encrypted metadata.
4. Multi-tenant enterprise ACLs.
5. Mobile clients.
6. Cross-user global deduplication.

---

## 24. Implementation architecture in repository

Recommended repo layout:

```text
fs2-devsync/
  Cargo.toml
  crates/
    fs2-cli/
    fs2-daemon/
    fs2-fuse/
    fs2-core/
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
    design.md
    protocol.md
    security.md
    user-guide.md
  examples/
    node-monorepo/
    rust-project/
  scripts/
    dev-backend.sh
    dev-client.sh
```

Crate responsibilities:

- `fs2-core`: IDs, node model, operation model, error types.
- `fs2-rules`: ignore parser, rule evaluation, built-in profiles.
- `fs2-crypto`: workspace keys, blob encryption, env secret encryption.
- `fs2-sync`: operation application, conflict detection, queue management.
- `fs2-daemon`: local state, daemon runtime, local RPC, sync loops.
- `fs2-fuse`: FUSE adapter.
- `fs2-cli`: command-line interface.
- `fs2-env`: dotenv parsing, secret store, env injection.
- `fs2-git`: Git detection/materialization helpers.
- `fs2-backend`: Axum service and Postgres/object-store integration.
- `fs2-testkit`: two-client simulations, fake backend, temp workspaces.

---

## 25. Critical edge cases

### 25.1 Editor temp files

Many editors write:

```text
file.ts.tmp
rename file.ts.tmp -> file.ts
```

The FUSE adapter must preserve atomic rename behavior and avoid uploading every transient temp file if ignored by rules.

### 25.2 Large repos

Do not load entire tree into memory. Use paginated DB access and stream directory entries.

### 25.3 `ripgrep` and indexers

Tools like `rg`, Spotlight, language servers, and IDE indexers may trigger hydration of many files. Provide:

- `fs2 hydrate` to make this intentional.
- cache budgets.
- optional “indexer mode” that slows or denies hydration after a threshold.

Default should favor correctness: if a process reads a file, hydrate it.

### 25.4 Package install storms

`npm install` creates thousands of files. If `node_modules` is generated, the daemon must not enqueue uploads for all of them.

The rule engine must run before upload queuing.

### 25.5 Case-only renames

On case-insensitive filesystems, renaming `foo.ts` to `Foo.ts` may require a two-step rename. Handle explicitly.

### 25.6 Disk full

When disk is full:

- stop hydration
- pause writes if necessary
- preserve dirty files
- emit status warning
- never delete dirty/conflict files to make space

### 25.7 Clock skew

Do not depend on wall-clock times for conflict resolution. Use revision IDs and cursors. Store mtimes for user-facing file semantics only.

### 25.8 Malicious paths

Reject:

- paths containing `..` as a traversal segment
- null bytes
- absolute path injection in workspace-relative APIs
- Windows reserved names if Windows support is enabled later

### 25.9 Secret leakage through materialized `.env`

`fs2 doctor` should warn if:

- `.env` is configured as normal sync
- `.env` is tracked by Git
- secret materialization file is world-readable

---

## 26. Acceptance criteria for MVP

The MVP is acceptable when all of the following are true:

1. A user can install client on two machines or two local test containers.
2. A new workspace can be created and mounted.
3. Creating a directory and file on A makes the directory and file metadata visible on B.
4. Reading the file on B fetches bytes on demand and returns correct content.
5. Editing the file on B updates A.
6. Offline edits queue and later sync.
7. Concurrent edits create a visible conflict and preserve both versions.
8. `node_modules` is not uploaded or downloaded under default rules.
9. A `.env` value imported on A can be used on B through `fs2 env exec` without server-side plaintext.
10. `.git` is not synced blindly; Git repos are diagnosed and materializable.
11. Cache prune never deletes dirty/pinned/conflict data.
12. `fs2 doctor` catches at least: missing FUSE, missing keychain, `.env` normal sync, path collisions, and generated directory without rule.
13. Test suite runs in CI with backend, object-store emulator, and two-client simulation.
14. A dogfood repo can be used for normal edit/test/commit workflow for multiple days without data loss.

---

## 27. Recommended first implementation path

Build a vertical slice before building every feature:

1. Local model and operation log.
2. Backend op commit and fetch.
3. Local SQLite apply logic.
4. Blob upload/download.
5. FUSE read-only metadata tree.
6. Lazy read hydration.
7. File write/upload.
8. Two-client sync.
9. Rule engine for `node_modules`.
10. Conflict detection.
11. Env secret sync.
12. Git-aware diagnostics.

This order avoids spending weeks on polish before the hardest primitives are proven.

---

## 28. Design risks

### 28.1 FUSE complexity

FUSE behavior differs across macOS and Linux. Mitigate by isolating the adapter and building a materialized agent mode that exercises the sync engine without FUSE.

### 28.2 Data loss through conflict bugs

Mitigate by making the core invariant simple: never overwrite local dirty bytes unless they have been uploaded and acknowledged. Add property tests and chaos tests.

### 28.3 Generated-directory floods

Mitigate by applying rules before enqueueing operations and shipping strong default profiles.

### 28.4 Secret handling mistakes

Mitigate by using high-level crypto libraries, OS keychain storage, redaction tests, and a narrow secret API.

### 28.5 Git user confusion

Mitigate with clear status messages: FS2 syncs the worktree; Git remains the source-control system unless explicitly configured otherwise.

### 28.6 Performance on huge trees

Mitigate with paginated manifests, SQLite indexes, lazy hydration, and cache budgets.

---

## 29. Glossary

- **Blob:** encrypted file content stored in object storage.
- **Hydration:** downloading bytes for a metadata-only file.
- **Metadata-only:** a file or directory exists in the visible tree, but file bytes are not local.
- **Pinned:** path that must remain hydrated and offline-available.
- **Generated:** path that is expected to be rebuilt locally, not synced.
- **Local-only:** path that may exist on one machine but is not uploaded or shown elsewhere.
- **Node:** stable object representing a file, directory, or symlink.
- **Revision:** version of a node’s content and portable metadata.
- **Operation:** append-only sync event applied to converge devices.
- **Tombstone:** deletion marker retained so stale devices do not resurrect deleted files.
- **Workspace:** synced root, such as `~/code`.

---

## 30. Source notes

The design intentionally treats the supplied product brief as the authoritative input. Publicly visible context checked during preparation indicated that the idea was framed as a developer-oriented Dropbox-like `code/` directory with environment sync, lazy content hydration, special `node_modules` handling, and a mention of Theo’s earlier `fs2` project not going far enough. The project should keep a clear disclaimer that Theo has not endorsed it.
