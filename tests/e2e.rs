use dropbox_dev::catalog::{diff_manifests, StructureChange, TreeEntry, TreeManifest};
use dropbox_dev::cli;
use dropbox_dev::env::{EnvReplica, StdlibTestKeyProvider, ENV_REDACTED_VALUE};
use dropbox_dev::foundation::{
    Architecture, InMemoryMigrationStore, MachineId, MachineIdProvenance, MigrationRunner,
    OsFamily, Platform, PlatformCapabilities, BASELINE_SCHEMA_VERSION,
};
use dropbox_dev::policy::{Action, PlatformPin, Policy, SYNCIGNORE_FILE_NAME};
use dropbox_dev::sync::{
    app_scoped_machine_id, backup_file_backed_sync_store, replay_operation_log,
    restore_file_backed_sync_store, store_blob_id_for_content, sync_content_hash,
    sync_initial_migration, ConvergenceAction, ConvergenceEngine, EndpointSecurityConfig,
    FileBackedSyncStore, OperationDraft, OperationKind, OperationRecord, PayloadEnvelope,
    PayloadKind, PolicyBranch, SharedSecret, TransportMode, SYNC_CONFLICT_SIDECAR_SUFFIX,
};
use dropbox_dev::vfs::{HydrationRequest, Hydrator, VfsAccessError, VfsMount, VfsSyncMetadata};
use dropbox_dev::watcher::{InMemoryTree, LocalIndexer, LocalTreeEntry, PolicyMetadata, SnapshotEntry};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const TEN_K_FILE_COUNT: usize = 10_000;
const TEN_K_STRUCTURE_INDEX_AND_SYNC_BUDGET_MS: u128 = 30_000;
const TEN_K_STRUCTURE_DIFF_BUDGET_MS: u128 = 10_000;
const HYDRATION_LATENCY_BUDGET_MS: u128 = 2_000;
const INTERRUPTED_SYNC_FILE_COUNT: usize = 2_048;
const PERFORMANCE_BUDGET_RATIONALE: &str = "CHUNK-09 CI budgets are deliberately predeclared above the stdlib in-memory baseline on commodity CI: 10k metadata index+sync planning must stay under 30s, 10k structure diff must stay under 10s, and a single cached-harness hydration must stay under 2s. These thresholds catch accidental unbounded behavior without depending on a specific workstation.";
const FUZZ_SEED: u64 = 0xC09E_2E5E_ED20_2609;
const FUZZ_CASES: usize = 128;
const ENV_SECRET: &str = "CHUNK09_SUPER_SECRET_DO_NOT_LEAK";

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[test]
fn multi_machine_project_sync_env_hydration_and_ignore_hardening() {
    let harness = Harness::new("project-env-hydration", &["mac-mini", "linux-box"]);
    let mac = harness.machine("mac-mini");
    let linux = harness.machine("linux-box");

    mac.write_file(
        SYNCIGNORE_FILE_NAME,
        b"ignored.log\nsecrets.local\nignored-dir/\n",
    );
    mac.write_file("src/app.rs", b"fn main() { println!(\"hello\"); }\n");
    mac.write_file("src/lazy.txt", b"lazy remote contents\n");
    mac.write_file("src/adjacent.txt", b"adjacent content syncs\n");
    mac.write_file("ignored.log", b"ignored local log\n");
    mac.write_file("ignored-dir/private.txt", b"ignored private dir\n");
    mac.write_file("secrets.local", b"machine local secret file\n");
    mac.write_file("node_modules/pkg/index.js", b"module must rebuild locally\n");

    expect_success(mac.run(["init"]));
    expect_success(linux.run(["init"]));
    let mac_start = expect_success(mac.run(["sync", "start"]));
    assert_output_contains(&mac_start.stdout, "sync_start=foreground-complete");
    assert_output_contains(&mac_start.stdout, "foreground_work=ok");

    let placeholders = expect_success(linux.run(["hydrate"]));
    assert_output_contains(&placeholders.stdout, "hydrate_status=ok");
    assert_output_contains(&placeholders.stdout, "manifest_source=remote");
    assert_output_contains(&placeholders.stdout, "placeholder_count=");

    let lazy = expect_success(linux.run(["hydrate", "src/lazy.txt"]));
    assert_output_contains(&lazy.stdout, "hydrate_status=ok");
    assert_output_contains(&lazy.stdout, "manifest_source=remote");
    assert_output_contains(&lazy.stdout, "path=src/lazy.txt");
    assert_output_contains(&lazy.stdout, "latency=remote-fetch");
    assert_output_contains(&lazy.stdout, "fetched=true");
    assert_output_contains(&lazy.stdout, "path_hydration_status=hydrated");
    assert!(
        !linux.path("src/lazy.txt").exists(),
        "CLI hydrate should prove lazy remote read without materializing the project file"
    );

    let linux_start = expect_success(linux.run(["sync", "start"]));
    assert_output_contains(&linux_start.stdout, "sync_start=foreground-complete");
    assert_output_contains(&linux_start.stdout, "foreground_work=ok");
    assert_eq!(linux.read_file("src/app.rs"), b"fn main() { println!(\"hello\"); }\n");
    assert_eq!(linux.read_file("src/lazy.txt"), b"lazy remote contents\n");
    assert_eq!(linux.read_file("src/adjacent.txt"), b"adjacent content syncs\n");
    assert_eq!(
        linux.read_file(SYNCIGNORE_FILE_NAME),
        b"ignored.log\nsecrets.local\nignored-dir/\n"
    );
    assert!(!linux.path("ignored.log").exists());
    assert!(!linux.path("ignored-dir/private.txt").exists());
    assert!(!linux.path("secrets.local").exists());
    assert!(!linux.path("node_modules/pkg/index.js").exists());

    let operation_paths = harness.operation_paths(mac);
    for forbidden in [
        "ignored.log",
        "ignored-dir/private.txt",
        "secrets.local",
        "node_modules/pkg/index.js",
    ] {
        assert!(
            !operation_paths.iter().any(|path| path == forbidden),
            "forbidden path `{forbidden}` reached operation log: {operation_paths:?}"
        );
    }

    let store_a = harness.store(mac);
    let store_b = harness.store(linux);
    let mut provider_a = StdlibTestKeyProvider::with_key(
        mac.machine_id.clone(),
        "v1",
        b"shared deterministic env key",
        1,
    )
    .unwrap();
    let mut provider_b = StdlibTestKeyProvider::with_key(
        linux.machine_id.clone(),
        "v1",
        b"shared deterministic env key",
        1,
    )
    .unwrap();
    let mut env_a = EnvReplica::new(&harness.project_id, mac.machine_id.clone(), provider_a.clone()).unwrap();
    let publish = env_a
        .set_shared(&store_a, "API_TOKEN", ENV_SECRET, 42)
        .unwrap();
    assert_eq!(publish.redacted_value.to_string(), ENV_REDACTED_VALUE);

    let mut env_b = EnvReplica::new(&harness.project_id, linux.machine_id.clone(), provider_b.clone()).unwrap();
    let ingest = env_b.ingest_from_transport(&store_b).unwrap();
    assert_eq!(ingest.fetched_payloads, 1);
    assert_eq!(ingest.applied_records, 1);
    let materialized = env_b.materialize().unwrap();
    assert_eq!(materialized.launcher_environment.get("API_TOKEN").map(String::as_str), Some(ENV_SECRET));
    assert!(materialized.redacted_session_export.contains(ENV_REDACTED_VALUE));
    assert!(!materialized.redacted_session_export.contains(ENV_SECRET));

    provider_a.rotate_key("v2", b"rotated deterministic env key", 2).unwrap();
    provider_b.rotate_key("v2", b"rotated deterministic env key", 2).unwrap();
    assert_transport_does_not_contain(&harness.transport_root, ENV_SECRET.as_bytes());
}

#[test]
fn queued_pause_resume_keeps_content_off_transport_until_resume() {
    let harness = Harness::new("pause-resume", &["machine-a", "machine-b"]);
    let machine_a = harness.machine("machine-a");
    let machine_b = harness.machine("machine-b");

    expect_success(machine_a.run(["init"]));
    expect_success(machine_b.run(["init"]));
    let pause = expect_success(machine_a.run(["sync", "pause"]));
    assert_output_contains(&pause.stdout, "sync_pause=paused");
    assert_output_contains(&pause.stdout, "sync_paused=true");

    machine_a.write_file("queued/01.txt", b"queued one\n");
    machine_a.write_file("queued/02.txt", b"queued two\n");
    let queued = expect_success(machine_a.run(["sync", "start"]));
    assert_output_contains(&queued.stdout, "sync_start=foreground-incomplete");
    assert_output_contains(&queued.stdout, "foreground_work=queued");
    assert_output_contains(&queued.stdout, "queued_operations=2");
    assert_transport_does_not_contain(&harness.transport_root, b"queued one");
    assert_transport_does_not_contain(&harness.transport_root, b"queued two");
    assert!(harness.operation_paths(machine_a).is_empty());

    let resume = expect_success(machine_a.run(["sync", "resume"]));
    assert_output_contains(&resume.stdout, "sync_resume=resumed");
    assert_output_contains(&resume.stdout, "sync_paused=false");
    assert_output_contains(&resume.stdout, "queued_operations=0");
    let operations = harness.store(machine_a).load_operation_log().unwrap();
    let queued_ops = operations
        .iter()
        .filter(|operation| operation.kind == OperationKind::PutContent)
        .filter(|operation| operation.path.starts_with("queued/"))
        .collect::<Vec<_>>();
    assert_eq!(queued_ops.len(), 2);
    assert!(queued_ops.windows(2).all(|pair| pair[0].sequence < pair[1].sequence));

    let b_start = expect_success(machine_b.run(["sync", "start"]));
    assert_output_contains(&b_start.stdout, "sync_start=foreground-complete");
    assert_eq!(machine_b.read_file("queued/01.txt"), b"queued one\n");
    assert_eq!(machine_b.read_file("queued/02.txt"), b"queued two\n");
}

#[test]
fn production_cli_two_machine_conflict_sidecar_preserves_both_edits() {
    let harness = Harness::new("cli-conflict-sidecar", &["machine-a", "machine-b"]);
    let machine_a = harness.machine("machine-a");
    let machine_b = harness.machine("machine-b");

    expect_success(machine_a.run(["init"]));
    expect_success(machine_b.run(["init"]));
    machine_b.write_file("src/conflict.txt", b"edit from machine b\n");
    // Conflict policy is last-writer-wins; write A after B so the published remote edit is the winner.
    std::thread::sleep(Duration::from_millis(20));
    machine_a.write_file("src/conflict.txt", b"edit from machine a\n");
    let machine_a_push = expect_success(machine_a.run(["sync", "start"]));
    assert_output_contains(&machine_a_push.stdout, "sync_start=foreground-complete");
    assert_output_contains(&machine_a_push.stdout, "foreground_work=ok");
    let machine_b_conflict = expect_success(machine_b.run(["sync", "start"]));
    assert_output_contains(&machine_b_conflict.stdout, "sync_start=foreground-incomplete");
    assert_output_contains(&machine_b_conflict.stdout, "foreground_work=unsupported");
    assert_output_contains(&machine_b_conflict.stdout, "foreground_unsupported_actions=1");
    assert_output_contains(&machine_b_conflict.stdout, "unsupported_actions_observed=1");
    assert_output_contains(&machine_b_conflict.stdout, "stale_worktree=stale");
    assert_output_contains(&machine_b_conflict.stdout, "recovery_command=sync recover-stale");

    assert_eq!(machine_a.read_file("src/conflict.txt"), b"edit from machine a\n");
    assert_eq!(machine_b.read_file("src/conflict.txt"), b"edit from machine b\n");

    let store = harness.store(machine_b);
    let operations = store.load_operation_log().unwrap();
    let remote_manifest = latest_manifest_from_operations(&store, &operations);
    let remote_entry = remote_manifest
        .entries
        .iter()
        .find(|entry| entry.path == "src/conflict.txt")
        .expect("remote manifest should retain machine A's published edit");
    let remote_hash = remote_entry
        .content_hash
        .as_deref()
        .expect("remote conflict entry should have a content blob");
    assert_eq!(
        store.fetch_content_blob(remote_hash).unwrap(),
        b"edit from machine a\n",
        "the remote winner blob must remain recoverable after the second machine detects a conflict"
    );

    let policy = Policy::new();
    let platform = test_platform(&machine_b.machine_id, OsFamily::Linux, Architecture::X86_64);
    let snapshot = LocalIndexer::new(&harness.project_id, policy.clone(), platform.clone())
        .index_project_root(&machine_b.project_root)
        .unwrap();
    let replay = replay_operation_log(&operations);
    let next_sequence = operations
        .iter()
        .map(|operation| operation.sequence)
        .max()
        .unwrap_or(0)
        + 1;
    let conflict_plan = ConvergenceEngine::new(&machine_b.machine_id, platform).plan_snapshot_with_remote_state(
        &snapshot,
        Some(&remote_manifest),
        Some(&replay),
        true,
        next_sequence,
        &policy,
    );
    let sidecar = conflict_plan
        .actions
        .iter()
        .find_map(|action| match action {
            ConvergenceAction::ConflictSidecar {
                path,
                sidecar_path,
                winner_machine_id,
                loser_machine_id,
                loser_content_hash,
                ..
            } if path == "src/conflict.txt" => Some((
                sidecar_path,
                winner_machine_id,
                loser_machine_id,
                loser_content_hash,
            )),
            _ => None,
        })
        .expect("overlapping CLI edits must surface a conflict sidecar action");
    assert!(
        sidecar.0.contains(SYNC_CONFLICT_SIDECAR_SUFFIX),
        "sidecar path should include the public conflict suffix: {}",
        sidecar.0
    );
    assert_ne!(sidecar.1, sidecar.2, "winner and loser machines must be distinct");
    assert!(
        !sidecar.3.is_empty(),
        "conflict sidecar must retain the losing content hash for manual recovery"
    );
}

#[test]
fn git_metadata_stale_worktree_and_failure_injection_are_observable_and_recoverable() {
    let harness = Harness::new("git-stale-failure", &["machine-a", "machine-b"]);
    let machine_a = harness.machine("machine-a");
    let machine_b = harness.machine("machine-b");

    expect_success(machine_a.run(["init"]));
    expect_success(machine_b.run(["init"]));
    machine_a.write_file("src/main.rs", b"fn main() {}\n");
    machine_a.write_file(".git/config", b"[remote]\nurl = should-not-sync\n");
    machine_a.write_file(".gitmodules", b"[submodule \"vendor/lib\"]\n");
    machine_a.write_file("vendor/lib/.git", b"gitdir: ../../.git/modules/vendor/lib\n");
    machine_a.write_file("vendor/lib/src/lib.rs", b"pub fn submodule_contents_sync() {}\n");

    let a_start = expect_success(machine_a.run(["sync", "start"]));
    assert_output_contains(&a_start.stdout, "sync_start=foreground-complete");
    let b_start = expect_success(machine_b.run(["sync", "start"]));
    assert_output_contains(&b_start.stdout, "sync_start=foreground-complete");
    assert_eq!(machine_b.read_file("src/main.rs"), b"fn main() {}\n");
    assert_eq!(
        machine_b.read_file("vendor/lib/src/lib.rs"),
        b"pub fn submodule_contents_sync() {}\n"
    );
    assert!(!machine_b.path(".git/config").exists());
    assert!(!machine_b.path(".gitmodules").exists());
    assert!(!machine_b.path("vendor/lib/.git").exists());
    for forbidden in [".git/config", ".gitmodules", "vendor/lib/.git"] {
        assert!(
            !harness.operation_paths(machine_a).iter().any(|path| path == forbidden),
            "git metadata path `{forbidden}` reached transport operations"
        );
    }

    machine_a.write_file("src/from-upstream.rs", b"pub const MISSED_PULL: bool = true;\n");
    let a_second = expect_success(machine_a.run(["sync", "start"]));
    assert_output_contains(&a_second.stdout, "sync_start=foreground-complete");
    let stale = expect_success(machine_b.run(["status"]));
    assert_output_contains(&stale.stdout, "stale_worktree=stale");
    assert_output_contains(&stale.stdout, "recovery_command=sync recover-stale");
    let recovered = expect_success(machine_b.run(["sync", "recover-stale"]));
    assert_output_contains(&recovered.stdout, "recover_status=ok");
    assert_eq!(
        machine_b.read_file("src/from-upstream.rs"),
        b"pub const MISSED_PULL: bool = true;\n"
    );

    let saved_transport = harness.root.join("transport.saved");
    fs::rename(&harness.transport_root, &saved_transport).unwrap();
    fs::write(&harness.transport_root, b"not a directory during partition").unwrap();
    let partitioned = expect_success(machine_b.run(["sync", "status"]));
    assert_output_contains(&partitioned.stdout, "transport=error");
    fs::remove_file(&harness.transport_root).unwrap();
    fs::rename(&saved_transport, &harness.transport_root).unwrap();
    let healed = expect_success(machine_b.run(["sync", "status"]));
    assert_output_contains(&healed.stdout, "transport=ok");

    seed_manifest_without_blob(
        &harness,
        machine_a,
        "manifest-missing-hydration-source",
        "offline-only.bin",
        "missing-hydration-source-blob",
    );
    let hydration_failure = machine_b.run(["hydrate", "offline-only.bin"]);
    assert!(
        !hydration_failure.status.success(),
        "offline hydration source should fail: {hydration_failure:?}"
    );
    assert!(
        hydration_failure.stderr.contains("No such file")
            || hydration_failure.stderr.contains("read")
            || hydration_failure.stderr.contains("blob"),
        "unexpected hydrate failure stderr: {}",
        hydration_failure.stderr
    );
}

#[test]
fn daemon_process_interruption_restart_recovers_foreground_sync() {
    let harness = Harness::new("daemon-interruption", &["machine-a", "machine-b"]);
    let machine_a = harness.machine("machine-a");
    let machine_b = harness.machine("machine-b");

    let init_a = expect_success(machine_a.run(["init"]));
    expect_success(machine_b.run(["init"]));
    let daemon_state_path = PathBuf::from(output_value(&init_a.stdout, "daemon_state_path"));
    for index in 0..INTERRUPTED_SYNC_FILE_COUNT {
        machine_a.write_file(
            &format!("bulk/file-{index:04}.txt"),
            format!("restart recovery payload {index}\n").as_bytes(),
        );
    }

    let mut child = machine_a.spawn(["sync", "start"]);
    wait_for_file_contains(&daemon_state_path, "lifecycle=running", Duration::from_secs(5));
    child.kill().unwrap();
    let killed = child.wait().unwrap();
    assert!(
        !killed.success(),
        "foreground sync process should be interrupted before restart recovery; status={killed}"
    );
    let interrupted_state = fs::read_to_string(&daemon_state_path).unwrap();
    assert!(
        interrupted_state.contains("lifecycle=running"),
        "killed foreground process should leave durable running state for restart recovery: {interrupted_state}"
    );

    let restarted = expect_success(machine_a.run(["sync", "start"]));
    assert_output_contains(&restarted.stdout, "sync_start=foreground-complete");
    assert_output_contains(&restarted.stdout, "daemon_lifecycle=stopped");
    assert_output_contains(&restarted.stdout, "last_error=none");

    let b_start = expect_success(machine_b.run(["sync", "start"]));
    assert_output_contains(&b_start.stdout, "sync_start=foreground-complete");
    assert_eq!(
        machine_b.read_file("bulk/file-0000.txt"),
        b"restart recovery payload 0\n"
    );
    assert_eq!(
        machine_b.read_file(&format!(
            "bulk/file-{:04}.txt",
            INTERRUPTED_SYNC_FILE_COUNT - 1
        )),
        format!(
            "restart recovery payload {}\n",
            INTERRUPTED_SYNC_FILE_COUNT - 1
        )
        .as_bytes()
    );
}

#[test]
fn edge_cases_platform_matrix_and_soak_cover_cross_os_policy() {
    let linux_machine = app_scoped_machine_id("edge-linux-machine").unwrap();
    let mac_machine = app_scoped_machine_id("edge-mac-machine").unwrap();
    let linux = test_platform(&linux_machine, OsFamily::Linux, Architecture::X86_64);
    let linux_peer = test_platform(
        &app_scoped_machine_id("edge-linux-peer").unwrap(),
        OsFamily::Linux,
        Architecture::X86_64,
    );
    let mac = test_platform(&mac_machine, OsFamily::Macos, Architecture::Aarch64);

    let linux_pinned_entry = SnapshotEntry::new(
        TreeEntry::file("bin/tool", 4, 10, 0o755, Some("fnv64:tool".to_owned())),
        PolicyMetadata::from_action(Action::PlatformPin(PlatformPin {
            os_family: OsFamily::Linux,
            architecture: Architecture::X86_64,
        })),
    );
    let linux_snapshot = dropbox_dev::watcher::IndexedSnapshot::new(
        "matrix-project",
        vec![linux_pinned_entry.clone()],
    );
    let linux_plan = ConvergenceEngine::new(&linux_machine, linux.clone()).plan_snapshot(
        &linux_snapshot,
        None,
        true,
        1,
        &Policy::new(),
    );
    assert!(linux_plan.actions.iter().any(|action| matches!(
        action,
        ConvergenceAction::PlatformPinAccepted { path } if path == "bin/tool"
    )));
    let same_os_plan = ConvergenceEngine::new(linux_peer.machine_id.value.clone(), linux_peer).plan_snapshot(
        &linux_snapshot,
        None,
        true,
        1,
        &Policy::new(),
    );
    assert!(same_os_plan.actions.iter().any(|action| matches!(
        action,
        ConvergenceAction::PlatformPinAccepted { path } if path == "bin/tool"
    )));
    let mac_plan = ConvergenceEngine::new(&mac_machine, mac.clone()).plan_snapshot(
        &linux_snapshot,
        None,
        true,
        1,
        &Policy::new(),
    );
    assert!(mac_plan.actions.iter().any(|action| matches!(
        action,
        ConvergenceAction::PlatformPinRedirected { path, required_os, .. }
            if path == "bin/tool" && required_os == "linux"
    )));


    let operations = vec![
        OperationRecord::from_draft(
            OperationDraft::new(1, "edge-project", &linux_machine, OperationKind::PutContent, "rename-old.txt")
                .content_hash("fnv64:rename")
                .payload_id(sync_content_hash(b"rename"))
                .modified_unix_millis(1)
                .permissions(0o644),
        ),
        OperationRecord::from_draft(
            OperationDraft::new(2, "edge-project", &linux_machine, OperationKind::MovePath, "rename-new.txt")
                .previous_path("rename-old.txt")
                .content_hash("fnv64:rename")
                .payload_id(sync_content_hash(b"rename"))
                .modified_unix_millis(2)
                .permissions(0o644),
        ),
        OperationRecord::from_draft(
            OperationDraft::new(3, "edge-project", &linux_machine, OperationKind::DeletePath, "delete-me.txt")
                .modified_unix_millis(3),
        ),
        OperationRecord::from_draft(
            OperationDraft::new(4, "edge-project", &linux_machine, OperationKind::PermissionChanged, "script.sh")
                .modified_unix_millis(4)
                .permissions(0o755),
        ),
        OperationRecord::from_draft(
            OperationDraft::new(5, "edge-project", &linux_machine, OperationKind::SymlinkChanged, "current")
                .modified_unix_millis(5)
                .symlink_target("releases/current")
                .content_hash("fnv64:symlink-target"),
        ),
        OperationRecord::from_draft(
            OperationDraft::new(6, "edge-project", &linux_machine, OperationKind::PutContent, "binary-large.bin")
                .content_hash("fnv64:large")
                .payload_id(sync_content_hash(&vec![0xA5; 1024 * 64]))
                .modified_unix_millis(6)
                .permissions(0o600),
        ),
    ];
    let replay = replay_operation_log(&operations);
    assert!(replay.entries.contains_key("rename-new.txt"));
    assert!(!replay.entries.contains_key("rename-old.txt"));
    assert!(replay.tombstones.contains_key("delete-me.txt"));
    assert_eq!(replay.entries.get("script.sh").unwrap().permissions, Some(0o755));
    assert_eq!(
        replay.entries.get("current").unwrap().symlink_target.as_deref(),
        Some("releases/current")
    );
    assert_eq!(replay.entries.get("binary-large.bin").unwrap().permissions, Some(0o600));

    let soak_root = TempRoot::new("watcher-soak");
    fs::create_dir_all(soak_root.path.join("soak")).unwrap();
    let indexer = LocalIndexer::new("soak-project", Policy::new(), linux);
    let mut previous = indexer.index_project_root(&soak_root.path).unwrap();
    let mut observed_events = 0usize;
    for round in 0..96_u32 {
        let path = soak_root.path.join("soak").join(format!("file-{round:03}.txt"));
        fs::write(&path, format!("round {round}\n")).unwrap();
        if round % 3 == 0 {
            fs::write(&path, format!("round {round} edited\n")).unwrap();
        }
        if round % 5 == 0 {
            let renamed = soak_root
                .path
                .join("soak")
                .join(format!("file-{round:03}.renamed.txt"));
            fs::rename(&path, &renamed).unwrap();
        }
        if round % 7 == 0 {
            let removed = soak_root
                .path
                .join("soak")
                .join(format!("file-{round:03}.renamed.txt"));
            if removed.exists() {
                fs::remove_file(removed).unwrap();
            }
        }
        let (snapshot, queue) = indexer
            .index_project_root_with_events(&soak_root.path, Some(&previous))
            .unwrap();
        observed_events += queue.len();
        previous = snapshot;
    }
    assert!(observed_events >= 90, "watcher soak lost too many events: {observed_events}");
    let final_snapshot = indexer.index_project_root(&soak_root.path).unwrap();
    assert_eq!(previous.manifest, final_snapshot.manifest);
}

#[test]
fn performance_budget_10k_and_hydration_are_predeclared_and_enforced() {
    assert!(PERFORMANCE_BUDGET_RATIONALE.contains("predeclared"));
    let machine_id = app_scoped_machine_id("perf-machine").unwrap();
    let platform = test_platform(&machine_id, OsFamily::Linux, Architecture::X86_64);
    let policy = Policy::new();
    let entries = (0..TEN_K_FILE_COUNT)
        .map(|index| {
            LocalTreeEntry::file(
                format!("src/generated/file-{index:05}.txt"),
                12,
                index as u64 + 1,
                0o644,
                Some(format!("fnv64:{index:016x}")),
            )
        })
        .collect::<Vec<_>>();
    let tree = InMemoryTree::new(entries);

    let started = Instant::now();
    let indexer = LocalIndexer::new("perf-project", policy.clone(), platform.clone());
    let snapshot = indexer.index_tree(&tree).unwrap();
    let engine = ConvergenceEngine::new(machine_id, platform);
    let plan = engine.plan_snapshot(&snapshot, None, true, 1, &policy);
    let index_and_plan = started.elapsed();
    assert_eq!(snapshot.entries.len(), TEN_K_FILE_COUNT);
    assert_eq!(
        plan.actions
            .iter()
            .filter(|action| matches!(action, ConvergenceAction::PushContent { .. }))
            .count(),
        TEN_K_FILE_COUNT
    );
    assert_duration_within_budget(
        "10k index+sync planning",
        index_and_plan,
        TEN_K_STRUCTURE_INDEX_AND_SYNC_BUDGET_MS,
    );

    let mut diff_after_entries = snapshot.manifest.entries.clone();
    let modified_path = diff_after_entries[0].path.clone();
    diff_after_entries[0].size_bytes += 1;
    diff_after_entries[0].modified_unix_millis += 1;
    diff_after_entries[0].content_hash = Some("fnv64:modified-10k".to_owned());
    let moved_before_path = diff_after_entries
        .last()
        .expect("10k manifest should have a move candidate")
        .path
        .clone();
    let mut moved_entry = diff_after_entries.pop().unwrap();
    moved_entry.path = "src/generated/file-moved-10k.txt".to_owned();
    diff_after_entries.push(moved_entry);
    let added_path = "src/generated/file-added-10k.txt";
    diff_after_entries.push(TreeEntry::file(
        added_path,
        42,
        99_999,
        0o644,
        Some("fnv64:added-10k".to_owned()),
    ));
    let diff_after = TreeManifest::new("perf-project-after-diff", "perf-project", diff_after_entries);
    let diff_started = Instant::now();
    let structure_diff = diff_manifests(&snapshot.manifest, &diff_after);
    let structure_diff_elapsed = diff_started.elapsed();
    assert_eq!(structure_diff.len(), 3);
    assert!(structure_diff.iter().any(|change| matches!(
        change,
        StructureChange::Modified { before, after }
            if before.path.as_str() == modified_path.as_str()
                && after.path.as_str() == modified_path.as_str()
    )));
    assert!(structure_diff.iter().any(|change| matches!(
        change,
        StructureChange::Moved { before, after }
            if before.path.as_str() == moved_before_path.as_str()
                && after.path.as_str() == "src/generated/file-moved-10k.txt"
    )));
    assert!(structure_diff.iter().any(|change| matches!(
        change,
        StructureChange::Added { entry } if entry.path.as_str() == added_path
    )));
    assert_duration_within_budget(
        "10k structure diff",
        structure_diff_elapsed,
        TEN_K_STRUCTURE_DIFF_BUDGET_MS,
    );

    let body = b"hydrated contents".to_vec();
    let content_hash = store_blob_id_for_content(&body);
    let manifest = TreeManifest::new(
        "perf-hydration-manifest",
        "perf-project",
        vec![TreeEntry::file(
            "src/generated/file-00000.txt",
            body.len() as u64,
            1,
            0o644,
            Some(content_hash),
        )],
    );
    let mut mount = VfsMount::materialize(&manifest, &VfsSyncMetadata::default());
    let hydration_started = Instant::now();
    let mut hydrator = StaticHydrator { body: body.clone() };
    let read = mount
        .read_file("src/generated/file-00000.txt", &mut hydrator)
        .unwrap();
    let hydration = hydration_started.elapsed();
    assert_eq!(read.bytes, body.as_slice());
    assert!(read.fetched);
    assert_duration_within_budget("single hydration", hydration, HYDRATION_LATENCY_BUDGET_MS);
}

#[test]
fn rollout_rollback_runbook_schema_and_store_restore_drill_are_exercised() {
    let runbook = include_str!("../docs/runbooks/chunk-09-rollout-rollback.md");
    for required in [
        "Preflight",
        "Rollout",
        "Rollback",
        "schema migration",
        "backup_file_backed_sync_store",
        "restore_file_backed_sync_store",
        "operator evidence",
    ] {
        assert!(runbook.contains(required), "runbook missing `{required}`");
    }

    let root = TempRoot::new("rollback-drill");
    let transport = root.path.join("transport");
    fs::create_dir_all(&transport).unwrap();
    let backup = root.path.join("backup-known-good");
    let project_id = "rollback-project";
    let machine_id = app_scoped_machine_id("rollback-machine").unwrap();
    let store = file_store(
        &transport,
        project_id,
        &machine_id,
        std::slice::from_ref(&machine_id),
        "rollback-token",
    );
    let baseline = OperationRecord::from_draft(
        OperationDraft::new(1, project_id, &machine_id, OperationKind::PutContent, "safe.txt")
            .content_hash("fnv64:safe")
            .payload_id(sync_content_hash(b"safe"))
            .modified_unix_millis(1)
            .permissions(0o644),
    );
    store.put_content_blob(&sync_content_hash(b"safe"), b"safe").unwrap();
    store.append_operation(&baseline).unwrap();
    let backup_report = backup_file_backed_sync_store(&transport, &backup).unwrap();
    assert!(!backup_report.copied_files.is_empty());

    let bad = OperationRecord::from_draft(
        OperationDraft::new(2, project_id, &machine_id, OperationKind::DeletePath, "safe.txt")
            .modified_unix_millis(2),
    );
    store.append_operation(&bad).unwrap();
    assert_eq!(store.load_operation_log().unwrap().len(), 2);
    let restore_report = restore_file_backed_sync_store(&backup, &transport).unwrap();
    assert!(!restore_report.copied_files.is_empty());
    let restored = file_store(
        &transport,
        project_id,
        &machine_id,
        std::slice::from_ref(&machine_id),
        "rollback-token",
    );
    let restored_operations = restored.load_operation_log().unwrap();
    assert_eq!(restored_operations, vec![baseline]);

    let runner = MigrationRunner::with_migrations([sync_initial_migration()]).unwrap();
    let mut migration_store = InMemoryMigrationStore::new();
    let applied = runner.apply(&mut migration_store).unwrap();
    assert_eq!(applied.schema_version, dropbox_dev::sync::SYNC_MIGRATION_VERSION);
    assert!(applied.product_table_count() > 0);
    let rolled_back = runner
        .rollback_to(&mut migration_store, BASELINE_SCHEMA_VERSION)
        .unwrap();
    assert_eq!(rolled_back.schema_version, BASELINE_SCHEMA_VERSION);
    assert_eq!(rolled_back.product_table_count(), 0);
    assert!(!migration_store.rolled_back_sql().is_empty());
}

#[test]
fn bounded_fuzz_manifest_policy_public_cli_and_daemon_control_parsing() {
    let mut rng = DeterministicRng::new(FUZZ_SEED);
    let linux = test_platform(
        &app_scoped_machine_id("fuzz-linux").unwrap(),
        OsFamily::Linux,
        Architecture::X86_64,
    );
    let mac = test_platform(
        &app_scoped_machine_id("fuzz-mac").unwrap(),
        OsFamily::Macos,
        Architecture::Aarch64,
    );
    let cli_harness = Harness::new("fuzz-cli-daemon", &["machine"]);
    let fuzz_machine = cli_harness.machine("machine");
    let init = expect_success(fuzz_machine.run(["init"]));
    let daemon_state_path = PathBuf::from(output_value(&init.stdout, "daemon_state_path"));
    let valid_daemon_state = fs::read_to_string(&daemon_state_path).unwrap();

    for case_index in 0..FUZZ_CASES {
        let policy_text = fuzz_policy_text(&mut rng, case_index);
        if let Ok(policy) = Policy::from_syncignore(&policy_text, "*.global-ignore\n") {
            for path in fuzz_paths(case_index) {
                let first = policy.evaluate(&path, &linux);
                let second = policy.evaluate(&path, &linux);
                assert_eq!(first, second);
                let _ = policy.evaluate(&path, &mac);
            }
        }

        let before = fuzz_manifest(&mut rng, "before", case_index, 6);
        let after = fuzz_manifest(&mut rng, "after", case_index, 6);
        let first_diff = diff_manifests(&before, &after);
        let second_diff = diff_manifests(&before, &after);
        assert_eq!(first_diff, second_diff);
        assert_eq!(before.serialize_deterministic(), before.serialize_deterministic());

        let operation = OperationRecord::from_draft(
            OperationDraft::new(
                case_index as u64 + 1,
                "fuzz-project",
                app_scoped_machine_id(&format!("fuzz-machine-{case_index}")).unwrap(),
                match case_index % 5 {
                    0 => OperationKind::PutContent,
                    1 => OperationKind::MovePath,
                    2 => OperationKind::PermissionChanged,
                    3 => OperationKind::GenericPayload,
                    _ => OperationKind::PolicyMarker,
                },
                format!("fuzz/path-{case_index}.txt"),
            )
            .previous_path(format!("fuzz/old-{case_index}.txt"))
            .content_hash(format!("fnv64:{:016x}", rng.next()))
            .payload_id(format!("payload-{:016x}", rng.next()))
            .modified_unix_millis(rng.next() % 10_000)
            .permissions(0o600 | (case_index as u32 % 0o177))
            .policy_branch(if case_index % 2 == 0 {
                PolicyBranch::Sync
            } else {
                PolicyBranch::Ignore
            }),
        );
        let wire = operation.serialize_deterministic();
        assert_eq!(OperationRecord::deserialize(wire.as_bytes()).unwrap(), operation);
        let mut corrupted = wire.into_bytes();
        if !corrupted.is_empty() {
            let index = (rng.next() as usize) % corrupted.len();
            corrupted[index] = corrupted[index].wrapping_add(1);
        }
        let _ = OperationRecord::deserialize(&corrupted);

        let envelope = PayloadEnvelope::seal(
            PayloadKind::generic("fuzz-protocol").unwrap(),
            app_scoped_machine_id(&format!("fuzz-envelope-{case_index}")).unwrap(),
            None,
            format!("payload case {case_index}").as_bytes(),
            &SharedSecret::from_pairing_token("fuzz-token").unwrap(),
        )
        .unwrap();
        let parsed_envelope = PayloadEnvelope::from_wire_bytes(&envelope.to_wire_bytes()).unwrap();
        assert_eq!(parsed_envelope.kind.as_wire(), envelope.kind.as_wire());

        let cli_args = fuzz_cli_args(&mut rng, case_index);
        let cli_output = fuzz_machine.run_args(&cli_args);
        assert!(
            !cli_output.stderr.contains("panicked"),
            "malformed CLI args should not panic: args={cli_args:?} stderr={}",
            cli_output.stderr
        );
        assert!(
            cli_output.status.success() || !cli_output.stderr.trim().is_empty(),
            "malformed CLI args should either succeed or report an error: args={cli_args:?} output={cli_output:?}"
        );

        let daemon_payload = fuzz_daemon_state_payload(&mut rng, case_index, &valid_daemon_state);
        fs::write(&daemon_state_path, daemon_payload).unwrap();
        let daemon_args = fuzz_daemon_command(case_index);
        let daemon_output = fuzz_machine.run_args(&daemon_args);
        assert!(
            !daemon_output.stderr.contains("panicked"),
            "malformed daemon control state should not panic: args={daemon_args:?} stderr={}",
            daemon_output.stderr
        );
        assert!(
            daemon_output.status.success() || !daemon_output.stderr.trim().is_empty(),
            "daemon control parser should either accept the state or report an error: args={daemon_args:?} output={daemon_output:?}"
        );
        fs::write(&daemon_state_path, &valid_daemon_state).unwrap();
    }

    assert!(cli::run(["dropbox-dev", "--help"]).unwrap().contains("Commands:"));
    assert!(cli::run(["dropbox-dev", "version"]).unwrap().contains("dropbox-dev"));
    assert!(cli::run(["dropbox-dev", "version-info"])
        .unwrap()
        .contains("daemon_supervision=foreground-file-state"));
    assert!(cli::run(["dropbox-dev", "version", "extra"]).is_err());
}

#[derive(Debug)]
struct CliOutput {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

#[derive(Debug)]
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "dropbox-dev-e2e-{label}-{}-{now}-{counter}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[derive(Debug)]
struct Harness {
    root: PathBuf,
    _temp: TempRoot,
    project_id: String,
    pairing_token: String,
    transport_root: PathBuf,
    machines: BTreeMap<String, Machine>,
}

impl Harness {
    fn new(label: &str, machine_labels: &[&str]) -> Self {
        let temp = TempRoot::new(label);
        let root = temp.path.clone();
        let transport_root = root.join("transport");
        fs::create_dir_all(&transport_root).unwrap();
        let project_id = format!("project-{label}");
        let pairing_token = format!("pairing-token-{label}");
        let mut machines = BTreeMap::new();
        for machine_label in machine_labels {
            let machine_id = app_scoped_machine_id(&format!("{label}-{machine_label}")).unwrap();
            let machine_root = root.join(machine_label);
            let project_root = machine_root.join("project");
            let cache_dir = machine_root.join("cache");
            let config_path = machine_root.join("config").join("config.kv");
            fs::create_dir_all(&project_root).unwrap();
            fs::create_dir_all(&cache_dir).unwrap();
            fs::create_dir_all(config_path.parent().unwrap()).unwrap();
            fs::write(
                &config_path,
                format!(
                    "machine_id={}\nroot_path={}\ntransport_endpoint={}\ncache_dir={}\n",
                    machine_id,
                    project_root.display(),
                    transport_root.display(),
                    cache_dir.display()
                ),
            )
            .unwrap();
            machines.insert(
                (*machine_label).to_owned(),
                Machine {
                    machine_id,
                    project_root,
                    config_path,
                    project_id: project_id.clone(),
                    pairing_token: pairing_token.clone(),
                },
            );
        }
        Self {
            root,
            _temp: temp,
            project_id,
            pairing_token,
            transport_root,
            machines,
        }
    }

    fn machine(&self, label: &str) -> &Machine {
        self.machines.get(label).unwrap_or_else(|| panic!("missing machine `{label}`"))
    }

    fn machine_ids(&self) -> Vec<String> {
        self.machines
            .values()
            .map(|machine| machine.machine_id.clone())
            .collect()
    }


    fn store(&self, machine: &Machine) -> FileBackedSyncStore {
        file_store(
            &self.transport_root,
            &self.project_id,
            &machine.machine_id,
            &self.machine_ids(),
            &self.pairing_token,
        )
    }

    fn operation_paths(&self, machine: &Machine) -> Vec<String> {
        self.store(machine)
            .load_operation_log()
            .unwrap()
            .into_iter()
            .map(|operation| operation.path)
            .collect()
    }
}

#[derive(Debug)]
struct Machine {
    machine_id: String,
    project_root: PathBuf,
    config_path: PathBuf,
    project_id: String,
    pairing_token: String,
}

impl Machine {
    fn run<const N: usize>(&self, args: [&str; N]) -> CliOutput {
        self.run_args(args)
    }

    fn run_args<I, S>(&self, args: I) -> CliOutput
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut command = self.configured_command();
        for arg in args {
            command.arg(arg.as_ref());
        }
        let output = command.output().unwrap();
        CliOutput {
            status: output.status,
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    fn spawn<const N: usize>(&self, args: [&str; N]) -> Child {
        let mut command = self.configured_command();
        for arg in args {
            command.arg(arg);
        }
        command.spawn().unwrap()
    }

    fn configured_command(&self) -> Command {
        let mut command = Command::new(cargo_bin());
        command
            .current_dir(&self.project_root)
            .env_remove("DROPBOX_DEV_CONFIG")
            .env_remove("DROPBOX_DEV_CACHE_DIR")
            .env_remove("DROPBOX_DEV_PROJECT_ID")
            .env_remove("DROPBOX_DEV_PAIRING_TOKEN")
            .env_remove("DROPBOX_DEV_AUTHORIZED_MACHINE_IDS")
            .env("DROPBOX_DEV_CONFIG", &self.config_path)
            .env("DROPBOX_DEV_PROJECT_ID", &self.project_id)
            .env("DROPBOX_DEV_PAIRING_TOKEN", &self.pairing_token)
            .env(
                "DROPBOX_DEV_AUTHORIZED_MACHINE_IDS",
                self.authorized_from_config_dir(),
            );
        command
    }

    fn authorized_from_config_dir(&self) -> String {
        let harness_root = self
            .config_path
            .ancestors()
            .nth(3)
            .expect("machine config should be rooted under harness");
        let mut ids = Vec::new();
        for entry in fs::read_dir(harness_root).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path().join("config").join("config.kv");
            if path.exists() {
                let text = fs::read_to_string(path).unwrap();
                for line in text.lines() {
                    if let Some(value) = line.strip_prefix("machine_id=") {
                        ids.push(value.to_owned());
                    }
                }
            }
        }
        ids.sort();
        ids.join(",")
    }

    fn write_file(&self, relative: &str, bytes: &[u8]) {
        let path = self.path(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, bytes).unwrap();
    }

    fn read_file(&self, relative: &str) -> Vec<u8> {
        fs::read(self.path(relative)).unwrap()
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.project_root.join(relative)
    }
}

fn cargo_bin() -> PathBuf {
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_dropbox-dev") {
        return PathBuf::from(path);
    }
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    if path.file_name() == Some(OsStr::new("deps")) {
        path.pop();
    }
    path.join("dropbox-dev")
}

fn expect_success(output: CliOutput) -> CliOutput {
    assert!(
        output.status.success(),
        "CLI command failed\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        output.stdout,
        output.stderr
    );
    output
}

fn assert_output_contains(output: &str, needle: &str) {
    assert!(
        output.contains(needle),
        "expected output to contain `{needle}`; output was:\n{output}"
    );
}

fn output_value<'a>(output: &'a str, key: &str) -> &'a str {
    for line in output.lines() {
        if let Some(value) = line
            .strip_prefix(key)
            .and_then(|suffix| suffix.strip_prefix('='))
        {
            return value;
        }
    }
    panic!("output did not contain `{key}=` line: {output}");
}

fn wait_for_file_contains(path: &Path, needle: &str, timeout: Duration) {
    let started = Instant::now();
    loop {
        if let Ok(text) = fs::read_to_string(path) {
            if text.contains(needle) {
                return;
            }
        }
        assert!(
            started.elapsed() < timeout,
            "timed out waiting for `{}` to contain `{needle}`",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn file_store(
    transport_root: &Path,
    project_id: &str,
    machine_id: &str,
    authorized_machine_ids: &[String],
    pairing_token: &str,
) -> FileBackedSyncStore {
    let secret = SharedSecret::from_pairing_token(pairing_token).unwrap();
    let endpoint = EndpointSecurityConfig::file_backed(
        TransportMode::Production,
        transport_root,
        authorized_machine_ids.to_vec(),
        secret,
    );
    FileBackedSyncStore::new(endpoint, project_id, machine_id).unwrap()
}

fn latest_manifest_from_operations(
    store: &FileBackedSyncStore,
    operations: &[OperationRecord],
) -> TreeManifest {
    let operation = operations
        .iter()
        .rev()
        .find(|operation| operation.kind == OperationKind::PutManifest)
        .expect("operation log should contain a manifest operation");
    let manifest_id = operation
        .manifest_id
        .as_deref()
        .or(operation.payload_id.as_deref())
        .expect("manifest operation should carry manifest_id or payload_id");
    store.fetch_manifest(manifest_id).unwrap()
}

fn seed_manifest_without_blob(
    harness: &Harness,
    machine: &Machine,
    manifest_id: &str,
    path: &str,
    missing_blob_id: &str,
) {
    let store = harness.store(machine);
    let manifest = TreeManifest::new(
        manifest_id,
        &harness.project_id,
        vec![TreeEntry::file(path, 64, 999, 0o644, Some(missing_blob_id.to_owned()))],
    );
    store.put_manifest(&manifest).unwrap();
    let operation = OperationRecord::from_draft(
        OperationDraft::new(
            999,
            harness.project_id.clone(),
            machine.machine_id.clone(),
            OperationKind::PutManifest,
            "manifest",
        )
        .manifest_id(manifest_id.to_owned())
        .payload_id(manifest_id.to_owned())
        .modified_unix_millis(999),
    );
    store.append_operation(&operation).unwrap();
}

fn assert_transport_does_not_contain(root: &Path, needle: &[u8]) {
    let files = collect_files(root);
    for file in files {
        let bytes = fs::read(&file).unwrap();
        assert!(
            !contains_bytes(&bytes, needle),
            "transport file `{}` unexpectedly contained plaintext `{}`",
            file.display(),
            String::from_utf8_lossy(needle)
        );
    }
}

fn collect_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if !root.exists() {
        return files;
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        if path.is_dir() {
            for entry in fs::read_dir(&path).unwrap() {
                stack.push(entry.unwrap().path());
            }
        } else if path.is_file() {
            files.push(path);
        }
    }
    files.sort();
    files
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack.windows(needle.len()).any(|window| window == needle)
}

fn test_platform(machine_id: &str, os_family: OsFamily, architecture: Architecture) -> Platform {
    Platform {
        os_version: Some(format!("test-{}", os_family.as_str())),
        capabilities: PlatformCapabilities::for_os(&os_family),
        os_family,
        architecture,
        machine_id: MachineId {
            value: machine_id.to_owned(),
            provenance: MachineIdProvenance::ConfigFile(PathBuf::from("test-config")),
        },
    }
}

fn assert_duration_within_budget(label: &str, observed: Duration, budget_ms: u128) {
    assert!(
        observed.as_millis() <= budget_ms,
        "{label} exceeded predeclared CHUNK-09 budget: observed={}ms budget={}ms rationale={}",
        observed.as_millis(),
        budget_ms,
        PERFORMANCE_BUDGET_RATIONALE
    );
}

struct StaticHydrator {
    body: Vec<u8>,
}

impl Hydrator for StaticHydrator {
    fn fetch_content(&mut self, _request: HydrationRequest<'_>) -> Result<Vec<u8>, VfsAccessError> {
        Ok(self.body.clone())
    }
}

struct DeterministicRng {
    state: u64,
}

impl DeterministicRng {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next(&mut self) -> u64 {
        let mut value = self.state;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.state = value;
        value
    }
}

fn fuzz_policy_text(rng: &mut DeterministicRng, case_index: usize) -> String {
    let mut text = String::new();
    let rules = [
        "target/",
        "!target/keep.txt",
        "*.tmp",
        "dist/*.js",
        "!dist/app.js",
        "node_modules/",
        ".git/",
        "build/**",
    ];
    for offset in 0..4 {
        let index = (rng.next() as usize + case_index + offset) % rules.len();
        text.push_str(rules[index]);
        text.push('\n');
    }
    text
}

fn fuzz_paths(case_index: usize) -> Vec<String> {
    vec![
        format!("src/file-{case_index}.rs"),
        format!("target/file-{case_index}.o"),
        "target/keep.txt".to_owned(),
        "node_modules/pkg/index.js".to_owned(),
        ".git/config".to_owned(),
        "dist/app.js".to_owned(),
        "dist/drop.js".to_owned(),
    ]
}

fn fuzz_manifest(
    rng: &mut DeterministicRng,
    label: &str,
    case_index: usize,
    entries: usize,
) -> TreeManifest {
    let mut seen = BTreeSet::new();
    let mut output = Vec::new();
    for index in 0..entries {
        let value = rng.next();
        let path = format!(
            "fuzz/{label}-{case_index}-{:02}-{:04}.txt",
            index,
            value % 997
        );
        if !seen.insert(path.clone()) {
            continue;
        }
        let size = value % 4096;
        let modified = (value >> 8) % 100_000;
        let permissions = 0o600 | ((value as u32) & 0o177);
        let hash = if value & 1 == 0 {
            Some(format!("fnv64:{value:016x}"))
        } else {
            None
        };
        output.push(TreeEntry::file(path, size, modified, permissions, hash));
    }
    TreeManifest::new(format!("manifest-{label}-{case_index}"), "fuzz-project", output)
}

fn fuzz_cli_args(rng: &mut DeterministicRng, case_index: usize) -> Vec<String> {
    let token = fuzz_arg_token(rng, case_index);
    match case_index % 12 {
        0 => vec!["sync".to_owned(), token],
        1 => vec!["sync".to_owned(), "start".to_owned(), token],
        2 => vec!["sync".to_owned(), "recover-stale".to_owned(), token],
        3 => vec!["hydrate".to_owned(), "one".to_owned(), token],
        4 => vec!["policy".to_owned(), "one".to_owned(), token],
        5 => vec!["env".to_owned(), token],
        6 => vec!["env".to_owned(), "set".to_owned(), token],
        7 => vec!["catalog".to_owned(), token],
        8 => vec!["watch".to_owned(), token],
        9 => vec!["doctor".to_owned(), token],
        10 => vec!["version".to_owned(), token],
        _ => vec![token],
    }
}

fn fuzz_daemon_command(case_index: usize) -> Vec<String> {
    match case_index % 5 {
        0 => vec!["status".to_owned()],
        1 => vec!["sync".to_owned(), "status".to_owned()],
        2 => vec!["sync".to_owned(), "stop".to_owned()],
        3 => vec!["sync".to_owned(), "resume".to_owned()],
        _ => vec!["sync".to_owned(), "start".to_owned()],
    }
}

fn fuzz_daemon_state_payload(
    rng: &mut DeterministicRng,
    case_index: usize,
    valid_state: &str,
) -> String {
    match case_index % 9 {
        0 => "not-key-value\n".to_owned(),
        1 => valid_state.replace("format=dropbox-dev-daemon-state-v1", "format=unsupported"),
        2 => valid_state.replace("lifecycle=stopped", "lifecycle=launching"),
        3 => valid_state.replace("sync_paused=false", "sync_paused=maybe"),
        4 => valid_state.replace(
            "queued_operations=0",
            &format!("queued_operations={}", fuzz_arg_token(rng, case_index)),
        ),
        5 => valid_state.replace("last_error=~", "last_error=unterminated\\"),
        6 => valid_state.replace("stale_worktree=unknown", "stale_worktree=sideways"),
        7 => valid_state.replace(
            "last_local_manifest=~",
            "last_local_manifest=catalog-manifest-lines-v1\\nentry\\tbad",
        ),
        _ => valid_state.replace(
            "hydration_placeholders=0",
            &format!("hydration_placeholders={}", usize::MAX),
        ),
    }
}

fn fuzz_arg_token(rng: &mut DeterministicRng, case_index: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-./:=@ ";
    let len = (rng.next() as usize + case_index) % 24 + 1;
    let mut token = String::new();
    for _ in 0..len {
        let index = (rng.next() as usize) % ALPHABET.len();
        token.push(ALPHABET[index] as char);
    }
    if token.trim().is_empty() {
        format!("arg-{case_index}")
    } else {
        token
    }
}
