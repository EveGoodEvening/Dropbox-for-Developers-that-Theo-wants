# README-Goal Progress Tracker — Dropbox for Developers that Theo wants

> Durable progress tracker for the README.md goal. Companion to `docs/planning/readme-goal-implementation-plan.md`. Tasks start unchecked and are checked only when their evidence is recorded. Current resume state is captured in **Next Up** and the **Summary table**; this file is the **sole mutable source of truth** for per-task/per-chunk status, verification evidence, commit rows, `👉 NEXT` placement, and resume state.

## How to use this tracker

- Keep this tracker mutable and current. The chunk checklist is the per-chunk **definition of done**; this tracker is where status/evidence changes are recorded.
- One section per canonical chunk ID (exact IDs from `local://planning-contract.json`). CHUNK-09 upstream fixes are recorded as scoped fix tasks/evidence under the affected existing chunk ID; canonical chunk IDs stay unchanged.
- Check a tracker task box only when its acceptance evidence is recorded in the chunk's **Verification evidence** subsection (below each chunk's task list).
- A chunk is "done" only when every tracker task is checked, **Review status** is `accepted`, and any blocker-tagged unresolved decision for that chunk is resolved.
- A chunk PR must update this tracker with status/evidence before review can be accepted; tracker evidence/status is part of review, not a post-review chore.
- After a chunk PR is squash-merged to `master`, master-side tracker edits are limited to recording the merge SHA in `Commit(s)` and moving `👉 NEXT` / Next Up markers. Any other status/evidence correction must be a separate tracker correction docs PR/commit (for example `docs(progress): update chunk-XX evidence`).
- The **Commit(s)** row records implementation commits, tracker/evidence commits, and the final merge SHA after squash merge.
- When a chunk starts, fill **Owner branch / worktree** with the exact lowercase, kebab-case branch and sibling `../dropbox-dev-chunk-NN-*` worktree path from `docs/planning/readme-goal-parallel-worktrees.md`; scoped CHUNK-09 upstream fixes use that artifact's §4 naming convention and remain under the affected existing chunk ID.
- `👉 NEXT` is task-level: every dependency-ready unchecked chunk has one marker on its first unchecked doable task. Parallel-ready waves have multiple simultaneous markers (02 ∥ 03 after CHUNK-01; 06 ∥ 07 after CHUNK-05). Move markers as tasks complete.
- After CHUNK-01 chooses the stack/toolchain, post-CHUNK-01 verification rows must record the exact stack-specific command or scenario run; this plan's behavior-level checks are not a substitute for concrete command evidence.

## Next Up

All planned chunks CHUNK-01 through CHUNK-09 are merged on `master`; final split review/classification is the only remaining orchestration gate.

No task-level `👉 NEXT` marker remains: all durable checklist chunks are implemented, verified, reviewed, and merged.

Dependency order: `01 → {02, 03} → 04 → 05 → {06, 07} → 08 → 09`. CHUNK-04 and CHUNK-05 are serialized (04 before 05); there is no 04 ∥ 05 parallel wave. When the `{02, 03}` or `{06, 07}` waves become dependency-ready, this section and the task lists must show multiple simultaneous `👉 NEXT` markers.

## Global status legend

- `[ ]` — not started
- `[~]` — in progress
- `[x]` — done (evidence recorded + review accepted)
- `[!]` — blocked (see Blocker/deferred reason)

---

## CHUNK-01-FOUNDATION

- **Status:** `[x]` done — implementation verified, review accepted, and merged
- **Owner branch / worktree:** `chunk-01-foundation` / `../dropbox-dev-chunk-01-foundation` (merged; worktree removal pending)
- **Commit(s):** `814afca` (`feat(foundation): bootstrap project skeleton and shared core`)
- **Review status:** accepted — correctness, security, and accounting reviews clean after fixes
- **Blocker / deferred reason:** U1 resolved 2026-06-27: Rust 2021 single Cargo package/binary (`dropbox-dev`) with `Cargo.lock`. U9 resolved 2026-06-27: build from scratch; local inspection found no FS2 code/API/manifest beyond README/planning mentions and README.md:24 says FS2 is "nowhere near enough". No CHUNK-01 blocker remains.
- **Depends on:** — (root)

### Tasks
- [x] Choose stack/language (resolve U1) and record rationale (Rust 2021; recorded in implementation plan via the planning-layer exception + this tracker)
- [x] Record the FS2 decision (resolve U9: build-from-scratch vs adopt-and-extend vs vendor) with rationale and date (build from scratch; recorded in implementation plan via the planning-layer exception + this tracker)
- [x] Create project manifest + module skeleton
- [x] Establish build command (`make build` or equivalent) — `make build` passed after security fixes on 2026-06-27
- [x] Establish test harness (`make test`) — `make test` passed after review fixes on 2026-06-27 with 15 unit tests + doc-tests
- [x] Establish lint/typecheck (`make lint`) — `make lint` passed after security fixes on 2026-06-27 (`cargo clippy --locked --all-targets -- -D warnings`)
- [x] Add config loading + structured logging + error model
- [x] Define shared `Platform`/OS identity type (canonical OS, architecture, machine-id provenance, app-scoped derived machine id) and export it for CHUNK-02/CHUNK-03 consumers
- [x] Implement migration framework (schema version table + runner; empty `v0` baseline, no product tables)
- [x] Add tool `.gitignore` entries for generated build/test/cache artifacts
- [x] Add minimal CLI `version`/`info` smoke shim; `<cli> info` prints app-scoped machine id, version, and config path; `--help` is optional only
- [x] Record verification evidence (see subsection below)

### Verification evidence

| Slot | Command/scenario | Result | Date / SHA | Output artifact / pasted summary | Tracker/evidence commit(s) |
|------|------------------|--------|------------|----------------------------------|----------------------------|
| Acceptance evidence | `make build`; `make test`; `make lint`; `make version`; `make info` | Passed after review fixes | 2026-06-27 / `814afca` | `make build` passed; `make test` passed 15 unit tests + doc-tests; `make lint` passed clippy with `-D warnings`; `make version` printed `dropbox-dev 0.1.0`; `make info` stdout included `machine_id=dropbox-dev-...`, `version=0.1.0`, `config_path=...`, `schema_version=v0`, `product_tables=0`; stderr logged only app-scoped machine id plus escaped fields/provenance and did not include raw OS machine id | `814afca` |
| Stack decision (U1) | Read `Cargo.toml`, `Cargo.lock`, `Makefile`, `src/lib.rs`, and implementation-plan U1 row | Recorded | 2026-06-27 / `814afca` | Rust 2021, one Cargo package/binary (`dropbox-dev`), one lockfile; rationale recorded in implementation plan CHUNK-01 section and U1 decision row | `814afca` |
| FS2 decision (U9) | Local FS2 reference inspection + README.md:24 review | Recorded | 2026-06-27 / `814afca` | Build from scratch; no FS2 code/API/manifest present locally beyond README/planning mentions; README says FS2 is insufficient | `814afca` |
| Migration baseline smoke | `make migration-info` | Passed after security fixes | 2026-06-27 / `814afca` | Alias of `make info`; output included `schema_version=v0` and `product_tables=0`; product schema tables are not introduced in CHUNK-01 | `814afca` |
| Migration framework consumer smoke | `cargo test foundation::migration` | Passed | 2026-06-27 / `814afca` | Feature migration registration, apply, duplicate-version rejection, and rollback hooks passed; downstream chunks can register product migrations after empty v0 without reopening foundation | `814afca` |
| Security regression smoke | raw OS machine-id leak check; unset config-home scenario; relative `DROPBOX_DEV_CONFIG` scenario | Passed | 2026-06-27 / `814afca` | Raw OS machine id was absent from stdout/stderr; env with `DROPBOX_DEV_CONFIG`, `XDG_CONFIG_HOME`, `APPDATA`, and `HOME` unset exited non-zero with `CONFIG_INVALID`; relative `DROPBOX_DEV_CONFIG=dropbox-dev/config.conf` exited non-zero with `CONFIG_INVALID` | `814afca` |
| Config/logging regression smoke | `cargo test foundation::config`; `cargo test foundation::logging` | Passed | 2026-06-27 / `814afca` | Config probing skips only NotFound and returns `CONFIG_IO` for metadata/read errors; logger escapes newline, carriage return, tab, and ASCII controls so structured log fields cannot inject raw line breaks/control bytes | `814afca` |

---

## CHUNK-02-CATALOG-STRUCTURE

- **Status:** `[x]` done — implementation verified, review accepted, and merged
- **Owner branch / worktree:** `chunk-02-catalog-structure` / `../dropbox-dev-chunk-02-catalog-structure` (merged; worktree removal pending)
- **Commit(s):** `542ee65` (`feat(catalog): add structure-first manifest model`)
- **Review status:** accepted — catalog correctness and wave integration reviews clean
- **Blocker / deferred reason:** U2 resolved 2026-06-27: deterministic versioned line-oriented stdlib-only manifest serialization. No CHUNK-02 blocker remains.
- **Depends on:** CHUNK-01-FOUNDATION
- **Parallel with:** CHUNK-03-IGNORE-PLATFORM-POLICY

### Tasks
- [x] Define project + directory structure model (transport-independent) using CHUNK-01's shared `Platform`/OS identity type for machine/platform fields
- [x] Implement cross-machine structure reconciliation (deterministic)
- [x] Property test: structure sync never fetches file contents
- [x] Unit test: two differing structures converge to one canonical record
- [x] Unit test: add/remove directory updates catalog idempotently
- [x] Resolve U2 (serialization format) with recorded rationale (PR + tracker)
- [x] Write `catalog_*` initial migration on CHUNK-01's empty baseline; verify apply + rollback
- [x] Record verification evidence (see subsection below)

### Verification evidence

| Slot | Command/scenario | Result | Date / SHA | Output artifact / pasted summary | Tracker/evidence commit(s) |
|------|------------------|--------|------------|----------------------------------|----------------------------|
| Acceptance evidence | `cargo test --locked -p dropbox-dev catalog::`; `cargo clippy --locked --all-targets -- -D warnings` | Passed | 2026-06-27 / `542ee65` | Catalog tests passed 8 tests; clippy passed with `-D warnings`; catalog review and wave integration review returned clean | `542ee65` |
| Additional task evidence | Staged `src/catalog/mod.rs` review | Passed | 2026-06-27 / `542ee65` | Transport-independent Project/Machine/TreeManifest/TreeEntry/PlaceholderRecord model; deterministic contentless serialization; add/remove/modify/move diff; catalog migration descriptor/runner | `542ee65` |

---

## CHUNK-03-IGNORE-PLATFORM-POLICY

- **Status:** `[x]` done — implementation verified, review accepted, and merged
- **Owner branch / worktree:** `chunk-03-ignore-platform-policy` / `../dropbox-dev-chunk-03-ignore-platform-policy` (merged; worktree removal pending)
- **Commit(s):** `8e73304` (`feat(policy): add syncignore and platform policy`)
- **Review status:** accepted — policy security/correctness and wave integration reviews clean after Git metadata fix
- **Blocker / deferred reason:** U3 resolved 2026-06-27: `.syncignore`-style policy distinct from `.gitignore`, with project/user overrides for ordinary built-ins and non-overridable local-only Git metadata. No CHUNK-03 blocker remains.
- **Depends on:** CHUNK-01-FOUNDATION
- **Parallel with:** CHUNK-02-CATALOG-STRUCTURE

### Tasks
- [x] Define ignore/exclusion mechanism distinct from `.gitignore` (unless explicitly chosen otherwise)
- [x] Resolve U3 (ignore format) with recorded rationale (PR + tracker)
- [x] Define platform-specific policy (`node_modules`, generated/dependency dirs, OS artifacts) for Mac + Linux using CHUNK-01's shared `Platform`/OS identity type
- [x] Record git/submodule metadata disposition: `.git/` directories and submodule `.git` pointer files are local-only metadata, `.gitmodules` behavior is explicit, and whole-folder sync replaces submodule workflows
- [x] Unit test: `node_modules/`, generated dir, and ignore-patterned file are excluded from sync
- [x] Unit test: platform-specific path handled per policy (not byte-synced)
- [x] Unit test: all three `Action` variants (`ignore`, `rebuild-locally`, `platform-pin`)
- [x] Unit test: ignore rules add/remove without touching `.gitignore`
- [x] Unit test: Git metadata policy covers `.git/`, submodule `.git` file pointers, and `.gitmodules` without touching `.gitignore`
- [x] Record verification evidence (see subsection below)

### Verification evidence

| Slot | Command/scenario | Result | Date / SHA | Output artifact / pasted summary | Tracker/evidence commit(s) |
|------|------------------|--------|------------|----------------------------------|----------------------------|
| Acceptance evidence | `cargo test --locked -p dropbox-dev policy::`; `cargo clippy --locked --all-targets -- -D warnings` | Passed | 2026-06-27 / `8e73304` | Policy tests passed 10 tests; clippy passed with `-D warnings`; policy rereview and wave integration review returned clean | `8e73304` |
| Additional task evidence | Staged `src/policy/mod.rs` review | Passed | 2026-06-27 / `8e73304` | Pure no-I/O `.syncignore` policy; node_modules rebuild-locally; generated/OS ignores; platform-pin; non-overridable Git metadata; ordinary built-in overrides retained | `8e73304` |

---

## CHUNK-04-WATCHER-INDEXER

- **Status:** `[x]` done — implementation verified, review accepted, and merged
- **Owner branch / worktree:** `chunk-04-watcher-indexer` / `../dropbox-dev-chunk-04-watcher-indexer` (merged; worktree removal pending)
- **Commit(s):** `97351fc` (`feat(watcher): add policy-aware indexer and durable events`)
- **Review status:** accepted — watcher correctness, integration, and accounting reviews clean after fixes
- **Blocker / deferred reason:** U4 resolved 2026-06-27: stdlib polling filesystem adapter over a real project root for macOS/Linux-compatible behavior; later native fsevents/inotify adapters may plug into the frozen contracts. No CHUNK-04 blocker remains.
- **Depends on:** CHUNK-02-CATALOG-STRUCTURE, CHUNK-03-IGNORE-PLATFORM-POLICY (CHUNK-01 is transitive through both)
- **Parallel with:** — (consumes 02 + 03; produces the interface 05 consumes)

### Tasks
- [x] Select filesystem watcher library per OS (resolve U4) with rationale (PR + tracker)
- [x] Implement local indexer consistent with catalog (02) and policy (03)
- [x] Integration test: create/edit/move/delete produces correct, deduped events
- [x] Integration test: ignored, Git metadata, and platform-specific paths produce no unsafe content-sync events, while snapshot/event queue preserves policy action metadata/rebuild hints/platform-pin decisions for CHUNK-05 (all `Action` variants)
- [x] Race test: rapid bulk changes converge to stable index
- [x] Handle renames, deletes, permissions, symlinks
- [x] Freeze snapshot/event-queue interface for CHUNK-05
- [x] Write `watcher_events` initial migration on CHUNK-01's empty baseline; verify apply + rollback
- [x] Record verification evidence (see subsection below)

### Verification evidence

| Slot | Command/scenario | Result | Date / SHA | Output artifact / pasted summary | Tracker/evidence commit(s) |
|------|------------------|--------|------------|----------------------------------|----------------------------|
| Acceptance evidence | `cargo test --locked -p dropbox-dev watcher::`; `cargo clippy --locked --all-targets -- -D warnings` | Passed | 2026-06-27 / `97351fc` | Watcher tests passed 21 tests; clippy passed with `-D warnings`; final clean review returned no material blockers | `97351fc` |
| Additional task evidence | Staged `src/watcher/mod.rs` review | Passed | 2026-06-27 / `97351fc` | Stdlib polling filesystem adapter; durable file-backed event queue; policy-aware indexing; move/delete/policy transition coalescing; symlink target payload; watcher_events migration descriptor | `97351fc` |

---

## CHUNK-05-SYNC-STORE-CONVERGENCE

- **Status:** `[x]` done — implementation verified, review accepted, and merged
- **Owner branch / worktree:** `chunk-05-sync-store-convergence` / `../dropbox-dev-chunk-05-sync-store-convergence` (merged; worktree removal pending)
- **Commit(s):** `3379287` (`feat(sync): add file-backed convergence store`)
- **Review status:** accepted — sync correctness/security/accounting reviews clean after fixes
- **Blocker / deferred reason:** U5 resolved: last-writer-wins + conflict sidecar + manual escape hatch. U10 resolved 2026-06-27: append-only file-backed sync store rooted at a configurable shared directory / mounted network path / object-store-like directory; local harness uses same contract. No CHUNK-05 blocker remains.
- **Depends on:** CHUNK-04-WATCHER-INDEXER, CHUNK-03-IGNORE-PLATFORM-POLICY (direct policy action contract; CHUNK-02/CHUNK-01 are transitive)
- **Parallel with:** — (serializes after 04; must complete before {06, 07})

### Tasks
- [x] Choose production sync transport/backend (resolve U10) with rationale (real cross-machine/backing path; loopback harness only)
- [x] Implement machine enrollment/auth/pairing
- [x] Implement authoritative sync store + operation log
- [x] Implement production cross-machine/backing transport path (mock only for deterministic tests; loopback only as local harness for the production contract)
- [x] Implement generic authenticated/encrypted payload + content transport contract
- [x] Implement convergence protocol (partial/offline, reconnect) branching on all `Action` variants from CHUNK-03 policy and the recorded Git metadata/submodule disposition
- [x] Implement conflict resolution per resolved U5 (last-writer-wins + conflict sidecar + manual escape hatch)
- [x] Integration test: two machines editing same file converge with conflict sidecar (no silent data loss)
- [x] Integration test: offline machine reconnects and converges
- [x] Property test: operation log replay is deterministic
- [x] Transport test: two daemon instances exchange generic manifest metadata + content payloads over the production transport path via the local harness (not env-specific blobs; not mock-only)
- [x] In-transit security test: plaintext generic payload/content does not cross the transport
- [x] All-policy-action + Git-metadata test: CHUNK-03 `ignore`, `rebuild-locally`, `platform-pin`, `.git/`, submodule `.git` files, and `.gitmodules` disposition branched in convergence
- [x] Write `sync_*` initial migration on CHUNK-01's empty baseline; verify apply + rollback
- [x] Implement + document sync-store rollback/backup/restore procedure; verify with a test
- [x] Record verification evidence (see subsection below)

### Verification evidence

| Slot | Command/scenario | Result | Date / SHA | Output artifact / pasted summary | Tracker/evidence commit(s) |
|------|------------------|--------|------------|----------------------------------|----------------------------|
| Acceptance evidence | `cargo test --locked -p dropbox-dev sync::`; `cargo clippy --locked --all-targets -- -D warnings` | Passed | 2026-06-27 / `3379287` | Sync tests passed 50 tests before final acceptance, then 39/41/43/48/50 incremental gates, final gate passed 50 tests; clippy passed with `-D warnings`; final acceptance review returned clean | `3379287` |
| Additional task evidence | Staged `src/sync/mod.rs` review | Passed | 2026-06-27 / `3379287` | File-backed transport; enrollment/auth/pairing; authenticated encrypted envelopes; convergence with policy/Git metadata branches; LWW sidecars; deterministic replay; remote delete/tombstone; metadata/symlink actions; backup/restore; sync migration | `3379287` |

---

## CHUNK-06-LAZY-HYDRATION-VFS

- **Status:** `[x]` done — implementation verified, review accepted, and merged
- **Owner branch / worktree:** `chunk-06-lazy-hydration-vfs` / `../dropbox-dev-chunk-06-lazy-hydration-vfs` (merged; worktree removal pending)
- **Commit(s):** `f27cfa6` (`feat(vfs): add lazy hydration cache`)
- **Review status:** accepted — VFS and wave integration reviews clean after fixes
- **Blocker / deferred reason:** U6 resolved 2026-06-27: transparent stub-file/metadata-cache approach with frozen Hydrator/VfsMount seam for later FUSE/native adapters. No CHUNK-06 blocker remains.
- **Depends on:** CHUNK-05-SYNC-STORE-CONVERGENCE (catalog/policy/foundation are transitive through CHUNK-05 unless this chunk deliberately records a direct import)
- **Parallel with:** CHUNK-07-ENV-SYNC

### Tasks
- [x] Choose VFS approach (resolve U6) with rationale (PR + tracker)
- [x] Implement structure-first sync with placeholder metadata (no content fetched)
- [x] Implement on-demand content hydration on file access
- [x] Integration test: fresh machine shows full structure with zero content fetched
- [x] Integration test: reading placeholder triggers exactly one fetch + caches
- [x] Integration test: remote update invalidates cached content (coherency)
- [x] Hydration test: all policy outcomes (`ignore`, `rebuild-locally`, `platform-pin`) and Git metadata/submodule outcomes enforced through CHUNK-05's exposed contract; no direct CHUNK-03 import unless explicitly recorded
- [x] Document access latency + failure modes
- [x] Record verification evidence (see subsection below)

### Verification evidence

| Slot | Command/scenario | Result | Date / SHA | Output artifact / pasted summary | Tracker/evidence commit(s) |
|------|------------------|--------|------------|----------------------------------|----------------------------|
| Acceptance evidence | `cargo test --locked -p dropbox-dev vfs::`; `cargo clippy --locked --all-targets -- -D warnings` | Passed | 2026-06-27 / `f27cfa6` | VFS tests passed 8 tests; clippy passed; final wave acceptance returned clean | `f27cfa6` |
| Additional task evidence | Staged `src/vfs/mod.rs` review | Passed | 2026-06-27 / `f27cfa6` | Stub-file/metadata-cache VFS; zero-fetch structure materialization; one-fetch hydration cache; coherency invalidation; CHUNK-05 policy/Git enforcement; conflict winner hydration; platform redirected denial | `f27cfa6` |

---

## CHUNK-07-ENV-SYNC

- **Status:** `[x]` done — implementation verified, review accepted, and merged
- **Owner branch / worktree:** `chunk-07-env-sync` / `../dropbox-dev-chunk-07-env-sync` (merged; worktree removal pending)
- **Commit(s):** `7de9fb2` (`feat(env): add encrypted environment sync`)
- **Review status:** accepted — env and wave integration reviews clean after fixes
- **Blocker / deferred reason:** U7 resolved 2026-06-27: per-machine keyring abstraction with local master key material supplied out-of-band, target-only override secrets, key versions, and rotation. No CHUNK-07 blocker remains.
- **Depends on:** CHUNK-05-SYNC-STORE-CONVERGENCE
- **Parallel with:** CHUNK-06-LAZY-HYDRATION-VFS

### Tasks
- [x] Choose encryption key management (resolve U7) with rationale (PR + tracker)
- [x] Define env payload schema carried over CHUNK-05's generic authenticated/encrypted transport contract (no env-specific transport bypass)
- [x] Implement env var sync with per-machine override semantics
- [x] Implement env materialization usable by developer processes through the documented shell/session/daemon-launch mechanism
- [x] Implement encryption at rest + env-specific in-transit/no-plaintext checks over CHUNK-05 transport
- [x] Security test: secrets encrypted at rest; env plaintext never appears in transport captures/logs/artifacts/tracker evidence
- [x] Key-management test: provisioning/rotation; a machine without the key cannot decrypt
- [x] Unit/integration test: env var on machine A appears on B with overrides honored and materialized for a developer process
- [x] Audit test: every env sync op emits auditable record
- [x] Env sync conflicts reuse resolved U5 policy (last-writer-wins + conflict sidecar)
- [x] Write `env_*` initial migration on CHUNK-01's empty baseline; verify apply + rollback
- [x] Record verification evidence (see subsection below)

### Verification evidence

| Slot | Command/scenario | Result | Date / SHA | Output artifact / pasted summary | Tracker/evidence commit(s) |
|------|------------------|--------|------------|----------------------------------|----------------------------|
| Acceptance evidence | `cargo test --locked -p dropbox-dev env::`; `cargo clippy --locked --all-targets -- -D warnings` | Passed | 2026-06-27 / `7de9fb2` | Env tests passed 12 tests; clippy passed; final wave acceptance returned clean | `7de9fb2` |
| Additional task evidence | Staged `src/env/mod.rs` review | Passed | 2026-06-27 / `7de9fb2` | Env payload schema over CHUNK-05 transport; per-machine overrides; target-only override encryption; never-sync policy; audit timestamps; materialization; conflict sidecars; env migration | `7de9fb2` |

---

## CHUNK-08-CLI-DAEMON-UX

- **Status:** `[x]` done — implementation verified, review accepted, and merged
- **Owner branch / worktree:** `chunk-08-cli-daemon-ux` / `../dropbox-dev-chunk-08-cli-daemon-ux` (merged; worktree removal pending)
- **Commit(s):** `82cc44b` (`feat(cli): add daemon UX commands`)
- **Review status:** accepted — final CHUNK-08 review clean after fixes
- **Blocker / deferred reason:** U8 resolved 2026-06-27: portable foreground stdlib supervision model with deterministic cache-dir state/control files; OS-specific service managers may wrap the same binary later. No CHUNK-08 blocker remains.
- **Depends on:** CHUNK-04-WATCHER-INDEXER, CHUNK-05-SYNC-STORE-CONVERGENCE, CHUNK-06-LAZY-HYDRATION-VFS, CHUNK-07-ENV-SYNC (CHUNK-01/02/03 are transitive)
- **Parallel with:** — (integrates all upstream surfaces; owns `cli/`)

### Tasks
- [x] Implement CLI commands: `init`, `status`, `sync`, `sync pause`, `sync resume`, `catalog`, `policy`, `watch`, `hydrate`, `env`, `doctor`, `version`/`info`
- [x] Implement daemon lifecycle (start/stop/`sync pause`/`sync resume`, robust, surfaces errors)
- [x] Choose daemon supervision model per OS (resolve U8) with rationale (PR + tracker)
- [x] CLI test: all commands behave as documented, including `sync pause`/`sync resume` state transitions (user-facing CLI verification lives here)
- [x] Daemon test: `sync pause`/`sync resume` honored; sync errors surfaced; hydration/env state reported
- [x] Doctor test: `doctor` checks transport reachable (real transport) + config + permissions + cache + policy
- [x] UX test: stale-worktree scenario (README.md:8) observable + recoverable from CLI
- [x] Record verification evidence (see subsection below)

### Verification evidence

| Slot | Command/scenario | Result | Date / SHA | Output artifact / pasted summary | Tracker/evidence commit(s) |
|------|------------------|--------|------------|----------------------------------|----------------------------|
| Acceptance evidence | `cargo test --locked -p dropbox-dev cli::`; `cargo test --locked -p dropbox-dev`; `cargo clippy --locked --all-targets -- -D warnings` | Passed | 2026-06-27 / `82cc44b` | CLI tests passed 27; package tests passed 151; clippy passed; final CHUNK-08 review returned clean | `82cc44b` |
| Additional task evidence | Staged `src/cli/mod.rs` review | Passed | 2026-06-27 / `82cc44b` | CLI commands, foreground daemon supervision, pause/resume draining, transport-backed hydrate/env export, doctor nonzero failure, stale recovery/local delete/rename, conflict-sidecar safety, policy-preserving manifest publication | `82cc44b` |

---

## CHUNK-09-E2E-HARDENING

- **Status:** `[x]` done — implementation verified, review accepted, and merged
- **Owner branch / worktree:** `chunk-09-e2e-hardening` / `../dropbox-dev-chunk-09-e2e-hardening` (merged; worktree removal pending)
- **Commit(s):** `d8bba01` (`test(e2e): add hardening gates`)
- **Review status:** accepted — final CHUNK-09 review clean after runbook gate fixes
- **Blocker / deferred reason:** None. CHUNK-09 complete; U9 was already resolved by CHUNK-01 and consumed here.
- **Depends on:** CHUNK-08-CLI-DAEMON-UX (all previous chunks transitive through the integrated CLI/daemon surface)
- **Parallel with:** — (final hardening layer; tests/runbook/fuzz only — no production module internals)

### Tasks
- [x] Build multi-machine E2E simulation (Mac + Linux) over the production transport's local multi-daemon harness
- [x] E2E test: sync real project tree with `node_modules`, ignored files (`.syncignore`), env vars (with a secret), lazy-hydrated file — assert convergence, no unwanted sync of ignored content, no plaintext secrets at rest or in transit, correct hydration
- [x] Queued `sync pause`/`sync resume` scenario: while `sync pause` is active, changes queue without content transfer; `sync resume` drains them in order with observable status
- [x] Ignore scenario: `.syncignore`-ignored content not transmitted/hydrated/reported-missing across two machines; adjacent non-ignored content still syncs
- [x] In-transit scenario: env secret encrypted over the production transport; transport capture contains no plaintext
- [x] Git metadata/submodule scenario: repo root `.git/`, submodule `.git` pointer file, `.gitmodules`, and submodule path contents follow CHUNK-03 disposition; local-only Git metadata is not transmitted/hydrated, and permitted folder contents sync normally
- [x] Stale-worktree/missed-git-pull scenario: one machine starts from a stale project tree that missed upstream Git updates; daemon/CLI makes the stale state observable and recovers through the product sync path without destructive Git operations
- [x] Edge-case coverage: conflicts, renames, deletes, permissions, symlinks, binary/large files
- [x] Offline/reconnect test: divergence converges on reconnect
- [x] Failure-injection scenarios: daemon kill/restart, network partition/heal, offline hydration-source unavailability
- [x] Mac↔Linux divergence test: platform-specific handling correct
- [x] Platform matrix: Mac↔Linux and same-OS machine combinations exercise shared `Platform` identity and platform-specific policy outcomes
- [x] Rollout/rollback drill: bad sync and schema migration upgrade/downgrade rolled back to known-good state (exercising CHUNK-05 procedure + release runbook)
- [x] Soak test: sustained watcher activity converges without lost events/divergence
- [x] Declare CHUNK-09 performance budgets before measurement: threshold values for 10k-file structure/index/sync work and hydration latency, with rationale recorded in evidence
- [x] 10k-file/performance test: large tree indexing/sync and hydration latency are measured after budget declaration and compared against the recorded thresholds
- [x] Document rollout/rollback procedure (runbook) including schema migration rollback and operator evidence capture
- [x] Run bounded fuzz target commands (fixed seed + time/iteration cap) for both manifest/policy pure functions and public command/protocol parsing surface
- [x] For any CHUNK-09 failure that belongs upstream, pause CHUNK-09, create/record a scoped upstream fix task under the affected existing chunk ID with immediate dependency + owner, move `👉 NEXT` to that fix work, record fix evidence/review status, then resume CHUNK-09 from the failed scenario
- [x] Record verification evidence (see subsection below)

### Verification evidence

| Slot | Command/scenario | Result | Date / SHA | Output artifact / pasted summary | Tracker/evidence commit(s) |
|------|------------------|--------|------------|----------------------------------|----------------------------|
| Acceptance evidence | `cargo test --locked --test e2e`; `cargo test --locked -p dropbox-dev`; `cargo clippy --locked --all-targets -- -D warnings` | Passed | 2026-06-27 / `b7be129` | Final master verification: E2E tests passed 9; package tests passed 172; clippy passed; final Env/CLI and E2E rereviews clean; final core fixes re-reviewed through tombstone directory descendant safety | `b7be129` |
| Upstream fix/resume evidence (append rows as needed) | Final split review production fixes | Passed | 2026-06-27 / `b7be129` | Final review found scoped production fixes after CHUNK-09: `62fa204` sync partial/tombstone handling; `a321976` env CLI/audit skip handling; `3656479` platform/reconnect CLI/E2E coverage; `7c5950e`/`cd35639` equal-time tombstone snapshot/replay handling; `a376c4e` foreground DeleteLocal consumer; `6218f59` preserved skipped tombstone paths during unrelated manifest publish; `b7be129` preserved changed descendants under tombstoned directories | `62fa204`, `a321976`, `3656479`, `7c5950e`, `cd35639`, `a376c4e`, `6218f59`, `b7be129` |
| Queued `sync pause`/`sync resume` gate | `cargo test --locked --test e2e queued_pause_resume_keeps_content_off_transport_until_resume` | Passed | 2026-06-27 / `d8bba01` | Queued changes stay off transport while paused and drain on resume | `d8bba01` |
| Stale-worktree/missed-git-pull E2E gate | `cargo test --locked --test e2e git_metadata_stale_worktree_and_failure_injection_are_observable_and_recoverable` | Passed | 2026-06-27 / `d8bba01` | Stale worktree observable/recoverable through product sync path | `d8bba01` |
| Failure-injection gate (daemon kill, network partition, offline hydration source) | `cargo test --locked --test e2e daemon_process_interruption_restart_recovers_foreground_sync` plus failure-injection/reconnect E2E | Passed | 2026-06-27 / `3656479` | Daemon interruption/restart, divergent offline reconnect, network partition/heal, and offline hydration source covered | `d8bba01`, `3656479` |
| Platform matrix gate | `cargo test --locked --test e2e edge_cases_platform_matrix_and_soak_cover_cross_os_policy` plus production CLI platform override coverage | Passed | 2026-06-27 / `3656479` | Mac/Linux simulated production CLI identities and same-OS platform policy outcomes covered | `d8bba01`, `3656479` |
| Predeclared performance budget + 10k-file/hydration comparison gate | `cargo test --locked --test e2e performance_budget_10k_and_hydration_are_predeclared_and_enforced` | Passed | 2026-06-27 / `d8bba01` | 10k index/sync, 10k structure diff, and hydration budgets declared and enforced | `d8bba01` |
| Rollout/rollback runbook + schema migration gate | `docs/runbooks/chunk-09-rollout-rollback.md`; `cargo test --locked --test e2e rollout_rollback_runbook_schema_and_store_restore_drill_are_exercised` | Passed | 2026-06-27 / `d8bba01` | Runbook created; schema/store rollback drill exercised | `d8bba01` |
| Bounded fuzz targets gate (manifest/policy + public command/protocol parsing) | `cargo test --locked --test e2e bounded_fuzz_manifest_policy_public_cli_and_daemon_control_parsing` | Passed | 2026-06-27 / `d8bba01` | Bounded fuzz covers manifest/policy plus public CLI and daemon-control parsing | `d8bba01` |

---

## Summary table

| Chunk | Status | Depends on | Parallel with | Blocker | Review |
|-------|--------|------------|---------------|---------|--------|
| CHUNK-01-FOUNDATION | `[x]` | — | — | U1 resolved; U9 resolved; implementation verified; review accepted; merged `814afca` | accepted |
| CHUNK-02-CATALOG-STRUCTURE | `[x]` | 01 | 03 | U2 resolved; merged `542ee65` | accepted |
| CHUNK-03-IGNORE-PLATFORM-POLICY | `[x]` | 01 | 02 | U3 resolved; merged `8e73304` | accepted |
| CHUNK-04-WATCHER-INDEXER | `[x]` | 02, 03 | — | U4 resolved; merged `97351fc` | accepted |
| CHUNK-05-SYNC-STORE-CONVERGENCE | `[x]` | 04, 03 | — | U5 resolved; U10 resolved; merged `3379287` | accepted |
| CHUNK-06-LAZY-HYDRATION-VFS | `[x]` | 05 | 07 | U6 resolved; merged `f27cfa6` | accepted |
| CHUNK-07-ENV-SYNC | `[x]` | 05 | 06 | U7 resolved; merged `7de9fb2` | accepted |
| CHUNK-08-CLI-DAEMON-UX | `[x]` | 04, 05, 06, 07 | — | U8 resolved; merged `82cc44b` | accepted |
| CHUNK-09-E2E-HARDENING | `[x]` | 08 | — | U9 consumed; merged `d8bba01`; final split fixes through `b7be129` | accepted |

**Current resume state:** All chunks CHUNK-01 through CHUNK-09 are complete, verified, reviewed, merged, and final split-review fixes are recorded through `b7be129`. No unchecked doable checklist tasks remain.
