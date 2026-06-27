# README Goal — Parallel Worktree Plan

> Rules for executing the chunk checklist (`docs/planning/readme-goal-chunk-checklist.md`)
> across parallel git worktrees without file/interface/migration/generated-artifact collisions.
>
> Chunk IDs are fixed by `local://planning-contract.json`. Dependency order is fixed by the
> implementation plan (`docs/planning/readme-goal-implementation-plan.md`):
>
> ```
> CHUNK-01-FOUNDATION
>   └─> { CHUNK-02-CATALOG-STRUCTURE, CHUNK-03-IGNORE-PLATFORM-POLICY }   // parallel
>         └─> CHUNK-04-WATCHER-INDEXER
>               └─> CHUNK-05-SYNC-STORE-CONVERGENCE   // consumes 04 snapshots/event queue
>                     └─> { CHUNK-06-LAZY-HYDRATION-VFS, CHUNK-07-ENV-SYNC }  // parallel
>                           └─> CHUNK-08-CLI-DAEMON-UX
>                                 └─> CHUNK-09-E2E-HARDENING
> ```
>
> **Canonical order:** `01 → {02, 03} → 04 → 05 → {06, 07} → 08 → 09`.
> CHUNK-04 and CHUNK-05 are **serialized** (04 before 05): CHUNK-05 consumes CHUNK-04's
> snapshots/event queue, so 04 must merge before 05 starts. There is no 04 ∥ 05 parallel wave.
>
> This artifact is planning only. No product code is written here, no gates/formatters/tests are
> run, and README.md is not edited.

---

## 1. Branch and worktree naming convention

Every chunk gets exactly one worktree and one branch. Names are lowercase, kebab-case, and embed
the full lowercase chunk ID (for example, `chunk-06-lazy-hydration-vfs`) so tooling can map a branch or path back to a checklist chunk unambiguously.

| Chunk ID | Worktree path (sibling of repo root) | Branch |
| --- | --- | --- |
| CHUNK-01-FOUNDATION | `../dropbox-dev-chunk-01-foundation` | `chunk-01-foundation` |
| CHUNK-02-CATALOG-STRUCTURE | `../dropbox-dev-chunk-02-catalog-structure` | `chunk-02-catalog-structure` |
| CHUNK-03-IGNORE-PLATFORM-POLICY | `../dropbox-dev-chunk-03-ignore-platform-policy` | `chunk-03-ignore-platform-policy` |
| CHUNK-04-WATCHER-INDEXER | `../dropbox-dev-chunk-04-watcher-indexer` | `chunk-04-watcher-indexer` |
| CHUNK-05-SYNC-STORE-CONVERGENCE | `../dropbox-dev-chunk-05-sync-store-convergence` | `chunk-05-sync-store-convergence` |
| CHUNK-06-LAZY-HYDRATION-VFS | `../dropbox-dev-chunk-06-lazy-hydration-vfs` | `chunk-06-lazy-hydration-vfs` |
| CHUNK-07-ENV-SYNC | `../dropbox-dev-chunk-07-env-sync` | `chunk-07-env-sync` |
| CHUNK-08-CLI-DAEMON-UX | `../dropbox-dev-chunk-08-cli-daemon-ux` | `chunk-08-cli-daemon-ux` |
| CHUNK-09-E2E-HARDENING | `../dropbox-dev-chunk-09-e2e-hardening` | `chunk-09-e2e-hardening` |

Rules:
- One worktree per chunk at a time. Do not spin a second worktree for the same chunk while the
  first is unmerged.
- Worktrees live as siblings of the main repo root (`..`), never inside the working tree, so they
  are never accidentally watched/indexed by the product's own watcher.
- The branch name and worktree path must agree on the full lowercase chunk ID; scripts may validate either, but must fail if they disagree.
- A `chunk-NN-*` branch is short-lived: it exists only from chunk start until merge + prune.

---

## 2. Concurrency map — what can run in parallel and what must serialize

### Parallel groups (may run concurrently in separate worktrees)

**Group A — after CHUNK-01 merges:**
- `CHUNK-02-CATALOG-STRUCTURE` (branch `chunk-02-catalog-structure`)
- `CHUNK-03-IGNORE-PLATFORM-POLICY` (branch `chunk-03-ignore-platform-policy`)

These two are parallel-safe because they own disjoint modules (`catalog/` vs `policy/`) and
neither imports the other. See §3 for the exact ownership table.

**Group B — after CHUNK-05 merges:**
- `CHUNK-06-LAZY-HYDRATION-VFS` (branch `chunk-06-lazy-hydration-vfs`)
- `CHUNK-07-ENV-SYNC` (branch `chunk-07-env-sync`)

Parallel-safe: `vfs/` vs `env/`, disjoint modules, disjoint interfaces. Both have immediate
dependency `CHUNK-05`; catalog/policy/foundation contracts are already present through the
canonical merged chain.

### Serialized chunks (must run alone, in order)

| Chunk | Why it must serialize |
| --- | --- |
| `CHUNK-01-FOUNDATION` | Defines the foundation surface, shared `Platform`/OS identity type, tool `.gitignore`, empty downstream module shells, minimal `version`/`info` CLI smoke path, and migration framework. Feature ownership of downstream shells transfers after this merges; nothing else can start until then. |
| `CHUNK-04-WATCHER-INDEXER` | Imports both `catalog/` (02) and `policy/` (03); both must be merged first. Owns the event queue and snapshot producer that 05 consumes. |
| `CHUNK-05-SYNC-STORE-CONVERGENCE` | Consumes CHUNK-04's snapshots/event queue; **04 must merge before 05 starts**. Owns the sync store protocol, production cross-machine transport/backing store, endpoint/security config, generic encrypted payload path, and conflict resolver reused by 06 and 07; must merge before either starts. |
| `CHUNK-08-CLI-DAEMON-UX` | Waits for both 06 and 07, then wires the transitive 04/05/06/07 stack into the daemon. Owns `cli/` beyond CHUNK-01's minimal skeleton. |
| `CHUNK-09-E2E-HARDENING` | Final gate; depends on the full daemon (08). Nothing runs concurrently with it. Tests/runbook/fuzz only — no production module internals. |

> **No 04 ∥ 05 parallel wave.** CHUNK-05 depends on CHUNK-04 (it consumes 04's snapshots/event
> queue). The implementation plan, progress tracker, and checklist all serialize 04 before 05.

### Execution timeline (canonical)

```
[01] ──merge──> {02 ‖ 03} ──both merge──> [04] ──merge──> [05] ──merge──> {06 ‖ 07} ──both merge──> [08] ──merge──> [09]
```

`‖` = concurrent worktrees. `──merge──>` = serialize: merge the upstream chunk into `master`
before starting the downstream chunk's worktree.

---

## 3. Ownership table — preventing overlapping files, interfaces, migrations, generated artifacts

Each chunk owns a **module directory** and a **set of interfaces**. No two concurrent chunks may
own the same directory, the same interface, or the same generated/migration artifact. The table
below is the contract; violating it is a merge-blocker.

| Chunk | Owns module dir | Owns interfaces / types | Owns migrations / generated | Forbidden to touch |
| --- | --- | --- | --- | --- |
| 01-FOUNDATION | `foundation/`, root manifest/initial lockfile, CI config, tool `.gitignore`, empty downstream module shells, minimal `cli/` version/info shim | `SyncError`, `ConfigError`, `WatchError`, `VfsError`, `EnvError`, `Logger`, `Config` schema, shared `Platform`/OS identity type, CLI version/info shim | **migration framework + schema version table + empty `v0` baseline only (no product tables)** | product feature internals in downstream shells |
| 02-CATALOG | `catalog/` | `Project`, `Machine` (using CHUNK-01 `Platform`/OS identity), `TreeManifest`, `PlaceholderRecord`, `CatalogStore` API, structure-diff API | **`catalog_*` initial migration (owned schema namespace)** | `policy/`, `watcher/`, `sync/`, `vfs/`, `env/`, `cli/` |
| 03-POLICY | `policy/` | `Policy`, `Action` (`ignore`/`rebuild-locally`/`platform-pin`), `.syncignore` loader API, explicit Git metadata policy for `.git/`, submodule `.git` gitfiles, and `.gitmodules`, `Policy::evaluate(path, Platform)` | policy table data file (built-in defaults) | `catalog/`, `watcher/`, `sync/`, `vfs/`, `env/`, `cli/` |
| 04-WATCHER | `watcher/` | `FsEvent`, `EventQueue`, `Indexer`, `SnapshotProducer` (frozen for 05, including policy-action metadata/rebuild hints/platform-pin decisions) | **`watcher_events` initial migration (owned schema namespace)** | `catalog/` internals, `policy/` internals, `sync/`, `vfs/`, `env/`, `cli/` |
| 05-SYNC | `sync/` | `SyncStore` protocol, generic payload envelope, `ConvergencePlan` consuming CHUNK-04 policy-action metadata, `ConflictResolver`, `SyncOp`, production transport/backend, endpoint/security config, enrollment/auth/pairing | **`sync_*` initial migration (owned schema namespace)** | `catalog/`, `policy/`, `watcher/`, `vfs/`, `env/`, `cli/` |
| 06-VFS | `vfs/` | `Hydrator`, `HydrationCache`, `VfsMount`, `PlaceholderMaterializer` | hydration cache dir layout | `env/`, `sync/` internals, `catalog/` internals, `cli/` |
| 07-ENV | `env/` | `EnvVar`, `EnvStore`, `EnvAuditLog`, `EnvCipher`, `EnvMaterializer`, env payload schema/key management over CHUNK-05 generic payloads | **`env_*` initial migration (owned schema namespace)**, encryption key location | `vfs/`, `sync/` internals, `catalog/` internals, `cli/` |
| 08-CLI | `cli/`, daemon entrypoint | `Command` surface, `DaemonControl` protocol, `Doctor` checks | daemon socket/pipe path layout | any module's internals (wiring only — may add imports, not redefine interfaces) |
| 09-E2E | `tests/e2e/`, fuzz targets, runbook | test harness interfaces only; bounded fuzz targets for manifest/policy and public command/protocol parsing | none (consumes existing schemas) | any production module's internals |

> **CLI ownership:** CHUNK-01 may create only the `cli/` shell plus the minimal
> `version` and `info` commands needed to prove the skeleton compiles. After CHUNK-01 merges,
> CHUNK-08 owns `cli/` and the full command surface. CHUNK-02 through CHUNK-07 are
> **forbidden to touch `cli/`** and verify via module-level tests / module harnesses
> only. User-facing CLI command verification (`<cli> catalog`, `<cli> policy`, `<cli> watch`,
> `<cli> sync`, `<cli> hydrate`, `<cli> env`, `<cli> sync pause`, `<cli> sync resume`) lives in CHUNK-08/09.
>
> **CHUNK-09 scope:** tests/runbook/fuzz only. If E2E exposes a defect in a production module,
> CHUNK-09 does **not** edit that module; it parks and opens a scoped upstream fix branch using
> the procedure in §4 before CHUNK-09 can pass.
>
> **CHUNK-09 hardening gates:** the final suite/runbook/fuzz layer includes stale-worktree /
> missed-`git pull` E2E, submodule/Git metadata E2E, queued-work `<cli> sync pause` /
> `<cli> sync resume`, real failure injection, the macOS/Linux platform matrix, predeclared
> sync/index and hydration-latency budgets with measured comparisons, a rollout/rollback plus
> schema migration runbook, and both fuzz target families (manifest/policy plus public command/protocol parsing).

### Interface stability rules
- An interface is **frozen** once the chunk that owns it merges. Downstream chunks may add
  methods/fields with defaults but MUST NOT rename or remove existing ones without a new chunk
  and a migration.
- CHUNK-01's empty downstream shells and minimal CLI shim are import/build placeholders only;
  they do not freeze feature interfaces. The owning chunk may replace the shell when it starts
  from merged `master`.
- CHUNK-01's shared `Platform`/OS identity type is frozen before CHUNK-02 and CHUNK-03 start in
  parallel. CHUNK-02 `Machine`, CHUNK-03 policy, and later platform-matrix checks consume that
  type instead of defining parallel platform enums.
- Concurrent chunks (Group A: 02 & 03; Group B: 06 & 07) MUST NOT import each other. If a real
  dependency is discovered mid-flight, stop, message the sibling chunk owner via IRC, and
  **serialize**: merge one first, then rebase the other onto it. Do not silently cross-import.
- The shared error/log/config/Platform surface (01) is the only cross-cutting interface any chunk
  may import freely.

### Migration / generated-artifact isolation
- CHUNK-01 owns the **migration framework + schema version table + empty `v0` baseline only**. It
  does **not** own product schemas. Product schemas are owned by their feature chunks:
  `catalog_*` by 02, `watcher_events` by 04, `sync_*` by 05, `env_*` by 07.
- Each schema-owning chunk writes its first migration on top of the empty baseline and verifies
  apply + rollback to the baseline. No two chunks may write the same table prefix.
- DB schema version bumps beyond a chunk's initial migration are owned by that schema-owning
  chunk (a new migration within the chunk or a clearly-scoped sub-PR) — never a drive-by edit in
  a concurrent chunk.
- Generated artifacts (build output, hydration cache, daemon socket) are never shared across
  worktrees: each worktree has its own build/cache dir, and the daemon socket path is derived
  from the worktree path, not a global location. Lockfiles follow the dependency rule below.

### Dependency / manifest / lockfile isolation
- CHUNK-01 picks the stack and creates the root manifest + initial lockfile. A later chunk that
  needs a dependency owns the manifest and lockfile delta together in that chunk branch; never
  commit one without the other.
- Concurrent chunks may not both edit the manifest or lockfile. If both need dependency changes,
  serialize before either edits (or have the earlier/upstream chunk own the shared dependency),
  then rebase the second on the merged lockfile.
- Progress-tracker/planning commits never carry product dependency, manifest, or lockfile
  changes; those changes travel with the product chunk that needs them.

### Checklist-item isolation
- A checklist item belongs to exactly one chunk. If two chunks appear to need the same item,
  the item belongs to the earlier chunk in the dependency order; the later chunk depends on it.
- The cross-chunk invariants at the end of the checklist are checked at every merge boundary,
  not owned by any single chunk.

---

## 4. Merge, remove, and prune steps after a clean chunk

A chunk is "clean" when it satisfies every item in its checklist section (the boxes are
definition-of-done guidance and do not need to be mutated), its acceptance criteria pass, and
its verification command(s) are green in its worktree. Before review is accepted, the chunk PR
must carry the progress-tracker status and verification evidence; the tracker is the sole
mutable status/evidence ledger. If a workflow also flips checklist boxes as documentation, it
must update the progress tracker in the same workflow. Then:

1. **Rebase onto current `master`.**
   - From the chunk's worktree: `git fetch origin && git rebase origin/master`.
   - Resolve any conflicts against the ownership table in §3. If a conflict touches a file the
     chunk does not own, STOP — this is a contract violation; escalate rather than resolve
     unilaterally.

2. **Re-run the chunk's verification command(s)** after the rebase to confirm the rebase did not
   break anything. A clean pre-rebase run does not count.

3. **Open / fast-forward the PR** for the `chunk-NN-*` branch into `master`. The PR must already
   include the chunk's progress-tracker status/evidence before review is accepted. Squash-merge
   is preferred so each chunk is exactly one commit on `master` with the chunk's commit message
   suggestion from the checklist.

4. **Post-squash tracker update** (`docs/planning/readme-goal-progress-tracker.md`) on
   `master`: record the final merge SHA and move `👉 NEXT` / resume state for downstream work.
   Status, review readiness, and verification evidence belong in the chunk PR before review;
   the post-squash master update is not the first evidence/status record. If a workflow updated
   checklist boxes too, that checklist documentation and tracker ledger update must have landed
   together; never let checklist boxes become the mutable status record.

5. **Remove the worktree** once `master` carries the merge: use the exact full-ID path from §1
   (for example, `git worktree remove ../dropbox-dev-chunk-05-sync-store-convergence`). If git
   refuses due to untracked files, inspect them — they should only be build/cache artifacts; delete those and retry.

6. **Delete the chunk branch**: `git branch -D chunk-NN-*` (after the worktree is removed and the
   branch is merged).

7. **Prune stale worktrees**: `git worktree prune` to clear any metadata for worktrees already
   deleted on disk.

8. **Notify downstream chunks**: the next serialized chunk (or the next parallel group) may now
   start. Broadcast via IRC so any waiting worker can spin its worktree from the updated
   `master`.

### Merge ordering for parallel groups
- For Group A ({02, 03}) and Group B ({06, 07}): merge in either order, but **both must be on
  `master` before the downstream serialized chunk (04, or 08 respectively) starts its worktree.
- If one of the two parallel chunks is ready and the other is not, merge the ready one, record
  its merge SHA / `👉 NEXT` movement in the tracker as applicable, and let the second continue.
  Do not start the downstream chunk until the second merges — the downstream chunk imports both.
- **CHUNK-04 must merge before CHUNK-05 starts** (05 consumes 04's snapshots/event queue). There
  is no 04 ∥ 05 parallel wave.

### Scoped upstream fix branches found by CHUNK-09
- If CHUNK-09 exposes a production-module defect, park CHUNK-09 and identify the owner from the
  ownership table (`catalog`, `policy`, `watcher`, `sync`, `vfs`, `env`, or `cli`). CHUNK-09 does
  not edit production internals in place.
- Fix chunk label: use `CHUNK-09-UPSTREAM-FIX-<OWNER-CHUNK-ID>-<SHORT-SLUG>` in tracker notes.
  Branch/worktree naming: use `chunk-09-e2e-hardening-upstream-fix-<owner-full-lowercase-chunk-id>-<short-slug>` and
  `../dropbox-dev-chunk-09-e2e-hardening-upstream-fix-<owner-full-lowercase-chunk-id>-<short-slug>`,
  where `<owner-full-lowercase-chunk-id>` is the affected owner's canonical full lowercase chunk ID, for example `chunk-05-sync-store-convergence`. This is not a new checklist chunk ID and does not rename the canonical chunks.
- The fix branch may touch only the owning module/test surface needed to correct the defect, plus
  its module-level verification evidence. It serializes against CHUNK-09 and any active owner
  worktree.
- Tracker procedure: when parking CHUNK-09, update the progress tracker on `master` with the
  blocked CHUNK-09 evidence, the scoped upstream-fix branch name, and `👉 NEXT` movement to the
  fix work under the affected existing chunk ID. The fix PR carries its status/evidence before
  review. After the fix squash-merge, the tracker records the fix merge SHA and `👉 NEXT` /
  resume movement back to CHUNK-09; CHUNK-09 then reruns the failed scenario and records resume
  verification in the tracker before it can pass. Downstream evidence lives in the tracker, not
  in hidden branch notes.

---

## 5. Conflict protection and user-owned-change protection

### Conflict protection
- **Ownership table is the first line of defense.** Conflicts should be impossible between
  concurrent chunks because they own disjoint files. If a conflict occurs, it signals a contract
  violation — treat it as a bug in the plan, not a routine merge.
- **Rebase, do not merge-commit, onto `master`.** A linear history makes ownership violations
  obvious in `git log` and keeps each chunk a single squash commit.
- **Never edit a file outside the chunk's owned set without an explicit, IRC-acknowledged
  exception.** This includes "trivial" edits like adding a log line to another module — those
  cause conflicts and blur ownership.
- **Shared planning files** (the implementation plan, this worktree doc, and other planning
  artifacts) are owned by the planning layer. A chunk may update only its progress-tracker rows
  through the tracker path in §4, and CHUNK-01 has the named decision-recording exception below.
- **Root manifest / dependency / lockfile changes** are controlled shared product files governed
  by the dependency/manifest/lockfile rule in §3. A chunk that needs a dependency must update the
  manifest and lockfile together in its own branch; concurrent chunks must not both edit them.
  If two concurrent chunks both need dependency changes, serialize them first.

### Planning-layer exception for CHUNK-01 decision recording
- CHUNK-01 must record the stack choice (U1) and the FS2 build-vs-adopt-vs-vendor decision (U9)
  in the implementation plan (`docs/planning/readme-goal-implementation-plan.md`) as well as the progress tracker.
  This is an **explicit, named exception** to the "chunks MUST NOT edit the implementation plan"
  rule below, because those two decisions are load-bearing inputs to every downstream chunk and
  must be visible in the plan. The edit is confined to the Unresolved Decisions table /
  CHUNK-01 section and is made by the CHUNK-01 worker (or the planning layer on their behalf
  after merge).
- All other open decisions (U2–U4, U6–U8, U10) are recorded in the chunk PR and the progress tracker row
  only; they are not edits to the implementation plan.
- **The planning layer replaces the illustrative verification commands** in the checklist/plan
  with the chosen toolchain after CHUNK-01 lands.

### User-owned-change protection
- **Treat unexpected changes as the user's work.** If a worktree shows modifications the worker
  did not make (e.g., `README.md` was edited, or a file appeared outside the owned set), STOP and
  message `Main` via IRC before doing anything. Do not revert, stage, or commit those changes.
- **README.md is read-only for all chunks.** No chunk edits `README.md`. The README is the goal,
  not an implementation artifact. Any README change is user-owned and out of scope.
- **Existing planning artifacts** (`docs/planning/*`) are owned by the planning layer. A chunk
  may update the progress tracker for its own items only; it MUST NOT edit the implementation
  plan, this worktree doc, or another chunk's tracker rows — **except the named CHUNK-01
  decision-recording exception above** (U1 stack + U9 FS2 in the implementation plan).
- **User-owned branches/commits on `master`** that appear during a chunk's run must be rebased
  onto, not overwritten. If a user commit on `master` conflicts with a chunk's owned file, the
  user wins by default; escalate to `Main` only if the user change appears to contradict the
  chunk's contract.
- **No destructive git operations** in a chunk worktree: no `git push --force` to `master`, no
  `git reset --hard` onto shared refs, no `git clean -fdx` on a worktree that might contain
  user-owned untracked files. `git clean` is only safe inside a chunk's own build/cache dir.

### Escalation path
1. Conflict on an owned file → resolve per the chunk's intent (the chunk owns it).
2. Conflict on an unowned or shared file → STOP, message `Main` and the sibling chunk owner via
   IRC, do not resolve unilaterally.
3. Unexpected modification in a worktree → STOP, message `Main`, do not touch.
4. Discovered mid-flight dependency between two concurrent chunks → STOP both, serialize per §2,
   rebase the dependent one onto the merged first one.
