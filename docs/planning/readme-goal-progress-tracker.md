# README-Goal Progress Tracker — Dropbox for Developers that Theo wants

> Durable progress tracker for the README.md goal. Companion to `docs/planning/readme-goal-implementation-plan.md`. Every task is **unchecked initially** — no implementation has started (resume mode: `pre_implementation`). This file is the **sole mutable source of truth** for per-task/per-chunk status, verification evidence, commit rows, `👉 NEXT` placement, and resume state.

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

At planning time, no chunk has started. The only dependency-ready unchecked chunk is `CHUNK-01-FOUNDATION`, so the only current `👉 NEXT` marker is on its first unchecked doable task: `Choose stack/language (resolve U1) and record rationale`. Resolving U1 is the first executable action, not an external blocker preventing work from starting — it is the first task of CHUNK-01.

> **👉 NEXT task: `Choose stack/language (resolve U1) and record rationale`** (CHUNK-01-FOUNDATION, first task below).

Dependency order: `01 → {02, 03} → 04 → 05 → {06, 07} → 08 → 09`. CHUNK-04 and CHUNK-05 are serialized (04 before 05); there is no 04 ∥ 05 parallel wave. When the `{02, 03}` or `{06, 07}` waves become dependency-ready, this section and the task lists must show multiple simultaneous `👉 NEXT` markers.

## Global status legend

- `[ ]` — not started
- `[~]` — in progress
- `[x]` — done (evidence recorded + review accepted)
- `[!]` — blocked (see Blocker/deferred reason)

---

## CHUNK-01-FOUNDATION

- **Status:** `[ ]` not started
- **Owner branch / worktree:** —
- **Commit(s):** —
- **Review status:** pending
- **Blocker / deferred reason:** U1 (stack/language choice) — **blocker**; U9 (FS2 build-vs-adopt-vs-vendor) — **blocker** and not already resolved. Both must be resolved by CHUNK-01 before review can be accepted; CHUNK-09 consumes U9 only after this tracker records the CHUNK-01 decision. Resolving U1 is the first task (not an external blocker).
- **Depends on:** — (root)

### Tasks
- [ ] 👉 NEXT — Choose stack/language (resolve U1) and record rationale (in the implementation plan via the planning-layer exception + this tracker)
- [ ] Record the FS2 decision (resolve U9: build-from-scratch vs adopt-and-extend vs vendor) with rationale and date (in the implementation plan via the planning-layer exception + this tracker); must land before CHUNK-02/CHUNK-06 design begin and before CHUNK-09 treats U9 as consumed
- [ ] Create project manifest + module skeleton
- [ ] Establish build command (`make build` or equivalent) — passes from clean clone
- [ ] Establish test harness (`make test`) — runs a passing no-op test
- [ ] Establish lint/typecheck (`make lint`) — passes
- [ ] Add config loading + structured logging + error model
- [ ] Define shared `Platform`/OS identity type (canonical OS, architecture, machine-id provenance) and export it for CHUNK-02/CHUNK-03 consumers
- [ ] Implement migration framework (schema version table + runner; empty `v0` baseline, no product tables)
- [ ] Add tool `.gitignore` entries for generated build/test/cache artifacts
- [ ] Add minimal CLI `version`/`info` smoke shim; `<cli> info` prints machine id, version, and config path; `--help` is optional only
- [ ] Record verification evidence (see subsection below)

### Verification evidence

| Slot | Command/scenario | Result | Date / SHA | Output artifact / pasted summary | Tracker/evidence commit(s) |
|------|------------------|--------|------------|----------------------------------|----------------------------|
| Acceptance evidence | _ | _ | _ | _ | _ |
| Additional task evidence (append rows as needed) | _ | _ | _ | _ | _ |

---

## CHUNK-02-CATALOG-STRUCTURE

- **Status:** `[ ]` not started
- **Owner branch / worktree:** —
- **Commit(s):** —
- **Review status:** pending
- **Blocker / deferred reason:** U2 (catalog serialization format) — deferred to chunk owner with rationale (checklist item + review criterion).
- **Depends on:** CHUNK-01-FOUNDATION
- **Parallel with:** CHUNK-03-IGNORE-PLATFORM-POLICY

### Tasks
- [ ] Define project + directory structure model (transport-independent) using CHUNK-01's shared `Platform`/OS identity type for machine/platform fields
- [ ] Implement cross-machine structure reconciliation (deterministic)
- [ ] Property test: structure sync never fetches file contents
- [ ] Unit test: two differing structures converge to one canonical record
- [ ] Unit test: add/remove directory updates catalog idempotently
- [ ] Resolve U2 (serialization format) with recorded rationale (PR + tracker)
- [ ] Write `catalog_*` initial migration on CHUNK-01's empty baseline; verify apply + rollback
- [ ] Record verification evidence (see subsection below)

### Verification evidence

| Slot | Command/scenario | Result | Date / SHA | Output artifact / pasted summary | Tracker/evidence commit(s) |
|------|------------------|--------|------------|----------------------------------|----------------------------|
| Acceptance evidence | _ | _ | _ | _ | _ |
| Additional task evidence (append rows as needed) | _ | _ | _ | _ | _ |

---

## CHUNK-03-IGNORE-PLATFORM-POLICY

- **Status:** `[ ]` not started
- **Owner branch / worktree:** —
- **Commit(s):** —
- **Review status:** pending
- **Blocker / deferred reason:** U3 (ignore file format — bespoke vs `.gitignore`-compatible superset) — deferred to chunk owner with rationale (checklist item + review criterion).
- **Depends on:** CHUNK-01-FOUNDATION
- **Parallel with:** CHUNK-02-CATALOG-STRUCTURE

### Tasks
- [ ] Define ignore/exclusion mechanism distinct from `.gitignore` (unless explicitly chosen otherwise)
- [ ] Resolve U3 (ignore format) with recorded rationale (PR + tracker)
- [ ] Define platform-specific policy (`node_modules`, generated/dependency dirs, OS artifacts) for Mac + Linux using CHUNK-01's shared `Platform`/OS identity type
- [ ] Record git/submodule metadata disposition: `.git/` directories and submodule `.git` pointer files are local-only metadata, `.gitmodules` behavior is explicit, and whole-folder sync replaces submodule workflows
- [ ] Unit test: `node_modules/`, generated dir, and ignore-patterned file are excluded from sync
- [ ] Unit test: platform-specific path handled per policy (not byte-synced)
- [ ] Unit test: all three `Action` variants (`ignore`, `rebuild-locally`, `platform-pin`)
- [ ] Unit test: ignore rules add/remove without touching `.gitignore`
- [ ] Unit test: Git metadata policy covers `.git/`, submodule `.git` file pointers, and `.gitmodules` without touching `.gitignore`
- [ ] Record verification evidence (see subsection below)

### Verification evidence

| Slot | Command/scenario | Result | Date / SHA | Output artifact / pasted summary | Tracker/evidence commit(s) |
|------|------------------|--------|------------|----------------------------------|----------------------------|
| Acceptance evidence | _ | _ | _ | _ | _ |
| Additional task evidence (append rows as needed) | _ | _ | _ | _ | _ |

---

## CHUNK-04-WATCHER-INDEXER

- **Status:** `[ ]` not started
- **Owner branch / worktree:** —
- **Commit(s):** —
- **Review status:** pending
- **Blocker / deferred reason:** U4 (watcher library per OS) — deferred to chunk owner; must work on macOS + Linux (checklist item + review criterion).
- **Depends on:** CHUNK-02-CATALOG-STRUCTURE, CHUNK-03-IGNORE-PLATFORM-POLICY (CHUNK-01 is transitive through both)
- **Parallel with:** — (consumes 02 + 03; produces the interface 05 consumes)

### Tasks
- [ ] Select filesystem watcher library per OS (resolve U4) with rationale (PR + tracker)
- [ ] Implement local indexer consistent with catalog (02) and policy (03)
- [ ] Integration test: create/edit/move/delete produces correct, deduped events
- [ ] Integration test: ignored, Git metadata, and platform-specific paths produce no unsafe content-sync events, while snapshot/event queue preserves policy action metadata/rebuild hints/platform-pin decisions for CHUNK-05 (all `Action` variants)
- [ ] Race test: rapid bulk changes converge to stable index
- [ ] Handle renames, deletes, permissions, symlinks
- [ ] Freeze snapshot/event-queue interface for CHUNK-05
- [ ] Write `watcher_events` initial migration on CHUNK-01's empty baseline; verify apply + rollback
- [ ] Record verification evidence (see subsection below)

### Verification evidence

| Slot | Command/scenario | Result | Date / SHA | Output artifact / pasted summary | Tracker/evidence commit(s) |
|------|------------------|--------|------------|----------------------------------|----------------------------|
| Acceptance evidence | _ | _ | _ | _ | _ |
| Additional task evidence (append rows as needed) | _ | _ | _ | _ | _ |

---

## CHUNK-05-SYNC-STORE-CONVERGENCE

- **Status:** `[ ]` not started
- **Owner branch / worktree:** —
- **Commit(s):** —
- **Review status:** pending
- **Blocker / deferred reason:** U5 (conflict resolution strategy) — **resolved**: last-writer-wins + conflict sidecar + manual escape hatch. U10 (production transport/backend choice) — **blocker**; must be chosen before review. Local loopback is only a test harness, not the production answer.
- **Depends on:** CHUNK-04-WATCHER-INDEXER, CHUNK-03-IGNORE-PLATFORM-POLICY (direct policy action contract; CHUNK-02/CHUNK-01 are transitive)
- **Parallel with:** — (serializes after 04; must complete before {06, 07})

### Tasks
- [ ] Choose production sync transport/backend (resolve U10) with rationale (real cross-machine/backing path; loopback harness only)
- [ ] Implement machine enrollment/auth/pairing
- [ ] Implement authoritative sync store + operation log
- [ ] Implement production cross-machine/backing transport path (mock only for deterministic tests; loopback only as local harness for the production contract)
- [ ] Implement generic authenticated/encrypted payload + content transport contract
- [ ] Implement convergence protocol (partial/offline, reconnect) branching on all `Action` variants from CHUNK-03 policy and the recorded Git metadata/submodule disposition
- [ ] Implement conflict resolution per resolved U5 (last-writer-wins + conflict sidecar + manual escape hatch)
- [ ] Integration test: two machines editing same file converge with conflict sidecar (no silent data loss)
- [ ] Integration test: offline machine reconnects and converges
- [ ] Property test: operation log replay is deterministic
- [ ] Transport test: two daemon instances exchange generic manifest metadata + content payloads over the production transport path via the local harness (not env-specific blobs; not mock-only)
- [ ] In-transit security test: plaintext generic payload/content does not cross the transport
- [ ] All-policy-action + Git-metadata test: CHUNK-03 `ignore`, `rebuild-locally`, `platform-pin`, `.git/`, submodule `.git` files, and `.gitmodules` disposition branched in convergence
- [ ] Write `sync_*` initial migration on CHUNK-01's empty baseline; verify apply + rollback
- [ ] Implement + document sync-store rollback/backup/restore procedure; verify with a test
- [ ] Record verification evidence (see subsection below)

### Verification evidence

| Slot | Command/scenario | Result | Date / SHA | Output artifact / pasted summary | Tracker/evidence commit(s) |
|------|------------------|--------|------------|----------------------------------|----------------------------|
| Acceptance evidence | _ | _ | _ | _ | _ |
| Additional task evidence (append rows as needed) | _ | _ | _ | _ | _ |

---

## CHUNK-06-LAZY-HYDRATION-VFS

- **Status:** `[ ]` not started
- **Owner branch / worktree:** —
- **Commit(s):** —
- **Review status:** pending
- **Blocker / deferred reason:** U6 (VFS approach — FUSE/platform native vs editor plugin vs stub files) — **blocker**; verification approach depends on it (checklist item + review criterion).
- **Depends on:** CHUNK-05-SYNC-STORE-CONVERGENCE (catalog/policy/foundation are transitive through CHUNK-05 unless this chunk deliberately records a direct import)
- **Parallel with:** CHUNK-07-ENV-SYNC

### Tasks
- [ ] Choose VFS approach (resolve U6) with rationale (PR + tracker)
- [ ] Implement structure-first sync with placeholder metadata (no content fetched)
- [ ] Implement on-demand content hydration on file access
- [ ] Integration test: fresh machine shows full structure with zero content fetched
- [ ] Integration test: reading placeholder triggers exactly one fetch + caches
- [ ] Integration test: remote update invalidates cached content (coherency)
- [ ] Hydration test: all policy outcomes (`ignore`, `rebuild-locally`, `platform-pin`) and Git metadata/submodule outcomes enforced through CHUNK-05's exposed contract; no direct CHUNK-03 import unless explicitly recorded
- [ ] Document access latency + failure modes
- [ ] Record verification evidence (see subsection below)

### Verification evidence

| Slot | Command/scenario | Result | Date / SHA | Output artifact / pasted summary | Tracker/evidence commit(s) |
|------|------------------|--------|------------|----------------------------------|----------------------------|
| Acceptance evidence | _ | _ | _ | _ | _ |
| Additional task evidence (append rows as needed) | _ | _ | _ | _ | _ |

---

## CHUNK-07-ENV-SYNC

- **Status:** `[ ]` not started
- **Owner branch / worktree:** —
- **Commit(s):** —
- **Review status:** pending
- **Blocker / deferred reason:** U7 (encryption key management) — **blocker**; cannot ship env sync without it (checklist item + review criterion).
- **Depends on:** CHUNK-05-SYNC-STORE-CONVERGENCE
- **Parallel with:** CHUNK-06-LAZY-HYDRATION-VFS

### Tasks
- [ ] Choose encryption key management (resolve U7) with rationale (PR + tracker)
- [ ] Define env payload schema carried over CHUNK-05's generic authenticated/encrypted transport contract (no env-specific transport bypass)
- [ ] Implement env var sync with per-machine override semantics
- [ ] Implement env materialization usable by developer processes through the documented shell/session/daemon-launch mechanism
- [ ] Implement encryption at rest + env-specific in-transit/no-plaintext checks over CHUNK-05 transport
- [ ] Security test: secrets encrypted at rest; env plaintext never appears in transport captures/logs/artifacts/tracker evidence
- [ ] Key-management test: provisioning/rotation; a machine without the key cannot decrypt
- [ ] Unit/integration test: env var on machine A appears on B with overrides honored and materialized for a developer process
- [ ] Audit test: every env sync op emits auditable record
- [ ] Env sync conflicts reuse resolved U5 policy (last-writer-wins + conflict sidecar)
- [ ] Write `env_*` initial migration on CHUNK-01's empty baseline; verify apply + rollback
- [ ] Record verification evidence (see subsection below)

### Verification evidence

| Slot | Command/scenario | Result | Date / SHA | Output artifact / pasted summary | Tracker/evidence commit(s) |
|------|------------------|--------|------------|----------------------------------|----------------------------|
| Acceptance evidence | _ | _ | _ | _ | _ |
| Additional task evidence (append rows as needed) | _ | _ | _ | _ | _ |

---

## CHUNK-08-CLI-DAEMON-UX

- **Status:** `[ ]` not started
- **Owner branch / worktree:** —
- **Commit(s):** —
- **Review status:** pending
- **Blocker / deferred reason:** U8 (daemon supervision model per OS) — deferred to chunk owner (checklist item + review criterion).
- **Depends on:** CHUNK-04-WATCHER-INDEXER, CHUNK-05-SYNC-STORE-CONVERGENCE, CHUNK-06-LAZY-HYDRATION-VFS, CHUNK-07-ENV-SYNC (CHUNK-01/02/03 are transitive)
- **Parallel with:** — (integrates all upstream surfaces; owns `cli/`)

### Tasks
- [ ] Implement CLI commands: `init`, `status`, `sync`, `sync pause`, `sync resume`, `catalog`, `policy`, `watch`, `hydrate`, `env`, `doctor`, `version`/`info`
- [ ] Implement daemon lifecycle (start/stop/`sync pause`/`sync resume`, robust, surfaces errors)
- [ ] Choose daemon supervision model per OS (resolve U8) with rationale (PR + tracker)
- [ ] CLI test: all commands behave as documented, including `sync pause`/`sync resume` state transitions (user-facing CLI verification lives here)
- [ ] Daemon test: `sync pause`/`sync resume` honored; sync errors surfaced; hydration/env state reported
- [ ] Doctor test: `doctor` checks transport reachable (real transport) + config + permissions + cache + policy
- [ ] UX test: stale-worktree scenario (README.md:8) observable + recoverable from CLI
- [ ] Record verification evidence (see subsection below)

### Verification evidence

| Slot | Command/scenario | Result | Date / SHA | Output artifact / pasted summary | Tracker/evidence commit(s) |
|------|------------------|--------|------------|----------------------------------|----------------------------|
| Acceptance evidence | _ | _ | _ | _ | _ |
| Additional task evidence (append rows as needed) | _ | _ | _ | _ | _ |

---

## CHUNK-09-E2E-HARDENING

- **Status:** `[ ]` not started
- **Owner branch / worktree:** —
- **Commit(s):** —
- **Review status:** pending
- **Blocker / deferred reason:** None for CHUNK-09 itself (consumes prior decisions, including resolved U5 and CHUNK-01's recorded U9 decision only after CHUNK-01 has resolved and recorded U9).
- **Depends on:** CHUNK-08-CLI-DAEMON-UX (all previous chunks transitive through the integrated CLI/daemon surface)
- **Parallel with:** — (final hardening layer; tests/runbook/fuzz only — no production module internals)

### Tasks
- [ ] Build multi-machine E2E simulation (Mac + Linux) over the production transport's local multi-daemon harness
- [ ] E2E test: sync real project tree with `node_modules`, ignored files (`.syncignore`), env vars (with a secret), lazy-hydrated file — assert convergence, no unwanted sync of ignored content, no plaintext secrets at rest or in transit, correct hydration
- [ ] Queued `sync pause`/`sync resume` scenario: while `sync pause` is active, changes queue without content transfer; `sync resume` drains them in order with observable status
- [ ] Ignore scenario: `.syncignore`-ignored content not transmitted/hydrated/reported-missing across two machines; adjacent non-ignored content still syncs
- [ ] In-transit scenario: env secret encrypted over the production transport; transport capture contains no plaintext
- [ ] Git metadata/submodule scenario: repo root `.git/`, submodule `.git` pointer file, `.gitmodules`, and submodule path contents follow CHUNK-03 disposition; local-only Git metadata is not transmitted/hydrated, and permitted folder contents sync normally
- [ ] Stale-worktree/missed-git-pull scenario: one machine starts from a stale project tree that missed upstream Git updates; daemon/CLI makes the stale state observable and recovers through the product sync path without destructive Git operations
- [ ] Edge-case coverage: conflicts, renames, deletes, permissions, symlinks, binary/large files
- [ ] Offline/reconnect test: divergence converges on reconnect
- [ ] Failure-injection scenarios: daemon kill/restart, network partition/heal, offline hydration-source unavailability
- [ ] Mac↔Linux divergence test: platform-specific handling correct
- [ ] Platform matrix: Mac↔Linux and same-OS machine combinations exercise shared `Platform` identity and platform-specific policy outcomes
- [ ] Rollout/rollback drill: bad sync and schema migration upgrade/downgrade rolled back to known-good state (exercising CHUNK-05 procedure + release runbook)
- [ ] Soak test: sustained watcher activity converges without lost events/divergence
- [ ] Declare CHUNK-09 performance budgets before measurement: threshold values for 10k-file structure/index/sync work and hydration latency, with rationale recorded in evidence
- [ ] 10k-file/performance test: large tree indexing/sync and hydration latency are measured after budget declaration and compared against the recorded thresholds
- [ ] Document rollout/rollback procedure (runbook) including schema migration rollback and operator evidence capture
- [ ] Run bounded fuzz target commands (fixed seed + time/iteration cap) for both manifest/policy pure functions and public command/protocol parsing surface
- [ ] For any CHUNK-09 failure that belongs upstream, pause CHUNK-09, create/record a scoped upstream fix task under the affected existing chunk ID with immediate dependency + owner, move `👉 NEXT` to that fix work, record fix evidence/review status, then resume CHUNK-09 from the failed scenario
- [ ] Record verification evidence (see subsection below)

### Verification evidence

| Slot | Command/scenario | Result | Date / SHA | Output artifact / pasted summary | Tracker/evidence commit(s) |
|------|------------------|--------|------------|----------------------------------|----------------------------|
| Acceptance evidence | _ | _ | _ | _ | _ |
| Upstream fix/resume evidence (append rows as needed) | _ | _ | _ | _ | _ |
| Queued `sync pause`/`sync resume` gate | _ | _ | _ | _ | _ |
| Stale-worktree/missed-git-pull E2E gate | _ | _ | _ | _ | _ |
| Failure-injection gate (daemon kill, network partition, offline hydration source) | _ | _ | _ | _ | _ |
| Platform matrix gate | _ | _ | _ | _ | _ |
| Predeclared performance budget + 10k-file/hydration comparison gate | _ | _ | _ | _ | _ |
| Rollout/rollback runbook + schema migration gate | _ | _ | _ | _ | _ |
| Bounded fuzz targets gate (manifest/policy + public command/protocol parsing) | _ | _ | _ | _ | _ |

---

## Summary table

| Chunk | Status | Depends on | Parallel with | Blocker | Review |
|-------|--------|------------|---------------|---------|--------|
| CHUNK-01-FOUNDATION | `[ ]` | — | — | U1 (blocker), U9 (blocker) | pending |
| CHUNK-02-CATALOG-STRUCTURE | `[ ]` | 01 | 03 | U2 (deferred) | pending |
| CHUNK-03-IGNORE-PLATFORM-POLICY | `[ ]` | 01 | 02 | U3 (deferred) | pending |
| CHUNK-04-WATCHER-INDEXER | `[ ]` | 02, 03 | — | U4 (deferred) | pending |
| CHUNK-05-SYNC-STORE-CONVERGENCE | `[ ]` | 04, 03 | — | U10 (blocker) | pending |
| CHUNK-06-LAZY-HYDRATION-VFS | `[ ]` | 05 | 07 | U6 (blocker) | pending |
| CHUNK-07-ENV-SYNC | `[ ]` | 05 | 06 | U7 (blocker) | pending |
| CHUNK-08-CLI-DAEMON-UX | `[ ]` | 04, 05, 06, 07 | — | U8 (deferred) | pending |
| CHUNK-09-E2E-HARDENING | `[ ]` | 08 | — | none (U9 consumed after CHUNK-01 resolves it) | pending |

**Next dependency-ready unchecked chunk(s):** `CHUNK-01-FOUNDATION` 👉 NEXT (first task: `Choose stack/language (resolve U1) and record rationale`). Future parallel waves must list multiple `👉 NEXT` tasks when both chunks are dependency-ready.
