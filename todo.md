# FS2 — Implementation Checklist

Companion to `design.md` (the spec — section references like **§11** point there). Work top-to-bottom; milestones are ordered by dependency. If `design.md` and this file disagree, `design.md` wins — fix this file in the same PR.

## Working agreements (read first)

- [ ] means not started; [x] means done **and** its acceptance criteria pass. Check items off in the PR that completes them. If you split or add tasks, keep IDs stable and append new ones (e.g. `T2.14a`).
- Definition of done for every task: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace` all pass; new behavior has tests; public items have doc comments.
- Keep commits scoped to one task or a tight group; reference task IDs in commit messages (e.g. `feat(sync): conflict copies (T2.9)`).
- When you make a mistake and fix it, or learn something reusable (crate quirk, API gotcha), append it to the `Lessons` section of the repo-root `CLAUDE.md`.
- Env var templates are named `env.example` (never `.env.example`).
- Don't add dependencies beyond §4's table without recording the reason in `CLAUDE.md` Lessons.
- Platform: develop/test on Linux and macOS only (§1.2). FUSE work is Linux-only behind the `fuse` feature.

---

## M0 — Skeleton (workspace, binaries boot, CI green)

- [ ] **T0.1 Workspace scaffolding** — Root `Cargo.toml` with `[workspace]` members for all crates in §5 and `[workspace.dependencies]` pinning every dep from §4. `rust-toolchain.toml` pinning current stable. Each crate compiles (may be near-empty `lib.rs`/`main.rs`). `crates/fs2-rules` already has a directory — give it its `Cargo.toml`.
  *Accept:* `cargo build --workspace` succeeds on a clean checkout.
- [ ] **T0.2 Repo hygiene** — `.gitignore` (`/target`, `*.db`, `.env`, `!env.example`), `CLAUDE.md` with build/test commands + empty `## Lessons` section, `env.example` containing `FS2_ADMIN_TOKEN=changeme` + commented server settings (§20.2).
  *Accept:* files exist; `env.example` matches §20.2 keys.
- [ ] **T0.3 CI** — `.github/workflows/ci.yml`: fmt check, clippy `-D warnings`, `cargo test --workspace`, on `ubuntu-latest` and `macos-latest`, with cargo cache.
  *Accept:* workflow passes on the PR.
- [ ] **T0.4 `fs2-core` foundations** — `DeviceId`, `FolderId` (UUID v4 newtypes), `EntryKind`, `RelPath` newtype enforcing §6 invariants (constructor rejects `..`, absolute, `\0`, non-NFC input gets normalized; unit tests incl. NFD→NFC), `VersionVector` with `increment/dominates/concurrent` (§7.3) + full algebra unit tests, shared error types.
  *Accept:* unit tests cover dominance/concurrency truth table (≥8 cases) and RelPath rejection cases.
- [ ] **T0.5 Config & dirs** — `fs2-core::paths` using `directories` (§4): config/data/runtime dirs, created on demand. Client `config.toml` load/save (§20.1); server config load with env-var overrides (§20.2).
  *Accept:* round-trip tests with tempdir HOME.
- [ ] **T0.6 Logging setup** — `tracing-subscriber` init helper (env filter, optional JSON, file layer stub) used by all three binaries.
  *Accept:* `fs2d --help`, `fs2 --help`, `fs2-server --help` run and log at info.
- [ ] **T0.7 Daemon+CLI IPC skeleton** — `fs2d` serves HTTP-over-UDS (§19) with `/status` returning `{version, folders: []}`; socket file mode 0600. `fs2 status` (clap skeleton with all §18 subcommands stubbed as "not implemented", exit 1) connects and renders it; exit 3 when daemon unreachable (§18).
  *Accept:* integration test spawns `fs2d`, runs `fs2 status` and `fs2 status --json`, asserts output + exit codes.
- [ ] **T0.8 Server skeleton** — `fs2-server` axum app: `/healthz`, `X-FS2-Api-Version` header middleware, error-body shape helper (§9), bearer-auth middleware stub, TLS/`--insecure` startup rule (§17 transport; refuse non-loopback plaintext).
  *Accept:* tests: healthz 200; non-loopback bind without TLS/--insecure refuses to start; error shape matches §9.

## M1 — Local pipeline (rules, chunking, store, scanner; no network)

- [ ] **T1.1 `fs2-rules`: gitignore matcher** — Pattern parsing + matching per §12 semantics (last-match-wins, `!`, `**`, dir-only `/` suffix, anchoring). May build on `globset` for glob compilation but layering/negation logic is ours.
  *Accept:* table-driven suite porting the documented gitignore examples (≥30 cases) passes.
- [ ] **T1.2 `fs2-rules`: layered RuleSet + explain** — Sources & precedence per §12 (built-ins incl. unoverridable `.env` guards and `.fs2/`; global file; `fs2.toml` patterns; hierarchical `.fs2ignore`). `matches()` + `explain()` API.
  *Accept:* tests: `node_modules/x` ignored by default; `!node_modules/` in `.fs2ignore` un-ignores; `!.env` does NOT un-ignore; explain names rule + source layer.
- [ ] **T1.3 `fs2-chunk`** — FastCDC (64K/256K/1M, §8), BLAKE3 full-file + per-chunk hashing, zstd compress/decompress helpers with hash verification on decode.
  *Accept:* determinism test (same input → same chunk list), boundary-shift test (edit in middle changes O(1) chunks), empty-file case, corrupt-zstd rejected.
- [ ] **T1.4 `fs2-store`: SQLite layer** — Embedded numbered migrations + `schema_version`; client schema per §7.1; WAL mode; typed query API (upsert entry, mark dirty, list dirty, get/set meta, folder CRUD, entry_chunks replace).
  *Accept:* migration idempotency test; CRUD round-trips; dirty index query uses the partial index (assert via EXPLAIN or just correctness).
- [ ] **T1.5 `fs2-store`: CAS** — `cas/<hh>/<hash>.zst` layout (§7.1), put (write tmp + rename), get, has, LRU eviction by atime honoring `cas_cache_gib` and the never-evict-dirty rule.
  *Accept:* put/get/has round-trip; eviction test with tiny cap; dirty-referenced chunk survives eviction.
- [ ] **T1.6 `fs2-scan`: full scanner** — Walk per §10.1: ignore-before-descend (never enters `node_modules`), stat compare vs index, rehash on mismatch, VV increment + dirty marking, deletion detection, NFC normalization at boundary, symlink/exec-bit capture (§21.1, §21.3).
  *Accept:* tempdir tests: initial scan indexes tree; touch-without-change (mtime bump, same content) causes no VV bump; content edit bumps VV once; deleting file produces local tombstone; `node_modules` contents absent from index.
- [ ] **T1.7 Daemon: folder add + status wiring** — `fs2 add <path>` (local-only for now: creates folder row with placeholder folder_id, runs initial scan), `fs2 ls`, `fs2 status [path]` with real counts (synced/dirty/…); overlap/nested-root refusal (§21.9).
  *Accept:* integration: add tempdir with sample tree incl. `node_modules` and `.env` → status shows correct counts, ignored stuff excluded; adding nested path refused.

## M2 — Sync (the "Dropbox works" milestone)

Server side:
- [ ] **T2.1 Server DB + devices** — Server schema (§7.2) with migrations; `POST /v1/devices/register` (admin token; issues token, stores BLAKE3 hash); bearer auth middleware validating against `devices` (constant-time), `last_seen` update.
  *Accept:* register→authed request 200; bad/absent token 401; admin-token gate enforced.
- [ ] **T2.2 Folder endpoints** — `GET/POST /v1/folders`, `DELETE` (admin token) per §9.1; unique name handling.
  *Accept:* CRUD tests incl. duplicate-name 400 and delete auth.
- [ ] **T2.3 Chunk endpoints** — `POST /v1/chunks/has` (batch ≤1000), `PUT /v1/chunks/{hash}` (verify hash after decompress, 64 MiB cap, idempotent re-PUT), `GET /v1/chunks/{hash}` streaming; server CAS reusing `fs2-store` CAS.
  *Accept:* round-trip; wrong-hash PUT rejected 400 and nothing stored; oversized 413; has() correctness.
- [ ] **T2.4 Manifest push** — `POST /v1/folders/{id}/entries` implementing §9.3 rules 1–5 exactly (accept/stale/conflict/missing_chunks), per-batch transaction, seq assignment, rel_path validation (§17 path safety), batch cap 500.
  *Accept:* unit tests per outcome incl. concurrent-VV → conflict returns `server_entry`; missing chunk → `missing_chunks` and row untouched; seq strictly increases.
- [ ] **T2.5 Manifest pull** — `GET .../manifest?since_seq=N&limit=` per §9.2 with pagination loop contract; tombstones included.
  *Accept:* delta test (push 3, pull since mid-seq gets 1); pagination test with limit=2.
- [ ] **T2.6 SSE events** — `/v1/events` per §9.4 (folder + vault events, 25 s heartbeat, fan-out to all connected devices).
  *Accept:* test client receives event after a push; heartbeat comments arrive; disconnect doesn't wedge server.
- [ ] **T2.7 Tombstone GC** — Server task purging tombstones older than 30 days (§11.2), runs daily + at startup.
  *Accept:* unit test with injected clock.

Client side:
- [ ] **T2.8 API client** — `fs2-daemon::api`: typed reqwest client for §9 endpoints, bearer auth, retry/backoff policy (§19), SSE consumer with jittered reconnect + unconditional-pull-on-reconnect.
  *Accept:* tests against in-process server (from fs2-testkit seed, see T2.12); reconnect test with server restart.
- [ ] **T2.9 Sync engine: pull/apply** — §11.1 pull phase + §11.4 atomic apply engine (tmp under `.fs2/tmp`, hash verify, exec bit, rename, fsync, stat-guard) + §11.2 delete-to-trash + dir/symlink apply + free-space check (§21.8).
  *Accept:* unit tests for apply atomicity (crash-sim: tmp left behind → cleaned on start), stat-guard abort, trash-not-unlink.
- [ ] **T2.10 Sync engine: push** — Dirty-entry upload (chunk → has → PUT ≤4 parallel → push batch) and outcome handling per §11.1 (accepted/stale/conflict/missing_chunks paths).
  *Accept:* covered via T2.12 scenarios + unit test for missing_chunks re-upload-once logic.
- [ ] **T2.11 Conflict copies** — §11.3: rename local to `stem.fs2-conflict-<ts>-<device><.ext>` (name collision → add `-2`), adopt server version, both kinds (file/dir/symlink mismatch), delete-vs-modify resurrection; `fs2 conflicts` + `resolve --keep mine|theirs`.
  *Accept:* scenario tests below + naming unit tests (dotfiles, no-extension, long stems).
- [ ] **T2.12 `fs2-testkit`** — In-process server + N daemons harness per §23 (tempdirs, real HTTP on random port, `scan_now()` deterministic mode), helpers `write/sync_all/assert_tree_eq/assert_conflict`.
  *Accept:* harness boots 1 server + 2 devices in <5 s in CI.
- [ ] **T2.13 Core scenarios (integration)** — Named tests per §23: create/modify/delete propagation A↔B; offline-both-edit → exactly one conflict copy + convergence; delete-vs-modify → resurrection; ignore defaults (assert `node_modules` never in server manifest); kill-daemon-mid-sync → restart → converge; tombstone propagation.
  *Accept:* all pass in CI on both OSes.
- [ ] **T2.14 Watcher** — `notify` + debouncer per §10.2 wired into daemon: targeted rescan of affected paths, self-inflicted-event suppression set, overflow → full rescan; 5-min poll timer + 60-min rescan timer.
  *Accept:* integration test (Linux CI): touch file → other device sees it via testkit without manual `scan_now`, < 5 s. (Keep timing generous; watcher is accelerant not correctness — §10.)
- [ ] **T2.15 CLI completion for M2** — Real `fs2 setup` (register device, write config), `fs2 add` (create server folder), `clone`, `sync`, `pause/resume`, `rm [--delete-remote]` with confirm, `ls/status` showing sync state + backoff reason; exit codes per §18.
  *Accept:* end-to-end smoke test in testkit driving the actual binaries (`assert_cmd`): setup → add on A → clone on B → edit → sync → identical trees.
- [ ] **T2.16 Daemon service install** — `fs2 daemon install|start|stop|status`: launchd plist (macOS) / systemd user unit (Linux) generation per §19.
  *Accept:* unit tests on generated unit/plist content; manual-run doc snippet in README.

## M3 — Developer ergonomics (ignore UX, hooks, git safety)

- [ ] **T3.1 `fs2.toml` loading + sync-awareness** — Parse §20.3 in daemon; ignore-layer wiring (already in rules from T1.2 — this task wires live reload when `fs2.toml`/`.fs2ignore` change via watcher: rebuild RuleSet, rescan folder).
  *Accept:* integration: add pattern to `.fs2ignore` on A → file stops syncing on both; removing pattern resyncs.
- [ ] **T3.2 `fs2 ignore check`** — CLI surface for `RuleSet::explain` (§12): prints decision, matching rule, source layer, for `--json` too.
  *Accept:* snapshot tests for the three layer types + built-in guard case.
- [ ] **T3.3 Hooks engine** — §13: `on_clone` + `on_change` (trigger paths, debounce), execution in folder root with captured logs to `<data_dir>/logs/hooks/`, 10-min timeout, single retry, status surfacing.
  *Accept:* integration: clone folder with `on_clone = ["touch installed.marker"]` (approved) → marker exists; on_change fires once per debounce window after remote apply of `package.json`.
- [ ] **T3.4 Hook approval model** — blake3(fs2.toml) approval store per device; unapproved → skip + warn; `fs2 hooks approve` shows diff (old vs new) and records; `fs2 hooks run` manual trigger.
  *Accept:* security test: freshly-cloned folder with hooks does NOT execute anything before approval (assert no side effect); content change invalidates approval.
- [ ] **T3.5 Git safeguards** — §14: `.git` lock detection pauses applies under that repo (2 s recheck, 10-min warn); built-in excludes for `.git/objects/tmp_*` and `*.lock`; `.git`-internal conflict copies quarantined to `.fs2/git-conflicts/`.
  *Accept:* integration: create `.git/index.lock` on B → remote applies under repo held, others proceed; remove lock → applies flow; conflict inside `.git/refs/...` lands in quarantine not `.git`.
- [ ] **T3.6 `fs2 doctor`** — Checks per §18/§21: server reachability+TLS, daemon health, watcher backend + inotify watch budget vs tree size, xattr support, case-insensitivity collision scan, ignore-drift between local rules and server manifest, version skew (client vs `X-FS2-Api-Version`), symlink-escape warnings.
  *Accept:* each check has a unit/integration test with a forced-failure fixture; human + `--json` output.

## M4 — Vault (env var sync)

- [ ] **T4.1 `fs2-vault` crypto core** — Account age X25519 keypair gen/load (`key.age`, 0600), encrypt-to-pubkey/decrypt helpers, pubkey upload to server meta at setup; `fs2 key export|import` (§15, §18).
  *Accept:* round-trip tests; import on second identity decrypts first identity's ciphertext; key file perms asserted.
- [ ] **T4.2 Server vault endpoints** — §9.6 GET/PUT with VV outcome semantics shared with manifest push (extract shared logic from T2.4), `vault` SSE events, shared folder seq counter.
  *Accept:* endpoint tests incl. conflict outcome; server-side test asserting stored bytes are valid age ciphertext and do NOT contain plaintext markers.
- [ ] **T4.3 Client vault flow** — `fs2 env link|unlink|ls|diff`; daemon watches linked files (they're ignore-guarded as entries), encrypt+PUT on change, decrypt+atomic-write (0600) on remote change, vault conflict copies per §15.
  *Accept:* testkit scenario: link `.env` on A → appears on B with 0600 and same content; concurrent edits → conflict copy; `.env` never appears in folder manifest (assert server-side); `env diff` output test.

## M5 — Structure mode & hydration

- [ ] **T5.1 Placeholders** — §16: structure-mode folders materialize remote files as 0-byte + xattr placeholders (`state='placeholder'`); scanner treats content-bearing/xattr-less placeholders as user data (dirty); xattr-support gate (§21.5); status counts placeholders + true sizes.
  *Accept:* testkit: B in structure mode gets placeholders with correct metadata; editing a placeholder locally converts it to dirty real content and syncs back.
- [ ] **T5.2 `fs2 hydrate|evict`** — Recursive variants; evict only when synced + hash matches server (§16); mode switching `full↔structure` per folder (hydrates all on →full).
  *Accept:* hydrate fetches content matching hash; evict-dirty refused with clear error; -r on directory works.
- [ ] **T5.3 FUSE mount (Linux, feature `fuse`)** — `fs2 mount|umount`: read-write passthrough over folder root; `open()` on placeholder blocks + hydrates then passes through; writes hit backing store (watcher picks them up).
  *Accept:* Linux-only optional CI job: mount, `cat` a placeholder → hydrated content; write through mount → syncs to other device; clean unmount.
- [ ] **T5.4 macOS stance** — Docs (README + `fs2 doctor` note): macOS structure mode is placeholder+explicit-hydrate only; default macOS folders to `full` (§16.3).
  *Accept:* doctor on macOS reports the limitation; docs section exists.

## M6 — Hardening & release

- [ ] **T6.1 Property-based convergence tests** — `proptest` per §23: random edit/delete sequences on 2–3 devices, random sync interleavings → identical trees, no written content lost (exists at path or as conflict copy).
  *Accept:* 256 cases in CI within time budget; shrinker produces readable minimal cases.
- [ ] **T6.2 Server chunk GC** — Nightly + on-demand job: mark-and-sweep unreferenced chunks vs manifest+live tombstones (§7.2); `--gc-now` flag.
  *Accept:* test: orphaned chunk removed, referenced chunk kept, chunk referenced only by tombstone kept until tombstone GC'd.
- [ ] **T6.3 Limits & rate limiting** — Body caps (10 MiB manifest JSON, 64 MiB chunk — verify from T2.3/T2.4), per-device token bucket 100 req/s (§17).
  *Accept:* 413/429 tests.
- [ ] **T6.4 Metrics + log rotation** — Server `/metrics` + `/healthz` (§22); daemon file logging with 5×10 MiB rotation.
  *Accept:* metrics endpoint exposes counters named in §22; rotation test with small cap override.
- [ ] **T6.5 Config reload** — Daemon live-reload of non-structural settings (§19).
  *Accept:* change log level in config.toml → takes effect without restart (integration).
- [ ] **T6.6 Release packaging** — GitHub Actions release workflow: build `fs2`/`fs2d` for macOS arm64 + Linux x86_64/arm64, `fs2-server` for Linux; checksums; version from git tag; `--version` embeds it.
  *Accept:* dry-run workflow produces artifacts on a tag push to a test tag.
- [ ] **T6.7 Docs pass** — README rewrite: quick start (server up via one binary + `env.example`, two-machine setup walkthrough), git-coexistence rule of thumb (§14), tombstone/offline-30-day caveat (§11.2), macOS structure-mode limitation, security model summary (§17). Man-page-style `docs/cli.md` generated or hand-written from §18.
  *Accept:* a fresh reader can go from zero to two synced machines following README only (verify by replaying steps in testkit-like manual run).
- [ ] **T6.8 Final audit vs design** — Sweep §21 edge cases one by one and link each to a test; anything untested gets a test or a documented deviation in this file + `design.md` update.
  *Accept:* table added at the bottom of this file mapping §21.1–§21.11 → test names.

---

## Deviations log

Record any intentional departure from `design.md` here (what, why, and the design.md section updated).

| Date | Task | Deviation | design.md updated? |
|------|------|-----------|--------------------|
