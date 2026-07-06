# Current Status: Remaining Unchecked Tasks

This file explains, in plain English, why the remaining unchecked items in `plan/todo.md` are not currently being implemented or marked complete.

## Summary

The remaining unchecked tasks are not forgotten. They fall into three broad buckets:

1. **Operating rules or roll-up checklists** — these are guidance or final acceptance lists, not standalone product features.
2. **Explicitly deferred work** — the plan itself says these belong after the MVP or after the vertical slice.
3. **Blocked work** — these depend on missing foundations such as the long-running daemon, local RPC socket, env CLI, key enrollment, conflict preservation, offline replay, or end-to-end sync tests.

All currently doable tasks found during the implementation loop have been implemented, verified, reviewed, committed, and pushed.

## 1. Section 0 working rules

These are development rules, not product features.

Examples:

- Treat `design.md` as the source of truth.
- Do not blindly sync `.git` internals.
- Do not leak secrets or tokens.
- Add tests with every feature.

Plain English: these are team rules for how implementation must be done. They should stay visible as standing constraints instead of being treated like one-time features to mark complete.

## 2. Phase 6.4 chunking

This is about splitting large files into chunks so a small edit does not require re-uploading the whole file.

It is not implemented now because the tracker itself says chunking is optional after the first vertical slice, and whole-file blobs are acceptable for the MVP slice.

Plain English: first make file sync work. Large-file incremental upload is a performance optimization for later.

## 3. Phase 7.4 redaction tests

This wants tests that:

- create a fake secret,
- run env set/list/status/error paths,
- capture logs and CLI output,
- prove the fake secret never appears.

This is blocked because the `fs2 env ...` CLI commands and a structured test logger do not exist yet.

Plain English: we cannot truthfully test that env commands do not leak secrets until the env commands exist.

## 4. Phase 13 hydration, pinning, and cache commands

This includes:

- `fs2 hydrate <path>`
- `fs2 pin`
- `fs2 unpin`
- `fs2 cache status`
- `fs2 cache prune`

These need a daemon or local RPC API to coordinate downloads, cache state, pin state, and pruning.

Plain English: these CLI commands need a background sync service to talk to. Without that service, implementing them would create fake buttons.

## 5. Phase 14 conflict preservation and conflict CLI

The goal is to preserve both versions when two machines edit the same file concurrently.

Remaining work includes:

- keeping local dirty bytes after a stale update is rejected,
- keeping the remote current version at the canonical path,
- writing a deterministic conflict copy,
- adding a conflict DB row,
- showing conflicts in `fs2 status`,
- adding `fs2 conflicts ...` commands.

This is blocked because the client-side conflict artifact flow, conflict DB APIs, status wiring, and hydration/remote-current contract are not complete.

Plain English: the backend can reject stale writes, but the client still needs the full safety workflow that keeps both copies and shows the user what happened.

## 6. Phase 15 offline mode, replay, and reconnect rebase

This includes:

- detecting backend outages,
- marking connection state offline,
- serving already hydrated files while offline,
- queuing writes,
- replaying pending operations after restart,
- rebasing pending local work after reconnect.

This is blocked because there is no complete long-running daemon that owns connection state, restartable sync loops, reconnect sequencing, and conflict preservation.

Plain English: offline behavior needs a continuously running sync brain. That brain is not fully implemented yet.

## 7. Phase 16 env CLI and import

This includes:

- `fs2 env set`
- `fs2 env list`
- `fs2 env unset`
- `fs2 env import`
- `fs2 env materialize`
- `fs2 env exec`

Some lower-level env pieces exist: data models, encryption, backend endpoints, and dotenv parsing. The user-facing CLI, API client env methods, workspace key availability, device enrollment flow, and import rule-writing behavior are not wired end to end.

Plain English: many parts of the secret system exist, but the user-facing `fs2 env ...` workflow is not connected yet.

## 8. Phase 17 Git materialization and submodule acceptance

The goal is for a new machine to run:

```bash
fs2 git materialize <path>
```

and safely reconstruct a functional Git repo from synced worktree data and recorded Git metadata.

This is blocked because:

- `fs2 git materialize` does not exist,
- remote Git metadata is not fully persisted/synced for this flow,
- hydrated worktree assumptions are not complete,
- cross-machine end-to-end sync tests are not in place.

Plain English: the project can inspect Git state, but it cannot yet rebuild `.git` safely on a new machine.

## 9. Phase 19.1 doctor actionable repair output

`fs2 doctor` can detect problems, but some warnings cannot yet include real repair commands.

Examples:

- daemon startup problems cannot point to `fs2 daemon run` or service install yet,
- workspace key problems cannot point to recovery/enrollment commands yet.

Plain English: doctor can diagnose some issues, but the repair commands for those issues do not exist yet.

## 10. Phase 20 daemon and service lifecycle

This includes:

- `fs2 daemon run`,
- opening a local RPC socket,
- initializing local DB state,
- running sync loops,
- running cache eviction loops,
- graceful shutdown,
- macOS LaunchAgent support,
- Linux systemd user service support.

This is a foundational dependency for many later features. It is blocked on the local RPC protocol and daemon ownership/runtime design.

Plain English: this is the background service. Many features depend on it, but it is a large foundation and should not be faked with a placeholder.

## 11. Phase 21 end-to-end test matrix

These tests cover:

- two-client happy path,
- conflict path,
- generated path behavior,
- env path behavior,
- Git path behavior.

They require real daemon/client sync, hydration, conflicts, offline mode, env exec, git materialization, and generated-path behavior.

Plain English: these are final system tests. Several features they need are not fully implemented yet, so the tests cannot be truthful yet.

## 12. Phase 22 performance pass

This includes:

- 100k-file metadata scale tests,
- cold manifest sync measurement,
- `readdir` latency measurement,
- hydration throughput,
- upload storm simulation,
- daemon CPU and memory checks.

This is deferred until the functional daemon, hydration path, upload queue, cache loop, and E2E paths exist.

Plain English: performance work should measure the real system. The real system is not complete enough yet.

## 13. Phase 23.1 remaining token tasks

Completed pieces include:

- tokens are stored in keychain,
- logout clears tokens,
- revoked devices are rejected.

Remaining pieces include:

- proving all logs redact tokens,
- automatic refresh-token flow,
- full token leakage tests.

This is blocked because there is no complete structured app/daemon logger, no test logger, and no refresh endpoint/client flow.

Plain English: token storage is safe, but the broader logging and refresh-token behavior still needs missing infrastructure.

## 14. Phase 23.2 workspace key handling

The goal is to ensure workspace keys are stored safely, private keys never go to the backend, revoked devices behave correctly, and devices without keys cannot decrypt files or env values.

Some key-store primitives exist, but the product flow is incomplete:

- workspace key generation/storage during workspace creation,
- device key enrollment,
- key recovery,
- revocation semantics,
- key-unavailable status reporting.

Plain English: the lock-and-key building blocks exist, but the full user/device lifecycle around those keys is not finished.

## 15. Phase 23.3 local RPC socket permissions

The remaining item is to restrict the local RPC socket to the current user.

This is blocked because the production local RPC socket does not exist yet.

Plain English: we cannot secure the socket permissions before there is a socket to secure.

## 16. Phase 24 documentation

This includes install, workflow, and safety guides.

It is deferred because several behaviors are still missing or unstable:

- daemon/service lifecycle,
- hydration and pinning,
- cache pruning,
- env commands,
- conflict handling,
- git materialization,
- device revocation/key recovery.

Plain English: documentation should describe the real product. Writing full user docs now would either be inaccurate or describe features that do not exist yet.

## 17. Phase 25 dogfood

This means using FS2 for a real project across machines/containers with generated directories, env secrets, and Git materialization.

It is blocked until the MVP sync, conflict, offline, env, generated-directory, Git, cache, and safety paths work end to end.

Plain English: the project is not ready to be used as its own daily sync tool yet.

## 18. Phase 26 public MVP polish

This includes:

- better CLI output,
- installer packages,
- release binaries,
- checksums,
- versioning,
- migrations.

It is deferred until the command/API/schema surface is stable.

Plain English: this is packaging and polish. It should happen after the engine and command set stop moving.

## 19. Phase 28 implementation-order checklist

This is a high-level roadmap, not a separate product feature.

Several early entries are already satisfied through their detailed sections. Later entries remain blocked by conflict/offline/env/git/cache/docs/dogfood work.

Plain English: this is an index of the plan. We should not mark it independently of the detailed work it points to.

## 20. Phase 29 definition of done

This is the MVP graduation checklist.

It depends on:

- two-client sync,
- lazy hydration,
- cross-client edits,
- conflict preservation,
- offline replay,
- env secret injection,
- Git materialization,
- cache pruning safety,
- docs,
- dogfood.

Plain English: this is the diploma. The project cannot receive it until the remaining major features are actually finished.

## 21. Phase 30 post-MVP roadmap

This includes:

- team workspaces,
- cloud agent mode,
- artifact cache,
- editor integration,
- Windows support,
- source-control replacement research.

It is deferred by its own heading: Post-MVP.

Plain English: these are future-version ideas. They should not distract from finishing the MVP.

## Final plain-English takeaway

The project has completed all currently safe, concrete implementation work found during the loop. The rest is not a random backlog; it is blocked or deferred because it depends on missing foundations, especially:

- `fs2d` daemon,
- local RPC socket,
- env CLI,
- workspace key enrollment/recovery,
- conflict preservation,
- offline replay,
- Git materialization,
- full end-to-end sync tests.

Until those foundations are designed and implemented, the remaining tasks should stay unchecked rather than being marked complete prematurely.
