# README-Goal Implementation Plan — Dropbox for Developers that Theo wants

> Durable planning artifact. Materializes the README.md product goal into a chunked, dependency-ordered, verifiable implementation plan. No product code is implemented by this artifact; this is planning only.
>
> **Companion durable artifacts** (required by `local://planning-contract.json`):
> - Chunk checklist: `docs/planning/readme-goal-chunk-checklist.md` — per-chunk definition of done (criteria only; not the mutable status ledger).
> - Parallel worktree plan: `docs/planning/readme-goal-parallel-worktrees.md` — parallel-safety and ownership contract.
> - Progress tracker: `docs/planning/readme-goal-progress-tracker.md` — **sole mutable source of truth for per-task/per-chunk status, `👉 NEXT` markers, commit/evidence rows, and resume state**.
>
> These artifacts are authoritative by role: this plan defines stable scope/dependencies/review criteria, the checklist defines done, the progress tracker is the sole mutable status/evidence ledger, and the worktree plan governs ownership. A chunk is reviewable only when its checklist items are satisfied, this plan's review criteria are met, tracker evidence is recorded, and the worktree ownership rules are satisfied.

## Resume Status

- **Mode:** `pre_implementation`
- **Repo state at planning time:** README-only seed project. The only repository file is `README.md` (project concept + high-level requirements, lines 1–24). No package manifests, source directories, config files, or tests exist.
- **Git evidence:** Single visible commit `0c6ed3a` (`docs(readme): add project overview and sync rationale`) on `master`/`origin/master`; `git show --stat` confirms it only added `README.md`. `git status --short --untracked-files=all` returned no output.
- **Implementation started:** No. No source architecture, persistence layer, transport, auth, conflict resolution, filesystem integration, or metadata model exists. Planning starts from a greenfield baseline.
- **Authoritative reference:** `README.md` (per `local://orchestration-readme-goal.md:2` — "Use README.md as the goal/reference for durable orchestration").

## README Requirements (source of truth)

Sourced from `README.md` and mirrored in `local://orchestration-readme-goal.md`.

| Ref | Requirement |
|-----|-------------|
| README.md:8–13 | Pain points: stale worktrees from missed `git pull`; env vars set on one machine but not another; inconsistent project directory structures across machines; git submodule friction. |
| README.md:15 | Desired behavior: Dropbox-like automatic consistency across machines (Mac Mini ×2, GMK Tech Box Linux). |
| README.md:19 | Automatic syncing of code folders, like Dropbox. |
| README.md:20 | Environment variable synchronization. |
| README.md:21 | On-demand downloading: sync structure first; fetch file contents only when a specific file is accessed (placeholder metadata / lazy hydration / virtual filesystem). |
| README.md:22 | `node_modules` and other platform-specific artifacts need special handling (not naive byte-sync). |
| README.md:23 | An ignore/exclusion mechanism analogous to Dropbox/Google Drive ignore behavior. |
| README.md:24 | FS2 (File System 2) exists but is insufficient; no FS2 code or references are present locally beyond this mention. |

## Assumptions & Rationale

- **A1 — Repo root:** The inspected root `~/gitfiles/EveGoodEvening/Dropbox-for-Developers-that-Theo-wants` is the authoritative project root. *(Rationale: harness-provided context.)*
- **A2 — README is authoritative:** `README.md` is the current product reference because the orchestration goal explicitly says so. *(Rationale: `local://orchestration-readme-goal.md:2`.)*
- **A3 — No hidden implementation:** Absence of manifests/source dirs means no implementation exists yet, not that code is hidden in non-root locations. *(Rationale: repository listing + targeted manifest/source glob checks found nothing.)*
- **A4 — External links not fetched:** Theo's X/YouTube links (`README.md:3`) were not retrieved; the README already states the local goal. *(Rationale: assignment scoped to local repo state.)*
- **A5 — Stack is open:** No stack is established. `node_modules` in the README is a product concern, not proof this project is Node/TypeScript. *(Rationale: no `package.json`/`Cargo.toml`/`go.mod`/`pyproject.toml` found.)* [INFERENCE]
- **A6 — Greenfield planning:** Planning must start from a greenfield baseline rather than adapting an existing codebase. *(Rationale: no existing architecture.)*

## Architecture Direction (planning level only)

This section defines the shape the implementation must take so that each chunk is verifiable. It is **not** a design spec; concrete APIs, data formats, and transport choices are deferred to chunk owners unless pinned below by an unresolved decision.

### Logical layers (top → bottom)

1. **CLI / Daemon / UX** — user-facing entrypoint, daemon lifecycle, status/observability. (CHUNK-08)
2. **Sync Store & Convergence** — authoritative remote state, real cross-machine transport/backend, machine enrollment/auth/pairing, conflict resolution, convergence protocol. (CHUNK-05)
3. **Lazy Hydration / VFS** — structure-first sync; on-demand content fetch; placeholder metadata; virtual filesystem semantics. (CHUNK-06)
4. **Env Sync** — environment variable synchronization with secret safety (at rest and in transit). (CHUNK-07)
5. **Watcher / Indexer** — filesystem watching, event dedup, local index of synced tree, snapshot/event-queue producer consumed by the sync store. (CHUNK-04)
6. **Catalog / Structure** — project + directory structure model; cross-machine structure reconciliation. (CHUNK-02)
7. **Ignore / Platform Policy** — ignore/exclusion mechanism; platform-specific handling rules (`node_modules`, generated dirs). (CHUNK-03)
8. **Foundation** — repo scaffolding, stack choice, build/test harness, migration framework, config, logging, error model, FS2 decision. (CHUNK-01)
9. **E2E Hardening** — multi-machine simulation over the real transport, edge cases, rollback drills, hardening. (CHUNK-09)

### Cross-cutting concerns (owned across chunks)

- Conflict resolution, partial/offline state, file-watching races, renames, deletes, permissions, symlinks, binary files, large files, divergent OS semantics. (CHUNK-05 leads; CHUNK-04, CHUNK-06, CHUNK-09 exercise.)
- Secret handling, encryption (at rest and in transit), per-machine overrides, auditability, leakage prevention for env sync. (CHUNK-07 leads; CHUNK-05 provides in-transit path; CHUNK-09 exercises.)
- Cache coherency and failure modes for lazy hydration. (CHUNK-06 leads; CHUNK-09 exercises.)
- Cross-machine transport, backing sync path, machine enrollment/auth/pairing, in-transit security. (CHUNK-05 owns the production transport/backend; local loopback is a test harness for that path, not the production design; CHUNK-08 wires doctor checks; CHUNK-09 exercises end-to-end.)
- Shared `Platform`/OS identity (canonical OS family, architecture, machine-id provenance) is a CHUNK-01 foundation contract consumed by CHUNK-02 catalog records and CHUNK-03 policy decisions; downstream chunks must not define competing platform enums.
- Git metadata and submodule safety are policy-owned, not ad hoc downstream behavior: CHUNK-03 defines the disposition for repository `.git/` directories, submodule `.git` pointer files, and `.gitmodules`; CHUNK-04 carries that policy through snapshots/events, CHUNK-05 enforces it at convergence, CHUNK-06 enforces it at hydration, and CHUNK-09 exercises it end-to-end.

## Chunks

Chunk IDs are exactly those from `local://planning-contract.json`. Each chunk lists dependencies, parallelism, verification, review criteria, rollback/risk, and unresolved decisions.

---

### CHUNK-01-FOUNDATION

- **Scope:** Repo scaffolding: stack/language decision, project manifest, build + test harness, **migration framework** (schema version table + migration runner; no product tables — those are owned by the schema-owning chunks), config loading, structured logging, error model, CI skeleton, shared `Platform`/OS identity type, tool-generated-artifact `.gitignore` entries, and a minimal `version`/`info` CLI smoke shim. **Records the FS2 build-vs-adopt-vs-vendor decision (U9).** Establishes the verifiable substrate every later chunk depends on.
- **Depends on:** — (root).
- **Parallel with:** Nothing (all other chunks depend on this).
- **Verification:**
  - `make build` (or language-equivalent) succeeds from clean clone.
  - `make test` runs the harness with a passing no-op test.
  - `make lint` / typecheck succeeds.
  - `<cli> version` (or stack wrapper such as `make run -- version`) prints the manifest/application version.
  - `<cli> info` (or stack wrapper such as `make run -- info`) prints machine id, version, and config path in a machine-readable form.
  - `<cli> --help` may exist as optional help smoke, but it is not the required CHUNK-01 acceptance path.
  - Migration runner loads an empty baseline schema and reports version `v0` with no product tables.
- **Review criteria:** Manifest + build/test/lint commands documented in repo; logging and error model present and used by the `version`/`info` smoke path; shared `Platform`/OS identity type documented/exported for CHUNK-02/CHUNK-03; tool `.gitignore` entries cover generated build/test/cache artifacts; migration framework present with a version table and runner; **FS2 decision (U9) recorded with rationale and date**; no dead scaffolding.
- **Rollback / risk:** Lowest risk. Rollback = revert scaffolding commit. Risk: stack choice is load-bearing and hard to reverse (see unresolved U1); FS2 decision shapes catalog/VFS design (U9).
- **Unresolved decisions:** U1 (stack choice) — **blocker** until chosen, because every downstream chunk's verification commands depend on it. U9 (FS2 build-vs-adopt-vs-vendor) — **blocker** until chosen by CHUNK-01; downstream chunks, including CHUNK-09, must not treat U9 as consumed until CHUNK-01 records the decision because catalog and VFS design (CHUNK-02/CHUNK-06) depend on whether FS2 covers the catalog/hydration surface.

---

### CHUNK-02-CATALOG-STRUCTURE

- **Scope:** Project + directory structure model: how a "project" and its tree are represented; cross-machine structure reconciliation (structure sync without content). Produces the catalog that structure-first sync (CHUNK-06) and the watcher/indexer (CHUNK-04) consume. Consumes CHUNK-01's shared `Platform`/OS identity type for machine/platform fields rather than defining its own. **Owns the `catalog_*` initial migration** (first schema for catalog persistence) on top of CHUNK-01's migration framework.
- **Depends on:** CHUNK-01-FOUNDATION.
- **Parallel with:** CHUNK-03-IGNORE-PLATFORM-POLICY (independent data models after CHUNK-01 establishes the shared `Platform`/OS identity contract).
- **Verification:**
  - Unit test: two machines with differing structures converge to a single canonical structure record.
  - Unit test: adding/removing a directory updates the catalog idempotently.
  - Property test: structure sync never fetches file contents.
  - Migration test: `catalog_*` schema applies cleanly from the empty baseline and rolls back to the baseline.
- **Review criteria:** Structure model is transport-independent; reconciliation is deterministic; no content bytes are transferred during structure sync; **catalog serialization format (U2) chosen with recorded rationale**; `catalog_*` migration owned and revertable.
- **Rollback / risk:** Revert catalog module + `catalog_*` migration. Risk: model must later support renames/moves without treating them as delete+create (deferred to CHUNK-05/CHUNK-09 exercise).
- **Unresolved decisions:** U2 (catalog serialization format) — **deferred** to chunk owner; must be reproducible across machines; rationale recorded in the chunk PR and progress tracker.

---

### CHUNK-03-IGNORE-PLATFORM-POLICY

- **Scope:** Ignore/exclusion mechanism (Dropbox/Google-Drive-style, not merely `.gitignore` reuse unless deliberately chosen), platform-specific handling policy (`node_modules`, generated/dependency dirs, OS-specific artifacts), and Git metadata/submodule policy (`.git/` directories, submodule `.git` pointer files, `.gitmodules`). Defines the rules the watcher/indexer and sync store consult, consuming CHUNK-01's shared `Platform`/OS identity type for OS/platform matching rather than defining its own.
- **Depends on:** CHUNK-01-FOUNDATION.
- **Parallel with:** CHUNK-02-CATALOG-STRUCTURE (independent data models after CHUNK-01 establishes the shared `Platform`/OS identity contract).
- **Verification:**
  - Unit test: `node_modules/`, a configured generated dir, and an ignore-patterned file are excluded from sync.
  - Unit test: platform-specific path is handled per policy (e.g., skipped, stubbed, or regenerated) rather than byte-synced.
  - Unit test: ignore rules can be added/removed without touching `.gitignore`.
  - Unit test: Git metadata policy handles repository `.git/` directories, submodule `.git` pointer files, and `.gitmodules` per the recorded CHUNK-03 disposition; local-only Git metadata never becomes sync content.
- **Review criteria:** Ignore semantics documented and distinct from `.gitignore` unless an explicit decision records otherwise; **ignore file format (U3) chosen with recorded rationale**; platform policy covers Mac + Linux (README.md:8 machines); Git metadata/submodule disposition recorded for `.git/`, submodule `.git` files, and `.gitmodules`; all three `Action` variants (`ignore`, `rebuild-locally`, `platform-pin`) are defined and unit-tested.
- **Rollback / risk:** Revert policy module. Risk: incorrect semantics either leak sensitive/generated files or omit necessary state.
- **Unresolved decisions:** U3 (ignore file format — bespoke vs `.gitignore`-compatible superset) — **deferred** to chunk owner with rationale recorded.

---

### CHUNK-04-WATCHER-INDEXER

- **Scope:** Filesystem watcher + local indexer: detect local changes, dedup events, maintain a local index of the synced tree consistent with the catalog (CHUNK-02) and policy (CHUNK-03). **Produces the snapshots and durable event queue that CHUNK-05 consumes.** Owns the `watcher_events` persistence schema.
- **Depends on:** CHUNK-02-CATALOG-STRUCTURE, CHUNK-03-IGNORE-PLATFORM-POLICY (CHUNK-01 is transitive through both).
- **Parallel with:** Nothing at this layer (consumes 02 + 03 outputs; produces the interface 05 consumes).
- **Verification:**
  - Integration test: create/edit/move/delete a file produces correct, deduped index events.
  - Integration test: ignored and platform-specific paths produce no content-sync events, while the snapshot/event queue preserves policy action metadata, rebuild hints, and platform-pin decisions for CHUNK-05 (all `Action` variants exercised: `ignore`, `rebuild-locally`, `platform-pin`).
  - Integration test: `.git/` directories, submodule `.git` pointer files, and `.gitmodules` are indexed/evented only according to CHUNK-03 policy; ignored Git metadata produces no content-sync events.
  - Race test: rapid bulk changes converge to a stable index.
  - Migration test: `watcher_events` schema applies cleanly from the baseline and rolls back.
- **Review criteria:** Watcher handles renames, deletes, permissions, symlinks; event ordering is documented; no lost events on bulk change; **watcher library per OS (U4) chosen with recorded rationale**; snapshot/event-queue interface frozen for CHUNK-05; policy actions, including Git metadata/submodule outcomes, do not emit unsafe content-sync events but do preserve policy metadata/rebuild hints/platform-pin decisions for downstream convergence.
- **Rollback / risk:** Revert watcher + `watcher_events` migration. Risk: filesystem-watching races and divergent OS semantics (Mac vs Linux) — primary hardening in CHUNK-09.
- **Unresolved decisions:** U4 (watcher library choice per OS) — **deferred** to chunk owner; must work on macOS and Linux; rationale recorded.

---

### CHUNK-05-SYNC-STORE-CONVERGENCE

- **Scope:** Authoritative remote sync store + convergence protocol: conflict resolution, partial/offline state, operation log, eventual consistency across machines. **Owns the production cross-machine transport/backing sync path** (daemon-to-daemon or backend selected by U10): machine enrollment/auth/pairing, the production transport, generic authenticated/encrypted payload + content envelopes, and the `sync_*` persistence schema. Local loopback is allowed only as a deterministic test harness exercising that same production transport contract; mock transport is only for deterministic lower-level tests. Consumes CHUNK-04's snapshots/event queue and CHUNK-03's policy action contract.
- **Depends on:** CHUNK-04-WATCHER-INDEXER, CHUNK-03-IGNORE-PLATFORM-POLICY (direct policy action contract used by convergence; CHUNK-02 and CHUNK-01 are transitive through CHUNK-04/CHUNK-03).
- **Parallel with:** Nothing at this layer (serializes after 04; must complete before {06, 07}).
- **Verification:**
  - Integration test: two machines editing the same file converge with the documented conflict resolution (no silent data loss).
  - Integration test: offline machine reconnects and converges.
  - Property test: operation log replay is deterministic.
  - Transport test: two daemon instances enroll/pair and exchange generic manifest metadata plus content payloads over the production transport path via its local harness (not a mock; loopback only as the harness).
  - In-transit security test: generic payload/content envelopes are authenticated and encrypted over the transport; plaintext does not cross the wire. Env-specific payload assertions belong to CHUNK-07.
  - Convergence test exercises all CHUNK-03 policy actions through the policy contract: `ignore` (no sync event), `rebuild-locally` (rebuild hint, no content), `platform-pin` (refused/redirected on mismatched platform), plus Git metadata disposition (`.git/` and submodule `.git` files never transmitted when classified local-only; `.gitmodules` follows the recorded policy).
  - Migration test: `sync_*` schema applies cleanly from the baseline and rolls back.
  - **Rollback/backup test: sync-store state can be backed up and restored to a known-good state via a documented CHUNK-05 procedure** (independent of CHUNK-09).
- **Review criteria:** Conflict policy documented and consistent with the resolved U5 policy (last-writer-wins + conflict sidecar + manual escape hatch); no silent overwrites; partial/offline behavior defined; **transport backend (U10) chosen with recorded rationale**; production cross-machine/backing transport path implemented (mock/loopback only as test harnesses); generic authenticated/encrypted payload/content transport verified; all policy actions and Git metadata disposition branched in the convergence plan; rollback procedure documented and tested.
- **Rollback / risk:** Revert store module + `sync_*` migration. Risk: highest correctness risk in the system (conflicts, partial state, renames, large files, user state). **CHUNK-05 owns its own rollback/backup/restore procedure for sync-store state** so its merge boundary is independently revertable; CHUNK-09 hardens the end-to-end rollback drill but does not introduce the first rollback plan for sync state.
- **Unresolved decisions:** U5 (conflict resolution strategy) — **resolved**: last-writer-wins with a conflict sidecar + a documented manual escape hatch. Rationale: simplest correct-by-default policy that never silently overwrites; the sidecar preserves the losing copy for manual resolution. U10 (transport/backend choice) — **blocker** for this chunk until chosen, because the production cross-machine/backing sync path depends on it; loopback is a test harness, not the production answer.

---

### CHUNK-06-LAZY-HYDRATION-VFS

- **Scope:** Structure-first sync with on-demand content fetch: placeholder metadata for files not yet local; hydrate file contents on access; virtual filesystem / editor-tool integration so normal file access works while content is absent.
- **Depends on:** CHUNK-05-SYNC-STORE-CONVERGENCE (catalog/policy/foundation are consumed transitively through CHUNK-05 unless this chunk deliberately adds a direct import).
- **Parallel with:** CHUNK-07-ENV-SYNC (independent surfaces; both sit on the store).
- **Verification:**
  - Integration test: a freshly synced machine shows full directory structure with zero file contents fetched.
  - Integration test: reading a placeholder file triggers exactly one content fetch and caches it.
  - Integration test: cache coherency — remote update invalidates the cached content.
  - Hydration test exercises policy outcomes exposed through CHUNK-05: `ignore` (not hydrated), `rebuild-locally` (triggers rebuild, not fetch), `platform-pin` (refused on mismatched platform), and Git metadata disposition (`.git/` and local-only submodule `.git` files never hydrated; `.gitmodules` follows the recorded policy). No direct CHUNK-03 import unless the chunk explicitly records that new dependency.
- **Review criteria:** Access latency and failure modes documented; hydration is lazy and cache-coherent; structure sync never fetches content; **VFS approach (U6) chosen with recorded rationale**; policy outcomes, including Git metadata/submodule outcomes, enforced at hydration time through the CHUNK-05 contract.
- **Rollback / risk:** Revert VFS module. Risk: cache coherency, latency, and failure modes when content is absent; OS-level filesystem integration is non-trivial.
- **Unresolved decisions:** U6 (VFS approach — FUSE/platform native vs editor plugin vs transparent stub files) — **blocker** for this chunk until chosen, because the verification approach depends on it; rationale recorded.

---

### CHUNK-07-ENV-SYNC

- **Scope:** Environment variable synchronization: env payload schema carried over CHUNK-05's generic authenticated/encrypted transport contract, per-machine overrides, env materialization usable by developer processes (for example documented shell/session/daemon launch integration), encryption at rest, key management, auditability, and secret-leakage prevention. Owns the `env_*` persistence schema, encryption key location, env-specific in-transit/no-plaintext checks, and guarantees that CHUNK-05 is not asked to understand env-specific blobs.
- **Depends on:** CHUNK-05-SYNC-STORE-CONVERGENCE.
- **Parallel with:** CHUNK-06-LAZY-HYDRATION-VFS (independent surfaces; both sit on the store).
- **Verification:**
  - Unit/integration test: env var set on machine A appears on machine B with per-machine override semantics honored and is materialized so developer processes can consume it through the documented mechanism.
  - Contract test: env payloads use CHUNK-05's generic authenticated/encrypted payload transport contract; there is no env-specific transport bypass or CHUNK-05 env parsing.
  - Security test: secrets are encrypted at rest; env plaintext never appears in transport captures, logs, artifacts, or tracker evidence.
  - Audit test: every env sync op emits an auditable record.
  - Key-management test: encryption keys are provisioned/rotated per the chosen U7 scheme; a machine without the key cannot decrypt.
  - Migration test: `env_*` schema applies cleanly from the baseline and rolls back.
- **Review criteria:** Encryption mandatory at rest; env payloads must use CHUNK-05's generic authenticated/encrypted transport contract with env-specific no-plaintext verification here; **key management (U7) chosen with recorded rationale**; per-machine overrides and developer-process materialization documented; no plaintext in logs/artifacts/transport; audit trail present; env sync conflicts reuse the resolved U5 conflict policy (last-writer-wins + conflict sidecar).
- **Rollback / risk:** Revert env-sync module + `env_*` migration. Risk: sensitive data — secret handling is first-class. Do not ship without encryption at rest and in transit.
- **Unresolved decisions:** U7 (encryption key management) — **blocker** for this chunk until chosen; rationale recorded.

---

### CHUNK-08-CLI-DAEMON-UX

- **Scope:** User-facing CLI + daemon lifecycle + status/observability: start/stop/status, `sync pause`/`sync resume` of syncing, sync state visibility, error surfacing, help text. **Owns the `cli/` module and the full command surface** (earlier chunks verify via module-level tests, not CLI commands, except CHUNK-01's minimal `version`/`info` shim). Wires the transport-reachable doctor check.
- **Depends on:** CHUNK-04-WATCHER-INDEXER, CHUNK-05-SYNC-STORE-CONVERGENCE, CHUNK-06-LAZY-HYDRATION-VFS, CHUNK-07-ENV-SYNC (CHUNK-01/02/03 are transitive).
- **Parallel with:** Nothing (integrates all upstream surfaces).
- **Verification:**
  - CLI test: `init`, `status`, `sync`, `sync pause`, `sync resume`, `catalog`, `policy`, `watch`, `hydrate`, `env`, `doctor`, `version`/`info` commands behave as documented, including `sync pause`/`sync resume` semantics.
  - Daemon test: daemon stays running, honors `sync pause`/`sync resume`, surfaces sync errors, and reports hydration/env state.
  - UX test: a stale-worktree scenario (README.md:8 pain point) is observable and recoverable from the CLI.
  - Doctor test: `doctor` checks config valid, **transport reachable**, permissions, cache dir writable, policy table loaded; exits non-zero with a remediation hint on any failure.
- **Review criteria:** All upstream capabilities are reachable from the CLI; `sync pause`/`sync resume` is explicit and observable; errors are actionable; daemon lifecycle is robust; **daemon supervision model (U8) chosen with recorded rationale**; transport-reachable check wired to CHUNK-05's real transport.
- **Rollback / risk:** Revert CLI/daemon. Risk: low correctness risk, high integration surface — depends on all upstream chunks being correct.
- **Unresolved decisions:** U8 (daemon supervision model per OS) — **deferred** to chunk owner; rationale recorded.

---

### CHUNK-09-E2E-HARDENING

- **Scope:** Multi-machine end-to-end simulation + hardening over the **real transport** (local multi-daemon harness of the production transport, not mock-only): stale-worktree/missed-pull recovery, conflicts, renames, deletes, permissions, symlinks, binary/large files, offline/reconnect, Mac↔Linux divergence, rollback drills, Git metadata/submodule safety, ignore/env edge cases, in-transit secret safety. **Tests/runbook/fuzz only — no production module internals.** Any failure discovered here spawns scoped upstream fix work under the affected existing chunk ID before CHUNK-09 can pass, tracked with the resumable procedure below; canonical chunk IDs stay unchanged.
- **Depends on:** CHUNK-08-CLI-DAEMON-UX (all previous chunks are transitive through the integrated CLI/daemon surface).
- **Parallel with:** Nothing (final hardening layer).
- **Verification:**
  - E2E test: two daemon instances (Mac + Linux simulation) sync a real project tree over the production transport's local multi-daemon harness with `node_modules`, ignored files (`.syncignore`), Git metadata/submodule fixtures, env vars (with a secret), and a lazy-hydrated file; assert convergence, no unwanted sync of ignored content or local-only Git metadata, no plaintext secrets at rest or in transit, correct hydration.
  - Queued `sync pause`/`sync resume` scenario: changes made while `sync pause` is active queue without content transfer, then `sync resume` drains them in order with observable status.
  - Stale-worktree/missed-pull scenario: one machine starts from a stale project tree that missed upstream Git updates; the daemon/CLI makes the stale state observable, syncs or remediates through the product path without destructive Git operations, and records recovery evidence.
  - Ignore scenario: `.syncignore`-ignored files/directories are not transmitted, hydrated, or reported as missing across two machines, while adjacent non-ignored content still syncs.
  - In-transit scenario: env secret is encrypted over the real transport; a transport capture contains no plaintext.
  - Git metadata/submodule scenario: repository `.git/` metadata and submodule `.git` pointer files follow CHUNK-03's local-only disposition, `.gitmodules` follows the recorded CHUNK-03 behavior, and submodule path contents sync as ordinary folder contents when policy permits.
  - Failure-injection scenarios: daemon kill/restart, network partition/heal, and offline hydration-source unavailability all recover or fail with bounded, observable errors.
  - Platform matrix: Mac↔Linux and same-OS machine combinations exercise shared `Platform` identity and platform-specific policy outcomes.
  - Performance/hydration budget gate: before measuring, declare threshold values for 10k-file structure/index/sync work and hydration latency; then record measured results and compare them against those budgets.
  - Rollout/rollback drill: a bad sync and a schema migration can be rolled back to a known-good state via the documented CHUNK-05 procedure plus release runbook.
  - Bounded fuzz targets: documented commands with fixed seed and time/iteration caps fuzz both manifest/policy pure functions and the public command/protocol parsing surface without unbounded runtime.
  - Upstream-fix tracking drill: when an E2E failure belongs to an upstream chunk, pause CHUNK-09, create/record a scoped upstream fix task under the affected existing chunk ID in the progress tracker with immediate dependency and owner, move `👉 NEXT` to the fix work, record fix evidence/review status, then resume CHUNK-09 from the failed scenario.
  - Soak test: sustained watcher activity converges without lost events or divergence.
- **Review criteria:** Every README capability (README.md:8–13 pain points and README.md:19–24 capabilities) is exercised end-to-end, including stale-worktree/missed-pull recovery and Git metadata/submodule safety; hardening gates pass for queued `sync pause`/`sync resume`, failure injection, platform matrix, predeclared 10k-file/performance + hydration latency budgets, rollout/rollback with schema migration, and both fuzz target families (manifest/policy and public command/protocol parsing); rollback procedure documented and tested; known edge cases enumerated and passing; **failures spawn scoped upstream fix work under existing chunk IDs using the progress tracker's resumable upstream-fix procedure** (CHUNK-09 does not edit production module internals); mock transport used only for deterministic lower-level tests, with at least one E2E path over the real transport.
- **Rollback / risk:** Revert hardening tests/fixes. Risk: this chunk surfaces latent bugs in every upstream chunk; budget for upstream fix work and keep its status/evidence resumable in the progress tracker before resuming CHUNK-09.
- **Unresolved decisions:** None for CHUNK-09 itself; it consumes prior decisions, including resolved U5 and CHUNK-01's recorded U9 decision only after CHUNK-01 has resolved and recorded U9.

---

## Dependency Order & Parallelism

```
CHUNK-01-FOUNDATION
├── CHUNK-02-CATALOG-STRUCTURE       ┐
└── CHUNK-03-IGNORE-PLATFORM-POLICY  ┘ (parallel pair; both depend only on 01)
        │
        └── CHUNK-04-WATCHER-INDEXER      (after 02+03)
                │
                └── CHUNK-05-SYNC-STORE-CONVERGENCE
                    (after 04; also consumes CHUNK-03 policy action contract directly)
                        │
                        ├── CHUNK-06-LAZY-HYDRATION-VFS  (after 05; parallel with 07)
                        └── CHUNK-07-ENV-SYNC            (after 05; parallel with 06)
                                │
CHUNK-08-CLI-DAEMON-UX  (after direct surfaces 04+05+06+07)
        │
CHUNK-09-E2E-HARDENING  (after 08; all previous chunks are transitive)
```

**Canonical order:** `01 → {02, 03} → 04 → 05 → {06, 07} → 08 → 09`.

**Parallel waves (max fan-out):**
1. Wave 1: `CHUNK-01-FOUNDATION` (solo).
2. Wave 2: `CHUNK-02-CATALOG-STRUCTURE` ∥ `CHUNK-03-IGNORE-PLATFORM-POLICY`.
3. Wave 3: `CHUNK-04-WATCHER-INDEXER` (solo).
4. Wave 4: `CHUNK-05-SYNC-STORE-CONVERGENCE` (solo; consumes 04).
5. Wave 5: `CHUNK-06-LAZY-HYDRATION-VFS` ∥ `CHUNK-07-ENV-SYNC`.
6. Wave 6: `CHUNK-08-CLI-DAEMON-UX` (solo).
7. Wave 7: `CHUNK-09-E2E-HARDENING` (solo).

**Next dependency-ready chunk(s) (unchecked, at planning time):** `CHUNK-01-FOUNDATION` — no dependencies, sole entry point. The first unchecked doable task within each dependency-ready chunk carries a `👉 NEXT` marker in the progress tracker; when a parallel wave is ready (02 ∥ 03, then 06 ∥ 07), multiple task-level `👉 NEXT` markers are expected.

## Review & Commit Boundaries

- One implementation commit/PR per chunk is preferred. Commit message convention for implementation work: `feat(chunk-XX): <summary>` or `chore(chunk-XX): <summary>`.
- Chunk branches/worktrees use the exact lowercase, kebab-case naming convention in `docs/planning/readme-goal-parallel-worktrees.md` (branch row plus sibling `../dropbox-dev-chunk-NN-*` path row); the progress tracker `Owner branch / worktree` row records both values when work starts, including scoped CHUNK-09 upstream-fix names from the worktree plan.
- The progress tracker is the **sole mutable source of truth** for task/chunk status, `👉 NEXT` placement, commit rows, verification evidence, and resume/upstream-fix state. This implementation plan and the chunk checklist do not carry mutable implementation status/evidence.
- A chunk PR must include its tracker status/evidence updates before review can be accepted; tracker evidence/status is part of review, not a post-review chore.
- After a chunk PR is squash-merged to `master`, master-side tracker edits are limited to recording the merge SHA in `Commit(s)` and moving `👉 NEXT` / Next Up markers. Any other status/evidence correction must be a separate tracker correction docs PR/commit.
- A chunk is reviewable only when its verification commands pass, its review criteria are met, its checklist items are satisfied, and the tracker records the evidence/commits that prove those facts.
- Blocker-tagged unresolved decisions (U1, U6, U7, U9, U10) must be resolved before the owning chunk's review can be marked accepted.
- Deferred decisions (U2, U3, U4, U8) may be resolved by the chunk owner with rationale recorded in the chunk's PR and progress tracker.
- U5 is **resolved** (last-writer-wins + conflict sidecar + manual escape hatch); all chunks must reuse this single policy.

## Migration / Testing / Rollback (cross-chunk)

- **Migration framework:** CHUNK-01 owns the migration runner + schema version table (empty `v0` baseline) only. **Product schemas are owned by their feature chunks**: `catalog_*` by CHUNK-02, `watcher_events` by CHUNK-04, `sync_*` by CHUNK-05, `env_*` by CHUNK-07. Each schema-owning chunk writes its first migration on top of the baseline and verifies apply + rollback. No two chunks share a table-name prefix. Schema version bumps are owned by the schema-owning chunk (a new migration within that chunk or a clearly-scoped sub-PR).
- **Testing:** Unit + integration per chunk (see each chunk's Verification); E2E + soak in CHUNK-09. E2E uses the real filesystem and the real local multi-daemon harness for the production transport; mock transport is used only for deterministic lower-level tests, never as the only executable cross-machine path.
- **Toolchain-specific verification commands:** CHUNK-01 chooses and documents the stack-level wrapper commands (`make build`/`make test`/`make lint` or equivalents). For CHUNK-02 through CHUNK-09, this plan names required behaviors; the progress tracker records the exact post-CHUNK-01 command/scenario, result, date/SHA, and artifact for each verification slot. If the toolchain command changes, the same PR/evidence commit updates CHUNK-01 docs and the relevant tracker evidence row.
- **Rollback:** Per-chunk revert is safe for chunks 01–04, 06–08. **CHUNK-05 owns its own sync-store rollback/backup/restore procedure** so its merge boundary is independently revertable. CHUNK-09 hardens the end-to-end rollback drill (exercising CHUNK-05's procedure + the release runbook) but does not introduce the first rollback plan for sync state. No destructive git operations during planning.

## Risks (cross-chunk, from context)

- **R1 — Greenfield correctness:** No existing architecture; sync must handle conflicts, partial/offline state, watcher races, renames, deletes, permissions, symlinks, binary/large files, divergent OS semantics. (Owned by CHUNK-05; exercised by CHUNK-04, CHUNK-06, CHUNK-09.)
- **R2 — Env secrets:** Env var sync is sensitive; encryption at rest and in transit, key management, per-machine overrides, auditability, leakage prevention are first-class. (Owned by CHUNK-07; in-transit path provided by CHUNK-05; exercised by CHUNK-09.)
- **R3 — `node_modules` / platform-specific:** Naive sync is huge, slow, non-portable, and wrong across Mac/Linux. (Owned by CHUNK-03; exercised by CHUNK-04, CHUNK-05, CHUNK-06, CHUNK-09.)
- **R4 — Lazy hydration:** Filesystem-level/editor integration for access-while-absent introduces latency, cache coherency, and failure-mode risks. (Owned by CHUNK-06; exercised by CHUNK-09.)
- **R5 — Ignore semantics:** Incorrect semantics leak sensitive/generated files or omit necessary state. (Owned by CHUNK-03; exercised end-to-end by CHUNK-09.)
- **R6 — FS2 dependency:** FS2 is mentioned as insufficient (README.md:24) but not present locally; the build-vs-adopt-vs-vendor decision (U9) is owned by CHUNK-01 and must be recorded before catalog/VFS design begin. (Owned by CHUNK-01.)
- **R7 — Cross-machine transport:** The README goal is automatic cross-machine sync; a production transport/backend with enrollment/auth/pairing and in-transit security must be implemented (CHUNK-05), not mock-only and not loopback-only. Local loopback is a harness for testing the production transport contract. (Owned by CHUNK-05; wired by CHUNK-08; exercised by CHUNK-09.)

## Unresolved Decisions

| ID | Decision | State | Owner chunk | Reason |
|----|----------|-------|-------------|--------|
| U1 | Stack / language choice | **Blocker** | CHUNK-01 | Every downstream verification command depends on it. |
| U2 | Catalog serialization format | Deferred | CHUNK-02 | Must be reproducible across machines; chunk owner decides with rationale recorded in PR + tracker. |
| U3 | Ignore file format (bespoke vs `.gitignore`-compatible superset) | Deferred | CHUNK-03 | Chunk owner decides; must remain distinct from `.gitignore` unless explicitly chosen otherwise; rationale recorded. |
| U4 | Filesystem watcher library per OS | Deferred | CHUNK-04 | Must work on macOS and Linux; chunk owner selects; rationale recorded. |
| U5 | Conflict resolution strategy (LWW vs 3-way vs operational) | **Resolved** | CHUNK-05 | Chosen: last-writer-wins with a conflict sidecar + a documented manual escape hatch. Rationale: simplest correct-by-default policy that never silently overwrites; the sidecar preserves the losing copy. All downstream chunks (CHUNK-07, CHUNK-09) reuse this single policy. |
| U6 | VFS approach (FUSE/platform native vs editor plugin vs stub files) | **Blocker** | CHUNK-06 | Verification approach depends on it; rationale recorded. |
| U7 | Encryption key management for env sync | **Blocker** | CHUNK-07 | Cannot ship env sync without it; rationale recorded. |
| U8 | Daemon supervision model per OS | Deferred | CHUNK-08 | Chunk owner selects per OS; rationale recorded. |
| U9 | Whether to integrate, adopt-and-extend, or vendor FS2 (or build-from-scratch) | **Blocker** | CHUNK-01 | README.md:24 says FS2 is insufficient; the build-vs-adopt-vs-vendor decision shapes catalog and VFS design (CHUNK-02/CHUNK-06) and must be recorded before those chunks begin. It is not already resolved; CHUNK-09 consumes U9 only after CHUNK-01 records the decision. Default stance: build-from-scratch unless an inspection proves FS2 covers the catalog/hydration surface. |
| U10 | Sync transport/backend choice (production daemon-to-daemon/backend path + machine enrollment/auth/pairing; local loopback only as harness) | **Blocker** | CHUNK-05 | The README goal is automatic cross-machine sync; the production transport/backing sync path depends on this choice. Mock transport is for deterministic tests only. |

## Out of Scope for This Artifact

- Implementing product code (this is a planning artifact).
- Editing `README.md` (authoritative reference; not to be modified).
- Running gates, formatters, tests, or commits (planning only).
- The companion artifacts `docs/planning/readme-goal-chunk-checklist.md`, `docs/planning/readme-goal-parallel-worktrees.md`, and `docs/planning/readme-goal-progress-tracker.md` are planning-contract artifacts with the role-specific authority described above; they are maintained by the planning layer, not product implementation code.
