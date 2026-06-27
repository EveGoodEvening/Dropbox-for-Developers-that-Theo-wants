# README Goal — Chunk Checklist

> Durable, reviewable, verifiable, committable checklist for materializing the README goal
> (Dropbox-for-Developers cross-machine code sync). Chunk IDs are fixed by
> `local://planning-contract.json` and MUST NOT be renamed or reordered.
>
> Companion artifacts:
> - Implementation plan: `docs/planning/readme-goal-implementation-plan.md`
> - Parallel worktree plan: `docs/planning/readme-goal-parallel-worktrees.md`
> - Progress tracker: `docs/planning/readme-goal-progress-tracker.md`
>
> README capability coverage map (every README requirement is owned by at least one chunk):
>
> | README requirement | Primary chunk(s) | Also exercised by |
> | --- | --- | --- |
> | README.md:8–13 pain points — stale worktrees from missed `git pull` | CHUNK-08-CLI-DAEMON-UX (stale-worktree UX scenario) | 09 (stale-worktree / missed-`git pull` E2E) |
> | README.md:8–13 pain points — env vars set on one machine but not another | CHUNK-07-ENV-SYNC | 08, 09 |
> | README.md:8–13 pain points — inconsistent project directory structures | CHUNK-02-CATALOG-STRUCTURE | 05, 08, 09 |
> | README.md:8–13 pain points — git submodule friction | CHUNK-03-IGNORE-PLATFORM-POLICY (explicit Git metadata policy for `.git/`, submodule `.git` files, and `.gitmodules`; disposition: whole-folder sync replaces submodule workflows — see CHUNK-03) | 09 (submodule + Git metadata E2E) |
> | README.md:15 / README.md:19 — Automatic code folder sync (Dropbox-like) | CHUNK-05-SYNC-STORE-CONVERGENCE | 04, 08, 09 |
> | README.md:20 — Environment variable synchronization | CHUNK-07-ENV-SYNC | 08, 09 |
> | README.md:21 — Structure-first on-demand hydration (fetch contents on access) | CHUNK-02-CATALOG-STRUCTURE, CHUNK-06-LAZY-HYDRATION-VFS | 05, 08 |
> | README.md:22 — node_modules / platform-specific handling | CHUNK-03-IGNORE-PLATFORM-POLICY | 04, 05, 06, 09 |
> | README.md:23 — Ignore semantics (Dropbox/Drive-style) | CHUNK-03-IGNORE-PLATFORM-POLICY | 04, 05, 09 |
> | README.md:24 — FS2 decision (insufficient; build vs. adopt vs. vendor) | CHUNK-01-FOUNDATION | 02, 06 |
>
> Conventions:
> - `[ ]` unchecked / `[x]` checked boxes are definition-of-done guidance for a chunk,
>   not the mutable status ledger. Treat them as proof items a worker satisfies; boxes may
>   remain unchecked unless a workflow explicitly updates this checklist as documentation.
>   The progress tracker is the sole mutable status/evidence ledger for actual status,
>   `👉 NEXT`, review state, verification evidence, and commit/merge-SHA notes.
> - "Target files/areas" are planning-level anchors, not a commitment to exact filenames; the
>   implementation plan may refine them. No product code is written in this artifact.
> - Verification commands are illustrative and assume the stack chosen in CHUNK-01. Replace the
>   binary name (`cargo`, `pnpm`, `go`, `python -m`) with the project's actual toolchain once
>   CHUNK-01 lands. **The planning layer replaces these illustrative commands with the chosen
>   toolchain after CHUNK-01 merges.** Never run gates/formatters/tests as part of editing THIS artifact.
> - **Decision recording:** Each open decision (U1–U4, U6–U10; U5 is pre-resolved)
>   is a checklist item in its owning chunk. Decisions are recorded in the chunk PR and the
>   owning chunk's progress-tracker row. CHUNK-01 additionally records the stack (U1) and FS2
>   (U9) decisions in the implementation plan via an explicit planning-layer exception (see
>   CHUNK-01 and the worktree plan §5); all other chunks record rationale in their PR + tracker row only.
> - **CLI ownership / verification:** CHUNK-01 may create only the empty `cli` shell and
>   minimal `version`/`info` skeleton smoke path. User-facing feature CLI implementation and
>   command verification live in CHUNK-08, with CHUNK-09 rechecking CLI behavior in E2E. CHUNK-02
>   through CHUNK-07 verify via module-level tests / module harnesses only and do not touch `cli/`.

---

## CHUNK-01-FOUNDATION — Repo/project bootstrap

**Depends on:** none.
**Parallel group:** root (must complete before any other chunk starts).
**Target files/areas:** project manifest (`package.json` / `Cargo.toml` / `go.mod` / `pyproject.toml` — pick one), `foundation` module, empty downstream module shells (`catalog`, `policy`, `watcher`, `sync`, `vfs`, `env`, `cli`) with no feature internals, shared config schema module, shared `Platform`/OS identity type, logging module, error-type module, **migration framework** (schema version table + migration runner; empty `v0` baseline, no product tables), minimal CLI `version`/`info` smoke path, CI/lint config, `.gitignore` for the tool itself.

### Checklist items
- [ ] Choose and record the implementation language/stack (resolve U1): one binary, one manifest, one lockfile. Record rationale in the implementation plan (planning-layer exception) and the progress tracker.
- [ ] Record the **FS2 decision** (resolve U9): investigate FS2 (README.md:24 says "nowhere near enough"); write a dated decision note — build-from-scratch vs. adopt-and-extend vs. vendor — in the implementation plan (planning-layer exception) and the progress tracker. Default stance: build-from-scratch unless an inspection proves FS2 covers the catalog/hydration surface. This must land before CHUNK-02/CHUNK-06 design begin.
- [ ] Create project manifest with name, version, license, and a single `build`/`compile` entrypoint.
- [ ] Scaffold module skeleton: `foundation`, `catalog`, `policy`, `watcher`, `sync`, `vfs`, `env`, `cli` empty modules (or equivalents) so later chunks have stable import targets; feature ownership transfers to the owning chunk after CHUNK-01 merges.
- [ ] Define a shared error type hierarchy (`SyncError`, `ConfigError`, `WatchError`, `VfsError`, `EnvError`) with stable string codes.
- [ ] Define a shared logging interface (level, structured fields, machine id, correlation id).
- [ ] Define the global config schema (machine id, root paths, transport endpoint, cache dir) with validation and a default loader.
- [ ] Define a shared `Platform` / OS identity type (normalized OS family, version, architecture, and capability flags) that CHUNK-02 `Machine`, CHUNK-03 policy, and later platform-matrix checks consume rather than redefining.
- [ ] Implement the migration framework: a schema version table and a migration runner that loads an empty `v0` baseline (no product tables). Product schemas (`catalog_*`, `watcher_events`, `sync_*`, `env_*`) are owned by their feature chunks, not here.
- [ ] Add a `.gitignore` for the tool's own build/dep artifacts (not the product ignore mechanism — that is CHUNK-03).
- [ ] Add minimal `version` and `info` commands wired through the CLI module to prove the skeleton compiles end-to-end; no feature commands or user-facing workflows yet.

### Acceptance criteria
- Project builds with one command from a clean clone.
- Minimal `version` and `info` commands print machine id, version, and config path.
- Shared `Platform`/OS identity type is defined in CHUNK-01 and available for CHUNK-02/CHUNK-03 before they start in parallel.
- Every later chunk's module exists as an importable (possibly empty) target, and feature ownership transfers to its owning chunk after CHUNK-01 merges.
- Migration framework loads the empty `v0` baseline and reports the version; no product tables exist yet.
- **FS2 decision (U9) is written down with a rationale and date, not deferred silently.**
- **Stack choice (U1) is written down with a rationale.**

### Verification command(s)
- `cargo build -p <crate>` / `pnpm build` / `go build ./...` / `python -m <pkg> --version` (use the chosen toolchain).
- `<cli> version` and `<cli> info` exit 0 and print version + machine id + config path.
- Migration runner command prints schema version `v0` with no product tables.
- `git status --short` shows only intended new files.

### Review criteria
- One stack chosen, no mixed manifests.
- Error types and logging are shared (not duplicated per chunk).
- Config schema validates and has defaults.
- Shared `Platform`/OS identity type is defined once in foundation and not redefined by CHUNK-02 or CHUNK-03.
- Migration framework present (version table + runner); no product schemas here.
- **FS2 decision (U9) is explicit and dated.**
- **Stack decision (U1) is explicit.**
- No product sync logic yet — this chunk is scaffolding only.

### Commit message suggestion
```
feat(foundation): bootstrap project skeleton, config, errors, logging, migration framework

CHUNK-01-FOUNDATION. Picks <stack>, scaffolds module targets for all
later chunks, adds the minimal version/info CLI shim and tool .gitignore,
defines shared error/log/config/Platform surface, adds the migration
framework (empty v0 baseline), and records the FS2 build-vs-adopt decision
and stack choice. No sync behavior yet.
```

---

## CHUNK-02-CATALOG-STRUCTURE — Project/catalog model + structure-first metadata

**Depends on:** CHUNK-01-FOUNDATION.
**Parallel group:** runs concurrently with CHUNK-03-IGNORE-PLATFORM-POLICY (disjoint files; see worktree plan).
**Target files/areas:** `catalog` module — project model, machine inventory, tree representation, placeholder/stub record schema, structure manifest serialization, `catalog_*` initial migration.

### Checklist items
- [ ] Define the `Project` model (id, root path, machine list, structure manifest id).
- [ ] Define the `Machine` model (id, shared CHUNK-01 `Platform`/OS identity, last-seen, online state).
- [ ] Define a structure-first `TreeManifest` that records the full directory/file tree with metadata (path, type, size, mtime, perms, hash slot) but **not** file contents.
- [ ] Define a `PlaceholderRecord` for not-yet-hydrated files (used by CHUNK-06): path, size, hash, hydration status, source machine.
- [ ] Choose and record the serialization format (resolve U2) with rationale (deterministic ordering, versioned) so two machines can diff structures without fetching contents.
- [ ] Implement structure diff: given two `TreeManifest`s, emit added/removed/modified/moved entries (contentless).
- [ ] Implement a local persistence path for the catalog (SQLite or JSON file under config dir) — schema only, no watcher feeding it yet.
- [ ] Write the `catalog_*` initial migration on top of CHUNK-01's empty baseline; verify apply + rollback to baseline.
- [ ] Add unit tests for structure diff covering add/remove/modify/move and identical trees.

### Acceptance criteria
- Two manifests with the same tree diff to "no changes".
- A renamed file is reported as a move, not an add+remove (when metadata permits).
- Manifest serialization is deterministic across runs (byte-stable for identical input).
- Placeholder records carry enough data for CHUNK-06 to fetch content later.
- `catalog_*` migration applies from the empty baseline and rolls back cleanly.

### Verification command(s)
- `cargo test -p <crate> catalog::` / `pnpm test catalog` / `go test ./catalog/...` — structure diff + serialization tests.
- Migration apply + rollback test for `catalog_*` (module-level test, not a CLI command).

### Review criteria
- No file contents are stored in the manifest (structure-first per README.md:21).
- Move detection is conservative: only assert a move when metadata strongly matches; otherwise emit add+remove.
- **Serialization format (U2) chosen with recorded rationale.**
- Schema is versioned for future migration; `catalog_*` migration owned and revertable.
- No dependency on CHUNK-03 policy yet (policy is applied by the watcher in CHUNK-04).
- No `cli/` edits (CLI command verification is CHUNK-08/09).

### Commit message suggestion
```
feat(catalog): structure-first project/tree model and contentless diff

CHUNK-02-CATALOG-STRUCTURE. Adds Project/Machine/TreeManifest models,
placeholder records for lazy hydration, deterministic manifest
serialization (U2), structure diff, and the catalog_* initial migration.
Stores no file contents.
```

---

## CHUNK-03-IGNORE-PLATFORM-POLICY — Ignore mechanism + platform-specific policy

**Depends on:** CHUNK-01-FOUNDATION.
**Parallel group:** runs concurrently with CHUNK-02-CATALOG-STRUCTURE.
**Target files/areas:** `policy` module — ignore rule engine, `.syncignore` semantics, platform-specific policy table (node_modules, build outputs, OS-specific files), explicit Git metadata policy for `.git/`, submodule `.git` files, and `.gitmodules`, policy evaluation API.

### Checklist items
- [ ] Choose and record the ignore rule format (resolve U3) with rationale (gitignore-style globs are a baseline, but semantics are Dropbox/Drive-style per README.md:23 — document the deltas explicitly).
- [ ] Implement a `.syncignore` loader with precedence: project-local > user-global > built-in defaults.
- [ ] Document semantic differences from `.gitignore` (e.g., whether negation `!` is supported, whether ignored files are tracked as "known-ignored" vs. "absent", directory-only rules).
- [ ] Build a platform-specific policy table using CHUNK-01's shared `Platform`/OS identity: `node_modules/`, `dist/`, `build/`, `.next/`, `target/`, `__pycache__/`, `.venv/`, OS files (`.DS_Store`, `Thumbs.db`), with per-entry action: `ignore` | `rebuild-locally` | `platform-pin`.
- [ ] **Git metadata + submodule disposition (README.md:8–13 "Git submodule hell"):** record an explicit policy for `.git/` directories, submodule `.git` files (gitfile pointers), and `.gitmodules`. Each path class must be intentionally handled as safe excluded/reconstructed metadata or deliberately synced metadata; no accidental default traversal. Record the disposition that whole-folder automatic sync replaces git submodule workflows, and record this policy/disposition in the chunk PR and progress tracker.
- [ ] Define `rebuild-locally` semantics for `node_modules` (README.md:22): do not byte-sync; record a manifest of dependencies and let the destination machine rebuild.
- [ ] Define `platform-pin` semantics for binaries that must match a platform (e.g., native addons): record a CHUNK-01 `Platform` tag, refuse to hydrate on a mismatched platform.
- [ ] Expose a `Policy::evaluate(path, platform: Platform) -> Action` API for CHUNK-04 to call.
- [ ] Add unit tests for each action class (`ignore`, `rebuild-locally`, `platform-pin`) and for precedence ordering.

### Acceptance criteria
- `node_modules/` evaluates to `rebuild-locally`, not `ignore` and not `sync`.
- A `.syncignore` rule overrides a built-in default with documented precedence.
- Negation and directory-only semantics are documented and tested.
- All three `Action` variants (`ignore`, `rebuild-locally`, `platform-pin`) are unit-tested.
- Policy API is pure (no I/O) so CHUNK-04 can call it in a hot loop.
- Git metadata policy and git submodule disposition are recorded, including `.git/`, submodule `.git` files, and `.gitmodules`.

### Verification command(s)
- `cargo test -p <crate> policy::` / `pnpm test policy` / `go test ./policy/...` — rule engine + precedence + platform table + all-action tests.

### Review criteria
- Ignore semantics are explicitly compared to `.gitignore`; deltas are documented, not accidental.
- **Ignore format (U3) chosen with recorded rationale.**
- Git metadata handling is explicit: `.git/`, submodule `.git` gitfiles, and `.gitmodules` are either safely excluded/reconstructed or deliberately synced, with no dangling gitfile pointers or accidental repository metadata traversal.
- `node_modules` handling matches README.md:22 (special handling, not naive sync).
- Policy is pure and fast; no filesystem access inside `evaluate`.
- No edits to the catalog model (CHUNK-02 owns that).
- No `cli/` edits (CLI command verification is CHUNK-08/09).

### Commit message suggestion
```
feat(policy): syncignore engine and platform-specific handling

CHUNK-03-IGNORE-PLATFORM-POLICY. Adds Dropbox/Drive-style ignore
semantics with documented gitignore deltas (U3), platform policy table
(node_modules rebuild-locally, native addons platform-pin), explicit Git
metadata handling for `.git/`, submodule `.git` files, and `.gitmodules`,
and a pure Policy::evaluate API for the watcher.
```

---

## CHUNK-04-WATCHER-INDEXER — Filesystem watcher + indexer

**Depends on:** CHUNK-02-CATALOG-STRUCTURE, CHUNK-03-IGNORE-PLATFORM-POLICY.
**Parallel group:** serializes after {02, 03}; **must complete before CHUNK-05** (05 consumes 04's snapshots/event queue).
**Target files/areas:** `watcher` module — platform FS event adapter, debouncing/coalescing, indexer that applies policy and writes catalog snapshots, event queue, `watcher_events` persistence schema.

### Checklist items
- [ ] Choose and record the filesystem watcher library per OS (resolve U4) with rationale (must work on macOS and Linux).
- [ ] Implement a platform FS event adapter (native events where available, polling fallback).
- [ ] Add event debouncing/coalescing for rapid write bursts and editor save-all.
- [ ] Implement an indexer that walks a project root, applies `Policy::evaluate` from CHUNK-03, and writes a `TreeManifest` via CHUNK-02's catalog API.
- [ ] Handle renames, deletes, and permission changes as first-class events (not just modify).
- [ ] Handle symlinks explicitly: record, do not follow into ignored trees; document behavior.
- [ ] Implement a durable event queue (persisted across restarts) so events are not lost on crash.
- [ ] Implement backpressure: if the indexer falls behind, coalesce and drop redundant events safely.
- [ ] **Freeze the snapshot/event-queue interface** that CHUNK-05 consumes (snapshot format + event-queue API, including policy action metadata, rebuild hints, and platform-pin decisions); document it for downstream.
- [ ] Write the `watcher_events` initial migration on top of CHUNK-01's empty baseline; verify apply + rollback to baseline.
- [ ] Add integration tests with a temp project tree: create/modify/rename/delete/ignore scenarios covering all `Action` variants (`ignore`, `rebuild-locally`, `platform-pin`), preserving action metadata while suppressing content-sync events.

### Acceptance criteria
- A create→modify→delete sequence within the debounce window results in a correct final catalog state, not three separate syncs.
- `node_modules/` changes produce `rebuild-locally` hint metadata for CHUNK-05 but no content sync events.
- `platform-pin` paths preserve the platform-pin decision metadata for CHUNK-05 but produce no content sync events.
- `ignore` paths produce no content sync events; any "known ignored" metadata is explicit and not interpreted as content to sync.
- A crash mid-index leaves the catalog either at the prior consistent snapshot or the next one — never partial.
- Symlinks into ignored dirs are not traversed.
- `watcher_events` migration applies from the empty baseline and rolls back cleanly.

### Verification command(s)
- `cargo test -p <crate> watcher:: -- --ignored` / `pnpm test watcher` / `go test ./watcher/...` — integration tests with temp trees.
- Migration apply + rollback test for `watcher_events` (module-level test, not a CLI command).

### Review criteria
- **Watcher library (U4) chosen with recorded rationale.**
- Policy is applied at index time (no raw FS events leak into the sync layer); all three `Action` variants are exercised, and policy action metadata/rebuild hints/platform-pin decisions are preserved in the frozen snapshot/event-queue contract for CHUNK-05 while content-sync events are suppressed.
- Event queue is durable; no silent event loss.
- Snapshot/event-queue interface is frozen and documented for CHUNK-05.
- Symlink and permission behavior is documented.
- No sync/convergence logic here (CHUNK-05 owns that).
- No `cli/` edits (CLI command verification is CHUNK-08/09).

### Commit message suggestion
```
feat(watcher): FS event adapter, debounced indexer, durable event queue

CHUNK-04-WATCHER-INDEXER. Adds platform FS watching with debounce/
coalesce, policy-aware indexing into the catalog, durable event queue,
policy-action metadata preservation for CHUNK-05, symlink/permission
handling, the watcher_events migration, and the frozen snapshot/event-queue
interface consumed by the sync layer.
```

---

## CHUNK-05-SYNC-STORE-CONVERGENCE — Sync store + convergence engine + production transport

**Depends on:** CHUNK-04-WATCHER-INDEXER (immediate; consumes CHUNK-04's frozen snapshot/event-queue interface and the CHUNK-03 policy actions already serialized through 04).
**Parallel group:** serializes after 04; must complete before {06, 07}.
**Target files/areas:** `sync` module — sync store, convergence engine, conflict resolution, partial/offline state, rename/delete propagation, permission/symlink propagation, **production cross-machine transport/backing store**, endpoint/security config, machine enrollment/auth/pairing, generic authenticated/encrypted payload path, `sync_*` persistence schema.

### Checklist items
- [ ] Choose and record the production sync transport/backing store (resolve U10) with rationale: peer-to-peer service endpoint and/or external service. Loopback is allowed only as a local test harness and cannot be the sole production path. Record endpoint and security configuration requirements.
- [ ] Implement machine enrollment/auth/pairing so two machine instances can establish a trusted sync relationship.
- [ ] Define the sync store protocol (transport-agnostic layer): fetch manifest, fetch content blob, push manifest, push content blob, notify event, and a generic typed payload envelope that downstream modules can use without CHUNK-05 knowing their schemas.
- [ ] Implement the production cross-machine transport/backing store on top of the protocol (not mock-only; not loopback-only in production).
- [ ] Implement endpoint/security config parsing and validation for the production transport (authorized peer, endpoint, and credential/key references; no plaintext secrets in logs).
- [ ] Implement authentication and in-transit encryption for content blobs and the generic payload envelope over the transport.
- [ ] Implement a convergence engine that diffs local and remote `TreeManifest`s and produces a plan (fetch, push, delete, move, rebuild-locally, ignore, platform-pin) — branch on **all** policy actions carried through CHUNK-04 snapshots, not only `rebuild-locally`.
- [ ] Implement conflict resolution per the **resolved U5 policy**: last-writer-wins with a conflict file sidecar + a documented manual escape hatch. Do not silently overwrite.
- [ ] Handle partial/offline state: queue operations, resume on reconnect, never leave a file in a half-fetched state (atomic temp + rename).
- [ ] Propagate renames and deletes (not just creates/modifies) across machines.
- [ ] Propagate permissions and symlink targets where the destination platform supports them; document where it cannot.
- [ ] Respect CHUNK-03 policy during convergence: never push `node_modules/` content (rebuild hint); never push `ignore` content; refuse/redirect `platform-pin` content on mismatched platforms.
- [ ] Write the `sync_*` initial migration on top of CHUNK-01's empty baseline; verify apply + rollback to baseline.
- [ ] **Implement and document a CHUNK-05 rollback/backup/restore procedure for sync-store state at the merge boundary** (back up the sync store + operation log, restore to a known-good state). Verify it with a test independent of CHUNK-09.
- [ ] Add integration tests for: concurrent edit conflict (assert conflict sidecar), offline-then-reconnect, rename across machines, delete propagation, large-file partial failure, production-transport exchange between two sync harness instances, endpoint/security config validation, in-transit encryption/no-plaintext capture, all policy actions, and rollback/restore.

### Acceptance criteria
- Two machines editing the same file produce a conflict sidecar, not silent data loss.
- A machine offline during edits converges correctly on reconnect.
- `node_modules/` is never transmitted as content; a rebuild hint is.
- `ignore` content is never transmitted; `platform-pin` content is refused/redirected on mismatched platforms.
- A half-fetched large file never appears at its final path until complete.
- Two sync harness instances enroll/pair and exchange a manifest + content blob over the **production cross-machine transport/backing store** (not a mock; loopback-only mode is test-only and not the sole production path).
- Content blobs and generic payload envelopes are encrypted/authenticated over the transport; a transport capture contains no plaintext.
- Endpoint/security config is validated and rejects unauthenticated or plaintext-only production settings.
- No env-specific payload/blob schema is introduced before CHUNK-07; CHUNK-05 exposes only the generic payload contract.
- `sync_*` migration applies from the empty baseline and rolls back cleanly.
- Sync-store state can be backed up and restored to a known-good state via the documented CHUNK-05 merge-boundary procedure.

### Verification command(s)
- `cargo test -p <crate> sync::` / `pnpm test sync` / `go test ./sync/...` — convergence + conflict + offline + production transport + endpoint/security config + in-transit + all-policy-action + rollback/restore tests.
- Migration apply + rollback test for `sync_*` (module-level test, not a CLI command).

### Review criteria
- **Transport/backing store (U10) chosen with recorded rationale; production cross-machine path implemented (mock only for deterministic tests; loopback only as a local harness).**
- Endpoint/security config rejects unauthenticated or plaintext-only production settings.
- Conflict policy is the resolved U5 policy (last-writer-wins + conflict sidecar + manual escape hatch); explicit and documented; no silent overwrites.
- Atomic temp-then-rename for content writes.
- Policy from CHUNK-03 is enforced at the sync boundary for all `Action` variants as carried through CHUNK-04 snapshots, not just at watch time.
- Content and generic payload in-transit encryption verified.
- No env-specific blob/schema ownership here; CHUNK-07 defines env payloads using the generic CHUNK-05 contract.
- Machine enrollment/auth/pairing implemented.
- **CHUNK-05 rollback/backup/restore procedure documented and tested at the merge boundary** (merge is independently revertable).
- No hydration/VFS surface here (CHUNK-06 owns on-demand fetch).
- No `cli/` edits (CLI command verification is CHUNK-08/09).

### Commit message suggestion
```
feat(sync): convergence engine, production transport, conflict resolution, rollback

CHUNK-05-SYNC-STORE-CONVERGENCE. Adds transport-agnostic sync store +
production cross-machine transport/backing store with endpoint/security
config, enrollment/auth/pairing, generic encrypted payload envelope,
manifest-diff convergence plan branching on all policy actions,
last-writer-wins + conflict sidecar (U5), partial/offline queueing with
atomic content writes, rename/delete/permission propagation, node_modules
rebuild-hint enforcement, the sync_* migration, and a sync-store
rollback/backup/restore procedure.
```

---

## CHUNK-06-LAZY-HYDRATION-VFS — On-demand content fetch / lazy hydration

**Depends on:** CHUNK-05-SYNC-STORE-CONVERGENCE (immediate; consumes the catalog placeholder and policy contracts through the canonical merged chain).
**Parallel group:** runs concurrently with CHUNK-07-ENV-SYNC (disjoint files; see worktree plan).
**Target files/areas:** `vfs` module — placeholder materialization, on-demand fetch hook, virtual filesystem surface, hydration cache, fallback for non-VFS access.

### Checklist items
- [ ] Choose and record the VFS approach (resolve U6) with rationale (FUSE / platform native vs editor plugin vs transparent stub files).
- [ ] Implement placeholder materialization: when a structure-only file is first seen locally, create a placeholder (sparse file / stub / VFS node) using CHUNK-02's `PlaceholderRecord`.
- [ ] Implement an on-demand fetch hook that pulls content on first access (open/read) via CHUNK-05's sync store.
- [ ] Implement a virtual filesystem surface where the platform supports it (FUSE / WinFsp / macFUSE), with a graceful fallback (transparent file download on `open`) where it does not.
- [ ] Implement a hydration cache with eviction policy and coherency invalidation on remote modify.
- [ ] Handle hydration failures (network down, source machine offline) with a clear error, not a zero-byte file.
- [ ] Respect CHUNK-03 platform policy for **all** `Action` variants at hydration time: `ignore` (not hydrated), `rebuild-locally` (triggers rebuild for `node_modules`, not a content fetch), `platform-pin` (refused on mismatched platform).
- [ ] Add integration tests: first-read hydration, cache hit, invalidation on remote modify, offline failure, platform-pin refusal, ignore non-hydration, rebuild-locally rebuild.

### Acceptance criteria
- A structure-only project can be opened and browsed without any file contents being local.
- First `read` of a file fetches exactly that file's content; unrelated files stay placeholders.
- A remote modify invalidates the cached content before the next read.
- `node_modules` hydration triggers a rebuild, not a content fetch.
- `ignore` paths are not hydrated; `platform-pin` paths are refused on mismatched platforms.
- Offline hydration fails with a non-zero exit and no zero-byte file at the target path.

### Verification command(s)
- `cargo test -p <crate> vfs::` / `pnpm test vfs` / `go test ./vfs/...` — hydration + cache + invalidation + all-policy-action tests.

### Review criteria
- **VFS approach (U6) chosen with recorded rationale.**
- Structure-first is preserved until access (README.md:21).
- No zero-byte files on failure.
- Platform policy enforced at hydration time for all `Action` variants.
- Fallback path is documented for platforms without a VFS.
- No `cli/` edits (CLI command verification is CHUNK-08/09).

### Commit message suggestion
```
feat(vfs): lazy hydration, on-demand fetch, hydration cache

CHUNK-06-LAZY-HYDRATION-VFS. Adds placeholder materialization, on-demand
content fetch on first access, VFS surface (U6) with non-VFS fallback,
hydration cache with remote-modify invalidation, and platform-policy
enforcement at hydration time for all Action variants.
```

---

## CHUNK-07-ENV-SYNC — Environment variable synchronization

**Depends on:** CHUNK-05-SYNC-STORE-CONVERGENCE (immediate; consumes CHUNK-05's generic authenticated/encrypted payload contract and the CHUNK-01 config/error surface through the merged chain).
**Parallel group:** runs concurrently with CHUNK-06-LAZY-HYDRATION-VFS.
**Target files/areas:** `env` module — env var store, encryption (at rest and in transit), key management, per-machine overrides, audit log, CHUNK-07-owned env payload schema over the CHUNK-05 generic payload contract, env materialization API for developer processes, `env_*` persistence schema, encryption key location.

### Checklist items
- [ ] Choose and record the encryption key management scheme (resolve U7) with rationale (provisioning, rotation, per-machine keys).
- [ ] Define an env var model (key, value, scope, owner machine, last-writer, secret flag).
- [ ] Implement at-rest encryption for values flagged secret (never log or persist plaintext secrets).
- [ ] Define the env-specific payload schema on top of CHUNK-05's generic authenticated/encrypted payload envelope; do not require CHUNK-05 to know env-specific blobs.
- [ ] Implement in-transit encryption/no-plaintext checks for env secrets over CHUNK-05's transport; prove plaintext does not cross the transport.
- [ ] Implement per-machine overrides: a var can be pinned to a machine and not propagated.
- [ ] Implement an audit log: every env var read/write/sync event recorded with machine id and timestamp.
- [ ] Integrate with CHUNK-05's sync store: env vars sync as CHUNK-07-owned payloads, with conflict resolution reusing the **resolved U5 policy** (last-writer-wins + conflict sidecar).
- [ ] Implement a redaction surface: logs, errors, and the later CLI rendering adapter redact secret values without CHUNK-07 editing `cli/`.
- [ ] Implement env materialization usable by developer processes (for example, launch-environment map or export stream) without writing plaintext secrets to logs or persistent config.
- [ ] Implement a `.envsyncignore`-style mechanism for vars that must never sync (e.g., machine-local tokens).
- [ ] Write the `env_*` initial migration on top of CHUNK-01's empty baseline; verify apply + rollback to baseline.
- [ ] Add unit/integration tests for encryption round-trip, key provisioning/rotation, override precedence, audit completeness, materialization, redaction, and no-plaintext in-transit capture.

### Acceptance criteria
- Secret values are encrypted at rest and in transit; plaintext never crosses the transport and is never logged.
- Env payloads use the CHUNK-05 generic authenticated/encrypted payload contract; CHUNK-07 owns the env-specific schema.
- A machine without the correct key cannot decrypt env secrets.
- A machine-local override is not propagated to other machines.
- Audit log records every write with machine id and timestamp.
- Env sync conflicts reuse the resolved U5 conflict policy (no silent overwrite).
- Env materialization can supply a developer process with the expected variables without logging or persisting plaintext secrets.
- `env_*` migration applies from the empty baseline and rolls back cleanly.

### Verification command(s)
- `cargo test -p <crate> env::` / `pnpm test env` / `go test ./env/...` — encryption, key management, in-transit/no-plaintext, override, audit, materialization, redaction tests.
- Migration apply + rollback test for `env_*` (module-level test, not a CLI command).

### Review criteria
- **Key management (U7) chosen with recorded rationale.**
- Secrets are never plaintext at rest, in transit, or in logs (README.md:20 sensitivity).
- In-transit encryption/no-plaintext checks use CHUNK-05's transport contract.
- Per-machine override and never-sync lists are distinct and documented.
- Audit log is append-only.
- Reuses the resolved U5 conflict resolution; does not invent a second policy.
- Env materialization is usable by developer processes without requiring CHUNK-08 CLI ownership.
- No `cli/` edits (CLI command verification is CHUNK-08/09).

### Commit message suggestion
```
feat(env): encrypted env var sync with key management, in-transit encryption, overrides, audit

CHUNK-07-ENV-SYNC. Adds env var model, at-rest + in-transit encryption
for secrets using CHUNK-05's generic payload contract, key management
(U7), per-machine overrides, append-only audit log, developer-process
materialization, redaction, .envsyncignore, reuse of the resolved U5
conflict policy, and the env_* initial migration.
```

---

## CHUNK-08-CLI-DAEMON-UX — CLI + daemon + UX wiring

**Depends on:** CHUNK-06-LAZY-HYDRATION-VFS, CHUNK-07-ENV-SYNC (immediate; consumes CHUNK-04 watcher and CHUNK-05 sync capabilities through the canonical merged chain).
**Parallel group:** serializes after {06, 07}; must complete before CHUNK-09.
**Target files/areas:** `cli` module + daemon process — command surface, daemon control loop, status/progress UX, config commands, doctor/health command. **Owns `cli/` and the full command surface** (earlier chunks did not touch `cli/` beyond CHUNK-01's minimal `version`/`info` skeleton).

### Checklist items
- [ ] Implement the command surface: `init`, `watch`, `sync`, `sync pause`, `sync resume`, `hydrate`, `env`, `policy`, `catalog`, `status`, `doctor`, `version`/`info`.
- [ ] Implement the daemon: long-running process hosting the watcher, sync engine, and VFS; supervised restart on crash.
- [ ] Choose and record the daemon supervision model per OS (resolve U8) with rationale.
- [ ] Implement a control protocol between the CLI and the daemon (socket / named pipe) so short-lived CLI commands talk to the long-lived daemon.
- [ ] Implement status/progress UX: current sync state, `sync pause`/`sync resume` state, pending ops, conflicts, hydration counts, env sync state.
- [ ] Implement a `doctor` command that checks: config valid, **transport reachable** (against CHUNK-05's production transport), permissions, cache dir writable, policy table loaded.
- [ ] Wire logging from all modules through the shared logger (CHUNK-01) with correlation ids per sync cycle.
- [ ] Implement graceful shutdown: drain in-flight ops, persist event queue, close transport.
- [ ] **Stale-worktree UX scenario (README.md:8–13 pain point):** a stale-worktree state is observable from the CLI and recoverable (e.g., `status` shows divergence, `sync` reconciles).
- [ ] Add smoke tests for each command against a running daemon with a temp project, including `sync pause`/`sync resume` while work is queued.

### Acceptance criteria
- `init` creates a project catalog and starts watching.
- `status` reflects real daemon state (not a static snapshot).
- `doctor` exits non-zero on any failed check with a remediation hint; the transport-reachable check hits CHUNK-05's production transport.
- Daemon survives a watcher crash via supervised restart.
- Graceful shutdown leaves no orphaned temp files.
- Stale-worktree scenario is observable and recoverable from the CLI.
- `sync pause` stops new sync work without dropping queued operations; `sync resume` drains the persisted queue and converges.

### Verification command(s)
- `cargo test -p <crate> cli::` / `pnpm test cli` / `go test ./cli/...` — command smoke tests.
- `<cli> doctor` exits 0 on a healthy setup; non-zero with a hint on a broken one.
- `<cli> catalog show <project>`, `<cli> policy explain <path>`, `<cli> watch <project> --once`, `<cli> sync status <project>`, `<cli> sync pause <project>`, `<cli> sync resume <project>`, `<cli> hydrate <project> --list`, `<cli> env list`, `<cli> env audit` behave as documented (these CLI verifications live here, not in CHUNK-02–07).

### Review criteria
- All prior chunks' features are reachable from the CLI.
- **Daemon supervision model (U8) chosen with recorded rationale.**
- Daemon is the single long-lived process; CLI is thin.
- No new sync/catalog/policy logic — only wiring.
- Graceful shutdown is tested, not theoretical.
- Stale-worktree UX scenario covers README.md:8–13.

### Commit message suggestion
```
feat(cli): daemon, command surface, status UX, doctor health check

CHUNK-08-CLI-DAEMON-UX. Wires watcher/sync/vfs/env into a supervised
daemon (U8) with a CLI control protocol, status/progress UX, `sync pause`
and `sync resume`, doctor health check (transport reachable), stale-worktree
UX scenario, shared logging with correlation ids, and graceful shutdown.
```

---

## CHUNK-09-E2E-HARDENING — End-to-end hardening and multi-machine scenarios

**Depends on:** CHUNK-08-CLI-DAEMON-UX.
**Parallel group:** final; nothing runs concurrently.
**Target files/areas:** `tests/e2e/` — multi-machine simulation over the production transport, failure injection, platform matrix, performance/correctness gates with predeclared sync/index and hydration-latency budgets, rollout/rollback runbook, bounded manifest/policy and public command/protocol parsing fuzz targets. **Tests/runbook/fuzz only — no production module internals.**

### Checklist items
- [ ] Build a multi-machine test harness: two or more daemon instances with isolated roots, using the **production transport configured for isolated local endpoints** (mock transport only for deterministic lower-level tests; loopback-only production is not allowed).
- [ ] Scenario: machine A edits, machine B reads — assert convergence and hydration correctness.
- [ ] Scenario: simultaneous edit on A and B — assert conflict sidecar (resolved U5 policy), no data loss.
- [ ] Scenario: B offline during A's edits, then reconnect — assert correct convergence.
- [ ] Scenario: CLI `sync pause` during queued sync work, then `sync resume` — assert queued operations persist, no duplicate writes occur, and convergence completes.
- [ ] Scenario: stale worktree / missed `git pull` — create a machine state that missed an upstream change, assert the CLI/daemon reports the divergence, then assert sync recovery converges without data loss.
- [ ] Scenario: `node_modules` changed on A — assert B rebuilds locally, no content sync.
- [ ] **Scenario: `.syncignore` / ignored files across two machines — ignored content is not transmitted, hydrated, or reported as missing, while adjacent non-ignored content still syncs (README.md:23).**
- [ ] Scenario: env var added on A with a secret — assert encrypted at rest on B, redacted in B's CLI, **and encrypted in transit** (a transport capture contains no plaintext).
- [ ] Scenario: a repository containing a git submodule — assert the CHUNK-03 Git metadata policy for `.git/`, the submodule `.git` gitfile, and `.gitmodules`: metadata is either safely excluded/reconstructed or deliberately synced, and the result has no dangling gitfile pointers, accidental raw repository metadata traversal, or breakage.
- [ ] Failure injection: kill daemon mid-sync, network partition, source machine offline at hydration time.
- [ ] Platform matrix: run the suite on macOS and Linux (README.md:8 target machines); document Windows where applicable.
- [ ] Performance gate: before running the 10k-file project measurement, declare sync/index and hydration-latency budget thresholds; then measure structure diff, sync/index, and content hydration latency and fail if results exceed the predeclared thresholds.
- [ ] Rollback drill: a bad sync is rolled back to a known-good state via the documented procedure (exercising CHUNK-05's sync-store rollback + the release runbook).
- [ ] Write a rollout/rollback runbook: how to ship, how to roll back a bad release, how to migrate a schema (per-namespace migrations owned by schema chunks).
- [ ] Add bounded fuzz targets for both manifest diff / policy evaluation and public command / daemon-control-protocol parsing; each fuzz command uses a fixed seed plus time or iteration cap.

### Acceptance criteria
- All listed scenarios pass deterministically (no flaky retries in green runs).
- No zero-byte files or plaintext secrets observed in any scenario (at rest or in transit).
- `node_modules` is never transmitted as content in any scenario.
- Ignored content is never transmitted, hydrated, or reported as missing.
- At least one E2E path runs over the production transport configured in the local harness (not mock-only; not a loopback-only production path).
- Rollback runbook is actionable from a clean checkout and exercises CHUNK-05's procedure.
- Any failure discovered here spawns a scoped upstream fix branch using the worktree plan/tracker procedure: the progress tracker records CHUNK-09 blocked evidence, `👉 NEXT` movement to the fix, fix evidence/merge SHA, and CHUNK-09 resume verification before CHUNK-09 can pass.
- Performance evidence includes the predeclared sync/index and hydration-latency thresholds, measured results for the 10k-file/hydration scenarios, and an explicit pass/fail comparison.
- Both fuzz target families (manifest/policy and public command/protocol parsing) run with bounded seeds/caps and record their commands/results.

### Verification command(s)
- `cargo test -p <crate> --test e2e` / `pnpm test e2e` / `go test ./tests/e2e/...` — full multi-machine suite over the production transport, including CLI `sync pause`/`sync resume`.
- `cargo fuzz run manifest_policy` / equivalent bounded fuzz target for manifest diff + policy evaluation.
- `cargo fuzz run command_protocol` / equivalent bounded fuzz target for public command + daemon-control-protocol parsing.

### Review criteria
- Scenarios cover every README requirement (README.md:8–13 pain points and README.md:19–24 capabilities), including stale worktrees / missed `git pull`, env sync, on-demand hydration, node_modules, ignore, CHUNK-01's FS2 decision, CHUNK-03's Git metadata/submodule disposition, and in-transit secret safety.
- Failure injection is real (process kill, partition), not simulated flags.
- Platform matrix matches README.md:8 (Mac Mini, Linux box).
- Runbook is tested, not aspirational.
- **CHUNK-09 is tests/runbook/fuzz only; failures use the progress-trackable scoped upstream fix procedure before CHUNK-09 resumes.**
- Mock transport is used only for deterministic lower-level tests; at least one E2E path uses the production transport.
- Performance budgets are declared before measurement and compared against the 10k-file sync/index and hydration-latency results.
- Fuzz coverage includes both manifest/policy fuzzing and public command/protocol parsing fuzzing, with fixed seed plus time/iteration cap.

### Commit message suggestion
```
test(e2e): multi-machine convergence, failure injection, platform matrix

CHUNK-09-E2E-HARDENING. Adds a multi-machine test harness over the real
transport covering concurrent edits, stale-worktree / missed-`git pull`,
queued `sync pause`/`sync resume`, offline reconnect, node_modules rebuild,
ignore semantics, encrypted env sync (at rest + in transit), CHUNK-03 Git
metadata/submodule disposition, failure injection, platform matrix,
predeclared performance/hydration-latency gates, both fuzz target families,
and a rollout/rollback + schema migration runbook. Failures use tracker-scoped
upstream fix branches.
```

---

## Cross-chunk invariants (checked at every chunk boundary)

- [ ] No chunk introduces a second sync engine, second policy engine, or second conflict resolver. The conflict resolver is the resolved U5 policy (last-writer-wins + conflict sidecar) everywhere.
- [ ] Every chunk's verification command is runnable from a clean checkout. E2E uses the production transport configured in a local harness; mock transport is for deterministic lower-level tests only.
- [ ] **Every chunk's commit is independently revertable without breaking earlier chunks.** CHUNK-05 satisfies this via its own sync-store rollback/backup/restore procedure; CHUNK-09 hardens the end-to-end drill but does not introduce the first rollback plan for sync state.
- [ ] CHUNK-02 through CHUNK-07 do not touch `cli/`; user-facing CLI command verification lives in CHUNK-08 and CHUNK-09.
- [ ] Product schemas (`catalog_*`, `watcher_events`, `sync_*`, `env_*`) are owned by their feature chunks; CHUNK-01 owns only the migration framework + empty `v0` baseline.
- [ ] U5 is the only decision pre-resolved at planning time; every blocker-tagged decision (U1, U6, U7, U9, U10) must have a recorded accepted decision before its owning chunk's review is accepted.
- [ ] Checklist items must be satisfied, but checklist boxes remain definition-of-done guidance and do not need to be mutated. If a workflow updates boxes as documentation, it also updates the progress tracker; the tracker remains the sole mutable status/evidence ledger whenever a chunk advances.
