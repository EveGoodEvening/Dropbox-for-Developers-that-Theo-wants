# FS2 — Design Document

**"Dropbox for Developers": cross-machine code sync with on-demand hydration.**

This document is self-contained: an implementing agent should be able to build the system from this file plus `todo.md` (the task checklist) without further context. When this document and `todo.md` disagree, this document wins; fix `todo.md` in the same PR.

- Product name: **FS2** (File System 2 — name from Theo's prototype; this is a clean-room implementation).
- CLI binary: `fs2` · Daemon binary: `fs2d` · Server binary: `fs2-server`.
- Origin/pain points: see `README.md`.

---

## 1. Goals and non-goals

### 1.1 Goals

| # | Goal | Milestone |
|---|------|-----------|
| G1 | Code folders sync automatically and continuously between machines, like Dropbox | M2 |
| G2 | Identical directory structure on every machine (same relative paths under a configurable root) | M2 |
| G3 | `.gitignore`-style ignore rules with sane developer defaults (`node_modules/`, `target/`, …) | M3 |
| G4 | Platform-specific artifacts (e.g. `node_modules`) stay local and can be *regenerated* per machine via hooks | M3 |
| G5 | Environment variable / secret sync — `.env` files sync encrypted, never in plaintext off-machine | M4 |
| G6 | Structure-first sync: metadata (tree) syncs everywhere; file contents fetched on demand | M5 |
| G7 | Safe coexistence with git repos inside synced folders | M3 |
| G8 | Self-hostable single-binary server; single-user (one person, N devices) | M2 |

### 1.2 Non-goals (v1)

- Windows support (Theo's fleet: 2× Mac Mini, 1× Linux box → target **macOS + Linux**).
- Multi-user teams, sharing, permissions, web UI, mobile apps.
- Content merging (no CRDT/text merge — conflicts produce conflict copies, like Dropbox/Syncthing).
- Peer-to-peer/LAN-direct transfer (hub-and-spoke via server only; P2P is a listed future extension, §26).
- Replacing git. FS2 syncs working state; git remains the versioning/collaboration tool.

---

## 2. Glossary

- **Device**: one machine running `fs2d`, registered with the server, has a stable `device_id` (UUID v4) and a bearer token.
- **Folder**: a sync root (e.g. `~/code/my-app`) registered with the server under a server-side `folder_id`. The unit of sync, ignore scoping, and policy.
- **Entry**: one file/dir/symlink inside a folder, identified by folder-relative path (`rel_path`, always `/`-separated, NFC-normalized).
- **Manifest**: the server's authoritative table of entries for a folder, each with a version vector and sequence number.
- **Version vector (VV)**: map `device_id → counter` per entry; used to detect concurrent edits (§11).
- **Chunk**: content-defined slice of a file (FastCDC), addressed by BLAKE3-256 hash; unit of dedupe and transfer.
- **CAS**: content-addressed store of chunks (client cache and server storage).
- **Placeholder**: an entry whose metadata is present locally but whose content has not been downloaded (§16).
- **Tombstone**: manifest record of a deleted entry, kept for 30 days so deletions propagate.
- **Vault item**: an encrypted secret file (e.g. `.env`) synced via the vault channel, never as a plain entry (§15).

---

## 3. Architecture overview

```
 Machine A                                  Server (self-hosted)
┌───────────────────────────────┐          ┌──────────────────────────────┐
│  fs2 (CLI) ──UDS/JSON──▶ fs2d │          │  fs2-server                  │
│                        daemon │          │  ┌────────────┐ ┌──────────┐ │
│  ┌─────────┐ ┌──────────────┐ │  HTTPS   │  │ SQLite     │ │ Chunk    │ │
│  │ watcher │ │ scanner      │ │  JSON +  │  │ manifests, │ │ CAS      │ │
│  │(notify) │ │ (full rescan)│ │  raw     │  │ devices,   │ │ (disk or │ │
│  └────┬────┘ └──────┬───────┘ │  chunks  │  │ vault      │ │  S3)     │ │
│       ▼             ▼         │  + SSE   │  └────────────┘ └──────────┘ │
│  ┌──────────────────────────┐ │◀────────▶└──────────────────────────────┘
│  │ local index (SQLite)     │ │                       ▲
│  │ + chunk cache (CAS)      │ │                       │ same protocol
│  └──────────┬───────────────┘ │               Machine B (fs2d) …
│             ▼                 │
│  ┌──────────────────────────┐ │
│  │ reconciler / sync engine │ │
│  └──────────────────────────┘ │
└───────────────────────────────┘
```

- **Hub-and-spoke, state-based sync**: the server holds the authoritative manifest per folder; devices push local changes and pull remote changes. No device talks to another device directly.
- **The daemon does everything**; the CLI is a thin client over a Unix domain socket. The daemon runs per user (launchd on macOS, systemd user unit on Linux).
- **State-based, not op-based**: we reconcile trees + version vectors (Syncthing model), not an operation log. This is far more robust to missed events, crashes, and offline periods.

---

## 4. Technology choices

The repo already contains `crates/fs2-rules/` → **Rust, Cargo workspace** is settled.

| Concern | Choice | Rationale |
|---|---|---|
| Language | Rust, edition 2024, stable toolchain pinned via `rust-toolchain.toml` | Existing scaffold; single static binaries; FUSE + perf needs |
| Async runtime | `tokio` (full features) | Ecosystem default |
| CLI parsing | `clap` (derive) | Ecosystem default |
| Server HTTP | `axum` | Tokio-native, SSE support built in |
| Client HTTP | `reqwest` (rustls, streaming) | No OpenSSL dependency |
| Serialization | `serde` + `serde_json` (API), TOML for config (`toml` crate) | Debuggable with curl; no codegen toolchain needed |
| Local DBs | `rusqlite` with `bundled` feature, WAL mode | Zero external deps; one DB file per role |
| Hashing | `blake3` | Fast, 32-byte digests |
| Chunking | `fastcdc` crate (v2020 variant) | Content-defined chunking for delta sync |
| Compression | `zstd` (level 3) | Per-chunk, on wire and at rest |
| FS watching | `notify` crate (+ `notify-debouncer-full`) | inotify/FSEvents abstraction |
| Ignore rules | `ignore`/`globset` internals NOT reused directly — implement gitignore semantics in `fs2-rules` (see §12; may depend on `globset` for glob compilation) | We need layered non-git semantics + programmatic explain |
| Secrets crypto | `age` crate (X25519 recipients) | Audited construction, simple key files |
| FUSE (Linux) | `fuser` crate | Only maintained option |
| Paths/dirs | `directories` crate (`ProjectDirs::from("dev","fs2","fs2")`) | XDG on Linux, Library/… on macOS |
| Logging | `tracing` + `tracing-subscriber` (env-filter, JSON option) | Structured logs |
| Errors | `thiserror` (libs), `anyhow` (binaries) | Convention |
| Test harness | `tempfile`, `assert_cmd`, `wiremock` optional; integration harness in `fs2-testkit` | §23 |

Pin every dependency with a caret requirement in the workspace `Cargo.toml` `[workspace.dependencies]` table; crates reference them with `workspace = true`.

---

## 5. Repository layout

```
/                       (Cargo workspace root)
├── Cargo.toml          workspace members + [workspace.dependencies]
├── rust-toolchain.toml
├── design.md           this file
├── todo.md             task checklist
├── README.md
├── CLAUDE.md           repo instructions + Lessons section (agents append lessons here)
├── env.example         template env vars for running the server locally  ← note: NOT ".env.example"
├── crates/
│   ├── fs2-core/       shared types: ids, VersionVector, EntryKind, Manifest types, errors
│   ├── fs2-proto/      API request/response DTOs (serde), API version consts, SSE event types
│   ├── fs2-chunk/      FastCDC chunking, BLAKE3 hashing, zstd frame helpers
│   ├── fs2-store/      client & server CAS (fs layout), SQLite schema/migrations/queries
│   ├── fs2-rules/      ignore engine (gitignore semantics, layered sources)   ← already exists (empty)
│   ├── fs2-scan/       full scanner + notify watcher + debounce
│   ├── fs2-sync/       reconciler: VV compare, conflict handling, apply engine (atomic writes)
│   ├── fs2-vault/      env/secret vault: age encryption, key mgmt
│   ├── fs2-vfs/        placeholders + hydration; Linux FUSE behind `fuse` feature flag
│   ├── fs2-daemon/     bin `fs2d`: orchestrates scan/watch/sync/IPC; service unit install
│   ├── fs2-server/     bin `fs2-server`: axum app, auth, manifest logic, chunk endpoints, SSE
│   ├── fs2-cli/        bin `fs2`: UDS client, human + `--json` output
│   └── fs2-testkit/    in-process server+daemon harness, scenario DSL for integration tests
└── .github/workflows/ci.yml   (fmt, clippy -D warnings, test, build matrix macOS+Linux)
```

Dependency direction (no cycles): `core ← proto ← {server, daemon, cli}`; `core ← {chunk, rules, store, scan, sync, vault, vfs} ← daemon`; `testkit` depends on `server` + `daemon`.

---

## 6. Identifiers, paths, and normalization

- `device_id`, `folder_id`: UUID v4 strings, generated at registration/creation, never reused.
- `rel_path`: folder-relative, `/`-separated, no leading `/`, no `.`/`..` segments, Unicode **NFC-normalized** before hashing/storage (macOS reports NFD; normalize at the scanner boundary). Reject paths containing `\0` or that are `.fs2`-reserved (§20).
- Entry identity is `(folder_id, rel_path)`. Renames are delete+create in v1 (dedupe makes re-upload cheap; rename detection is a future optimization, §26).
- Case sensitivity: treat paths as **case-sensitive** internally. If applying a change would collide on a case-insensitive filesystem (macOS default), mark the entry `conflict_case` and surface it in `fs2 status` instead of clobbering (§21.6).

---

## 7. Data model

### 7.1 Client index (SQLite, one DB per device at `<data_dir>/index.db`)

```sql
-- schema_version table drives migrations; embed migrations in fs2-store as numbered SQL files.
CREATE TABLE folders (
  folder_id   TEXT PRIMARY KEY,
  name        TEXT NOT NULL,
  root_path   TEXT NOT NULL,             -- absolute local path
  mode        TEXT NOT NULL DEFAULT 'full',  -- 'full' | 'structure' (§16)
  paused      INTEGER NOT NULL DEFAULT 0,
  last_seq    INTEGER NOT NULL DEFAULT 0     -- highest server seq applied (§9.4)
);

CREATE TABLE entries (
  folder_id   TEXT NOT NULL,
  rel_path    TEXT NOT NULL,
  kind        TEXT NOT NULL,              -- 'file' | 'dir' | 'symlink'
  size        INTEGER NOT NULL DEFAULT 0,
  mtime_ns    INTEGER NOT NULL DEFAULT 0, -- local observed mtime (change detection only, not synced ordering)
  mode_exec   INTEGER NOT NULL DEFAULT 0, -- executable bit only; full POSIX modes are NOT synced
  symlink_target TEXT,                    -- for kind='symlink'
  content_hash TEXT,                      -- BLAKE3 hex of full content (files only)
  vv          TEXT NOT NULL,              -- JSON {"<device_id>": counter, ...}
  state       TEXT NOT NULL,              -- 'synced' | 'dirty' | 'syncing' | 'placeholder' | 'conflict'
  deleted     INTEGER NOT NULL DEFAULT 0, -- local tombstone
  PRIMARY KEY (folder_id, rel_path)
);
CREATE INDEX entries_dirty ON entries(folder_id, state) WHERE state != 'synced';

CREATE TABLE entry_chunks (               -- chunk list for local files (last known)
  folder_id  TEXT NOT NULL,
  rel_path   TEXT NOT NULL,
  seq        INTEGER NOT NULL,            -- 0-based position
  chunk_hash TEXT NOT NULL,
  offset     INTEGER NOT NULL,
  len        INTEGER NOT NULL,
  PRIMARY KEY (folder_id, rel_path, seq)
);

CREATE TABLE meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);
-- keys: device_id, device_token, server_url, account_pubkey, schema_version …
```

Client chunk cache CAS on disk: `<data_dir>/cas/<hh>/<hash>.zst` where `hh` = first 2 hex chars. LRU-evict by atime to a configurable cap (default 10 GiB); never evict chunks belonging to `dirty`/`syncing` entries.

### 7.2 Server DB (SQLite at `<server_data_dir>/server.db`)

```sql
CREATE TABLE devices (
  device_id  TEXT PRIMARY KEY,
  name       TEXT NOT NULL,
  token_hash TEXT NOT NULL,          -- BLAKE3 of bearer token
  created_at INTEGER NOT NULL,
  last_seen  INTEGER
);

CREATE TABLE folders (
  folder_id TEXT PRIMARY KEY,
  name      TEXT NOT NULL UNIQUE,
  next_seq  INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE manifest (
  folder_id  TEXT NOT NULL,
  rel_path   TEXT NOT NULL,
  seq        INTEGER NOT NULL,       -- folder-scoped, monotonically increasing; bumped on every accepted write
  kind       TEXT NOT NULL,
  size       INTEGER NOT NULL DEFAULT 0,
  mode_exec  INTEGER NOT NULL DEFAULT 0,
  symlink_target TEXT,
  content_hash TEXT,
  chunks     TEXT,                   -- JSON [{"h":hash,"o":offset,"l":len}, ...]
  vv         TEXT NOT NULL,
  deleted    INTEGER NOT NULL DEFAULT 0,
  deleted_at INTEGER,                -- unix seconds; tombstone GC after 30 days
  updated_by TEXT NOT NULL,          -- device_id
  PRIMARY KEY (folder_id, rel_path)
);
CREATE INDEX manifest_seq ON manifest(folder_id, seq);

CREATE TABLE vault_items (
  folder_id  TEXT NOT NULL,
  rel_path   TEXT NOT NULL,
  seq        INTEGER NOT NULL,       -- shares folders.next_seq counter
  ciphertext BLOB NOT NULL,          -- age-encrypted; server cannot read (§15)
  vv         TEXT NOT NULL,
  deleted    INTEGER NOT NULL DEFAULT 0,
  updated_by TEXT NOT NULL,
  PRIMARY KEY (folder_id, rel_path)
);
```

Server CAS: same disk layout as the client (`chunks/<hh>/<hash>.zst`). A `BlobStore` trait fronts it (`get/put/has/delete`); v1 ships the filesystem impl; S3 impl is future work (§26). Refcounting/GC of unreferenced chunks: nightly job walks `manifest.chunks` + non-GC'd tombstones and deletes unreferenced blobs (M6 polish; disk is cheap until then).

### 7.3 Version vectors

`VersionVector = BTreeMap<DeviceId, u64>` in `fs2-core`, serialized as JSON object. Operations:

- `increment(self_id)`: called whenever the local device observes a *local* change to an entry (create/modify/delete).
- `dominates(a, b)`: every counter in `b` is ≤ its counter in `a` (missing = 0), and `a != b`.
- `concurrent(a, b)`: neither dominates and `a != b`.

---

## 8. Chunking and hashing

- FastCDC parameters: **min 64 KiB, avg 256 KiB, max 1 MiB**. Files smaller than min are a single chunk. Empty files have `content_hash = blake3("")` and an empty chunk list.
- `content_hash` = BLAKE3-256 of the **whole file content** (not the chunk list), lowercase hex.
- Chunk id = BLAKE3-256 of the **uncompressed** chunk bytes. Stored/transferred zstd-compressed; the receiver decompresses and **must verify the hash** before accepting (server on PUT, client on GET/apply).
- Dedupe is global across folders on the server (single CAS namespace).

---

## 9. Sync protocol (HTTP API)

All endpoints under `/v1`. JSON bodies unless noted. Auth: `Authorization: Bearer <device_token>` on everything except `POST /v1/devices/register` (which requires the **admin token**, a static secret from server config). TLS is the deployment's job (reverse proxy or `--tls-cert/--tls-key` flags); the server refuses to start on non-loopback interfaces without TLS unless `--insecure` is passed.

Every response includes `X-FS2-Api-Version: 1`. Clients send `X-FS2-Client: fs2d/<semver>`. Error body shape everywhere: `{"error": {"code": "conflict|not_found|unauthorized|bad_request|payload_too_large|internal", "message": "..."}}`.

### 9.1 Device & folder management

```
POST /v1/devices/register     (admin token)  {name} → {device_id, device_token}
GET  /v1/folders                             → [{folder_id, name, next_seq}]
POST /v1/folders                             {name} → {folder_id}
DELETE /v1/folders/{id}                      (admin token; destructive)
```

### 9.2 Manifest pull (delta)

```
GET /v1/folders/{id}/manifest?since_seq=N
→ { "seq": <max seq in folder>,
    "entries": [ {rel_path, seq, kind, size, mode_exec, symlink_target?,
                  content_hash?, chunks?, vv, deleted, updated_by}, ... ] }
```

Returns all manifest rows with `seq > N` (tombstones included), ordered by seq, paginated via `?limit=` + repeated calls (`entries` capped at 5 000 rows; client loops until returned max seq == header seq).

### 9.3 Manifest push

```
POST /v1/folders/{id}/entries
{ "entries": [ {rel_path, kind, size, mode_exec, symlink_target?, content_hash?, chunks?, vv, deleted}, ... ] }  (≤ 500/batch)
→ { "results": [ {rel_path, "outcome": "accepted"|"conflict"|"missing_chunks"|"stale",
                  "server_entry": {...}?, "missing": [chunk_hash]?} ] }
```

Server logic per entry, in one SQLite transaction per batch:

1. Load current row. If none → accept (create), assign `seq = next_seq++`.
2. If `pushed.vv` **dominates** stored vv → accept, overwrite row, assign new seq.
3. If stored vv dominates or equals → `stale` (client is behind; it must pull and reconcile — not an error).
4. If **concurrent** → `conflict`; server keeps its row and returns it as `server_entry`; the *client* performs conflict-copy handling (§11.3) and re-pushes.
5. Before accepting a file entry, verify every hash in `chunks` exists in CAS; otherwise reply `missing_chunks` + list, don't mutate. (Client uploads chunks first, §9.5; this check catches races/evictions.)

### 9.4 Change notification (SSE)

```
GET /v1/events        (SSE stream)
event: folder   data: {"folder_id": "...", "seq": 123}
event: vault    data: {"folder_id": "...", "seq": 124}
: heartbeat every 25s
```

Fired after any accepted push. Clients react by pulling `manifest?since_seq=last_seq`. On stream drop, clients reconnect with jittered backoff (1s→60s) and do an unconditional pull for every folder (missed-event safety). **SSE is an optimization; correctness never depends on it** — the daemon also polls every folder every 5 min.

### 9.5 Chunk transfer

```
POST /v1/chunks/has            {"hashes": [...]} (≤ 1000) → {"missing": [...]}
PUT  /v1/chunks/{hash}         body: zstd bytes (raw, Content-Type: application/zstd; ≤ 64 MiB compressed)
GET  /v1/chunks/{hash}         → zstd bytes | 404
```

Upload flow for a dirty file: chunk it → `has` in batches → `PUT` missing (4 parallel uploads max) → push manifest entry. `PUT` of an existing chunk is a cheap 200 no-op.

### 9.6 Vault

```
GET /v1/folders/{id}/vault?since_seq=N       → {seq, items:[{rel_path, seq, ciphertext_b64, vv, deleted, updated_by}]}
PUT /v1/folders/{id}/vault/{rel_path}        {ciphertext_b64, vv, deleted} → same outcome semantics as §9.3
```

---

## 10. Change detection

Two cooperating sources feed the same per-folder work queue in `fs2-daemon`:

### 10.1 Full scan (`fs2-scan`)

Runs at: daemon start, folder add, watcher overflow/error, and every 60 min per folder. Walk the tree (depth-first, skipping ignored dirs *before* descending — critical so `node_modules` is never even walked):

For each on-disk entry, compare `(kind, size, mtime_ns, mode_exec, symlink_target)` with the index row. On mismatch (or missing row) → rehash file, and if `content_hash` changed (or metadata like `mode_exec` changed) → `vv.increment(self)`, `state='dirty'`. Index rows whose path no longer exists on disk (and aren't placeholders) → `deleted=1`, `vv.increment(self)`, `state='dirty'`. mtime-only changes with identical hash update the index row silently (no VV bump, nothing synced).

### 10.2 Watcher

`notify` recursive watcher per folder root with `notify-debouncer-full`, 500 ms debounce window. Events map to targeted re-stat/rehash of the affected paths (same logic as scan, scoped). On overflow / watch-loss, schedule a full rescan of the folder. Ignore-filtered paths are dropped as early as possible. Apply-engine writes (§11.4) are self-inflicted events: the daemon keeps a short-lived set of "paths I just wrote + expected `(size, mtime)`" and skips matching events.

---

## 11. Reconciliation and conflicts (`fs2-sync`)

### 11.1 Sync loop (per folder)

Single-flight per folder; triggered by: dirty entries appearing, SSE ping, poll timer, `fs2 sync` command.

```
loop_iteration(folder):
  pull remote delta (manifest?since_seq=folder.last_seq)
  for each remote entry e_r, with local row e_l (may be absent):
    if e_l absent or e_r.vv dominates e_l.vv:      apply_remote(e_r)          # §11.4
    elif e_l.vv dominates or equals e_r.vv:         skip (we're newer/equal; push phase handles it)
    else (concurrent):                              handle_conflict(e_l, e_r)  # §11.3
  folder.last_seq = pulled seq
  for each dirty local entry: upload chunks; push in batches; handle per-entry outcomes:
    accepted → state='synced'
    stale    → leave dirty; next iteration's pull will resolve
    conflict → handle_conflict(local, server_entry); retry next iteration
    missing_chunks → re-upload listed chunks, re-push once; else error state + backoff
```

### 11.2 Delete semantics

- Delete vs concurrent **modify** → **modify wins** (file is resurrected; deleting device gets it back). Rationale: never lose data.
- Delete vs delete → converges trivially.
- Applying a remote delete moves the local file to the OS trash if possible (`trash` crate), else to `<data_dir>/trash/<folder_id>/<timestamped rel_path>`; never `unlink` user data directly. Local trash retained 30 days.
- Tombstones GC'd server-side after 30 days; a device offline longer than that may resurrect deletions (accepted trade-off; documented in README).

### 11.3 Conflict copies

On concurrent file edits, the device that detects the conflict (always a client, §9.3 rule 4):

1. Renames its local version to `"<stem>.fs2-conflict-<YYYYMMDD-HHMMSS>-<device_name><.ext>"` (new entry, fresh VV, pushed like any create).
2. Applies the server's version to the original path, adopting `server_entry.vv`.
3. Records the event; `fs2 conflicts` lists conflict files; `fs2 conflicts resolve <path> --keep mine|theirs` cleans up.

Concurrent dir/file kind mismatches: the local kind is conflict-renamed, remote applied. Symlinks are compared by target string.

### 11.4 Apply engine (atomic writes)

To apply a remote file: assemble content in `<folder_root>/.fs2/tmp/<rand>` (same filesystem → atomic rename) from CAS chunks (download missing ones, 4-parallel), verify full-file BLAKE3 == `content_hash`, set exec bit, `rename()` over the destination, then update the index row (`state='synced'`, adopt remote vv). fsync file + parent dir. If the local file changed between hash-check and rename (stat before/after), abort and mark dirty — next iteration reconciles.

---

## 12. Ignore engine (`fs2-rules`)

Gitignore-compatible matching semantics (last match wins, `!` negation, `**`, trailing `/` = dir-only, leading `/` anchors to the source's directory). Sources, lowest → highest precedence:

1. **Built-in defaults** (compiled in; overridable by later layers via `!` patterns):
   `node_modules/`, `.pnpm-store/`, `bower_components/`, `target/`, `dist/`, `build/`, `out/`, `.next/`, `.nuxt/`, `.turbo/`, `.svelte-kit/`, `.output/`, `coverage/`, `__pycache__/`, `*.pyc`, `.venv/`, `venv/`, `.tox/`, `.mypy_cache/`, `.pytest_cache/`, `.ruff_cache/`, `.gradle/`, `.cache/`, `.parcel-cache/`, `*.o`, `*.a`, `*.so`, `*.dylib`, `*.class`, `.DS_Store`, `._*`, `Thumbs.db`, `*.swp`, `*~`, `.direnv/`, `.fs2/`
   plus **secret guards that cannot be un-ignored**: `.env`, `.env.*`, `!env.example` (plaintext env files never sync as entries — vault only, §15).
2. Global user file: `<config_dir>/ignore` (gitignore syntax).
3. Per-folder config `fs2.toml` `[ignore] patterns = [...]` (§20.3) — this file lives in the tree and syncs, so ignore behavior is consistent across machines.
4. `.fs2ignore` files, hierarchical like `.gitignore` (any directory; patterns relative to that directory). These sync as normal entries.

`.git/` **is synced** (§14) except built-in unoverridable excludes `.git/objects/tmp_*`, `*.lock` under `.git/`, and `.git/index.lock`-guarded pauses.

API (used by scanner, watcher, apply engine — all three consult the same engine):

```rust
pub struct RuleSet { /* layered, compiled */ }
impl RuleSet {
    pub fn matches(&self, rel_path: &str, is_dir: bool) -> Decision; // Ignore | Sync
    pub fn explain(&self, rel_path: &str, is_dir: bool) -> Vec<RuleHit>; // for `fs2 ignore check`
}
```

Ignored paths: never scanned, watched events dropped, never pushed; remote entries matching local ignore rules are still applied (server is source of truth for what exists) — mismatched rules across machines are surfaced by `fs2 doctor`.

---

## 13. Platform-specific artifacts & hooks

Ignored artifacts like `node_modules` must be *regenerated* per machine. Per-folder `fs2.toml`:

```toml
[hooks]
# Runs (in folder root) after a folder is first cloned onto a device:
on_clone = ["pnpm install"]
# Runs when any listed trigger path changed due to a *remote* apply:
[[hooks.on_change]]
paths = ["package.json", "pnpm-lock.yaml"]
run = "pnpm install"
debounce_secs = 30
```

**Security model (direnv-style):** hooks arrive via sync = remote code execution risk. The daemon never runs hooks unless the *exact content* of `fs2.toml` has been approved on *this device*: approval stores `blake3(fs2.toml)` in the client index; any change requires re-approval (`fs2 hooks approve <folder>` shows a diff and re-approves). Unapproved hooks → skipped + warning in `fs2 status`. Hook runs: captured output to `<data_dir>/logs/hooks/`, 10 min timeout, failures reported in status, never retried automatically more than once.

---

## 14. Git coexistence

Decision: **sync `.git` by default** — that's the point ("forgot to git pull" disappears; the repo state itself teleports). Safeguards:

- While `<repo>/.git/index.lock` (or `.git/shallow.lock`, MERGE_HEAD lock activity) exists locally, the folder's *apply* phase pauses for paths under that repo (uploads may continue). Recheck every 2 s, warn after 10 min.
- Never sync `.git/objects/tmp_*` or `*.lock` under `.git/` (§12).
- `.git` objects are immutable-by-name → naturally conflict-free. Refs/index/HEAD can conflict; conflict copies inside `.git` are quarantined to `.fs2/git-conflicts/` instead of polluting `.git`, and `fs2 doctor` explains recovery (`git fsck`, reflog).
- Docs must state the rule of thumb: don't run concurrent git mutations on two machines against the same folder at the same moment.

---

## 15. Env var / secret sync (vault, `fs2-vault`)

Plaintext `.env*` files never sync as entries (unoverridable ignore, §12). Instead:

- **Account key**: one age X25519 keypair per *account* (not per device), generated by `fs2 setup` on the first device, stored at `<config_dir>/key.age` (mode 0600). The public key is uploaded to server meta; the private key is copied to other devices by the user (`fs2 key export` prints the age secret key; `fs2 key import` on the new device). Server never sees the private key.
- `fs2 env link <file>` (e.g. `.env`, `.env.local`) registers the file as a vault item: content encrypted with `age` to the account public key → `PUT /vault/...`. The daemon watches linked files (they're in the ignore set, so the vault path is the only channel) and re-pushes on change with VV semantics identical to entries; incoming vault items are decrypted and written atomically (0600) to the same rel_path on other devices.
- Conflicts: same VV rules; conflict copy named `.env.fs2-conflict-…` locally (never uploaded as an entry — it matches `.env.*` guard; it *is* uploaded as a vault conflict item).
- `fs2 env ls` / `fs2 env unlink <file>` / `fs2 env diff <file>` (local vs decrypted remote).
- Server stores only ciphertext → env secrets are E2E-encrypted from day one, even though regular file content is not (full E2EE is future, §26).

---

## 16. Structure-first sync & on-demand hydration (`fs2-vfs`)

Folder `mode` (client-side, per device — machine A can be `full` while B is `structure`):

- **`full`** (default): contents of all non-ignored entries are downloaded eagerly.
- **`structure`**: metadata always syncs; file *contents* are fetched on demand. New remote files materialize as **placeholders**: a 0-byte file with xattr `user.fs2.placeholder=1` (macOS: `fs2.placeholder`), index `state='placeholder'`, true size shown in `fs2 status`/VFS.

Hydration triggers:

1. **CLI**: `fs2 hydrate <path> [-r]` downloads content in place; `fs2 evict <path>` re-dehydrates (only if `state='synced'` and hash matches server — never evict dirty data).
2. **FUSE (Linux only, feature `fuse`, M5)**: `fs2 mount <folder>` exposes a read-write passthrough FUSE mount over the folder root where `open()` on a placeholder blocks, hydrates via CAS, then passes through. Writes always go to the backing store and mark entries dirty via the normal watcher path. This mode is opt-in; the placeholder+CLI model is the portable baseline.
3. **macOS**: no kernel integration in v1 (macFUSE requires a kext, FSKit is immature). macOS structure mode = placeholders + explicit hydrate + a `fs2 hydrate --watch` convenience that hydrates on first `open` attempt is NOT possible without kernel help; document this honestly. Default macOS folders to `full` mode.

Safety rule: local *edits* to a placeholder (size>0 or missing xattr detected by scanner) are treated as new local content (dirty) — user data wins over placeholder bookkeeping.

---

## 17. Security

- **Transport**: TLS (reverse proxy or built-in rustls); server refuses non-loopback plaintext without `--insecure`.
- **AuthN**: per-device bearer tokens (32 random bytes, base64url), server stores BLAKE3 hash. Admin token (from server config/env, see `env.example`) gates device registration and destructive ops. Constant-time comparison.
- **Secrets**: vault is E2EE via age (§15). Device tokens and the age private key are 0600 files; never logged.
- **Hooks**: content-hash approval per device (§13) — sync channel cannot achieve silent RCE.
- **Path safety**: server and client both reject `rel_path` traversal (`..`, absolute, `\0`); apply engine joins + canonicalizes and verifies the result stays under the folder root; symlinks are never followed when writing (`O_NOFOLLOW` semantics on the final component's parent path).
- **Limits**: request body caps (manifest push 10 MiB JSON, chunk 64 MiB), per-device rate limit (token bucket, 100 req/s) — cheap insurance, not DoS-proofing.

---

## 18. CLI specification (`fs2`)

All commands support `--json`. The CLI talks only to the daemon over UDS (`<runtime_dir>/fs2d.sock`, fallback `<data_dir>/fs2d.sock`), except `setup`/`key`/`daemon` which touch config directly. If the daemon isn't running, commands that need it print how to start it (they do not auto-spawn in v1).

```
fs2 setup --server <url> --admin-token <t> --name <device-name>
                          # registers device, generates account key (first device) → config.toml
fs2 key export | import   # account age key transfer between devices
fs2 daemon install|start|stop|status   # writes launchd plist / systemd user unit; start/stop via it
fs2 add <path> [--name n] [--mode full|structure]   # create server folder + start syncing
fs2 clone <name|folder_id> <path> [--mode ...]      # join existing folder on this machine
fs2 ls                    # folders: name, path, mode, paused, sync state
fs2 status [path]         # per-folder: counts (synced/dirty/placeholder/conflict), queue, last error
fs2 sync [path]           # force a sync iteration now, wait, report
fs2 pause|resume [path]
fs2 rm <path> [--delete-remote]   # stop syncing locally; optionally delete server folder (confirm)
fs2 conflicts [resolve <path> --keep mine|theirs]
fs2 ignore check <path>   # explain decision with matching rule + source (fs2-rules::explain)
fs2 hooks approve <folder> | hooks run <folder> <hook>
fs2 env link|unlink|ls|diff [file]
fs2 hydrate <path> [-r] | evict <path> [-r]
fs2 mount <folder> | umount <folder>        # Linux + feature "fuse" only
fs2 doctor                # connectivity, TLS, watcher health, xattr support, ignore drift, version skew
```

Exit codes: 0 ok, 1 generic error, 2 usage, 3 daemon unreachable, 4 conflict-related.

---

## 19. Daemon (`fs2d`)

- Single process per user. Subsystems as tokio tasks per folder: watcher, scanner timer, sync loop; plus global: SSE listener, IPC server, vault watcher.
- **IPC**: HTTP over UDS (axum + hyperlocal-equivalent), JSON mirroring `fs2-proto` DTOs — same shapes the CLI renders. Endpoints: `/status`, `/folders` CRUD, `/sync`, `/pause`, `/conflicts`, `/env/*`, `/hydrate`, `/hooks/*`, `/shutdown`. Socket mode 0600.
- Crash safety: all state transitions go through SQLite; on start, entries in `syncing` revert to `dirty`; a full scan reconciles reality.
- Backoff: per-folder exponential backoff (1s→5min, jittered) on server/network errors; `fs2 status` shows the reason.
- Config reload: daemon watches `config.toml` mtime and reloads non-structural settings (log level, cache cap) without restart.

---

## 20. Configuration files

### 20.1 Client `<config_dir>/config.toml`

```toml
server_url = "https://sync.example.com:8420"
device_name = "mac-mini-1"
cas_cache_gib = 10
log_level = "info"
# device_token + device_id + account key path live in index.db meta / key.age, not here
```

### 20.2 Server config (`fs2-server --config server.toml`, env vars override; template in `env.example`)

```toml
listen = "0.0.0.0:8420"
data_dir = "/var/lib/fs2"
admin_token = "…"          # or env FS2_ADMIN_TOKEN
tls_cert = ""              # optional; empty + non-loopback listen ⇒ refuse unless --insecure
tls_key = ""
```

### 20.3 Per-folder `fs2.toml` (in tree, synced; all sections optional)

```toml
[ignore]
patterns = ["*.log", "!keep.log"]

[hooks]        # see §13
on_clone = ["pnpm install"]
```

Reserved in every folder root: `.fs2/` directory (tmp, git-conflicts quarantine) — always ignored, never synced.

---

## 21. Edge cases (normative)

1. **Symlinks**: synced as symlink entries (target string). Never followed for content. Targets pointing outside the folder are allowed but flagged by `fs2 doctor`.
2. **Hardlinks**: not preserved (synced as independent files).
3. **Permissions**: only the executable bit syncs. Files created 0644/dirs 0755 (umask-respecting); vault files 0600.
4. **Big files**: no special casing in v1; chunking handles them. Warn in status for files > 1 GiB.
5. **Zero-byte + placeholder ambiguity**: resolved via xattr (§16); on filesystems without xattr support, structure mode is refused (`fs2 doctor` checks).
6. **Case-insensitive collisions**: §6 — `conflict_case` state, no clobbering.
7. **Clock skew**: irrelevant to correctness (VVs, not timestamps); timestamps are display-only.
8. **Disk full**: apply engine checks free space ≥ 2× file size before assembly; on ENOSPC, folder enters error state with a clear message.
9. **Same folder added twice / nested sync roots**: refused (`fs2 add` checks overlap against existing roots).
10. **In-flight edits**: §11.4 stat-before/after guard.
11. **Server data loss**: clients detect `folder_id` unknown → status error, never mass-delete local data. Re-`add` re-uploads (CAS dedupe makes this cheap).

---

## 22. Observability

- `tracing` everywhere; `RUST_LOG`-style filtering; `--log-json`. Daemon logs to `<data_dir>/logs/fs2d.log` (rotated, 5×10 MiB) + stderr.
- Server: `/metrics` Prometheus text (request counts/latencies, chunk bytes in/out, folders, manifest sizes, SSE clients) and `/healthz`.
- `fs2 status --json` is the machine interface for anything a user-side dashboard would need.

---

## 23. Testing strategy

- **Unit tests** per crate alongside code (`#[cfg(test)]`): VV algebra, chunking determinism (fixed seeds/corpus), rules engine (table-driven gitignore-semantics suite is the important one — port gitignore's documented examples as cases), path normalization, migrations.
- **`fs2-testkit` integration harness** (the backbone — most behavior is proven here): spin up in-process `fs2-server` (random port, tempdir) + N in-process daemon instances (tempdir roots, real SQLite, real filesystem, real HTTP; watcher optional — tests can call `scan_now()` deterministically instead of relying on notify timing). Scenario helpers: `write(dev, path, content)`, `sync_all(until_stable)`, `assert_tree_eq(devA, devB)`, `assert_conflict(path)`.
  Core scenarios (each is a named test): create/modify/delete propagation; offline edits both sides → conflict copy; delete-vs-modify resurrection; ignore defaults (node_modules never uploaded); `fs2.toml` ignore layering; hooks approval gating (hook not run before approval); vault round-trip (server sees only ciphertext — assert!); placeholder hydrate/evict; git repo with rapid commits on one side; kill daemon mid-sync → restart → converges; tombstone GC.
- **Property tests** (`proptest`): random edit sequences on 2–3 devices with random sync interleavings must converge to identical trees with no data loss (any written content survives somewhere, possibly as a conflict copy).
- CI: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` on `ubuntu-latest` + `macos-latest`. FUSE tests: Linux-only, behind `--features fuse`, may require `--privileged`/user_allow_other — keep them in an optional CI job that is allowed to be skipped locally.

---

## 24. Performance targets (v1, guide decisions — don't micro-optimize past them)

- 100k-entry folder: full scan < 10 s warm; incremental change → other machine visible in < 3 s on LAN-quality link.
- Daemon idle: < 1% CPU, < 150 MiB RSS with 10 folders / 300k entries.
- Server: single box, SQLite, handles 3 devices easily; not a scale project.

---

## 25. Milestones

Each milestone ends with all CI green and its integration scenarios passing. `todo.md` breaks these into tasks.

- **M0 — Skeleton**: workspace, CI, core types (ids, VV), config/dirs, logging, CLI/daemon/server binaries that start and talk (UDS `/status`, server `/healthz`), `env.example`, CLAUDE.md.
- **M1 — Local pipeline**: rules engine, chunker, store (index + CAS + migrations), scanner; `fs2 add` + `fs2 status` show real local state (no server sync yet).
- **M2 — Sync**: server manifest/chunk/SSE endpoints, device registration, sync engine (push/pull/VV/conflict copies/tombstones/atomic apply/trash), watcher, `clone/sync/pause/conflicts`, testkit + core scenarios. **This is the "Dropbox works" milestone.**
- **M3 — Developer ergonomics**: `.fs2ignore` + `fs2.toml` layering, `fs2 ignore check`, hooks + approval model, git safeguards, `fs2 doctor`.
- **M4 — Vault**: account key mgmt, `fs2 env *`, encrypted vault sync, key export/import.
- **M5 — Structure mode**: placeholders + hydrate/evict, structure-mode sync, Linux FUSE mount (feature-gated).
- **M6 — Hardening**: property tests, chunk GC, rate limits, metrics, log rotation, packaging (release workflow: static-ish binaries for macOS arm64 + Linux x86_64/arm64), docs.

---

## 26. Future extensions (explicitly out of scope for v1)

Full E2EE for file content (encrypt chunks client-side; kills server dedupe-across-plaintext, fine for single user); S3 `BlobStore`; LAN peer-to-peer transfer; rename detection; macOS FSKit provider; Windows (CFAPI placeholders); multi-user/orgs; web dashboard; selective per-directory sync policies.

## 27. Risks

| Risk | Mitigation |
|---|---|
| inotify watch limits on huge trees | ignore-before-descend, watch-count check in `doctor`, rescan fallback |
| `.git` concurrent-mutation corruption | §14 safeguards + docs; conflict quarantine; worst case `git fsck`+reflog |
| notify event loss/coalescing (FSEvents) | periodic full rescan is the source of truth; events are only an accelerant |
| SQLite contention in daemon | WAL, single writer task per DB, short transactions |
| Placeholder edits by other tools | scanner treats content-bearing placeholders as user data (§16) |
| Users expecting macOS on-demand magic | docs + `fs2 doctor` set expectations (§16.3) |
