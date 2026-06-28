# CHUNK-09 Rollout/Rollback Runbook

This runbook is the CHUNK-09 E2E evidence artifact for rollout, rollback, schema migration rollback, and operator evidence capture. It is intentionally stdlib/toolchain-only and references the production file-backed transport exercised by the E2E suite.

## Preflight

1. Build the release candidate from a clean checkout and record the commit SHA.
2. Run every staged CHUNK-09 deterministic E2E gate before rollout:
   - `cargo test --locked --test e2e multi_machine_project_sync_env_hydration_and_ignore_hardening`
   - `cargo test --locked --test e2e queued_pause_resume_keeps_content_off_transport_until_resume`
   - `cargo test --locked --test e2e production_cli_two_machine_conflict_sidecar_preserves_both_edits`
   - `cargo test --locked --test e2e git_metadata_stale_worktree_and_failure_injection_are_observable_and_recoverable`
   - `cargo test --locked --test e2e daemon_process_interruption_restart_recovers_foreground_sync`
   - `cargo test --locked --test e2e edge_cases_platform_matrix_and_soak_cover_cross_os_policy`
   - `cargo test --locked --test e2e performance_budget_10k_and_hydration_are_predeclared_and_enforced`
   - `cargo test --locked --test e2e rollout_rollback_runbook_schema_and_store_restore_drill_are_exercised`
   - `cargo test --locked --test e2e bounded_fuzz_manifest_policy_public_cli_and_daemon_control_parsing`
3. Confirm the predeclared performance budgets before reading measurements:
   - 10k metadata index + sync planning budget: 30,000 ms.
   - 10k structure-diff budget: 10,000 ms.
   - Single lazy hydration budget: 2,000 ms.
   - Rationale: these thresholds are above the stdlib in-memory CI baseline and catch unbounded scans, diff regressions, or hydration regressions without binding release health to one workstation.
4. Capture operator evidence: command, result, date/SHA, transport path, machine IDs, and any output artifact path.

## Rollout

1. Stop foreground daemon invocations on every participating machine with `dropbox-dev sync stop`.
2. Capture a known-good production transport backup before changing binaries or schemas. The product API exercised by the E2E drill is `backup_file_backed_sync_store(source_root, backup_root)`; operationally this is a recursive immutable copy of the file-backed sync-store root to a new backup directory.
3. Record backup evidence: source root, backup root, copied file count, command/operator, and SHA.
4. Install the new binary on one canary machine, then run:
   - `dropbox-dev doctor`
   - `dropbox-dev sync status`
   - `dropbox-dev sync start`
5. Verify another authorized machine can run `dropbox-dev sync start` and converge through the same transport root.
6. Continue the rollout only after the canary has clean status, no plaintext secret evidence in transport capture, and no stale worktree requiring manual action.

## Rollback

1. Stop foreground daemon invocations on every machine with `dropbox-dev sync stop`.
2. Restore the known-good transport backup. The product API exercised by CHUNK-09 is `restore_file_backed_sync_store(backup_root, destination_root)`, which replaces the destination root wholesale so operation logs, manifests, and encrypted payload envelopes return to one consistent state.
3. Reinstall the previously known-good binary.
4. Run `dropbox-dev sync status` on each machine and record status output.
5. Run `dropbox-dev sync recover-stale` only when status reports a stale worktree and the operator has confirmed the remote manifest is the restored known-good version. Do not run destructive Git commands as part of this product rollback.
6. Capture operator evidence: backup root, destination root, restored file count, per-machine status output, and any skipped/recovered paths.

## Schema migration rollback

1. Schema-owning chunks provide namespace migrations. CHUNK-09 consumes that contract and tests rollback with the sync namespace migration.
2. Before applying a schema migration, capture the file-backed sync-store backup described above.
3. Apply the migration with the release binary and record the resulting schema version and product table list.
4. If rollback is required, stop daemon invocations, restore the file-backed sync-store backup, and run the migration runner rollback path to the previous version or `v0` baseline as appropriate.
5. Verify the schema version, product table list, and restored operation log before resuming sync.

## Operator evidence checklist

- Release SHA and binary version.
- Machine IDs and platform pair tested (Mac to Linux, Linux to Linux or same-OS pair where applicable).
- Transport root and backup root.
- Preflight command outputs and pass/fail status.
- 10k index/sync, structure-diff, and hydration measured durations compared with the predeclared budgets.
- Rollback restore copied-file count and post-restore `dropbox-dev sync status` output.
- Any upstream blocker mapping if CHUNK-09 exposes a production defect.
