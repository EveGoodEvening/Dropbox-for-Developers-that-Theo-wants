//! User-facing CLI and portable daemon UX layer.
//!
//! The upstream chunks intentionally expose library contracts without a process
//! manager. CHUNK-08 keeps the binary stdlib-only and makes daemon state
//! observable through deterministic cache-dir files so every command can report
//! what it did or why it cannot proceed.

use crate::catalog::{
    HydrationStatus, TreeEntry, TreeEntryKind, TreeManifest, CATALOG_MANIFEST_FORMAT_VERSION,
};
use crate::env::{
    EnvKeyProvider, EnvKeyVersionRecord, EnvMachineOverrideSecret, EnvMasterKey,
    EnvNeverSyncPolicy, EnvReplica, EnvSyncError, ENV_REDACTED_VALUE,
};
use crate::foundation::{
    Architecture, Config, ConfigError, EnvError, InMemoryMigrationStore, LogEvent, LogLevel,
    Logger, MachineId, MigrationRunner, OsFamily, Platform, PlatformCapabilities, StderrLogger,
    SyncError,
};
use crate::policy::{Action, Policy, SYNCIGNORE_FILE_NAME};
use crate::sync::{
    replay_operation_log, store_blob_id_for_content, write_content_atomically, ConvergenceAction,
    ConvergenceEngine, ConvergencePlan, EndpointSecurityConfig, FileBackedSyncStore,
    OperationKind, OperationRecord, ReplayState, SharedSecret, TransportMode,
};
use crate::vfs::{
    AccessLatency, HydrationRequest, Hydrator, VfsAccessError, VfsMount, VfsResult,
    VfsSyncMetadata, VFS_CACHE_LAYOUT,
};
use crate::watcher::{
    EventKind, EventQueue, FsEvent, IndexedSnapshot, LocalIndexer, PolicyMetadata, SnapshotEntry,
};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fmt;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const APP_NAME: &str = "dropbox-dev";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const DAEMON_STATE_FORMAT_VERSION: &str = "dropbox-dev-daemon-state-v1";
const DAEMON_STATE_FILE_STEM: &str = "daemon-state";
const DAEMON_STATE_FILE_EXTENSION: &str = "txt";
const DEFAULT_PROJECT_ID: &str = "default-project";
const UNCONFIGURED_TRANSPORT: &str = "unconfigured";
const PLATFORM_OS_OVERRIDE_ENV: &str = "DROPBOX_DEV_PLATFORM_OS";
const PLATFORM_ARCH_OVERRIDE_ENV: &str = "DROPBOX_DEV_PLATFORM_ARCH";
const QUEUED_OPERATIONS_UNDRAINED: &str =
    "queued operations remain pending; no durable queue payloads were available to drain";

/// U8 daemon supervision decision.
///
/// The portable baseline is a foreground, cooperative supervisor with durable
/// state/control files in the configured cache directory. That is deliberately
/// less clever than spawning OS services from the product: launchd, systemd, or
/// Windows Service wrappers can supervise this binary later without changing the
/// CLI contract, while tests exercise the same stdlib file state and avoid
/// platform-specific signal/PID semantics.
pub const U8_DAEMON_SUPERVISION_RATIONALE: &str = "portable foreground stdlib supervisor: CLI commands mutate durable cache-dir state/control files and daemon work runs cooperatively in-process; OS service managers may wrap the binary later without changing command semantics; tests use the same state files instead of platform-specific signal or pid supervision";

pub fn run_from_env() -> Result<String, SyncError> {
    run_with_logger(std::env::args(), &StderrLogger)
}

pub fn run<I, S>(args: I) -> Result<String, SyncError>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    run_with_logger(args, &StderrLogger)
}

pub fn run_with_logger<I, S>(args: I, logger: &dyn Logger) -> Result<String, SyncError>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut args = args.into_iter().map(Into::into);
    let _binary = args.next();
    let command = args.next();
    let rest = args.collect::<Vec<_>>();

    match command.as_deref() {
        None | Some("--help") | Some("-h") => Ok(help()),
        Some("version") | Some("--version") | Some("-V") => {
            expect_no_args(command.as_deref().unwrap_or("version"), &rest)?;
            Ok(version())
        }
        Some("version-info") => {
            expect_no_args("version-info", &rest)?;
            Ok(version_info())
        }
        Some("doctor") => {
            expect_no_args("doctor", &rest)?;
            doctor_from_env(logger)
        }
        Some(command) => {
            let runtime = CliRuntime::from_env()?;
            run_command(command, &rest, logger, &runtime)
        }
    }
}

fn run_command(
    command: &str,
    rest: &[String],
    logger: &dyn Logger,
    runtime: &CliRuntime,
) -> Result<String, SyncError> {
    match command {
        "init" => {
            expect_no_args("init", rest)?;
            init_command(runtime, logger)
        }
        "status" => {
            expect_no_args("status", rest)?;
            status_command(runtime)
        }
        "sync" => sync_command(rest, runtime),
        "catalog" => {
            expect_no_args("catalog", rest)?;
            catalog_command(runtime)
        }
        "policy" => policy_command(rest, runtime),
        "watch" => {
            expect_no_args("watch", rest)?;
            watch_command(runtime)
        }
        "hydrate" => hydrate_command(rest, runtime),
        "env" => env_command(rest, runtime),
        "info" => {
            expect_no_args("info", rest)?;
            info(runtime, logger)
        }
        other => Err(SyncError::cli(format!("unknown command `{other}`"))),
    }
}

fn version() -> String {
    format!("{APP_NAME} {VERSION}\n")
}

fn version_info() -> String {
    let mut output = String::new();
    push_kv(&mut output, "app_name", APP_NAME);
    push_kv(&mut output, "version", VERSION);
    push_kv(&mut output, "daemon_supervision", "foreground-file-state");
    push_kv(
        &mut output,
        "daemon_supervision_rationale",
        U8_DAEMON_SUPERVISION_RATIONALE,
    );
    output
}

fn info(runtime: &CliRuntime, logger: &dyn Logger) -> Result<String, SyncError> {
    let config = &runtime.config;
    let mut store = InMemoryMigrationStore::new();
    let migration_report = MigrationRunner::empty_v0().apply(&mut store)?;

    logger.emit(
        &LogEvent::new(LogLevel::Info, "cli info", config.machine_id.clone())
            .with_field("machine_id_provenance", config.machine_id_provenance.to_string())
            .with_correlation_id("cli-info")
            .with_field("version", VERSION)
            .with_field("config_path", config.config_path.display().to_string())
            .with_field("schema_version", migration_report.schema_version.clone())
            .with_field("project_id", runtime.project_id.clone())
            .with_field("daemon_supervision", "foreground-file-state"),
    )?;

    let mut output = String::new();
    push_kv(&mut output, "machine_id", &config.machine_id);
    push_kv(&mut output, "version", VERSION);
    push_kv(&mut output, "config_path", config.config_path.display());
    push_kv(&mut output, "schema_version", &migration_report.schema_version);
    push_kv(
        &mut output,
        "product_tables",
        migration_report.product_table_count(),
    );
    push_kv(&mut output, "project_id", &runtime.project_id);
    push_kv(&mut output, "cache_dir", config.cache_dir.display());
    push_kv(
        &mut output,
        "daemon_state_path",
        DaemonController::new(runtime).state_path().display(),
    );
    Ok(output)
}

fn init_command(runtime: &CliRuntime, logger: &dyn Logger) -> Result<String, SyncError> {
    let controller = DaemonController::new(runtime);
    let initialized = controller.init()?;
    logger.emit(
        &LogEvent::new(LogLevel::Info, "cli init", runtime.config.machine_id.clone())
            .with_correlation_id("cli-init")
            .with_field("cache_dir", runtime.config.cache_dir.display().to_string())
            .with_field("daemon_state_path", controller.state_path().display().to_string())
            .with_field("initialized", initialized.to_string()),
    )?;

    let state = controller.load_state()?;
    let mut output = String::new();
    push_kv(&mut output, "init_status", "ok");
    push_kv(&mut output, "initialized", initialized);
    push_kv(&mut output, "config_path", runtime.config.config_path.display());
    push_kv(&mut output, "cache_dir", runtime.config.cache_dir.display());
    append_state_report(&mut output, &state, &controller.state_path());
    Ok(output)
}

fn status_command(runtime: &CliRuntime) -> Result<String, SyncError> {
    let controller = DaemonController::new(runtime);
    let state = controller.load_state()?;
    let worktree = detect_worktree(runtime, &state);

    let mut output = String::new();
    push_kv(&mut output, "status", "ok");
    append_state_report(&mut output, &state, &controller.state_path());
    match worktree {
        Ok(mut report) => {
            report.status = visible_worktree_status(&state, &report);
            append_worktree_report(&mut output, &report);
        }
        Err(error) => {
            push_kv(&mut output, "stale_worktree", "unknown");
            push_kv(&mut output, "stale_worktree_error", error);
        }
    }
    Ok(output)
}

fn sync_command(rest: &[String], runtime: &CliRuntime) -> Result<String, SyncError> {
    let subcommand = rest.first().map(String::as_str).unwrap_or("status");
    let remaining = if rest.is_empty() { &[][..] } else { &rest[1..] };
    let controller = DaemonController::new(runtime);

    match subcommand {
        "status" => {
            expect_no_args("sync status", remaining)?;
            sync_status(runtime)
        }
        "start" => {
            expect_no_args("sync start", remaining)?;
            let run = controller.start_foreground()?;
            let mut output = String::new();
            let complete = run.unsupported_actions == 0
                && run.state.queued_operations == 0
                && run.state.last_error.is_none()
                && run.worktree.status != WorktreeStatus::Stale;
            push_kv(
                &mut output,
                "sync_start",
                if complete {
                    "foreground-complete"
                } else {
                    "foreground-incomplete"
                },
            );
            push_kv(
                &mut output,
                "foreground_work",
                if run.unsupported_actions > 0 {
                    "unsupported"
                } else if run.state.queued_operations > 0 {
                    "queued"
                } else if run.worktree.status == WorktreeStatus::Stale {
                    "pending"
                } else {
                    "ok"
                },
            );
            push_kv(&mut output, "foreground_actions_applied", run.applied_actions);
            push_kv(
                &mut output,
                "foreground_unsupported_actions",
                run.unsupported_actions,
            );
            append_state_report(&mut output, &run.state, &controller.state_path());
            append_worktree_report(&mut output, &run.worktree);
            Ok(output)
        }
        "stop" => {
            expect_no_args("sync stop", remaining)?;
            let changed = controller.stop()?;
            let mut output = String::new();
            push_kv(&mut output, "sync_stop", if changed { "stopped" } else { "already-stopped" });
            append_state_report(&mut output, &controller.load_state()?, &controller.state_path());
            Ok(output)
        }
        "pause" => {
            expect_no_args("sync pause", remaining)?;
            let changed = controller.pause()?;
            let mut output = String::new();
            push_kv(&mut output, "sync_pause", if changed { "paused" } else { "already-paused" });
            append_state_report(&mut output, &controller.load_state()?, &controller.state_path());
            Ok(output)
        }
        "resume" => {
            expect_no_args("sync resume", remaining)?;
            let resume = controller.resume()?;
            let mut output = String::new();
            push_kv(
                &mut output,
                "sync_resume",
                if resume.changed { "resumed" } else { "already-unpaused" },
            );
            append_state_report(&mut output, &resume.state, &controller.state_path());
            Ok(output)
        }
        "recover-stale" => {
            expect_no_args("sync recover-stale", remaining)?;
            recover_stale_worktree(runtime)
        }
        other => Err(SyncError::cli(format!(
            "unknown sync subcommand `{other}`; expected status, start, stop, pause, resume, or recover-stale"
        ))),
    }
}

fn sync_status(runtime: &CliRuntime) -> Result<String, SyncError> {
    let controller = DaemonController::new(runtime);
    let state = controller.load_state()?;
    let mut output = String::new();
    push_kv(&mut output, "sync_status", "ok");
    append_state_report(&mut output, &state, &controller.state_path());

    match connect_transport(runtime) {
        Ok(store) => match store.load_operation_log() {
            Ok(operations) => {
                push_kv(&mut output, "transport", "ok");
                push_kv(&mut output, "transport_operations", operations.len());
                push_kv(&mut output, "transport_root", store.shared_root().display());
            }
            Err(error) => {
                push_kv(&mut output, "transport", "error");
                push_kv(&mut output, "transport_error", error);
            }
        },
        Err(error) => {
            push_kv(&mut output, "transport", "error");
            push_kv(&mut output, "transport_error", error);
        }
    }

    Ok(output)
}

fn catalog_command(runtime: &CliRuntime) -> Result<String, SyncError> {
    let (root, _policy, snapshot) = index_current_project(runtime)?;
    let mut output = String::new();
    push_kv(&mut output, "catalog_status", "ok");
    push_kv(&mut output, "project_id", &snapshot.project_id);
    push_kv(&mut output, "root", root.display());
    push_kv(&mut output, "manifest_id", &snapshot.manifest.id);
    push_kv(&mut output, "entry_count", snapshot.manifest.entries.len());
    for (index, entry) in snapshot.manifest.canonical_entries().into_iter().enumerate() {
        push_kv(
            &mut output,
            &format!("entry.{}", index + 1),
            format!(
                "{} {} size={} hash={}",
                entry.path,
                entry.kind.as_str(),
                entry.size_bytes,
                entry.content_hash.as_deref().unwrap_or("-")
            ),
        );
    }
    Ok(output)
}

fn policy_command(rest: &[String], runtime: &CliRuntime) -> Result<String, SyncError> {
    if rest.len() > 1 {
        return Err(SyncError::cli("policy accepts at most one relative path"));
    }
    let root = project_root(runtime)?;
    let policy = load_policy(&root)?;
    let path = rest.first().map(String::as_str).unwrap_or(".");
    let action = policy.evaluate(path, &runtime.platform);

    let mut output = String::new();
    push_kv(&mut output, "policy_status", "ok");
    push_kv(&mut output, "root", root.display());
    push_kv(&mut output, "path", path);
    push_kv(&mut output, "action", action_label(&action));
    push_kv(&mut output, "project_rules", policy.project_rules().len());
    push_kv(&mut output, "user_rules", policy.user_rules().len());
    Ok(output)
}

fn watch_command(runtime: &CliRuntime) -> Result<String, SyncError> {
    let (root, _policy, snapshot) = index_current_project(runtime)?;
    let queue = EventQueue::from_snapshots(None, &snapshot);

    let mut output = String::new();
    push_kv(&mut output, "watch_status", "ok");
    push_kv(&mut output, "root", root.display());
    push_kv(&mut output, "snapshot_id", &snapshot.id);
    push_kv(&mut output, "entry_count", snapshot.entries.len());
    push_kv(&mut output, "event_count", queue.len());
    push_kv(
        &mut output,
        "content_sync_event_count",
        queue.content_sync_events().count(),
    );
    for (index, event) in queue.iter().enumerate() {
        push_kv(
            &mut output,
            &format!("event.{}", index + 1),
            format!("{} {}", event.kind.as_str(), event.path),
        );
    }
    Ok(output)
}

fn hydrate_command(rest: &[String], runtime: &CliRuntime) -> Result<String, SyncError> {
    if rest.len() > 1 {
        return Err(SyncError::cli("hydrate accepts at most one relative path"));
    }
    let (root, policy, snapshot) = index_current_project(runtime)?;
    let state = DaemonController::new(runtime).load_state()?;
    let store = connect_transport(runtime)?;
    let operations = store
        .load_operation_log()
        .map_err(|error| SyncError::cli(error.to_string()))?;
    let replay = replay_operation_log(&operations);
    let remote_manifest = latest_remote_manifest(&store, &operations)?;
    let engine = ConvergenceEngine::new(runtime.config.machine_id.clone(), runtime.platform.clone());
    let plan = engine.plan_snapshot_with_remote_state(
        &snapshot,
        remote_manifest.as_ref(),
        Some(&replay),
        !state.sync_paused,
        next_sequence(&operations),
        &policy,
    );
    let mut sync_metadata = VfsSyncMetadata::from_replay_state(&replay);
    sync_metadata.merge_convergence_plan(&plan);
    let (manifest_source, manifest) = match remote_manifest.as_ref() {
        Some(manifest) => ("remote", manifest),
        None => ("local-snapshot", &snapshot.manifest),
    };
    let mut mount = VfsMount::materialize(manifest, &sync_metadata);

    let mut output = String::new();
    push_kv(&mut output, "hydrate_status", "ok");
    push_kv(&mut output, "root", root.display());
    push_kv(&mut output, "manifest_id", mount.manifest_id());
    push_kv(&mut output, "manifest_source", manifest_source);
    push_kv(&mut output, "cache_layout", VFS_CACHE_LAYOUT.version);
    push_kv(&mut output, "placeholder_count", mount.placeholder_records().len());
    push_kv(&mut output, "cached_content_count", mount.cached_content_count());
    push_kv(&mut output, "sync_metadata_outcomes", sync_metadata.outcomes().len());
    push_kv(&mut output, "convergence_actions", plan.actions.len());

    if let Some(path) = rest.first() {
        let relative = normalize_relative_cli_path(path)?;
        let path_source_machine = mount
            .node(&relative)
            .map(|node| node.source_machine_id.clone())
            .unwrap_or_else(|| "missing".to_owned());
        let path_outcome = mount
            .node(&relative)
            .map(|node| node.persisted_outcome())
            .unwrap_or("missing");
        let mut hydrator = SyncStoreHydrator { store: &store };
        let (bytes, latency, fetched, content_hash) = {
            let read = mount
                .read_file(&relative, &mut hydrator)
                .map_err(|error| SyncError::cli(error.to_string()))?;
            (
                read.bytes.len(),
                read.latency,
                read.fetched,
                read.cache_version.content_hash.clone(),
            )
        };
        let status = mount
            .node(&relative)
            .map(|node| hydration_status_label(&node.hydration_status))
            .unwrap_or("missing");
        push_kv(&mut output, "path", relative);
        push_kv(&mut output, "path_source_machine", path_source_machine);
        push_kv(&mut output, "path_convergence_outcome", path_outcome);
        push_kv(&mut output, "content_hash", content_hash);
        push_kv(&mut output, "bytes", bytes);
        push_kv(&mut output, "latency", latency_label(latency));
        push_kv(&mut output, "fetched", fetched);
        push_kv(&mut output, "path_hydration_status", status);
        push_kv(&mut output, "cached_content_count", mount.cached_content_count());
    }

    Ok(output)
}

fn env_command(rest: &[String], runtime: &CliRuntime) -> Result<String, SyncError> {
    let subcommand = rest.first().map(String::as_str).unwrap_or("status");
    let remaining = if rest.is_empty() { &[][..] } else { &rest[1..] };
    match subcommand {
        "status" => {
            expect_no_args("env status", remaining)?;
            let state = DaemonController::new(runtime).load_state()?;
            let mut output = String::new();
            push_kv(&mut output, "env_status", "ok");
            push_kv(&mut output, "machine_id", &runtime.config.machine_id);
            push_kv(&mut output, "materialized_variables", state.env_variables);
            push_kv(&mut output, "conflict_sidecars", state.env_conflicts);
            push_kv(&mut output, "redacted_value", ENV_REDACTED_VALUE);
            Ok(output)
        }
        "export" => env_export_command(remaining, runtime),
        "list" => env_list_command(remaining, runtime),
        "audit" => env_audit_command(remaining, runtime),
        "publish" => env_publish_command(remaining, runtime),
        "override" => env_override_command(remaining, runtime),
        "never-sync" => env_never_sync_command(remaining, runtime),
        other => Err(SyncError::cli(format!(
            "unknown env subcommand `{other}`; expected status, export, list, audit, publish, override, or never-sync"
        ))),
    }
}

fn env_export_command(remaining: &[String], runtime: &CliRuntime) -> Result<String, SyncError> {
    expect_no_args("env export", remaining)?;
    let controller = DaemonController::new(runtime);
    let mut state = controller.load_state()?;
    let store = connect_transport(runtime).inspect_err(|error| {
        let _ = controller.record_error(&error.to_string());
    })?;
    let mut replica = env_replica(runtime).inspect_err(|error| {
        let _ = controller.record_error(&error.to_string());
    })?;
    let ingest = replica
        .ingest_from_transport(&store)
        .map_err(env_error)
        .inspect_err(|error| {
            let _ = controller.record_error(&error.to_string());
        })?;
    let materialization = replica
        .materialize()
        .map_err(env_error)
        .inspect_err(|error| {
            let _ = controller.record_error(&error.to_string());
        })?;
    let conflict_sidecars = replica.state().conflict_sidecars().len();
    state.env_variables = materialization.launcher_environment.len();
    state.env_conflicts = conflict_sidecars;
    state.last_error = None;
    controller.save_state(&state)?;

    let mut output = String::new();
    push_kv(&mut output, "env_export_status", "ok");
    push_kv(&mut output, "fetched_payloads", ingest.fetched_payloads);
    push_kv(&mut output, "applied_records", ingest.applied_records);
    push_kv(&mut output, "materialized_variables", state.env_variables);
    push_kv(&mut output, "conflict_sidecars", state.env_conflicts);
    push_kv(
        &mut output,
        "redacted_session_export",
        materialization.redacted_session_export,
    );
    Ok(output)
}

fn env_list_command(remaining: &[String], runtime: &CliRuntime) -> Result<String, SyncError> {
    expect_no_args("env list", remaining)?;
    let controller = DaemonController::new(runtime);
    let mut state = controller.load_state()?;
    let store = connect_transport(runtime).inspect_err(|error| {
        let _ = controller.record_error(&error.to_string());
    })?;
    let mut replica = env_replica(runtime).inspect_err(|error| {
        let _ = controller.record_error(&error.to_string());
    })?;
    let ingest = replica
        .ingest_from_transport(&store)
        .map_err(env_error)
        .inspect_err(|error| {
            let _ = controller.record_error(&error.to_string());
        })?;
    let materialization = replica
        .materialize()
        .map_err(env_error)
        .inspect_err(|error| {
            let _ = controller.record_error(&error.to_string());
        })?;
    let conflict_sidecars = replica.state().conflict_sidecars().len();
    state.env_variables = materialization.launcher_environment.len();
    state.env_conflicts = conflict_sidecars;
    state.last_error = None;
    controller.save_state(&state)?;

    let records = replica.state().records();
    let mut output = String::new();
    push_kv(&mut output, "env_list_status", "ok");
    push_kv(&mut output, "fetched_payloads", ingest.fetched_payloads);
    push_kv(&mut output, "applied_records", ingest.applied_records);
    push_kv(&mut output, "record_count", records.len());
    push_kv(&mut output, "materialized_variables", state.env_variables);
    push_kv(&mut output, "conflict_sidecars", state.env_conflicts);
    push_kv(&mut output, "redacted_value", ENV_REDACTED_VALUE);
    for (index, record) in records.iter().enumerate() {
        let prefix = format!("env.{}", index + 1);
        push_kv(&mut output, &format!("{prefix}.name"), &record.name);
        push_kv(&mut output, &format!("{prefix}.scope"), record.scope.as_wire());
        push_kv(
            &mut output,
            &format!("{prefix}.author_machine_id"),
            &record.author_machine_id,
        );
        push_kv(&mut output, &format!("{prefix}.key_version"), &record.key_version);
        push_kv(&mut output, &format!("{prefix}.value"), ENV_REDACTED_VALUE);
    }
    Ok(output)
}

fn env_audit_command(remaining: &[String], runtime: &CliRuntime) -> Result<String, SyncError> {
    expect_no_args("env audit", remaining)?;
    let controller = DaemonController::new(runtime);
    let mut state = controller.load_state()?;
    let store = connect_transport(runtime).inspect_err(|error| {
        let _ = controller.record_error(&error.to_string());
    })?;
    let mut replica = env_replica(runtime).inspect_err(|error| {
        let _ = controller.record_error(&error.to_string());
    })?;
    let ingest = replica
        .ingest_from_transport(&store)
        .map_err(env_error)
        .inspect_err(|error| {
            let _ = controller.record_error(&error.to_string());
        })?;
    let materialization = replica
        .materialize()
        .map_err(env_error)
        .inspect_err(|error| {
            let _ = controller.record_error(&error.to_string());
        })?;
    let conflict_sidecars = replica.state().conflict_sidecars().len();
    state.env_variables = materialization.launcher_environment.len();
    state.env_conflicts = conflict_sidecars;
    state.last_error = None;
    controller.save_state(&state)?;

    let entries = replica.audit_log().entries();
    let mut output = String::new();
    push_kv(&mut output, "env_audit_status", "ok");
    push_kv(&mut output, "fetched_payloads", ingest.fetched_payloads);
    push_kv(&mut output, "applied_records", ingest.applied_records);
    push_kv(&mut output, "materialized_variables", state.env_variables);
    push_kv(&mut output, "conflict_sidecars", state.env_conflicts);
    push_kv(&mut output, "audit_events", entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let prefix = format!("audit.{}", index + 1);
        push_kv(&mut output, &format!("{prefix}.ordinal"), entry.ordinal);
        push_kv(
            &mut output,
            &format!("{prefix}.event_unix_millis"),
            entry.event_unix_millis,
        );
        push_kv(&mut output, &format!("{prefix}.operation"), entry.operation.as_wire());
        push_kv(&mut output, &format!("{prefix}.status"), entry.status.as_wire());
        push_kv(&mut output, &format!("{prefix}.actor_machine_id"), &entry.actor_machine_id);
        push_kv(
            &mut output,
            &format!("{prefix}.env_name"),
            entry.env_name.as_deref().unwrap_or("none"),
        );
        push_kv(
            &mut output,
            &format!("{prefix}.scope"),
            entry
                .scope
                .as_ref()
                .map(|scope| scope.as_wire())
                .unwrap_or_else(|| "none".to_owned()),
        );
        push_kv(
            &mut output,
            &format!("{prefix}.payload_id"),
            entry.payload_id.as_deref().unwrap_or("none"),
        );
        push_kv(
            &mut output,
            &format!("{prefix}.key_version"),
            entry.key_version.as_deref().unwrap_or("none"),
        );
        push_kv(&mut output, &format!("{prefix}.value"), entry.redacted_value);
    }
    Ok(output)
}

fn env_publish_command(remaining: &[String], runtime: &CliRuntime) -> Result<String, SyncError> {
    expect_arg_count("env publish", remaining, 2, "NAME VALUE")?;
    let controller = DaemonController::new(runtime);
    let mut state = controller.load_state()?;
    let store = connect_transport(runtime).inspect_err(|error| {
        let _ = controller.record_error(&error.to_string());
    })?;
    let mut replica = env_replica(runtime).inspect_err(|error| {
        let _ = controller.record_error(&error.to_string());
    })?;
    let ingest = replica
        .ingest_from_transport(&store)
        .map_err(env_error)
        .inspect_err(|error| {
            let _ = controller.record_error(&error.to_string());
        })?;
    let report = replica
        .set_shared(&store, &remaining[0], &remaining[1], cli_current_unix_millis())
        .map_err(env_error)
        .inspect_err(|error| {
            let _ = controller.record_error(&error.to_string());
        })?;
    let materialization = replica
        .materialize()
        .map_err(env_error)
        .inspect_err(|error| {
            let _ = controller.record_error(&error.to_string());
        })?;
    let conflict_sidecars = replica.state().conflict_sidecars().len();
    state.env_variables = materialization.launcher_environment.len();
    state.env_conflicts = conflict_sidecars;
    state.last_error = None;
    controller.save_state(&state)?;

    let mut output = String::new();
    push_kv(&mut output, "env_publish_status", "ok");
    push_kv(&mut output, "fetched_payloads", ingest.fetched_payloads);
    push_kv(&mut output, "applied_records", ingest.applied_records);
    push_kv(&mut output, "env_name", report.env_name);
    push_kv(&mut output, "scope", report.scope.as_wire());
    push_kv(&mut output, "payload_id", report.payload_id);
    push_kv(&mut output, "operation_id", report.operation_id);
    push_kv(&mut output, "key_version", report.key_version);
    push_kv(&mut output, "redacted_value", report.redacted_value);
    push_kv(&mut output, "materialized_variables", state.env_variables);
    push_kv(&mut output, "conflict_sidecars", state.env_conflicts);
    Ok(output)
}

fn env_override_command(remaining: &[String], runtime: &CliRuntime) -> Result<String, SyncError> {
    expect_arg_count("env override", remaining, 3, "TARGET_MACHINE_ID NAME VALUE")?;
    let controller = DaemonController::new(runtime);
    let mut state = controller.load_state()?;
    let store = connect_transport(runtime).inspect_err(|error| {
        let _ = controller.record_error(&error.to_string());
    })?;
    let mut replica = env_replica(runtime).inspect_err(|error| {
        let _ = controller.record_error(&error.to_string());
    })?;
    let ingest = replica
        .ingest_from_transport(&store)
        .map_err(env_error)
        .inspect_err(|error| {
            let _ = controller.record_error(&error.to_string());
        })?;
    let report = replica
        .set_override_for_machine(
            &store,
            &remaining[0],
            &remaining[1],
            &remaining[2],
            cli_current_unix_millis(),
        )
        .map_err(env_error)
        .inspect_err(|error| {
            let _ = controller.record_error(&error.to_string());
        })?;
    let materialization = replica
        .materialize()
        .map_err(env_error)
        .inspect_err(|error| {
            let _ = controller.record_error(&error.to_string());
        })?;
    let conflict_sidecars = replica.state().conflict_sidecars().len();
    state.env_variables = materialization.launcher_environment.len();
    state.env_conflicts = conflict_sidecars;
    state.last_error = None;
    controller.save_state(&state)?;

    let mut output = String::new();
    push_kv(&mut output, "env_override_status", "ok");
    push_kv(&mut output, "fetched_payloads", ingest.fetched_payloads);
    push_kv(&mut output, "applied_records", ingest.applied_records);
    push_kv(&mut output, "target_machine_id", &remaining[0]);
    push_kv(&mut output, "env_name", report.env_name);
    push_kv(&mut output, "scope", report.scope.as_wire());
    push_kv(&mut output, "payload_id", report.payload_id);
    push_kv(&mut output, "operation_id", report.operation_id);
    push_kv(&mut output, "key_version", report.key_version);
    push_kv(&mut output, "redacted_value", report.redacted_value);
    push_kv(&mut output, "materialized_variables", state.env_variables);
    push_kv(&mut output, "conflict_sidecars", state.env_conflicts);
    Ok(output)
}

fn env_never_sync_command(remaining: &[String], runtime: &CliRuntime) -> Result<String, SyncError> {
    let action = remaining.first().map(String::as_str).unwrap_or("list");
    let rest = if remaining.is_empty() { &[][..] } else { &remaining[1..] };
    match action {
        "list" => {
            expect_no_args("env never-sync list", rest)?;
            let config = load_cli_env_never_sync_config(runtime)?;
            let mut output = String::new();
            push_kv(&mut output, "env_never_sync_status", "ok");
            push_kv(&mut output, "policy_path", env_never_sync_path(runtime).display());
            append_env_never_sync_config(&mut output, &config);
            Ok(output)
        }
        "add" | "remove" => env_never_sync_update_command(action, rest, runtime),
        other => Err(SyncError::cli(format!(
            "unknown env never-sync subcommand `{other}`; expected list, add, or remove"
        ))),
    }
}

fn env_never_sync_update_command(
    action: &str,
    remaining: &[String],
    runtime: &CliRuntime,
) -> Result<String, SyncError> {
    expect_arg_count(
        &format!("env never-sync {action}"),
        remaining,
        2,
        "name|prefix|suffix VALUE",
    )?;
    let kind = remaining[0].as_str();
    let value = remaining[1].as_str();
    validate_cli_env_never_sync_rule(kind, value)?;
    let mut config = load_cli_env_never_sync_config(runtime)?;
    let changed = match (action, kind) {
        ("add", "name") => config.names.insert(value.to_owned()),
        ("add", "prefix") => config.prefixes.insert(value.to_owned()),
        ("add", "suffix") => config.suffixes.insert(value.to_owned()),
        ("remove", "name") => config.names.remove(value),
        ("remove", "prefix") => config.prefixes.remove(value),
        ("remove", "suffix") => config.suffixes.remove(value),
        _ => unreachable!("validated env never-sync action and kind"),
    };
    save_cli_env_never_sync_config(runtime, &config)?;

    let mut output = String::new();
    push_kv(&mut output, "env_never_sync_status", "ok");
    push_kv(&mut output, "action", action);
    push_kv(&mut output, "changed", changed);
    push_kv(&mut output, "rule_kind", kind);
    push_kv(&mut output, "rule_value", value);
    push_kv(&mut output, "policy_path", env_never_sync_path(runtime).display());
    append_env_never_sync_config(&mut output, &config);
    Ok(output)
}

fn validate_cli_env_never_sync_rule(kind: &str, value: &str) -> Result<(), SyncError> {
    let mut policy = EnvNeverSyncPolicy::allow_all();
    match kind {
        "name" => {
            policy.deny_name(value).map_err(env_error)?;
        }
        "prefix" => {
            policy.deny_prefix(value).map_err(env_error)?;
        }
        "suffix" => {
            policy.deny_suffix(value).map_err(env_error)?;
        }
        other => {
            return Err(SyncError::cli(format!(
                "unknown env never-sync rule kind `{other}`; expected name, prefix, or suffix"
            )));
        }
    }
    Ok(())
}

fn cli_current_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

fn doctor_from_env(logger: &dyn Logger) -> Result<String, SyncError> {
    let config = match Config::load_default() {
        Ok(config) => config,
        Err(error) => {
            let mut output = String::new();
            push_kv(&mut output, "doctor_status", "fail");
            push_kv(&mut output, "config", "fail");
            push_kv(&mut output, "issue_count", 1);
            push_kv(&mut output, "issue.1", error);
            return Err(SyncError::cli(output));
        }
    };
    let current_dir = env::current_dir()
        .map_err(|error| SyncError::platform(format!("read current directory failed: {error}")))?;
    let runtime = CliRuntime::from_process_config(config, current_dir)?;
    logger.emit(
        &LogEvent::new(LogLevel::Info, "cli doctor", runtime.config.machine_id.clone())
            .with_correlation_id("cli-doctor")
            .with_field("config_path", runtime.config.config_path.display().to_string()),
    )?;
    doctor_report(&runtime)
}

fn doctor_report(runtime: &CliRuntime) -> Result<String, SyncError> {
    let mut checks = Vec::new();

    checks.push(match runtime.config.validate() {
        Ok(()) => DoctorCheck::ok(
            "config",
            format!("{} roots={}", runtime.config.config_path.display(), runtime.config.root_paths.len()),
        ),
        Err(error) => DoctorCheck::fail("config", error.to_string()),
    });

    let root = project_root(runtime);
    match &root {
        Ok(root) if root.is_dir() => checks.push(DoctorCheck::ok("root", root.display().to_string())),
        Ok(root) => checks.push(DoctorCheck::fail(
            "root",
            format!("project root is not a directory: {}", root.display()),
        )),
        Err(error) => checks.push(DoctorCheck::fail("root", error.to_string())),
    }

    checks.push(match fs::create_dir_all(&runtime.config.cache_dir) {
        Ok(()) if runtime.config.cache_dir.is_dir() => {
            DoctorCheck::ok("cache", runtime.config.cache_dir.display().to_string())
        }
        Ok(()) => DoctorCheck::fail(
            "cache",
            format!("cache path is not a directory: {}", runtime.config.cache_dir.display()),
        ),
        Err(error) => DoctorCheck::fail(
            "cache",
            format!("create `{}` failed: {error}", runtime.config.cache_dir.display()),
        ),
    });

    checks.push(match permission_probe(&runtime.config.cache_dir) {
        Ok(()) => DoctorCheck::ok("permissions", "cache read/write probe succeeded"),
        Err(error) => DoctorCheck::fail("permissions", error),
    });

    checks.push(match connect_transport(runtime) {
        Ok(store) => match store.load_operation_log() {
            Ok(operations) => DoctorCheck::ok(
                "transport",
                format!(
                    "{} operations={} project={}",
                    store.shared_root().display(),
                    operations.len(),
                    store.project_id()
                ),
            ),
            Err(error) => DoctorCheck::fail("transport", error.to_string()),
        },
        Err(error) => DoctorCheck::fail("transport", error.to_string()),
    });

    checks.push(match root.and_then(|root| load_policy(&root).map(|policy| (root, policy))) {
        Ok((root, policy)) => DoctorCheck::ok(
            "policy",
            format!(
                "{} project_rules={} user_rules={}",
                root.join(SYNCIGNORE_FILE_NAME).display(),
                policy.project_rules().len(),
                policy.user_rules().len()
            ),
        ),
        Err(error) => DoctorCheck::fail("policy", error.to_string()),
    });

    let failures = checks.iter().filter(|check| !check.ok).collect::<Vec<_>>();
    let mut output = String::new();
    push_kv(
        &mut output,
        "doctor_status",
        if failures.is_empty() { "ok" } else { "fail" },
    );
    for check in &checks {
        push_kv(&mut output, check.name, if check.ok { "ok" } else { "fail" });
        push_kv(&mut output, &format!("{}.detail", check.name), &check.detail);
    }
    let failure_count = failures.len();
    push_kv(&mut output, "issue_count", failure_count);
    for (index, failure) in failures.iter().enumerate() {
        push_kv(
            &mut output,
            &format!("issue.{}", index + 1),
            format!("{}: {}", failure.name, failure.detail),
        );
    }
    if failure_count == 0 {
        Ok(output)
    } else {
        Err(SyncError::cli(output))
    }
}

fn recover_stale_worktree(runtime: &CliRuntime) -> Result<String, SyncError> {
    let controller = DaemonController::new(runtime);
    let mut state = controller.load_state()?;
    let root = project_root(runtime)?;
    let policy = load_policy(&root)?;
    let snapshot = index_snapshot(runtime, &root, policy.clone())?;
    let store = connect_transport(runtime).inspect_err(|error| {
        let _ = controller.record_error(&error.to_string());
    })?;
    let operations = store.load_operation_log().map_err(|error| {
        let sync_error = SyncError::cli(error.to_string());
        let _ = controller.record_error(&sync_error.to_string());
        sync_error
    })?;
    let Some(remote_manifest) = latest_remote_manifest(&store, &operations)? else {
        let mut output = String::new();
        push_kv(&mut output, "recover_status", "noop");
        push_kv(&mut output, "reason", "no remote manifest operation found");
        return Ok(output);
    };

    let replay = replay_operation_log(&operations);
    let engine = ConvergenceEngine::new(runtime.config.machine_id.clone(), runtime.platform.clone());
    let plan = engine.plan_snapshot_with_remote_state(
        &snapshot,
        Some(&remote_manifest),
        Some(&replay),
        !state.sync_paused,
        next_sequence(&operations),
        &policy,
    );
    let remote_entries = remote_manifest
        .entries
        .iter()
        .map(|entry| (entry.path.clone(), entry.kind))
        .collect::<BTreeMap<_, _>>();

    let blocked_conflict_paths = unresolved_conflict_sidecar_paths(&plan);
    let mut recovered_paths = Vec::new();
    let mut recovered_path_set = BTreeSet::new();
    let mut skipped_actions = 0usize;
    for action in &plan.actions {
        if convergence_action_touches_any_path(action, &blocked_conflict_paths) {
            skipped_actions += 1;
            continue;
        }

        match action {
            ConvergenceAction::FetchContent {
                path,
                store_blob_id: Some(store_blob_id),
                temp_suffix,
            } => {
                let outcome = fetch_project_content(
                    &root,
                    &store,
                    path,
                    store_blob_id,
                    temp_suffix,
                    "create recovery parent failed",
                )
                .inspect_err(|error| {
                    let _ = controller.record_error(&error.to_string());
                })?;
                if outcome == ProjectFetchOutcome::Materialized
                    && recovered_path_set.insert(path.clone())
                {
                    recovered_paths.push(path.clone());
                } else if outcome == ProjectFetchOutcome::PreservedExisting {
                    skipped_actions += 1;
                }
            }
            ConvergenceAction::PropagatePermissions { path, permissions }
                if matches!(remote_entries.get(path), Some(TreeEntryKind::Directory)) =>
            {
                let directory = resolve_project_path(&root, path)?;
                fs::create_dir_all(&directory).map_err(|error| {
                    SyncError::from(ConfigError::io(
                        directory,
                        format!("create recovered directory failed: {error}"),
                    ))
                })?;
                if !apply_foreground_permissions(runtime, &root, path, *permissions)? {
                    skipped_actions += 1;
                }
                if recovered_path_set.insert(path.clone()) {
                    recovered_paths.push(path.clone());
                }
            }
            ConvergenceAction::PropagatePermissions { path, permissions }
                if recovered_path_set.contains(path) =>
            {
                if !apply_foreground_permissions(runtime, &root, path, *permissions)? {
                    skipped_actions += 1;
                }
            }
            ConvergenceAction::FetchContent { store_blob_id: None, .. } => skipped_actions += 1,
            ConvergenceAction::PushContent { .. }
            | ConvergenceAction::FetchSymlinkTargetMetadata { .. }
            | ConvergenceAction::DeleteLocal { .. }
            | ConvergenceAction::DeleteRemote { .. }
            | ConvergenceAction::MovePath { .. }
            | ConvergenceAction::PropagatePermissions { .. }
            | ConvergenceAction::PropagateSymlink { .. }
            | ConvergenceAction::QueueOffline { .. }
            | ConvergenceAction::ConflictSidecar { .. } => skipped_actions += 1,
            ConvergenceAction::RebuildLocally { .. }
            | ConvergenceAction::Ignore { .. }
            | ConvergenceAction::PlatformPinAccepted { .. }
            | ConvergenceAction::PlatformPinRedirected { .. }
            | ConvergenceAction::GitMetadataLocalOnly { .. }
            | ConvergenceAction::Noop { .. } => {}
        }
    }

    let recovered_snapshot = if !recovered_paths.is_empty() {
        Some(index_snapshot(runtime, &root, policy.clone())?)
    } else {
        None
    };
    let recovery_fully_clean = if let Some(snapshot) = recovered_snapshot.as_ref() {
        let final_plan = engine.plan_snapshot_with_remote_state(
            snapshot,
            Some(&remote_manifest),
            Some(&replay),
            !state.sync_paused,
            next_sequence(&operations),
            &policy,
        );
        !convergence_plan_has_pending_work(&final_plan, Some(&remote_manifest))
    } else {
        skipped_actions == 0
    };
    if recovery_fully_clean {
        let clean_snapshot = recovered_snapshot.unwrap_or(snapshot);
        state.last_local_manifest = Some(clean_snapshot.manifest);
    }
    state.stale_worktree = if !recovery_fully_clean {
        WorktreeStatus::Stale
    } else if recovered_paths.is_empty() {
        WorktreeStatus::Clean
    } else {
        WorktreeStatus::Recovered
    };
    state.hydration_hydrated += recovered_paths.len();
    state.hydration_placeholders = state.hydration_placeholders.saturating_sub(recovered_paths.len());
    state.last_error = None;
    controller.save_state(&state)?;

    let mut output = String::new();
    push_kv(&mut output, "recover_status", "ok");
    push_kv(&mut output, "recovered_paths", recovered_paths.len());
    push_kv(&mut output, "skipped_actions", skipped_actions);
    for (index, path) in recovered_paths.iter().enumerate() {
        push_kv(&mut output, &format!("recovered.{}", index + 1), path);
    }
    append_state_report(&mut output, &state, &controller.state_path());
    Ok(output)
}

fn help() -> String {
    format!(
        "{APP_NAME} {VERSION}\n\nCommands:\n  init                 Initialize cache and daemon state\n  status               Print daemon, sync, hydration, env, and stale-worktree state\n  sync [status]        Print sync/transport state\n  sync start|stop      Run one foreground sync pass or stop persisted controls\n  sync pause|resume    Pause/resume sync and expose queue state\n  sync recover-stale   Materialize missing remote files from the sync transport\n  catalog              Print current project catalog snapshot\n  policy [path]        Evaluate .syncignore/platform policy for a path\n  watch                Poll current project and print watcher events\n  hydrate [path]       Print VFS hydration state, or hydrate one transport-backed path\n  env status|export    Print or materialize redacted environment sync state\n  env list|audit       List redacted env records or audit events\n  env publish NAME VALUE\n                       Publish a shared env value without printing plaintext\n  env override TARGET_MACHINE_ID NAME VALUE\n                       Publish a machine override without printing plaintext\n  env never-sync list|add|remove name|prefix|suffix VALUE\n                       Manage local env names that must never be published\n  info                 Print configuration and migration metadata\n  version-info         Print version and daemon-supervision rationale\n  doctor               Check config, cache, transport, permissions, and policy\n"
    )
}

#[derive(Debug, Clone)]
struct CliRuntime {
    config: Config,
    platform: crate::foundation::Platform,
    current_dir: PathBuf,
    project_id: String,
    pairing_token: String,
    authorized_machine_ids: Vec<String>,
}

impl CliRuntime {
    fn from_env() -> Result<Self, SyncError> {
        let config = Config::load_default()?;
        let current_dir = env::current_dir()
            .map_err(|error| SyncError::platform(format!("read current directory failed: {error}")))?;
        Self::from_process_config(config, current_dir)
    }

    fn from_process_config(config: Config, current_dir: PathBuf) -> Result<Self, SyncError> {
        let project_id = env::var("DROPBOX_DEV_PROJECT_ID")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| default_project_id(&config, &current_dir));
        let pairing_token = env::var("DROPBOX_DEV_PAIRING_TOKEN")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| format!("local-pairing:{}", config.machine_id));
        let authorized_machine_ids = env::var("DROPBOX_DEV_AUTHORIZED_MACHINE_IDS")
            .ok()
            .map(|value| parse_csv(&value))
            .filter(|values| !values.is_empty())
            .unwrap_or_else(|| vec![config.machine_id.clone()]);
        let platform = platform_from_process_env(&config)?;
        Ok(Self {
            config,
            platform,
            current_dir,
            project_id,
            pairing_token,
            authorized_machine_ids,
        })
    }

    #[cfg(test)]
    fn for_test(
        config: Config,
        platform: crate::foundation::Platform,
        current_dir: PathBuf,
        project_id: impl Into<String>,
        pairing_token: impl Into<String>,
        authorized_machine_ids: Vec<String>,
    ) -> Self {
        Self {
            config,
            platform,
            current_dir,
            project_id: project_id.into(),
            pairing_token: pairing_token.into(),
            authorized_machine_ids,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LifecycleState {
    Stopped,
    Running,
}

impl LifecycleState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Running => "running",
        }
    }

    fn from_str(value: &str) -> Result<Self, SyncError> {
        match value {
            "stopped" => Ok(Self::Stopped),
            "running" => Ok(Self::Running),
            other => Err(SyncError::cli(format!(
                "invalid daemon lifecycle state `{other}`"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorktreeStatus {
    Unknown,
    Clean,
    Stale,
    Recovered,
}

impl WorktreeStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Clean => "clean",
            Self::Stale => "stale",
            Self::Recovered => "recovered",
        }
    }

    fn from_str(value: &str) -> Result<Self, SyncError> {
        match value {
            "unknown" => Ok(Self::Unknown),
            "clean" => Ok(Self::Clean),
            "stale" => Ok(Self::Stale),
            "recovered" => Ok(Self::Recovered),
            other => Err(SyncError::cli(format!("invalid worktree state `{other}`"))),
        }
    }
}

fn visible_worktree_status(state: &DaemonState, report: &WorktreeReport) -> WorktreeStatus {
    if state.stale_worktree == WorktreeStatus::Recovered && report.status == WorktreeStatus::Clean {
        WorktreeStatus::Recovered
    } else {
        report.status
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DaemonState {
    lifecycle: LifecycleState,
    sync_paused: bool,
    queued_operations: usize,
    last_error: Option<String>,
    hydration_placeholders: usize,
    hydration_hydrated: usize,
    env_variables: usize,
    env_conflicts: usize,
    stale_worktree: WorktreeStatus,
    last_local_manifest: Option<TreeManifest>,
}

impl Default for DaemonState {
    fn default() -> Self {
        Self {
            lifecycle: LifecycleState::Stopped,
            sync_paused: false,
            queued_operations: 0,
            last_error: None,
            hydration_placeholders: 0,
            hydration_hydrated: 0,
            env_variables: 0,
            env_conflicts: 0,
            stale_worktree: WorktreeStatus::Unknown,
            last_local_manifest: None,
        }
    }
}

impl DaemonState {
    fn serialize(&self) -> String {
        let mut output = String::new();
        push_raw_state(&mut output, "format", DAEMON_STATE_FORMAT_VERSION);
        push_raw_state(&mut output, "lifecycle", self.lifecycle.as_str());
        push_raw_state(&mut output, "sync_paused", bool_state(self.sync_paused));
        push_raw_state(&mut output, "queued_operations", self.queued_operations);
        push_raw_state(
            &mut output,
            "last_error",
            self.last_error.as_deref().unwrap_or("~"),
        );
        push_raw_state(
            &mut output,
            "hydration_placeholders",
            self.hydration_placeholders,
        );
        push_raw_state(&mut output, "hydration_hydrated", self.hydration_hydrated);
        push_raw_state(&mut output, "env_variables", self.env_variables);
        push_raw_state(&mut output, "env_conflicts", self.env_conflicts);
        push_raw_state(&mut output, "stale_worktree", self.stale_worktree.as_str());
        let last_local_manifest = self
            .last_local_manifest
            .as_ref()
            .map(TreeManifest::serialize_deterministic)
            .unwrap_or_else(|| "~".to_owned());
        push_raw_state(&mut output, "last_local_manifest", last_local_manifest);
        output
    }

    fn parse(text: &str) -> Result<Self, SyncError> {
        let mut fields = BTreeMap::new();
        for (line_index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(SyncError::cli(format!(
                    "daemon state line {} is not key=value",
                    line_index + 1
                )));
            };
            fields.insert(key.trim().to_owned(), decode_state_value(value.trim())?);
        }

        let format = required_state(&fields, "format")?;
        if format != DAEMON_STATE_FORMAT_VERSION {
            return Err(SyncError::cli(format!(
                "unsupported daemon state format `{format}`"
            )));
        }

        let last_error = required_state(&fields, "last_error")?;
        let last_local_manifest = fields
            .get("last_local_manifest")
            .map(String::as_str)
            .unwrap_or("~");
        Ok(Self {
            lifecycle: LifecycleState::from_str(required_state(&fields, "lifecycle")?)?,
            sync_paused: parse_state_bool(required_state(&fields, "sync_paused")?, "sync_paused")?,
            queued_operations: parse_state_usize(
                required_state(&fields, "queued_operations")?,
                "queued_operations",
            )?,
            last_error: if last_error == "~" {
                None
            } else {
                Some(last_error.to_owned())
            },
            hydration_placeholders: parse_state_usize(
                required_state(&fields, "hydration_placeholders")?,
                "hydration_placeholders",
            )?,
            hydration_hydrated: parse_state_usize(
                required_state(&fields, "hydration_hydrated")?,
                "hydration_hydrated",
            )?,
            env_variables: parse_state_usize(required_state(&fields, "env_variables")?, "env_variables")?,
            env_conflicts: parse_state_usize(required_state(&fields, "env_conflicts")?, "env_conflicts")?,
            stale_worktree: WorktreeStatus::from_str(required_state(&fields, "stale_worktree")?)?,
            last_local_manifest: if last_local_manifest == "~" {
                None
            } else {
                Some(parse_state_manifest(last_local_manifest)?)
            },
        })
    }
}

struct DaemonController<'a> {
    runtime: &'a CliRuntime,
}

struct ForegroundRun {
    state: DaemonState,
    worktree: WorktreeReport,
    applied_actions: usize,
    unsupported_actions: usize,
}

struct ResumeOutcome {
    changed: bool,
    state: DaemonState,
}

struct PlannedWorktree {
    root: PathBuf,
    policy: Policy,
    snapshot: IndexedSnapshot,
    store: FileBackedSyncStore,
    operations: Vec<OperationRecord>,
    remote_manifest: Option<TreeManifest>,
    remote_state: ReplayState,
    plan: ConvergencePlan,
    local_events: Vec<FsEvent>,
}

impl<'a> DaemonController<'a> {
    fn new(runtime: &'a CliRuntime) -> Self {
        Self { runtime }
    }

    fn state_path(&self) -> PathBuf {
        let root = daemon_state_root_identity(self.runtime);
        let project = safe_filename_component(&self.runtime.project_id);
        let root_text = root.to_string_lossy();
        let identity_hash = stable_identity_hash(&self.runtime.project_id, root_text.as_bytes());
        self.runtime.config.cache_dir.join(format!(
            "{DAEMON_STATE_FILE_STEM}-{project}-{identity_hash}.{DAEMON_STATE_FILE_EXTENSION}"
        ))
    }

    fn init(&self) -> Result<bool, SyncError> {
        if let Some(parent) = self.runtime.config.config_path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                SyncError::from(ConfigError::io(
                    parent.to_path_buf(),
                    format!("create config parent failed: {error}"),
                ))
            })?;
        }
        fs::create_dir_all(&self.runtime.config.cache_dir).map_err(|error| {
            SyncError::from(ConfigError::io(
                self.runtime.config.cache_dir.clone(),
                format!("create cache dir failed: {error}"),
            ))
        })?;
        let path = self.state_path();
        if path.exists() {
            let _ = self.load_state()?;
            Ok(false)
        } else {
            self.save_state(&DaemonState::default())?;
            Ok(true)
        }
    }

    fn load_state(&self) -> Result<DaemonState, SyncError> {
        let path = self.state_path();
        match fs::read_to_string(&path) {
            Ok(text) => DaemonState::parse(&text),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(DaemonState::default()),
            Err(error) => Err(SyncError::from(ConfigError::io(
                path,
                format!("read daemon state failed: {error}"),
            ))),
        }
    }

    fn save_state(&self, state: &DaemonState) -> Result<(), SyncError> {
        fs::create_dir_all(&self.runtime.config.cache_dir).map_err(|error| {
            SyncError::from(ConfigError::io(
                self.runtime.config.cache_dir.clone(),
                format!("create cache dir failed: {error}"),
            ))
        })?;
        let path = self.state_path();
        fs::write(&path, state.serialize()).map_err(|error| {
            SyncError::from(ConfigError::io(path, format!("write daemon state failed: {error}")))
        })
    }

    fn start_foreground(&self) -> Result<ForegroundRun, SyncError> {
        let mut state = self.load_state()?;
        state.lifecycle = LifecycleState::Running;
        self.save_state(&state)?;

        let pass = execute_foreground_sync_pass(self.runtime, &state);
        state.lifecycle = LifecycleState::Stopped;
        match pass {
            Ok((mut report, applied_actions, unsupported_actions, final_manifest)) => {
                report.status = visible_worktree_status(&state, &report);
                state.stale_worktree = report.status;
                state.queued_operations = report.queued_count;
                state.last_error = foreground_pass_error(
                    unsupported_actions,
                    report.queued_count,
                    report.status == WorktreeStatus::Stale,
                );
                if foreground_pass_fully_converged(&report, unsupported_actions) {
                    state.last_local_manifest = Some(final_manifest);
                }
                self.save_state(&state)?;
                Ok(ForegroundRun {
                    state,
                    worktree: report,
                    applied_actions,
                    unsupported_actions,
                })
            }
            Err(error) => {
                state.last_error = Some(error.to_string());
                self.save_state(&state)?;
                Err(error)
            }
        }
    }

    fn stop(&self) -> Result<bool, SyncError> {
        let mut state = self.load_state()?;
        let changed = state.lifecycle != LifecycleState::Stopped || state.sync_paused;
        state.lifecycle = LifecycleState::Stopped;
        state.sync_paused = false;
        self.save_state(&state)?;
        Ok(changed)
    }

    fn pause(&self) -> Result<bool, SyncError> {
        let mut state = self.load_state()?;
        let changed = !state.sync_paused;
        state.sync_paused = true;
        self.save_state(&state)?;
        Ok(changed)
    }

    fn resume(&self) -> Result<ResumeOutcome, SyncError> {
        let mut state = self.load_state()?;
        let changed = state.sync_paused || state.queued_operations > 0;
        if !changed {
            return Ok(ResumeOutcome { changed, state });
        }

        state.sync_paused = false;
        state.lifecycle = LifecycleState::Running;
        self.save_state(&state)?;

        let pass = execute_foreground_sync_pass(self.runtime, &state);
        state.lifecycle = LifecycleState::Stopped;
        match pass {
            Ok((mut report, _applied_actions, unsupported_actions, final_manifest)) => {
                report.status = visible_worktree_status(&state, &report);
                state.stale_worktree = report.status;
                state.queued_operations = report.queued_count;
                state.last_error = foreground_pass_error(
                    unsupported_actions,
                    report.queued_count,
                    report.status == WorktreeStatus::Stale,
                );
                if foreground_pass_fully_converged(&report, unsupported_actions) {
                    state.last_local_manifest = Some(final_manifest);
                }
                self.save_state(&state)?;
                Ok(ResumeOutcome { changed, state })
            }
            Err(error) => {
                state.last_error = Some(error.to_string());
                self.save_state(&state)?;
                Err(error)
            }
        }
    }

    fn record_error(&self, error: &str) -> Result<(), SyncError> {
        let mut state = self.load_state()?;
        state.last_error = Some(error.to_owned());
        self.save_state(&state)
    }
}

#[cfg(test)]
#[derive(Debug, Clone)]
struct ForegroundDaemon {
    state: DaemonState,
    session: crate::sync::SyncSession,
}

#[cfg(test)]
impl Default for ForegroundDaemon {
    fn default() -> Self {
        Self {
            state: DaemonState::default(),
            session: crate::sync::SyncSession::offline(),
        }
    }
}

#[cfg(test)]
impl ForegroundDaemon {
    fn start(&mut self) {
        self.state.lifecycle = LifecycleState::Running;
        self.session.online = !self.state.sync_paused;
    }

    fn stop(&mut self) {
        self.state.lifecycle = LifecycleState::Stopped;
        self.state.sync_paused = false;
        self.session.online = false;
    }

    fn pause(&mut self) -> Result<(), SyncError> {
        if self.state.lifecycle != LifecycleState::Running {
            return Err(SyncError::cli("cannot pause stopped foreground daemon"));
        }
        self.state.sync_paused = true;
        self.session.online = false;
        Ok(())
    }

    fn resume(
        &mut self,
        store: &mut crate::sync::AuthoritativeSyncStore,
    ) -> Result<Vec<OperationRecord>, SyncError> {
        if self.state.lifecycle != LifecycleState::Running {
            return Err(SyncError::cli("cannot resume stopped foreground daemon"));
        }
        self.state.sync_paused = false;
        let drained = self.session.reconnect(store);
        self.state.queued_operations = self.session.offline_queue.len();
        Ok(drained)
    }

    fn submit_operation(
        &mut self,
        operation: OperationRecord,
        store: &mut crate::sync::AuthoritativeSyncStore,
    ) -> Result<crate::sync::SyncSubmitResult, SyncError> {
        if self.state.lifecycle != LifecycleState::Running {
            return Err(SyncError::cli("cannot submit operation while daemon is stopped"));
        }
        let result = self.session.submit_operation(operation, store);
        self.state.queued_operations = self.session.offline_queue.len();
        Ok(result)
    }

    fn record_error(&mut self, error: impl Into<String>) {
        self.state.last_error = Some(error.into());
    }

    fn record_hydration_summary(&mut self, placeholders: usize, hydrated: usize) {
        self.state.hydration_placeholders = placeholders;
        self.state.hydration_hydrated = hydrated;
    }

    fn record_env_materialization(&mut self, materialization: &crate::env::EnvMaterialization, conflicts: usize) {
        self.state.env_variables = materialization.launcher_environment.len();
        self.state.env_conflicts = conflicts;
    }
}

#[derive(Debug)]
struct DoctorCheck {
    name: &'static str,
    ok: bool,
    detail: String,
}

impl DoctorCheck {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            ok: true,
            detail: detail.into(),
        }
    }

    fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            ok: false,
            detail: detail.into(),
        }
    }
}

#[derive(Debug, Clone)]
struct WorktreeReport {
    status: WorktreeStatus,
    remote_manifest_id: Option<String>,
    action_count: usize,
    fetch_count: usize,
    push_count: usize,
    queued_count: usize,
    unsupported_count: usize,
}

fn detect_worktree(runtime: &CliRuntime, state: &DaemonState) -> Result<WorktreeReport, SyncError> {
    let planned = plan_current_worktree(runtime, state)?;
    Ok(worktree_report_from_plan(state, &planned))
}

fn append_worktree_report(output: &mut String, report: &WorktreeReport) {
    push_kv(output, "stale_worktree", report.status.as_str());
    push_kv(
        output,
        "remote_manifest_id",
        report.remote_manifest_id.as_deref().unwrap_or("none"),
    );
    push_kv(output, "convergence_actions", report.action_count);
    push_kv(output, "remote_fetch_actions", report.fetch_count);
    push_kv(output, "local_push_actions", report.push_count);
    push_kv(output, "queued_operations_observed", report.queued_count);
    push_kv(output, "unsupported_actions_observed", report.unsupported_count);
    if report.status == WorktreeStatus::Stale {
        push_kv(output, "recovery_command", "sync recover-stale");
    }
}

fn append_state_report(output: &mut String, state: &DaemonState, state_path: &Path) {
    push_kv(output, "daemon_lifecycle", state.lifecycle.as_str());
    push_kv(output, "daemon_supervision", "foreground-file-state");
    push_kv(output, "sync_paused", state.sync_paused);
    push_kv(output, "queued_operations", state.queued_operations);
    push_kv(output, "last_error", state.last_error.as_deref().unwrap_or("none"));
    push_kv(output, "hydration_placeholders", state.hydration_placeholders);
    push_kv(output, "hydration_hydrated", state.hydration_hydrated);
    push_kv(output, "env_variables", state.env_variables);
    push_kv(output, "env_conflicts", state.env_conflicts);
    push_kv(output, "state_stale_worktree", state.stale_worktree.as_str());
    push_kv(output, "daemon_state_path", state_path.display());
}

fn connect_transport(runtime: &CliRuntime) -> Result<FileBackedSyncStore, SyncError> {
    let endpoint = runtime.config.transport_endpoint.trim();
    if endpoint.is_empty() || endpoint == UNCONFIGURED_TRANSPORT {
        return Err(SyncError::cli(
            "transport_endpoint is unconfigured; set an absolute file-backed transport path",
        ));
    }
    let shared_root = PathBuf::from(endpoint);
    if !shared_root.is_absolute() {
        return Err(SyncError::cli(format!(
            "transport_endpoint `{}` is not absolute",
            shared_root.display()
        )));
    }
    let secret = SharedSecret::from_pairing_token(&runtime.pairing_token)
        .map_err(|error| SyncError::cli(error.to_string()))?;
    let endpoint = EndpointSecurityConfig::file_backed(
        TransportMode::Production,
        shared_root,
        runtime.authorized_machine_ids.clone(),
        secret,
    );
    FileBackedSyncStore::new(
        endpoint,
        runtime.project_id.clone(),
        runtime.config.machine_id.clone(),
    )
    .map_err(|error| SyncError::cli(error.to_string()))
}

const CLI_ENV_KEY_VERSION: &str = "v1";
const CLI_ENV_KEY_CREATED_LOGICAL_MILLIS: u64 = 0;
const CLI_ENV_NEVER_SYNC_FORMAT_VERSION: &str = "dropbox-dev-env-never-sync-v1";
const CLI_ENV_NEVER_SYNC_FILE_STEM: &str = "env-never-sync";
const CLI_ENV_NEVER_SYNC_FILE_EXTENSION: &str = "txt";

/// CLI adapter for CHUNK-07's out-of-band key provider seam.
///
/// CHUNK-08 does not add a second persistent key configuration surface; its
/// baseline derives deterministic local env key material from the existing
/// pairing token that already gates access to the file-backed transport.
#[derive(Debug, Clone)]
struct CliEnvKeyProvider {
    machine_id: String,
    key: EnvMasterKey,
    override_secret: EnvMachineOverrideSecret,
}

impl CliEnvKeyProvider {
    fn new(runtime: &CliRuntime) -> Result<Self, SyncError> {
        let key = EnvMasterKey::new(CLI_ENV_KEY_VERSION, env_key_material(runtime, None))
            .map_err(env_error)?;
        let override_secret = EnvMachineOverrideSecret::new(
            CLI_ENV_KEY_VERSION,
            runtime.config.machine_id.clone(),
            env_key_material(runtime, Some(&runtime.config.machine_id)),
        )
        .map_err(env_error)?;
        Ok(Self {
            machine_id: runtime.config.machine_id.clone(),
            key,
            override_secret,
        })
    }

    fn version_record(&self) -> EnvKeyVersionRecord {
        EnvKeyVersionRecord {
            keyring_machine_id: self.machine_id.clone(),
            key_version: self.key.key_version().to_owned(),
            key_digest: self.key.digest_hex(),
            created_logical_millis: CLI_ENV_KEY_CREATED_LOGICAL_MILLIS,
            active: true,
        }
    }
}

impl EnvKeyProvider for CliEnvKeyProvider {
    fn keyring_machine_id(&self) -> &str {
        &self.machine_id
    }

    fn latest_key_version(&self) -> Result<EnvKeyVersionRecord, EnvSyncError> {
        Ok(self.version_record())
    }

    fn key_for_version(&self, key_version: &str) -> Result<EnvMasterKey, EnvSyncError> {
        if key_version == self.key.key_version() {
            return Ok(self.key.clone());
        }
        Err(EnvSyncError::MissingKey(format!(
            "env key version `{key_version}` is not available in CLI keyring"
        )))
    }

    fn override_secret_for_target(
        &self,
        key_version: &str,
        target_machine_id: &str,
    ) -> Result<EnvMachineOverrideSecret, EnvSyncError> {
        if key_version != self.override_secret.key_version() {
            return Err(EnvSyncError::MissingKey(format!(
                "env override secret key version `{key_version}` is not available in CLI keyring"
            )));
        }
        if target_machine_id != self.override_secret.machine_id() {
            return Err(EnvSyncError::MissingKey(format!(
                "env override secret for target machine `{target_machine_id}` is not available in CLI keyring"
            )));
        }
        Ok(self.override_secret.clone())
    }

    fn key_version_records(&self) -> Vec<EnvKeyVersionRecord> {
        vec![self.version_record()]
    }
}

fn env_replica(runtime: &CliRuntime) -> Result<EnvReplica<CliEnvKeyProvider>, SyncError> {
    let never_sync_policy = load_cli_env_never_sync_config(runtime)?.to_policy()?;
    EnvReplica::new_with_never_sync_policy(
        runtime.project_id.clone(),
        runtime.config.machine_id.clone(),
        CliEnvKeyProvider::new(runtime)?,
        never_sync_policy,
    )
    .map_err(env_error)
}

fn env_key_material(runtime: &CliRuntime, target_machine_id: Option<&str>) -> Vec<u8> {
    let mut material = Vec::new();
    push_env_key_material_part(
        &mut material,
        if target_machine_id.is_some() {
            "dropbox-dev-cli-env-override-key-v1"
        } else {
            "dropbox-dev-cli-env-shared-key-v1"
        },
    );
    push_env_key_material_part(&mut material, &runtime.project_id);
    push_env_key_material_part(&mut material, runtime.pairing_token.trim());
    if let Some(machine_id) = target_machine_id {
        push_env_key_material_part(&mut material, machine_id);
    }
    material
}

fn push_env_key_material_part(material: &mut Vec<u8>, value: &str) {
    material.extend_from_slice(&(value.len() as u64).to_be_bytes());
    material.extend_from_slice(value.as_bytes());
}

fn env_error(error: impl fmt::Display) -> SyncError {
    SyncError::from(EnvError::invalid(error.to_string()))
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CliEnvNeverSyncConfig {
    names: BTreeSet<String>,
    prefixes: BTreeSet<String>,
    suffixes: BTreeSet<String>,
}

impl CliEnvNeverSyncConfig {
    fn parse(text: &str) -> Result<Self, SyncError> {
        let mut config = Self::default();
        let mut format_seen = false;
        for (line_index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(SyncError::cli(format!(
                    "env never-sync policy line {} is not key=value",
                    line_index + 1
                )));
            };
            let key = key.trim();
            let value = decode_state_value(value.trim())?;
            match key {
                "format" => {
                    if value != CLI_ENV_NEVER_SYNC_FORMAT_VERSION {
                        return Err(SyncError::cli(format!(
                            "unsupported env never-sync policy format `{value}`"
                        )));
                    }
                    format_seen = true;
                }
                "name" => {
                    config.names.insert(value);
                }
                "prefix" => {
                    config.prefixes.insert(value);
                }
                "suffix" => {
                    config.suffixes.insert(value);
                }
                other => {
                    return Err(SyncError::cli(format!(
                        "env never-sync policy contains unknown key `{other}`"
                    )));
                }
            }
        }
        if !format_seen {
            return Err(SyncError::cli("env never-sync policy is missing format"));
        }
        let _ = config.to_policy()?;
        Ok(config)
    }

    fn serialize(&self) -> String {
        let mut output = String::new();
        push_raw_state(&mut output, "format", CLI_ENV_NEVER_SYNC_FORMAT_VERSION);
        for name in &self.names {
            push_raw_state(&mut output, "name", name);
        }
        for prefix in &self.prefixes {
            push_raw_state(&mut output, "prefix", prefix);
        }
        for suffix in &self.suffixes {
            push_raw_state(&mut output, "suffix", suffix);
        }
        output
    }

    fn to_policy(&self) -> Result<EnvNeverSyncPolicy, SyncError> {
        let mut policy = EnvNeverSyncPolicy::allow_all();
        for name in &self.names {
            policy.deny_name(name.clone()).map_err(env_error)?;
        }
        for prefix in &self.prefixes {
            policy.deny_prefix(prefix.clone()).map_err(env_error)?;
        }
        for suffix in &self.suffixes {
            policy.deny_suffix(suffix.clone()).map_err(env_error)?;
        }
        Ok(policy)
    }

    fn rule_count(&self) -> usize {
        self.names.len() + self.prefixes.len() + self.suffixes.len()
    }
}

fn load_cli_env_never_sync_config(
    runtime: &CliRuntime,
) -> Result<CliEnvNeverSyncConfig, SyncError> {
    let path = env_never_sync_path(runtime);
    match fs::read_to_string(&path) {
        Ok(text) => CliEnvNeverSyncConfig::parse(&text),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(CliEnvNeverSyncConfig::default()),
        Err(error) => Err(SyncError::from(ConfigError::io(
            path,
            format!("read env never-sync policy failed: {error}"),
        ))),
    }
}

fn save_cli_env_never_sync_config(
    runtime: &CliRuntime,
    config: &CliEnvNeverSyncConfig,
) -> Result<(), SyncError> {
    fs::create_dir_all(&runtime.config.cache_dir).map_err(|error| {
        SyncError::from(ConfigError::io(
            runtime.config.cache_dir.clone(),
            format!("create cache dir failed: {error}"),
        ))
    })?;
    let path = env_never_sync_path(runtime);
    fs::write(&path, config.serialize()).map_err(|error| {
        SyncError::from(ConfigError::io(
            path,
            format!("write env never-sync policy failed: {error}"),
        ))
    })
}

fn env_never_sync_path(runtime: &CliRuntime) -> PathBuf {
    let root = daemon_state_root_identity(runtime);
    let project = safe_filename_component(&runtime.project_id);
    let root_text = root.to_string_lossy();
    let identity_hash = stable_identity_hash(&runtime.project_id, root_text.as_bytes());
    runtime.config.cache_dir.join(format!(
        "{CLI_ENV_NEVER_SYNC_FILE_STEM}-{project}-{identity_hash}.{CLI_ENV_NEVER_SYNC_FILE_EXTENSION}"
    ))
}

fn append_env_never_sync_config(output: &mut String, config: &CliEnvNeverSyncConfig) {
    push_kv(output, "never_sync_rules", config.rule_count());
    push_kv(output, "name_count", config.names.len());
    push_kv(output, "prefix_count", config.prefixes.len());
    push_kv(output, "suffix_count", config.suffixes.len());
    for (index, name) in config.names.iter().enumerate() {
        push_kv(output, &format!("never_sync.name.{}", index + 1), name);
    }
    for (index, prefix) in config.prefixes.iter().enumerate() {
        push_kv(output, &format!("never_sync.prefix.{}", index + 1), prefix);
    }
    for (index, suffix) in config.suffixes.iter().enumerate() {
        push_kv(output, &format!("never_sync.suffix.{}", index + 1), suffix);
    }
}

fn latest_remote_manifest(
    store: &FileBackedSyncStore,
    operations: &[OperationRecord],
) -> Result<Option<TreeManifest>, SyncError> {
    let Some(operation) = operations
        .iter()
        .rev()
        .find(|operation| operation.kind == OperationKind::PutManifest)
    else {
        return Ok(None);
    };
    let manifest_id = operation
        .manifest_id
        .as_deref()
        .or(operation.payload_id.as_deref())
        .ok_or_else(|| {
            SyncError::cli(format!(
                "manifest operation `{}` did not include manifest_id or payload_id",
                operation.id
            ))
        })?;
    store
        .fetch_manifest(manifest_id)
        .map(Some)
        .map_err(|error| SyncError::cli(error.to_string()))
}

fn next_sequence(operations: &[OperationRecord]) -> u64 {
    operations
        .iter()
        .map(|operation| operation.sequence)
        .max()
        .unwrap_or(0)
        + 1
}

fn snapshot_from_manifest_with_policy(
    manifest: &TreeManifest,
    policy: &Policy,
    platform: &crate::foundation::Platform,
) -> IndexedSnapshot {
    let entries = manifest
        .entries
        .iter()
        .cloned()
        .map(|entry| {
            let action = policy.evaluate(&entry.path, platform);
            SnapshotEntry::new(entry, PolicyMetadata::from_action(action))
        })
        .collect::<Vec<_>>();
    IndexedSnapshot::new(manifest.project_id.clone(), entries)
}

fn local_delete_rename_events(
    previous_manifest: Option<&TreeManifest>,
    current: &IndexedSnapshot,
    policy: &Policy,
    platform: &crate::foundation::Platform,
) -> Vec<FsEvent> {
    let Some(previous_manifest) = previous_manifest else {
        return Vec::new();
    };
    let previous = snapshot_from_manifest_with_policy(previous_manifest, policy, platform);
    EventQueue::from_snapshots(Some(&previous), current)
        .content_sync_events()
        .filter(|event| matches!(event.kind, EventKind::Deleted | EventKind::Moved))
        .cloned()
        .collect()
}

fn local_event_affected_paths(events: &[FsEvent]) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    for event in events {
        paths.insert(event.path.clone());
        if let Some(previous_path) = &event.previous_path {
            paths.insert(previous_path.clone());
        }
    }
    paths
}

fn convergence_action_touches_any_path(
    action: &ConvergenceAction,
    paths: &BTreeSet<String>,
) -> bool {
    match action {
        ConvergenceAction::MovePath { from, to } => paths.contains(from) || paths.contains(to),
        _ => convergence_action_path(action)
            .map(|path| paths.contains(path))
            .unwrap_or(false),
    }
}

fn convergence_action_descends_from_any_path(
    action: &ConvergenceAction,
    paths: &BTreeSet<String>,
) -> bool {
    match action {
        ConvergenceAction::MovePath { from, to } => {
            path_descends_from_any(from, paths) || path_descends_from_any(to, paths)
        }
        _ => convergence_action_path(action)
            .map(|path| path_descends_from_any(path, paths))
            .unwrap_or(false),
    }
}

fn path_descends_from_any(path: &str, parents: &BTreeSet<String>) -> bool {
    parents.iter().any(|parent| is_descendant_path(path, parent))
}

fn operation_touches_any_path(operation: &OperationRecord, paths: &BTreeSet<String>) -> bool {
    paths.contains(&operation.path)
        || operation
            .previous_path
            .as_ref()
            .map(|path| paths.contains(path))
            .unwrap_or(false)
}

fn plan_current_worktree(
    runtime: &CliRuntime,
    state: &DaemonState,
) -> Result<PlannedWorktree, SyncError> {
    let root = project_root(runtime)?;
    let policy = load_policy(&root)?;
    let snapshot = index_snapshot(runtime, &root, policy.clone())?;
    let store = connect_transport(runtime)?;
    let operations = store
        .load_operation_log()
        .map_err(|error| SyncError::cli(error.to_string()))?;
    let remote_manifest = latest_remote_manifest(&store, &operations)?;
    let replay = replay_operation_log(&operations);
    let engine = ConvergenceEngine::new(runtime.config.machine_id.clone(), runtime.platform.clone());
    let local_events = local_delete_rename_events(
        state.last_local_manifest.as_ref(),
        &snapshot,
        &policy,
        &runtime.platform,
    );
    let start_sequence = next_sequence(&operations);
    let mut plan = engine.plan_snapshot_with_remote_state(
        &snapshot,
        remote_manifest.as_ref(),
        Some(&replay),
        !state.sync_paused,
        start_sequence + local_events.len() as u64,
        &policy,
    );

    if !local_events.is_empty() {
        let affected_paths = local_event_affected_paths(&local_events);
        plan.actions
            .retain(|action| !convergence_action_touches_any_path(action, &affected_paths));
        plan.queued_operations
            .retain(|operation| !operation_touches_any_path(operation, &affected_paths));

        let mut actions = Vec::new();
        let mut queued_operations = Vec::new();
        for (event_sequence, event) in (start_sequence..).zip(local_events.iter()) {
            let event_plan = engine.plan_event(
                &runtime.project_id,
                event,
                !state.sync_paused,
                event_sequence,
            );
            actions.extend(event_plan.actions);
            queued_operations.extend(event_plan.queued_operations);
        }
        actions.extend(plan.actions);
        queued_operations.extend(plan.queued_operations);
        plan = ConvergencePlan {
            actions,
            queued_operations,
        };
    }

    Ok(PlannedWorktree {
        root,
        policy,
        snapshot,
        store,
        operations,
        remote_manifest,
        remote_state: replay,
        plan,
        local_events,
    })
}

fn worktree_report_from_plan(state: &DaemonState, planned: &PlannedWorktree) -> WorktreeReport {
    let fetch_count = planned
        .plan
        .actions
        .iter()
        .filter(|action| matches!(action, ConvergenceAction::FetchContent { .. }))
        .count();
    let push_count = planned
        .plan
        .actions
        .iter()
        .filter(|action| matches!(action, ConvergenceAction::PushContent { .. }))
        .count();
    let planned_queue_count = planned_queue_count(&planned.plan);
    let unsupported_count = unsupported_foreground_action_count(
        &planned.plan,
        planned.remote_manifest.as_ref(),
    );
    let stale = planned_queue_count > 0
        || unsupported_count > 0
        || planned
            .plan
            .actions
            .iter()
            .any(foreground_action_requires_completion);

    WorktreeReport {
        status: if stale {
            WorktreeStatus::Stale
        } else if planned.remote_manifest.is_none() {
            WorktreeStatus::Unknown
        } else {
            WorktreeStatus::Clean
        },
        remote_manifest_id: planned.remote_manifest.as_ref().map(|manifest| manifest.id.clone()),
        action_count: planned.plan.actions.len(),
        fetch_count,
        push_count,
        queued_count: state.queued_operations.max(planned_queue_count),
        unsupported_count,
    }
}

fn execute_foreground_sync_pass(
    runtime: &CliRuntime,
    state: &DaemonState,
) -> Result<(WorktreeReport, usize, usize, TreeManifest), SyncError> {
    let planned = plan_current_worktree(runtime, state)?;
    execute_planned_foreground_sync_pass(runtime, state, planned)
}

fn execute_planned_foreground_sync_pass(
    runtime: &CliRuntime,
    state: &DaemonState,
    planned: PlannedWorktree,
) -> Result<(WorktreeReport, usize, usize, TreeManifest), SyncError> {
    if planned_queue_count(&planned.plan) > 0 {
        let report = worktree_report_from_plan(state, &planned);
        let unsupported_count = report.unsupported_count;
        return Ok((report, 0, unsupported_count, planned.snapshot.manifest));
    }

    let blocked_conflict_paths = unresolved_conflict_sidecar_paths(&planned.plan);
    let mut preserved_manifest_paths = unsupported_foreground_action_paths(
        &planned.plan,
        planned.remote_manifest.as_ref(),
    );
    preserved_manifest_paths.extend(policy_suppressed_manifest_paths(&planned.plan));
    let mut next_operation_sequence = next_sequence(&planned.operations);
    let mut applied_actions = 0usize;
    let mut remote_changed = false;
    let mut local_tree_changed_by_remote = false;
    let mut deleted_local_tree_roots = BTreeSet::new();

    for action in &planned.plan.actions {
        if convergence_action_touches_any_path(action, &blocked_conflict_paths) {
            continue;
        }
        if convergence_action_descends_from_any_path(action, &deleted_local_tree_roots) {
            continue;
        }

        match action {
            ConvergenceAction::PushContent {
                path,
                source_content_hash,
            } => {
                push_foreground_content(
                    runtime,
                    &planned.root,
                    &planned.snapshot,
                    &planned.store,
                    path,
                    source_content_hash.as_deref(),
                    &mut next_operation_sequence,
                )?;
                applied_actions += 1;
                remote_changed = true;
            }
            ConvergenceAction::FetchContent {
                path,
                store_blob_id: Some(store_blob_id),
                temp_suffix,
            } => {
                let outcome = fetch_project_content(
                    &planned.root,
                    &planned.store,
                    path,
                    store_blob_id,
                    temp_suffix,
                    "create foreground sync parent failed",
                )?;
                match outcome {
                    ProjectFetchOutcome::Materialized => {
                        applied_actions += 1;
                        local_tree_changed_by_remote = true;
                    }
                    ProjectFetchOutcome::PreservedExisting => {
                        preserved_manifest_paths.insert(path.clone());
                    }
                }
            }
            ConvergenceAction::DeleteLocal { path } => {
                let delete_local_was_directory = planned
                    .snapshot
                    .entry(path)
                    .map(|entry| entry.catalog_entry.kind == TreeEntryKind::Directory)
                    .unwrap_or(false);
                let delete_local_applied = match delete_foreground_local_path(
                    runtime,
                    &planned.root,
                    &planned.policy,
                    &planned.snapshot,
                    &planned.remote_state,
                    path,
                )? {
                    ForegroundDeleteLocalOutcome::Removed => {
                        applied_actions += 1;
                        local_tree_changed_by_remote = true;
                        true
                    }
                    ForegroundDeleteLocalOutcome::AlreadyAbsent => {
                        local_tree_changed_by_remote = true;
                        true
                    }
                    ForegroundDeleteLocalOutcome::PreservedChanged => {
                        preserved_manifest_paths.insert(path.clone());
                        false
                    }
                };
                if delete_local_applied && delete_local_was_directory {
                    deleted_local_tree_roots.insert(path.clone());
                }
            }
            ConvergenceAction::DeleteRemote { .. } | ConvergenceAction::MovePath { .. } => {
                record_foreground_local_event_operation(
                    runtime,
                    &planned.store,
                    &planned.local_events,
                    action,
                    &mut next_operation_sequence,
                )?;
                applied_actions += 1;
                remote_changed = true;
            }
            ConvergenceAction::PropagatePermissions { path, permissions } => {
                let has_push_content = plan_has_push_content(&planned.plan, path);
                match foreground_permission_resolution(
                    &planned.snapshot,
                    planned.remote_manifest.as_ref(),
                    path,
                    *permissions,
                ) {
                    ForegroundPermissionResolution::LocalWins if has_push_content => {}
                    ForegroundPermissionResolution::LocalWins => {
                        record_foreground_permissions(
                            runtime,
                            &planned.snapshot,
                            &planned.store,
                            path,
                            *permissions,
                            &mut next_operation_sequence,
                        )?;
                        applied_actions += 1;
                        remote_changed = true;
                    }
                    ForegroundPermissionResolution::RemoteWins { kind } => {
                        let created_directory = if kind == TreeEntryKind::Directory {
                            create_foreground_directory(&planned.root, path)?;
                            true
                        } else {
                            false
                        };
                        let applied_permissions = apply_foreground_permissions(
                            runtime,
                            &planned.root,
                            path,
                            *permissions,
                        )?;
                        if applied_permissions || created_directory {
                            applied_actions += 1;
                        }
                        if !applied_permissions {
                            preserved_manifest_paths.insert(path.clone());
                        }
                    }
                    ForegroundPermissionResolution::Unsupported => {}
                }
            }
            ConvergenceAction::Noop { .. }
            | ConvergenceAction::Ignore { .. }
            | ConvergenceAction::PlatformPinAccepted { .. }
            | ConvergenceAction::PlatformPinRedirected { .. }
            | ConvergenceAction::GitMetadataLocalOnly { .. }
            | ConvergenceAction::RebuildLocally { .. } => {}
            ConvergenceAction::QueueOffline { .. } => {}
            unsupported if !foreground_action_supported(unsupported, planned.remote_manifest.as_ref()) => {}
            unsupported => {
                return Err(SyncError::cli(format!(
                    "unsupported foreground convergence action `{}`",
                    convergence_action_label(unsupported)
                )));
            }
        }
    }

    let mut final_state = state.clone();
    if remote_changed {
        let snapshot = index_snapshot(runtime, &planned.root, planned.policy.clone())?;
        publish_foreground_manifest(
            runtime,
            &planned.root,
            &snapshot,
            &planned.store,
            planned.remote_manifest.as_ref(),
            &preserved_manifest_paths,
            &mut next_operation_sequence,
        )?;
        final_state.last_local_manifest = Some(snapshot.manifest);
    } else if local_tree_changed_by_remote {
        let snapshot = index_snapshot(runtime, &planned.root, planned.policy.clone())?;
        final_state.last_local_manifest = Some(snapshot.manifest);
    }
    if !final_state.sync_paused {
        final_state.queued_operations = 0;
    }

    let final_plan = plan_current_worktree(runtime, &final_state)?;
    let final_report = worktree_report_from_plan(&final_state, &final_plan);
    let final_unsupported_count = final_report.unsupported_count;
    Ok((
        final_report,
        applied_actions,
        final_unsupported_count,
        final_plan.snapshot.manifest,
    ))
}

fn foreground_pass_fully_converged(report: &WorktreeReport, unsupported_actions: usize) -> bool {
    report.queued_count == 0
        && unsupported_actions == 0
        && report.status != WorktreeStatus::Stale
}

fn foreground_pass_error(
    unsupported_actions: usize,
    queued_operations: usize,
    stale_worktree: bool,
) -> Option<String> {
    if unsupported_actions > 0 {
        Some(format!(
            "unsupported foreground convergence actions remain: {unsupported_actions}"
        ))
    } else if queued_operations > 0 {
        Some(QUEUED_OPERATIONS_UNDRAINED.to_owned())
    } else if stale_worktree {
        Some("foreground sync pass left convergence actions pending".to_owned())
    } else {
        None
    }
}

fn planned_queue_count(plan: &ConvergencePlan) -> usize {
    let queue_offline_count = plan
        .actions
        .iter()
        .filter(|action| matches!(action, ConvergenceAction::QueueOffline { .. }))
        .count();
    plan.queued_operations.len().max(queue_offline_count)
}

fn convergence_plan_has_pending_work(
    plan: &ConvergencePlan,
    remote_manifest: Option<&TreeManifest>,
) -> bool {
    planned_queue_count(plan) > 0
        || unsupported_foreground_action_count(plan, remote_manifest) > 0
        || plan.actions.iter().any(foreground_action_requires_completion)
}

fn unsupported_foreground_action_count(
    plan: &ConvergencePlan,
    remote_manifest: Option<&TreeManifest>,
) -> usize {
    plan.actions
        .iter()
        .filter(|action| !foreground_action_supported(action, remote_manifest))
        .count()
}

fn unsupported_foreground_action_paths(
    plan: &ConvergencePlan,
    remote_manifest: Option<&TreeManifest>,
) -> BTreeSet<String> {
    plan.actions
        .iter()
        .filter(|action| !foreground_action_supported(action, remote_manifest))
        .filter_map(convergence_action_path)
        .map(str::to_owned)
        .collect()
}

fn unresolved_conflict_sidecar_paths(plan: &ConvergencePlan) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    for action in &plan.actions {
        if let ConvergenceAction::ConflictSidecar {
            path, sidecar_path, ..
        } = action
        {
            paths.insert(path.clone());
            paths.insert(sidecar_path.clone());
        }
    }
    paths
}

fn policy_suppressed_manifest_paths(plan: &ConvergencePlan) -> BTreeSet<String> {
    let explicit_remote_removals = plan
        .actions
        .iter()
        .filter_map(|action| match action {
            ConvergenceAction::DeleteRemote { path } => Some(path.as_str()),
            ConvergenceAction::MovePath { from, .. } => Some(from.as_str()),
            _ => None,
        })
        .collect::<BTreeSet<_>>();

    plan.actions
        .iter()
        .filter_map(|action| match action {
            ConvergenceAction::Ignore { path }
            | ConvergenceAction::RebuildLocally { path }
            | ConvergenceAction::PlatformPinAccepted { path }
            | ConvergenceAction::PlatformPinRedirected { path, .. }
            | ConvergenceAction::GitMetadataLocalOnly { path } => Some(path.clone()),
            _ => None,
        })
        .filter(|path| !explicit_remote_removals.contains(path.as_str()))
        .collect()
}

fn convergence_action_path(action: &ConvergenceAction) -> Option<&str> {
    match action {
        ConvergenceAction::PushContent { path, .. }
        | ConvergenceAction::FetchContent { path, .. }
        | ConvergenceAction::FetchSymlinkTargetMetadata { path, .. }
        | ConvergenceAction::DeleteLocal { path }
        | ConvergenceAction::DeleteRemote { path }
        | ConvergenceAction::PropagatePermissions { path, .. }
        | ConvergenceAction::PropagateSymlink { path, .. }
        | ConvergenceAction::RebuildLocally { path }
        | ConvergenceAction::Ignore { path }
        | ConvergenceAction::PlatformPinAccepted { path }
        | ConvergenceAction::PlatformPinRedirected { path, .. }
        | ConvergenceAction::GitMetadataLocalOnly { path }
        | ConvergenceAction::QueueOffline { path, .. }
        | ConvergenceAction::ConflictSidecar { path, .. } => Some(path),
        ConvergenceAction::MovePath { to, .. } => Some(to),
        ConvergenceAction::Noop { .. } => None,
    }
}

fn foreground_action_supported(
    action: &ConvergenceAction,
    _remote_manifest: Option<&TreeManifest>,
) -> bool {
    match action {
        ConvergenceAction::PushContent { .. } => true,
        ConvergenceAction::FetchContent {
            store_blob_id: Some(_),
            ..
        } => true,
        ConvergenceAction::DeleteLocal { .. }
        | ConvergenceAction::DeleteRemote { .. }
        | ConvergenceAction::MovePath { .. } => true,
        ConvergenceAction::PropagatePermissions { .. } => true,
        ConvergenceAction::Noop { .. }
        | ConvergenceAction::Ignore { .. }
        | ConvergenceAction::PlatformPinAccepted { .. }
        | ConvergenceAction::PlatformPinRedirected { .. }
        | ConvergenceAction::GitMetadataLocalOnly { .. }
        | ConvergenceAction::RebuildLocally { .. }
        | ConvergenceAction::QueueOffline { .. } => true,
        ConvergenceAction::FetchContent { store_blob_id: None, .. }
        | ConvergenceAction::FetchSymlinkTargetMetadata { .. }
        | ConvergenceAction::PropagateSymlink { .. }
        | ConvergenceAction::ConflictSidecar { .. } => false,
    }
}

fn foreground_action_requires_completion(action: &ConvergenceAction) -> bool {
    matches!(
        action,
        ConvergenceAction::PushContent { .. }
            | ConvergenceAction::FetchContent { .. }
            | ConvergenceAction::FetchSymlinkTargetMetadata { .. }
            | ConvergenceAction::DeleteLocal { .. }
            | ConvergenceAction::DeleteRemote { .. }
            | ConvergenceAction::MovePath { .. }
            | ConvergenceAction::PropagatePermissions { .. }
            | ConvergenceAction::PropagateSymlink { .. }
            | ConvergenceAction::QueueOffline { .. }
            | ConvergenceAction::ConflictSidecar { .. }
    )
}

fn push_foreground_content(
    runtime: &CliRuntime,
    root: &Path,
    snapshot: &crate::watcher::IndexedSnapshot,
    store: &FileBackedSyncStore,
    path: &str,
    source_content_hash: Option<&str>,
    next_operation_sequence: &mut u64,
) -> Result<(), SyncError> {
    let relative = normalize_relative_cli_path(path)?;
    let entry = snapshot.entry(&relative).ok_or_else(|| {
        SyncError::cli(format!(
            "planned push path `{relative}` was not present in the indexed snapshot"
        ))
    })?;
    if entry.catalog_entry.kind != TreeEntryKind::File {
        return Err(SyncError::cli(format!(
            "planned push path `{relative}` is not a file"
        )));
    }
    let final_path = resolve_project_path(root, &relative)?;
    let bytes = fs::read(&final_path).map_err(|error| {
        SyncError::from(ConfigError::io(
            final_path.clone(),
            format!("read content for foreground push failed: {error}"),
        ))
    })?;
    let store_blob_id = store_blob_id_for_content(&bytes);
    store
        .put_content_blob(&store_blob_id, &bytes)
        .map_err(|error| SyncError::cli(error.to_string()))?;
    let source_hash = source_content_hash
        .map(str::to_owned)
        .or_else(|| entry.catalog_entry.content_hash.clone())
        .ok_or_else(|| {
            SyncError::cli(format!(
                "planned push path `{relative}` did not include a source content hash"
            ))
        })?;
    let operation = OperationRecord::from_draft(
        crate::sync::OperationDraft::new(
            *next_operation_sequence,
            runtime.project_id.clone(),
            runtime.config.machine_id.clone(),
            OperationKind::PutContent,
            relative,
        )
        .content_hash(source_hash)
        .payload_id(store_blob_id)
        .modified_unix_millis(entry.catalog_entry.modified_unix_millis)
        .permissions(entry.catalog_entry.permissions),
    );
    *next_operation_sequence += 1;
    store
        .append_operation(&operation)
        .map_err(|error| SyncError::cli(error.to_string()))?;
    Ok(())
}

fn record_foreground_permissions(
    runtime: &CliRuntime,
    snapshot: &crate::watcher::IndexedSnapshot,
    store: &FileBackedSyncStore,
    path: &str,
    permissions: u32,
    next_operation_sequence: &mut u64,
) -> Result<(), SyncError> {
    let relative = normalize_relative_cli_path(path)?;
    let entry = snapshot.entry(&relative).ok_or_else(|| {
        SyncError::cli(format!(
            "planned permissions path `{relative}` was not present in the indexed snapshot"
        ))
    })?;
    let operation = OperationRecord::from_draft(
        crate::sync::OperationDraft::new(
            *next_operation_sequence,
            runtime.project_id.clone(),
            runtime.config.machine_id.clone(),
            OperationKind::PermissionChanged,
            relative,
        )
        .modified_unix_millis(entry.catalog_entry.modified_unix_millis)
        .permissions(permissions),
    );
    *next_operation_sequence += 1;
    store
        .append_operation(&operation)
        .map_err(|error| SyncError::cli(error.to_string()))?;
    Ok(())
}

fn local_event_matches_action(event: &FsEvent, action: &ConvergenceAction) -> bool {
    match (event.kind, action) {
        (EventKind::Deleted, ConvergenceAction::DeleteRemote { path }) => {
            event.path.as_str() == path.as_str()
        }
        (EventKind::Moved, ConvergenceAction::MovePath { from, to }) => {
            event.previous_path.as_deref() == Some(from.as_str())
                && event.path.as_str() == to.as_str()
        }
        _ => false,
    }
}

fn record_foreground_local_event_operation(
    runtime: &CliRuntime,
    store: &FileBackedSyncStore,
    events: &[FsEvent],
    action: &ConvergenceAction,
    next_operation_sequence: &mut u64,
) -> Result<(), SyncError> {
    let event = events
        .iter()
        .find(|event| local_event_matches_action(event, action))
        .ok_or_else(|| {
            SyncError::cli(format!(
                "foreground action `{}` did not have a matching durable watcher event",
                convergence_action_label(action)
            ))
        })?;
    let operation = OperationRecord::from_watcher_event(
        runtime.project_id.clone(),
        runtime.config.machine_id.clone(),
        *next_operation_sequence,
        event,
    );
    *next_operation_sequence += 1;
    store
        .append_operation(&operation)
        .map_err(|error| SyncError::cli(error.to_string()))?;
    Ok(())
}

fn plan_has_push_content(plan: &ConvergencePlan, path: &str) -> bool {
    plan.actions.iter().any(|action| {
        matches!(
            action,
            ConvergenceAction::PushContent { path: action_path, .. } if action_path == path
        )
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForegroundDeleteLocalOutcome {
    Removed,
    AlreadyAbsent,
    PreservedChanged,
}

fn delete_foreground_local_path(
    runtime: &CliRuntime,
    root: &Path,
    policy: &Policy,
    planned_snapshot: &IndexedSnapshot,
    remote_state: &ReplayState,
    path: &str,
) -> Result<ForegroundDeleteLocalOutcome, SyncError> {
    let relative = normalize_relative_cli_path(path)?;
    let Some(planned_entry) = planned_snapshot.entry(&relative) else {
        return Ok(ForegroundDeleteLocalOutcome::AlreadyAbsent);
    };
    if !remote_tombstone_obsoletes_snapshot_entry(remote_state, planned_entry) {
        return Ok(ForegroundDeleteLocalOutcome::PreservedChanged);
    }

    let current_snapshot = index_snapshot(runtime, root, policy.clone())?;
    let Some(current_entry) = current_snapshot.entry(&relative) else {
        return Ok(ForegroundDeleteLocalOutcome::AlreadyAbsent);
    };
    if current_entry != planned_entry
        || !remote_tombstone_obsoletes_snapshot_entry(remote_state, current_entry)
        || !foreground_delete_descendants_safe(remote_state, &current_snapshot, &relative)
    {
        return Ok(ForegroundDeleteLocalOutcome::PreservedChanged);
    }

    let target = resolve_project_path(root, &relative)?;
    let metadata = match fs::symlink_metadata(&target) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(ForegroundDeleteLocalOutcome::AlreadyAbsent);
        }
        Err(error) => {
            return Err(SyncError::from(ConfigError::io(
                target,
                format!("inspect local tombstone target failed: {error}"),
            )));
        }
    };
    remove_foreground_delete_target(&target, metadata.file_type().is_dir())
}

fn foreground_delete_descendants_safe(
    remote_state: &ReplayState,
    snapshot: &IndexedSnapshot,
    delete_root: &str,
) -> bool {
    snapshot
        .entries
        .iter()
        .filter(|entry| is_descendant_path(entry.path(), delete_root))
        .all(|entry| {
            entry.allows_content_sync()
                && remote_tombstone_obsoletes_snapshot_entry_at_or_under(
                    remote_state,
                    entry,
                    delete_root,
                )
        })
}

fn is_descendant_path(path: &str, parent: &str) -> bool {
    path.len() > parent.len()
        && path.starts_with(parent)
        && path.as_bytes().get(parent.len()) == Some(&b'/')
}

fn remote_tombstone_obsoletes_snapshot_entry(
    remote_state: &ReplayState,
    entry: &SnapshotEntry,
) -> bool {
    remote_state
        .tombstones
        .get(entry.path())
        .map(|tombstone| tombstone.modified_unix_millis >= entry.catalog_entry.modified_unix_millis)
        .unwrap_or(false)
}

fn remote_tombstone_obsoletes_snapshot_entry_at_or_under(
    remote_state: &ReplayState,
    entry: &SnapshotEntry,
    delete_root: &str,
) -> bool {
    remote_state
        .tombstones
        .get(entry.path())
        .or_else(|| remote_state.tombstones.get(delete_root))
        .map(|tombstone| tombstone.modified_unix_millis >= entry.catalog_entry.modified_unix_millis)
        .unwrap_or(false)
}

fn remove_foreground_delete_target(
    path: &Path,
    is_directory: bool,
) -> Result<ForegroundDeleteLocalOutcome, SyncError> {
    let result = if is_directory {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    match result {
        Ok(()) => Ok(ForegroundDeleteLocalOutcome::Removed),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Ok(ForegroundDeleteLocalOutcome::AlreadyAbsent)
        }
        Err(error) => Err(SyncError::from(ConfigError::io(
            path.to_path_buf(),
            format!("delete local tombstone target failed: {error}"),
        ))),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectFetchOutcome {
    Materialized,
    PreservedExisting,
}

fn fetch_project_content(
    root: &Path,
    store: &FileBackedSyncStore,
    path: &str,
    store_blob_id: &str,
    temp_suffix: &str,
    parent_error: &str,
) -> Result<ProjectFetchOutcome, SyncError> {
    let bytes = store
        .fetch_content_blob(store_blob_id)
        .map_err(|error| SyncError::cli(error.to_string()))?;
    let final_path = resolve_project_path(root, path)?;
    if !temp_suffix.is_empty() && project_path_entry_exists(&final_path)? {
        return Ok(ProjectFetchOutcome::PreservedExisting);
    }
    if let Some(parent) = final_path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            SyncError::from(ConfigError::io(
                parent.to_path_buf(),
                format!("{parent_error}: {error}"),
            ))
        })?;
    }
    write_content_atomically(&final_path, &bytes).map_err(|error| SyncError::cli(error.to_string()))?;
    Ok(ProjectFetchOutcome::Materialized)
}

fn project_path_entry_exists(path: &Path) -> Result<bool, SyncError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(SyncError::from(ConfigError::io(
            path.to_path_buf(),
            format!("inspect sync target failed: {error}"),
        ))),
    }
}

fn create_foreground_directory(root: &Path, path: &str) -> Result<(), SyncError> {
    let directory = resolve_project_path(root, path)?;
    fs::create_dir_all(&directory).map_err(|error| {
        SyncError::from(ConfigError::io(
            directory,
            format!("create foreground sync directory failed: {error}"),
        ))
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForegroundPermissionResolution {
    LocalWins,
    RemoteWins { kind: TreeEntryKind },
    Unsupported,
}

fn foreground_permission_resolution(
    snapshot: &crate::watcher::IndexedSnapshot,
    remote_manifest: Option<&TreeManifest>,
    path: &str,
    permissions: u32,
) -> ForegroundPermissionResolution {
    let local_entry = snapshot.entry(path);
    let remote_entry = remote_manifest_entry(remote_manifest, path);
    let kind = local_entry
        .map(|entry| entry.catalog_entry.kind)
        .or_else(|| remote_entry.map(|entry| entry.kind));

    if kind == Some(TreeEntryKind::Symlink) {
        return ForegroundPermissionResolution::Unsupported;
    }

    if local_entry
        .map(|entry| entry.catalog_entry.permissions == permissions)
        .unwrap_or(false)
    {
        ForegroundPermissionResolution::LocalWins
    } else if let Some(kind) = kind {
        ForegroundPermissionResolution::RemoteWins { kind }
    } else {
        ForegroundPermissionResolution::Unsupported
    }
}

fn apply_foreground_permissions(
    runtime: &CliRuntime,
    root: &Path,
    path: &str,
    permissions: u32,
) -> Result<bool, SyncError> {
    if !runtime.platform.capabilities.supports_posix_permissions {
        return Ok(false);
    }
    apply_posix_permissions(root, path, permissions)
}

#[cfg(unix)]
fn apply_posix_permissions(root: &Path, path: &str, permissions: u32) -> Result<bool, SyncError> {
    use std::os::unix::fs::PermissionsExt;

    let final_path = resolve_project_path(root, path)?;
    let metadata = fs::symlink_metadata(&final_path).map_err(|error| {
        SyncError::from(ConfigError::io(
            final_path.clone(),
            format!("read foreground sync permissions target failed: {error}"),
        ))
    })?;
    if metadata.file_type().is_symlink() {
        return Ok(false);
    }
    let mut current_permissions = metadata.permissions();
    current_permissions.set_mode(permissions & 0o7777);
    fs::set_permissions(&final_path, current_permissions).map_err(|error| {
        SyncError::from(ConfigError::io(
            final_path,
            format!("apply foreground sync permissions failed: {error}"),
        ))
    })?;
    Ok(true)
}

#[cfg(not(unix))]
fn apply_posix_permissions(_root: &Path, _path: &str, _permissions: u32) -> Result<bool, SyncError> {
    Ok(false)
}

fn publish_foreground_manifest(
    runtime: &CliRuntime,
    root: &Path,
    snapshot: &crate::watcher::IndexedSnapshot,
    store: &FileBackedSyncStore,
    remote_manifest: Option<&TreeManifest>,
    preserved_manifest_paths: &BTreeSet<String>,
    next_operation_sequence: &mut u64,
) -> Result<String, SyncError> {
    let entries = foreground_manifest_entries(
        root,
        snapshot,
        store,
        remote_manifest,
        preserved_manifest_paths,
    )?;
    let manifest_id = foreground_manifest_id(&runtime.project_id, &entries);
    let manifest = TreeManifest::new(manifest_id.clone(), runtime.project_id.clone(), entries);
    store
        .put_manifest(&manifest)
        .map_err(|error| SyncError::cli(error.to_string()))?;
    let operation = OperationRecord::from_draft(
        crate::sync::OperationDraft::new(
            *next_operation_sequence,
            runtime.project_id.clone(),
            runtime.config.machine_id.clone(),
            OperationKind::PutManifest,
            "manifest",
        )
        .manifest_id(manifest_id.clone())
        .payload_id(manifest_id.clone())
        .modified_unix_millis(*next_operation_sequence),
    );
    *next_operation_sequence += 1;
    store
        .append_operation(&operation)
        .map_err(|error| SyncError::cli(error.to_string()))?;
    Ok(manifest_id)
}

fn foreground_manifest_entries(
    root: &Path,
    snapshot: &crate::watcher::IndexedSnapshot,
    store: &FileBackedSyncStore,
    remote_manifest: Option<&TreeManifest>,
    preserved_manifest_paths: &BTreeSet<String>,
) -> Result<Vec<TreeEntry>, SyncError> {
    let mut entries = remote_manifest
        .map(|manifest| {
            manifest
                .entries
                .iter()
                .filter(|entry| preserved_manifest_paths.contains(&entry.path))
                .map(|entry| (entry.path.clone(), entry.clone()))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();

    for entry in snapshot.content_sync_entries() {
        if preserved_manifest_paths.contains(entry.path()) {
            continue;
        }

        let manifest_entry = match entry.catalog_entry.kind {
            TreeEntryKind::File => {
                let path = resolve_project_path(root, entry.path())?;
                let bytes = fs::read(&path).map_err(|error| {
                    SyncError::from(ConfigError::io(
                        path.clone(),
                        format!("read content for foreground manifest failed: {error}"),
                    ))
                })?;
                let store_blob_id = store_blob_id_for_content(&bytes);
                store
                    .put_content_blob(&store_blob_id, &bytes)
                    .map_err(|error| SyncError::cli(error.to_string()))?;
                TreeEntry::file(
                    entry.path(),
                    entry.catalog_entry.size_bytes,
                    entry.catalog_entry.modified_unix_millis,
                    entry.catalog_entry.permissions,
                    Some(store_blob_id),
                )
            }
            TreeEntryKind::Directory => TreeEntry::directory(
                entry.path(),
                entry.catalog_entry.modified_unix_millis,
                entry.catalog_entry.permissions,
            ),
            TreeEntryKind::Symlink => TreeEntry::symlink(
                entry.path(),
                entry.catalog_entry.modified_unix_millis,
                entry.catalog_entry.permissions,
                entry.catalog_entry.content_hash.clone(),
            ),
        };
        entries.insert(entry.path().to_owned(), manifest_entry);
    }

    Ok(entries.into_values().collect())
}

fn foreground_manifest_id(project_id: &str, entries: &[TreeEntry]) -> String {
    let mut hash = stable_hash_init();
    stable_hash_update(&mut hash, b"foreground-manifest-v1");
    stable_hash_update(&mut hash, project_id.as_bytes());
    let mut ordered = entries.iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.kind.cmp(&right.kind))
            .then_with(|| left.size_bytes.cmp(&right.size_bytes))
            .then_with(|| left.modified_unix_millis.cmp(&right.modified_unix_millis))
            .then_with(|| left.permissions.cmp(&right.permissions))
            .then_with(|| left.content_hash.cmp(&right.content_hash))
    });
    for entry in ordered {
        stable_hash_update(&mut hash, entry.path.as_bytes());
        stable_hash_update(&mut hash, entry.kind.as_str().as_bytes());
        stable_hash_update(&mut hash, &entry.size_bytes.to_le_bytes());
        stable_hash_update(&mut hash, &entry.modified_unix_millis.to_le_bytes());
        stable_hash_update(&mut hash, &entry.permissions.to_le_bytes());
        if let Some(content_hash) = &entry.content_hash {
            stable_hash_update(&mut hash, content_hash.as_bytes());
        }
    }
    format!("foreground-manifest-{hash:016x}")
}

fn remote_manifest_entry<'a>(
    remote_manifest: Option<&'a TreeManifest>,
    path: &str,
) -> Option<&'a TreeEntry> {
    remote_manifest?
        .entries
        .iter()
        .find(|entry| entry.path == path)
}

fn convergence_action_label(action: &ConvergenceAction) -> &'static str {
    match action {
        ConvergenceAction::PushContent { .. } => "push-content",
        ConvergenceAction::FetchContent { .. } => "fetch-content",
        ConvergenceAction::FetchSymlinkTargetMetadata { .. } => "fetch-symlink-target-metadata",
        ConvergenceAction::DeleteLocal { .. } => "delete-local",
        ConvergenceAction::DeleteRemote { .. } => "delete-remote",
        ConvergenceAction::MovePath { .. } => "move-path",
        ConvergenceAction::PropagatePermissions { .. } => "propagate-permissions",
        ConvergenceAction::PropagateSymlink { .. } => "propagate-symlink",
        ConvergenceAction::RebuildLocally { .. } => "rebuild-locally",
        ConvergenceAction::Ignore { .. } => "ignore",
        ConvergenceAction::PlatformPinAccepted { .. } => "platform-pin-accepted",
        ConvergenceAction::PlatformPinRedirected { .. } => "platform-pin-redirected",
        ConvergenceAction::GitMetadataLocalOnly { .. } => "git-metadata-local-only",
        ConvergenceAction::QueueOffline { .. } => "queue-offline",
        ConvergenceAction::ConflictSidecar { .. } => "conflict-sidecar",
        ConvergenceAction::Noop { .. } => "noop",
    }
}

fn daemon_state_root_identity(runtime: &CliRuntime) -> PathBuf {
    let root = runtime
        .config
        .root_paths
        .first()
        .cloned()
        .unwrap_or_else(|| runtime.current_dir.clone());
    if root.is_absolute() {
        root
    } else {
        runtime.current_dir.join(root)
    }
}

fn safe_filename_component(value: &str) -> String {
    let mut sanitized = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            sanitized.push(ch);
        } else if !sanitized.ends_with('-') {
            sanitized.push('-');
        }
    }
    let sanitized = sanitized.trim_matches('-');
    if sanitized.is_empty() {
        DEFAULT_PROJECT_ID.to_owned()
    } else {
        sanitized.to_owned()
    }
}

fn stable_identity_hash(project_id: &str, root: &[u8]) -> String {
    let mut hash = stable_hash_init();
    stable_hash_update(&mut hash, b"daemon-state-v1");
    stable_hash_update(&mut hash, project_id.as_bytes());
    stable_hash_update(&mut hash, root);
    format!("{hash:016x}")
}

fn stable_hash_init() -> u64 {
    14_695_981_039_346_656_037
}

fn stable_hash_update(hash: &mut u64, bytes: &[u8]) {
    const FNV_PRIME: u64 = 1_099_511_628_211;
    for byte in bytes.iter().copied().chain([0xff]) {
        *hash ^= u64::from(byte);
        *hash = hash.wrapping_mul(FNV_PRIME);
    }
}

fn index_current_project(
    runtime: &CliRuntime,
) -> Result<(PathBuf, Policy, crate::watcher::IndexedSnapshot), SyncError> {
    let root = project_root(runtime)?;
    let policy = load_policy(&root)?;
    let snapshot = index_snapshot(runtime, &root, policy.clone())?;
    Ok((root, policy, snapshot))
}

fn index_snapshot(
    runtime: &CliRuntime,
    root: &Path,
    policy: Policy,
) -> Result<crate::watcher::IndexedSnapshot, SyncError> {
    let indexer = LocalIndexer::new(runtime.project_id.clone(), policy, runtime.platform.clone());
    indexer.index_project_root(root).map_err(SyncError::from)
}

fn project_root(runtime: &CliRuntime) -> Result<PathBuf, SyncError> {
    let root = runtime
        .config
        .root_paths
        .first()
        .cloned()
        .unwrap_or_else(|| runtime.current_dir.clone());
    if root.as_os_str().is_empty() {
        Err(SyncError::cli("project root is empty"))
    } else {
        Ok(root)
    }
}

fn load_policy(root: &Path) -> Result<Policy, SyncError> {
    let path = root.join(SYNCIGNORE_FILE_NAME);
    let project_rules = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(SyncError::from(ConfigError::io(
                path,
                format!("read policy failed: {error}"),
            )))
        }
    };
    Policy::from_syncignore(&project_rules, "").map_err(|error| {
        SyncError::cli(format!(
            "failed to parse {}: {error}",
            root.join(SYNCIGNORE_FILE_NAME).display()
        ))
    })
}

fn permission_probe(cache_dir: &Path) -> Result<(), String> {
    fs::create_dir_all(cache_dir)
        .map_err(|error| format!("create `{}` failed: {error}", cache_dir.display()))?;
    let probe = cache_dir.join("doctor-permissions.probe");
    fs::write(&probe, b"probe")
        .map_err(|error| format!("write `{}` failed: {error}", probe.display()))?;
    let bytes = fs::read(&probe)
        .map_err(|error| format!("read `{}` failed: {error}", probe.display()))?;
    fs::remove_file(&probe)
        .map_err(|error| format!("remove `{}` failed: {error}", probe.display()))?;
    if bytes == b"probe" {
        Ok(())
    } else {
        Err(format!("probe `{}` returned unexpected bytes", probe.display()))
    }
}
struct SyncStoreHydrator<'a> {
    store: &'a FileBackedSyncStore,
}

impl Hydrator for SyncStoreHydrator<'_> {
    fn fetch_content(&mut self, request: HydrationRequest<'_>) -> VfsResult<Vec<u8>> {
        if request.project_id != self.store.project_id() {
            return Err(VfsAccessError::remote_fetch_failed(
                request.path,
                format!(
                    "hydration requested project `{}` from store project `{}`",
                    request.project_id,
                    self.store.project_id()
                ),
            ));
        }
        self.store
            .fetch_content_blob(request.content_hash)
            .map_err(|error| VfsAccessError::remote_fetch_failed(request.path, error.to_string()))
    }
}

fn resolve_project_path(root: &Path, relative: &str) -> Result<PathBuf, SyncError> {
    let relative = normalize_relative_cli_path(relative)?;
    Ok(root.join(relative))
}

fn normalize_relative_cli_path(path: &str) -> Result<String, SyncError> {
    let value = path.trim().replace('\\', "/");
    if value.is_empty() || value == "." {
        return Err(SyncError::cli("path must name a project file"));
    }
    if value.starts_with('/') || value.split('/').any(|part| part == "..") {
        return Err(SyncError::cli(format!(
            "path `{path}` must be relative and must not contain .."
        )));
    }
    Ok(value)
}

fn expect_no_args(command: &str, rest: &[String]) -> Result<(), SyncError> {
    if rest.is_empty() {
        Ok(())
    } else {
        Err(SyncError::cli(format!(
            "command `{command}` does not accept extra arguments: {}",
            rest.join(" ")
        )))
    }
}

fn expect_arg_count(
    command: &str,
    rest: &[String],
    expected_count: usize,
    usage: &str,
) -> Result<(), SyncError> {
    if rest.len() == expected_count {
        Ok(())
    } else {
        Err(SyncError::cli(format!(
            "command `{command}` expects {usage}; received {} argument(s)",
            rest.len()
        )))
    }
}

fn default_project_id(config: &Config, current_dir: &Path) -> String {
    let source = config.root_paths.first().map(PathBuf::as_path).unwrap_or(current_dir);
    let Some(name) = source.file_name().and_then(|name| name.to_str()) else {
        return DEFAULT_PROJECT_ID.to_owned();
    };
    let mut sanitized = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            sanitized.push(ch);
        } else if !sanitized.ends_with('-') {
            sanitized.push('-');
        }
    }
    let sanitized = sanitized.trim_matches('-');
    if sanitized.is_empty() {
        DEFAULT_PROJECT_ID.to_owned()
    } else {
        format!("project-{sanitized}")
    }
}

fn platform_from_process_env(config: &Config) -> Result<Platform, SyncError> {
    let mut platform = Platform::detect();
    let mut override_applied = false;

    if let Some(os_family) = platform_os_override()? {
        platform.os_family = os_family;
        platform.os_version = Some(format!("simulated-{}", platform.os_family.as_str()));
        platform.capabilities = PlatformCapabilities::for_os(&platform.os_family);
        override_applied = true;
    }
    if let Some(architecture) = platform_architecture_override()? {
        platform.architecture = architecture;
        override_applied = true;
    }

    if override_applied {
        platform.machine_id = MachineId {
            value: config.machine_id.clone(),
            provenance: config.machine_id_provenance.clone(),
        };
    }

    Ok(platform)
}

fn platform_os_override() -> Result<Option<OsFamily>, SyncError> {
    let Some(value) = required_trimmed_env(PLATFORM_OS_OVERRIDE_ENV)? else {
        return Ok(None);
    };
    Ok(Some(match value.to_ascii_lowercase().as_str() {
        "linux" => OsFamily::Linux,
        "macos" | "darwin" => OsFamily::Macos,
        "windows" => OsFamily::Windows,
        other => OsFamily::Other(other.to_owned()),
    }))
}

fn platform_architecture_override() -> Result<Option<Architecture>, SyncError> {
    let Some(value) = required_trimmed_env(PLATFORM_ARCH_OVERRIDE_ENV)? else {
        return Ok(None);
    };
    Ok(Some(match value.to_ascii_lowercase().as_str() {
        "x86_64" | "amd64" => Architecture::X86_64,
        "aarch64" | "arm64" => Architecture::Aarch64,
        "arm" => Architecture::Arm,
        other => Architecture::Other(other.to_owned()),
    }))
}

fn required_trimmed_env(key: &str) -> Result<Option<String>, SyncError> {
    match env::var(key) {
        Ok(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                Err(SyncError::cli(format!("{key} must not be empty when set")))
            } else {
                Ok(Some(trimmed.to_owned()))
            }
        }
        Err(env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(SyncError::platform(format!("read {key} failed: {error}"))),
    }
}

fn parse_csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect()
}

fn action_label(action: &Action) -> String {
    match action {
        Action::PlatformPin(pin) => format!(
            "platform-pin:{}:{}",
            pin.os_family.as_str(),
            pin.architecture.as_str()
        ),
        other => other.as_str().to_owned(),
    }
}

fn hydration_status_label(status: &HydrationStatus) -> &'static str {
    match status {
        HydrationStatus::NotHydrated => "not-hydrated",
        HydrationStatus::Hydrating => "hydrating",
        HydrationStatus::Hydrated => "hydrated",
    }
}

fn latency_label(latency: AccessLatency) -> &'static str {
    match latency {
        AccessLatency::MetadataOnly => "metadata-only",
        AccessLatency::CachedContent => "cached-content",
        AccessLatency::RemoteFetch => "remote-fetch",
    }
}

fn bool_state(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

fn push_kv(output: &mut String, key: &str, value: impl fmt::Display) {
    let value = sanitize_output_value(&value.to_string());
    writeln!(output, "{key}={value}").expect("writing to String cannot fail");
}

fn sanitize_output_value(value: &str) -> String {
    value.replace('\n', "\\n").replace('\r', "\\r")
}

fn push_raw_state(output: &mut String, key: &str, value: impl fmt::Display) {
    writeln!(output, "{key}={}", encode_state_value(&value.to_string()))
        .expect("writing to String cannot fail");
}

fn encode_state_value(value: &str) -> String {
    let mut output = String::new();
    for ch in value.chars() {
        match ch {
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            other => output.push(other),
        }
    }
    output
}

fn decode_state_value(value: &str) -> Result<String, SyncError> {
    let mut output = String::new();
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            output.push(ch);
            continue;
        }
        let Some(escaped) = chars.next() else {
            return Err(SyncError::cli("daemon state value ended after escape"));
        };
        match escaped {
            '\\' => output.push('\\'),
            'n' => output.push('\n'),
            'r' => output.push('\r'),
            other => {
                return Err(SyncError::cli(format!(
                    "unsupported daemon state escape `\\{other}`"
                )))
            }
        }
    }
    Ok(output)
}

fn required_state<'a>(fields: &'a BTreeMap<String, String>, key: &str) -> Result<&'a str, SyncError> {
    fields
        .get(key)
        .map(String::as_str)
        .ok_or_else(|| SyncError::cli(format!("daemon state is missing `{key}`")))
}

fn parse_state_bool(value: &str, key: &str) -> Result<bool, SyncError> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(SyncError::cli(format!(
            "daemon state `{key}` expected bool, found `{other}`"
        ))),
    }
}

fn parse_state_usize(value: &str, key: &str) -> Result<usize, SyncError> {
    value.parse::<usize>().map_err(|error| {
        SyncError::cli(format!(
            "daemon state `{key}` expected non-negative integer, found `{value}`: {error}"
        ))
    })
}


fn parse_state_manifest(value: &str) -> Result<TreeManifest, SyncError> {
    let mut format_seen = false;
    let mut manifest_id = None;
    let mut project_id = None;
    let mut entries = Vec::new();

    for (line_index, line) in value.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        let fields = line.split('\t').collect::<Vec<_>>();
        match fields.first().copied() {
            Some("format") if fields.len() == 2 => {
                if format_seen {
                    return Err(SyncError::cli("daemon manifest state has duplicate format line"));
                }
                let format = decode_catalog_manifest_field(fields[1])?;
                if format != CATALOG_MANIFEST_FORMAT_VERSION {
                    return Err(SyncError::cli(format!(
                        "unsupported daemon manifest state format `{format}`"
                    )));
                }
                format_seen = true;
            }
            Some("manifest") if fields.len() == 3 => {
                if manifest_id.is_some() {
                    return Err(SyncError::cli("daemon manifest state has duplicate header line"));
                }
                manifest_id = Some(decode_catalog_manifest_field(fields[1])?);
                project_id = Some(decode_catalog_manifest_field(fields[2])?);
            }
            Some("entry") if fields.len() == 7 => {
                let path = decode_catalog_manifest_field(fields[1])?;
                let kind = match decode_catalog_manifest_field(fields[2])?.as_str() {
                    "directory" => TreeEntryKind::Directory,
                    "file" => TreeEntryKind::File,
                    "symlink" => TreeEntryKind::Symlink,
                    other => {
                        return Err(SyncError::cli(format!(
                            "unknown daemon manifest entry kind `{other}`"
                        )))
                    }
                };
                let size_bytes = parse_state_manifest_u64(
                    &decode_catalog_manifest_field(fields[3])?,
                    "size_bytes",
                )?;
                let modified_unix_millis = parse_state_manifest_u64(
                    &decode_catalog_manifest_field(fields[4])?,
                    "modified_unix_millis",
                )?;
                let permissions = parse_state_manifest_u32(
                    &decode_catalog_manifest_field(fields[5])?,
                    "permissions",
                )?;
                let content_hash = if fields[6] == "-" {
                    None
                } else {
                    Some(decode_catalog_manifest_field(fields[6])?)
                };
                entries.push(TreeEntry::new(
                    path,
                    kind,
                    size_bytes,
                    modified_unix_millis,
                    permissions,
                    content_hash,
                ));
            }
            Some(tag) => {
                return Err(SyncError::cli(format!(
                    "invalid daemon manifest state line {} tagged `{tag}` with {} fields",
                    line_index + 1,
                    fields.len()
                )));
            }
            None => {}
        }
    }

    if !format_seen {
        return Err(SyncError::cli("daemon manifest state is missing format line"));
    }
    let manifest_id = manifest_id
        .ok_or_else(|| SyncError::cli("daemon manifest state is missing header line"))?;
    let project_id = project_id
        .ok_or_else(|| SyncError::cli("daemon manifest state is missing project id"))?;
    Ok(TreeManifest::new(manifest_id, project_id, entries))
}

fn decode_catalog_manifest_field(value: &str) -> Result<String, SyncError> {
    let bytes = value.as_bytes();
    let mut index = 0;
    let mut output = Vec::new();
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(SyncError::cli("truncated daemon manifest percent escape"));
            }
            let high = hex_nibble(bytes[index + 1])?;
            let low = hex_nibble(bytes[index + 2])?;
            output.push((high << 4) | low);
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(output)
        .map_err(|error| SyncError::cli(format!("daemon manifest field is not UTF-8: {error}")))
}

fn hex_nibble(byte: u8) -> Result<u8, SyncError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        other => Err(SyncError::cli(format!(
            "invalid daemon manifest hex byte `{}`",
            other as char
        ))),
    }
}

fn parse_state_manifest_u64(value: &str, name: &str) -> Result<u64, SyncError> {
    value.parse::<u64>().map_err(|error| {
        SyncError::cli(format!(
            "daemon manifest field `{name}` is not a u64 `{value}`: {error}"
        ))
    })
}

fn parse_state_manifest_u32(value: &str, name: &str) -> Result<u32, SyncError> {
    value.parse::<u32>().map_err(|error| {
        SyncError::cli(format!(
            "daemon manifest field `{name}` is not a u32 `{value}`: {error}"
        ))
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::TreeEntry;
    use crate::env::EnvMaterialization;
    use crate::foundation::{
        Architecture, MachineId, MachineIdProvenance, OsFamily, Platform, PlatformCapabilities,
    };
    use crate::sync::{
        app_scoped_machine_id, sync_content_hash, AuthoritativeSyncStore, OperationDraft,
        SyncSubmitResult,
    };
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[derive(Default)]
    struct NullLogger;

    impl Logger for NullLogger {
        fn emit(&self, _event: &LogEvent) -> Result<(), SyncError> {
            Ok(())
        }
    }

    #[test]
    fn cli_commands_report_state_and_errors_instead_of_silent_noops() {
        let fixture = Fixture::new("all-commands");
        fixture.write_file("app.txt", b"hello");
        fixture.write_file(SYNCIGNORE_FILE_NAME, b"ignored.log\n");
        let app_blob = fixture.seed_remote_file("manifest-app", "app.txt", b"hello", 1);

        let init = fixture.run(["dropbox-dev", "init"]).unwrap();
        assert_contains(&init, "init_status=ok");
        assert_contains(&init, "daemon_lifecycle=stopped");

        let start = fixture.run(["dropbox-dev", "sync", "start"]).unwrap();
        assert_contains(&start, "sync_start=foreground-complete");
        assert_contains(&start, "foreground_work=ok");
        assert_contains(&start, "daemon_lifecycle=stopped");

        let pause = fixture.run(["dropbox-dev", "sync", "pause"]).unwrap();
        assert_contains(&pause, "sync_pause=paused");
        assert_contains(&pause, "daemon_lifecycle=stopped");
        assert_contains(&pause, "sync_paused=true");

        let resume = fixture.run(["dropbox-dev", "sync", "resume"]).unwrap();
        assert_contains(&resume, "sync_resume=resumed");
        assert_contains(&resume, "sync_paused=false");

        let catalog = fixture.run(["dropbox-dev", "catalog"]).unwrap();
        assert_contains(&catalog, "catalog_status=ok");
        assert_contains(&catalog, "entry_count=");

        let policy = fixture
            .run(["dropbox-dev", "policy", "node_modules/react/index.js"])
            .unwrap();
        assert_contains(&policy, "policy_status=ok");
        assert_contains(&policy, "action=rebuild-locally");

        let watch = fixture.run(["dropbox-dev", "watch"]).unwrap();
        assert_contains(&watch, "watch_status=ok");
        assert_contains(&watch, "event_count=");

        let hydrate = fixture.run(["dropbox-dev", "hydrate", "app.txt"]).unwrap();
        assert_contains(&hydrate, "hydrate_status=ok");
        assert_contains(&hydrate, "manifest_source=remote");
        assert_contains(&hydrate, &format!("content_hash={app_blob}"));
        assert_contains(&hydrate, "path_hydration_status=hydrated");
        assert_contains(&hydrate, "fetched=true");

        let env = fixture.run(["dropbox-dev", "env"]).unwrap();
        assert_contains(&env, "env_status=ok");
        assert_contains(&env, "redacted_value=<redacted>");

        let sync = fixture.run(["dropbox-dev", "sync"]).unwrap();
        assert_contains(&sync, "sync_status=ok");
        assert_contains(&sync, "transport=ok");

        let status = fixture.run(["dropbox-dev", "status"]).unwrap();
        assert_contains(&status, "status=ok");
        assert_contains(&status, "daemon_supervision=foreground-file-state");

        let version_info = fixture.run(["dropbox-dev", "version-info"]).unwrap();
        assert_contains(&version_info, "daemon_supervision=foreground-file-state");
        assert_contains(&version_info, "daemon_supervision_rationale=");
    }

    #[test]
    fn foreground_file_state_u8_supervision_queues_pause_resume_and_reports_runtime_summaries() {
        let mut daemon = ForegroundDaemon::default();
        let mut store = AuthoritativeSyncStore::new("project");
        let operation = OperationRecord::from_draft(
            OperationDraft::new(
                1,
                "project",
                "machine-a",
                OperationKind::PutContent,
                "queued.txt",
            )
            .content_hash("hash-a")
            .modified_unix_millis(1)
            .permissions(0o644),
        );

        daemon.start();
        daemon.pause().unwrap();
        let queued = daemon
            .submit_operation(operation.clone(), &mut store)
            .expect("paused daemon queues");
        assert!(matches!(queued, SyncSubmitResult::Queued(_)));
        assert_eq!(daemon.state.queued_operations, 1);
        assert_eq!(store.operation_log().len(), 0);

        let drained = daemon.resume(&mut store).expect("resume drains queue");
        assert_eq!(drained, vec![operation.clone()]);
        assert_eq!(store.operation_log(), &[operation]);
        assert!(!daemon.state.sync_paused);
        assert_eq!(daemon.state.queued_operations, 0);

        daemon.record_error("SYNC_IO: injected disk failure");
        daemon.record_hydration_summary(3, 2);
        let mut launcher_environment = BTreeMap::new();
        launcher_environment.insert("API_TOKEN".to_owned(), "secret".to_owned());
        let materialization = EnvMaterialization {
            machine_id: "machine-a".to_owned(),
            launcher_environment,
            session_export: "export API_TOKEN='secret'\n".to_owned(),
            redacted_session_export: "export API_TOKEN=<redacted>\n".to_owned(),
            redacted_log_fields: BTreeMap::from([(
                "API_TOKEN".to_owned(),
                ENV_REDACTED_VALUE.to_owned(),
            )]),
        };
        daemon.record_env_materialization(&materialization, 1);

        assert_eq!(daemon.state.last_error.as_deref(), Some("SYNC_IO: injected disk failure"));
        assert_eq!(daemon.state.hydration_placeholders, 3);
        assert_eq!(daemon.state.hydration_hydrated, 2);
        assert_eq!(daemon.state.env_variables, 1);
        assert_eq!(daemon.state.env_conflicts, 1);
        assert!(U8_DAEMON_SUPERVISION_RATIONALE.contains("foreground"));
        assert!(U8_DAEMON_SUPERVISION_RATIONALE.contains("state"));
        daemon.stop();
        assert_eq!(daemon.state.lifecycle, LifecycleState::Stopped);
        assert!(!daemon.state.sync_paused);
    }

    #[test]
    fn env_export_materializes_transport_env_records_and_persists_counts() {
        let fixture = Fixture::new("env-export-transport");
        let store = connect_transport(&fixture.runtime).unwrap();
        let mut publisher = env_replica(&fixture.runtime).unwrap();
        publisher
            .set_shared(&store, "API_TOKEN", "secret-from-transport", 1_000)
            .unwrap();

        let export = fixture.run(["dropbox-dev", "env", "export"]).unwrap();

        assert_contains(&export, "env_export_status=ok");
        assert_contains(&export, "fetched_payloads=1");
        assert_contains(&export, "applied_records=1");
        assert_contains(&export, "materialized_variables=1");
        assert_contains(&export, "conflict_sidecars=0");
        assert_contains(
            &export,
            "redacted_session_export=# env-session-export-lines-v1\\nexport API_TOKEN=<redacted>\\n",
        );
        assert!(
            !export.contains("secret-from-transport"),
            "env export must not print plaintext secrets; output was:\n{export}"
        );
        let state = DaemonController::new(&fixture.runtime).load_state().unwrap();
        assert_eq!(state.env_variables, 1);
        assert_eq!(state.env_conflicts, 0);
    }

    #[test]
    fn env_cli_exposes_list_audit_publish_override_and_never_sync_management() {
        let fixture = Fixture::new("env-cli-management");
        let machine_id = fixture.runtime.config.machine_id.clone();

        let publish = fixture
            .run(["dropbox-dev", "env", "publish", "API_TOKEN", "super-secret"])
            .unwrap();
        assert_contains(&publish, "env_publish_status=ok");
        assert_contains(&publish, "env_name=API_TOKEN");
        assert_contains(&publish, "scope=shared");
        assert_contains(&publish, "redacted_value=<redacted>");
        assert!(!publish.contains("super-secret"), "publish leaked secret: {publish}");

        let override_output = run_with_runtime(
            vec![
                "dropbox-dev".to_owned(),
                "env".to_owned(),
                "override".to_owned(),
                machine_id.clone(),
                "API_TOKEN".to_owned(),
                "override-secret".to_owned(),
            ],
            &NullLogger,
            &fixture.runtime,
        )
        .unwrap();
        assert_contains(&override_output, "env_override_status=ok");
        assert_contains(&override_output, &format!("target_machine_id={machine_id}"));
        assert_contains(&override_output, "scope=machine:");
        assert_contains(&override_output, "redacted_value=<redacted>");
        assert!(
            !override_output.contains("override-secret"),
            "override leaked secret: {override_output}"
        );

        let list = fixture.run(["dropbox-dev", "env", "list"]).unwrap();
        assert_contains(&list, "env_list_status=ok");
        assert_contains(&list, "record_count=2");
        assert_contains(&list, "materialized_variables=1");
        assert_contains(&list, "env.1.value=<redacted>");
        assert_contains(&list, "scope=shared");
        assert_contains(&list, &format!("scope=machine:{machine_id}"));
        assert!(!list.contains("super-secret"), "list leaked shared secret: {list}");
        assert!(!list.contains("override-secret"), "list leaked override secret: {list}");

        let audit = fixture.run(["dropbox-dev", "env", "audit"]).unwrap();
        assert_contains(&audit, "env_audit_status=ok");
        assert_contains(&audit, "operation=value-applied");
        assert_contains(&audit, "value=<redacted>");
        assert!(!audit.contains("super-secret"), "audit leaked shared secret: {audit}");
        assert!(!audit.contains("override-secret"), "audit leaked override secret: {audit}");

        let add_never_sync = fixture
            .run([
                "dropbox-dev",
                "env",
                "never-sync",
                "add",
                "name",
                "LOCAL_ONLY_TOKEN",
            ])
            .unwrap();
        assert_contains(&add_never_sync, "env_never_sync_status=ok");
        assert_contains(&add_never_sync, "changed=true");
        assert_contains(&add_never_sync, "never_sync.name.1=LOCAL_ONLY_TOKEN");

        let policy = fixture.run(["dropbox-dev", "env", "never-sync", "list"]).unwrap();
        assert_contains(&policy, "env_never_sync_status=ok");
        assert_contains(&policy, "name_count=1");
        assert_contains(&policy, "never_sync.name.1=LOCAL_ONLY_TOKEN");

        let denied = fixture
            .run([
                "dropbox-dev",
                "env",
                "publish",
                "LOCAL_ONLY_TOKEN",
                "local-only-secret",
            ])
            .unwrap_err();
        assert_contains(&denied.to_string(), "never-sync");
        assert!(
            !denied.to_string().contains("local-only-secret"),
            "never-sync error leaked secret: {denied}"
        );

        let remove_never_sync = fixture
            .run([
                "dropbox-dev",
                "env",
                "never-sync",
                "remove",
                "name",
                "LOCAL_ONLY_TOKEN",
            ])
            .unwrap();
        assert_contains(&remove_never_sync, "changed=true");
        assert_contains(&remove_never_sync, "name_count=0");
    }

    #[test]
    fn doctor_checks_real_transport_config_permissions_cache_and_policy() {
        let fixture = Fixture::new("doctor");
        fixture.write_file(SYNCIGNORE_FILE_NAME, b"ignored.log\n");
        let store = connect_transport(&fixture.runtime).unwrap();
        assert_eq!(store.load_operation_log().unwrap().len(), 0);

        let doctor = fixture.run(["dropbox-dev", "doctor"]).unwrap();

        assert_contains(&doctor, "doctor_status=ok");
        assert_contains(&doctor, "config=ok");
        assert_contains(&doctor, "root=ok");
        assert_contains(&doctor, "cache=ok");
        assert_contains(&doctor, "permissions=ok");
        assert_contains(&doctor, "transport=ok");
        assert_contains(&doctor, "policy=ok");
        assert_contains(&doctor, "issue_count=0");
    }

    #[test]
    fn doctor_failure_returns_error_instead_of_success_status() {
        let fixture = Fixture::new("doctor-fail");
        fs::remove_dir_all(&fixture.runtime.config.root_paths[0]).unwrap();

        let error = fixture.run(["dropbox-dev", "doctor"]).unwrap_err();

        assert_eq!(error.code(), "CLI_INVALID_COMMAND");
        assert_contains(&error.to_string(), "doctor_status=fail");
        assert_contains(&error.to_string(), "root=fail");
        assert_contains(&error.to_string(), "issue_count=1");
    }

    #[test]
    fn sync_start_runs_one_foreground_pass_and_persists_stopped_state() {
        let fixture = Fixture::new("foreground-start");
        let bytes = b"remote";
        fixture.seed_remote_file("manifest-start", "remote.txt", bytes, 1);

        let start = fixture.run(["dropbox-dev", "sync", "start"]).unwrap();

        assert_contains(&start, "sync_start=foreground-complete");
        assert_contains(&start, "foreground_work=ok");
        assert_contains(&start, "foreground_actions_applied=2");
        assert_contains(&start, "daemon_lifecycle=stopped");
        assert_contains(&start, "stale_worktree=clean");
        assert_eq!(
            fs::read(fixture.runtime.config.root_paths[0].join("remote.txt")).unwrap(),
            bytes
        );
        let state = DaemonController::new(&fixture.runtime).load_state().unwrap();
        assert_eq!(state.lifecycle, LifecycleState::Stopped);
        assert_eq!(state.stale_worktree, WorktreeStatus::Clean);
        assert_eq!(state.last_error, None);
    }

    #[test]
    fn sync_start_pushes_local_content_to_store_before_reporting_complete() {
        let fixture = Fixture::new("foreground-push");
        let bytes = b"local foreground contents";
        fixture.write_file("local.txt", bytes);

        let start = fixture.run(["dropbox-dev", "sync", "start"]).unwrap();

        assert_contains(&start, "sync_start=foreground-complete");
        assert_contains(&start, "foreground_work=ok");
        assert_contains(&start, "foreground_actions_applied=1");
        assert_contains(&start, "stale_worktree=clean");
        let store = connect_transport(&fixture.runtime).unwrap();
        let operations = store.load_operation_log().unwrap();
        assert!(operations.iter().any(|operation| {
            operation.kind == OperationKind::PutContent
                && operation.path == "local.txt"
                && operation.payload_id.as_deref() == Some(sync_content_hash(bytes).as_str())
        }));
        assert!(operations
            .iter()
            .any(|operation| operation.kind == OperationKind::PutManifest));
        let manifest = latest_remote_manifest(&store, &operations).unwrap().unwrap();
        let entry = manifest
            .entries
            .iter()
            .find(|entry| entry.path == "local.txt")
            .unwrap();
        assert_eq!(entry.content_hash.as_deref(), Some(sync_content_hash(bytes).as_str()));
        assert_eq!(store.fetch_content_blob(sync_content_hash(bytes).as_str()).unwrap(), bytes);
        let state = DaemonController::new(&fixture.runtime).load_state().unwrap();
        assert_eq!(state.stale_worktree, WorktreeStatus::Clean);
        assert_eq!(state.queued_operations, 0);
        assert_eq!(state.last_error, None);
    }

    #[test]
    fn sync_start_preserves_policy_suppressed_remote_only_entries_when_pushing_manifest() {
        let fixture = Fixture::new("foreground-preserve-policy-remote");
        let suppressed_paths = [
            "dist/remote.js",
            "app/node_modules/pkg/index.js",
            "native/libaddon.so",
            ".git/config",
        ];
        fixture.seed_remote_files(
            "manifest-policy-suppressed",
            &[
                (suppressed_paths[0], b"ignored generated" as &[u8]),
                (suppressed_paths[1], b"dependency artifact" as &[u8]),
                (suppressed_paths[2], b"native binary" as &[u8]),
                (suppressed_paths[3], b"[core]\n" as &[u8]),
            ],
            1,
        );
        let local_bytes = b"local foreground contents";
        fixture.write_file("local.txt", local_bytes);

        let start = fixture.run(["dropbox-dev", "sync", "start"]).unwrap();

        assert_contains(&start, "sync_start=foreground-complete");
        assert_contains(&start, "foreground_work=ok");
        assert_contains(&start, "foreground_actions_applied=1");
        let store = connect_transport(&fixture.runtime).unwrap();
        let operations = store.load_operation_log().unwrap();
        assert!(!operations.iter().any(|operation| {
            operation.kind == OperationKind::DeletePath
                && suppressed_paths.contains(&operation.path.as_str())
        }));
        let manifest = latest_remote_manifest(&store, &operations).unwrap().unwrap();
        for path in suppressed_paths {
            assert!(manifest.entries.iter().any(|entry| entry.path == path));
        }
        let local_entry = manifest
            .entries
            .iter()
            .find(|entry| entry.path == "local.txt")
            .unwrap();
        assert_eq!(
            local_entry.content_hash.as_deref(),
            Some(sync_content_hash(local_bytes).as_str())
        );
    }

    #[test]
    fn sync_start_converges_local_delete_without_resurrecting_remote_file() {
        let fixture = Fixture::new("foreground-local-delete");
        let bytes = b"remote contents to delete";
        fixture.seed_remote_file("manifest-delete", "delete-me.txt", bytes, 1);
        fixture.run(["dropbox-dev", "sync", "start"]).unwrap();
        let local_path = fixture.runtime.config.root_paths[0].join("delete-me.txt");
        assert_eq!(fs::read(&local_path).unwrap(), bytes);

        fs::remove_file(&local_path).unwrap();
        let start = fixture.run(["dropbox-dev", "sync", "start"]).unwrap();

        assert_contains(&start, "sync_start=foreground-complete");
        assert_contains(&start, "foreground_work=ok");
        assert_contains(&start, "foreground_actions_applied=1");
        assert_contains(&start, "queued_operations=0");
        assert!(!local_path.exists());
        let store = connect_transport(&fixture.runtime).unwrap();
        let operations = store.load_operation_log().unwrap();
        assert!(operations.iter().any(|operation| {
            operation.kind == OperationKind::DeletePath && operation.path == "delete-me.txt"
        }));
        let manifest = latest_remote_manifest(&store, &operations).unwrap().unwrap();
        assert!(!manifest.entries.iter().any(|entry| entry.path == "delete-me.txt"));
        let state = DaemonController::new(&fixture.runtime).load_state().unwrap();
        assert_eq!(state.stale_worktree, WorktreeStatus::Clean);
        assert_eq!(state.queued_operations, 0);
        assert_eq!(state.last_error, None);
    }

    #[test]
    fn sync_start_converges_local_rename_as_remote_move() {
        let fixture = Fixture::new("foreground-local-rename");
        let bytes = b"remote contents to rename";
        fixture.seed_remote_file("manifest-rename", "old-name.txt", bytes, 1);
        fixture.run(["dropbox-dev", "sync", "start"]).unwrap();
        let root = &fixture.runtime.config.root_paths[0];
        let old_path = root.join("old-name.txt");
        let new_path = root.join("new-name.txt");
        assert_eq!(fs::read(&old_path).unwrap(), bytes);

        fs::rename(&old_path, &new_path).unwrap();
        let start = fixture.run(["dropbox-dev", "sync", "start"]).unwrap();

        assert_contains(&start, "sync_start=foreground-complete");
        assert_contains(&start, "foreground_work=ok");
        assert_contains(&start, "foreground_actions_applied=1");
        assert!(!old_path.exists());
        assert_eq!(fs::read(&new_path).unwrap(), bytes);
        let store = connect_transport(&fixture.runtime).unwrap();
        let operations = store.load_operation_log().unwrap();
        assert!(operations.iter().any(|operation| {
            operation.kind == OperationKind::MovePath
                && operation.previous_path.as_deref() == Some("old-name.txt")
                && operation.path == "new-name.txt"
        }));
        let manifest = latest_remote_manifest(&store, &operations).unwrap().unwrap();
        assert!(!manifest.entries.iter().any(|entry| entry.path == "old-name.txt"));
        assert!(manifest.entries.iter().any(|entry| entry.path == "new-name.txt"));
        let state = DaemonController::new(&fixture.runtime).load_state().unwrap();
        assert_eq!(state.stale_worktree, WorktreeStatus::Clean);
        assert_eq!(state.queued_operations, 0);
        assert_eq!(state.last_error, None);
    }

    #[test]
    fn sync_start_applies_remote_delete_tombstone_to_stale_local_path() {
        let fixture = Fixture::new("foreground-remote-delete-tombstone");
        let bytes = b"remote contents deleted elsewhere";
        fixture.seed_remote_file_with_metadata(
            "manifest-remote-delete-before",
            "stale.txt",
            bytes,
            1,
            10,
            0o644,
        );
        fixture.run(["dropbox-dev", "sync", "start"]).unwrap();
        let local_path = fixture.runtime.config.root_paths[0].join("stale.txt");
        assert_eq!(fs::read(&local_path).unwrap(), bytes);

        fixture.append_remote_delete_tombstone(
            "manifest-remote-delete-after",
            "stale.txt",
            3,
            u64::MAX,
        );
        let controller = DaemonController::new(&fixture.runtime);
        let state = controller.load_state().unwrap();
        let planned = plan_current_worktree(&fixture.runtime, &state).unwrap();
        assert!(planned.plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::DeleteLocal { path } if path == "stale.txt"
        )));
        assert_eq!(
            unsupported_foreground_action_count(&planned.plan, planned.remote_manifest.as_ref()),
            0
        );

        let start = fixture.run(["dropbox-dev", "sync", "start"]).unwrap();

        assert_contains(&start, "sync_start=foreground-complete");
        assert_contains(&start, "foreground_work=ok");
        assert_contains(&start, "foreground_actions_applied=1");
        assert_contains(&start, "foreground_unsupported_actions=0");
        assert_contains(&start, "last_error=none");
        assert_contains(&start, "stale_worktree=clean");
        assert!(!local_path.exists());
        let status = fixture.run(["dropbox-dev", "status"]).unwrap();
        assert_contains(&status, "state_stale_worktree=clean");
        assert_contains(&status, "stale_worktree=clean");
        assert_contains(&status, "remote_fetch_actions=0");
        assert_contains(&status, "local_push_actions=0");
        assert_contains(&status, "queued_operations_observed=0");
        assert_contains(&status, "unsupported_actions_observed=0");
        let final_state = controller.load_state().unwrap();
        assert_eq!(final_state.stale_worktree, WorktreeStatus::Clean);
        assert_eq!(final_state.last_error, None);
    }

    #[test]
    fn foreground_pass_preserves_changed_tombstoned_path_when_publishing_unrelated_push() {
        let fixture = Fixture::new("foreground-preserve-changed-tombstone");
        let remote_bytes = b"remote contents deleted elsewhere";
        let changed_bytes = b"local edit after planning must survive";
        let unrelated_bytes = b"unrelated foreground push";
        fixture.seed_remote_file_with_metadata(
            "manifest-preserve-tombstone-before",
            "stale.txt",
            remote_bytes,
            1,
            10,
            0o644,
        );
        fixture.run(["dropbox-dev", "sync", "start"]).unwrap();
        let root = &fixture.runtime.config.root_paths[0];
        let stale_path = root.join("stale.txt");
        assert_eq!(fs::read(&stale_path).unwrap(), remote_bytes);

        fixture.append_remote_delete_tombstone(
            "manifest-preserve-tombstone-after",
            "stale.txt",
            3,
            u64::MAX,
        );
        fixture.write_file("unrelated.txt", unrelated_bytes);
        let controller = DaemonController::new(&fixture.runtime);
        let state = controller.load_state().unwrap();
        let planned = plan_current_worktree(&fixture.runtime, &state).unwrap();
        assert!(planned.plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::DeleteLocal { path } if path == "stale.txt"
        )));
        assert!(planned.plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::PushContent { path, .. } if path == "unrelated.txt"
        )));

        fixture.write_file("stale.txt", changed_bytes);
        let (report, applied_actions, unsupported_actions, _final_manifest) =
            execute_planned_foreground_sync_pass(&fixture.runtime, &state, planned).unwrap();

        assert_eq!(fs::read(&stale_path).unwrap(), changed_bytes);
        assert_eq!(applied_actions, 1);
        assert_eq!(unsupported_actions, 0);
        assert_eq!(report.status, WorktreeStatus::Stale);
        assert_eq!(report.queued_count, 0);
        assert!(report.action_count > 0);
        assert_eq!(
            foreground_pass_error(
                unsupported_actions,
                report.queued_count,
                report.status == WorktreeStatus::Stale,
            )
            .as_deref(),
            Some("foreground sync pass left convergence actions pending"),
        );
        let pending = plan_current_worktree(&fixture.runtime, &state).unwrap();
        assert!(pending.plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::DeleteLocal { path } if path == "stale.txt"
        )));

        let store = connect_transport(&fixture.runtime).unwrap();
        let operations = store.load_operation_log().unwrap();
        assert!(operations.iter().any(|operation| {
            operation.kind == OperationKind::PutContent
                && operation.path == "unrelated.txt"
                && operation.payload_id.as_deref()
                    == Some(sync_content_hash(unrelated_bytes).as_str())
        }));
        let manifest = latest_remote_manifest(&store, &operations).unwrap().unwrap();
        assert!(!manifest.entries.iter().any(|entry| entry.path == "stale.txt"));
        let unrelated_entry = manifest
            .entries
            .iter()
            .find(|entry| entry.path == "unrelated.txt")
            .unwrap();
        assert_eq!(
            unrelated_entry.content_hash.as_deref(),
            Some(sync_content_hash(unrelated_bytes).as_str()),
        );
    }

    #[test]
    fn sync_start_applies_remote_move_tombstone_by_removing_stale_source() {
        let fixture = Fixture::new("foreground-remote-move-tombstone");
        let bytes = b"remote contents moved elsewhere";
        fixture.seed_remote_file_with_metadata(
            "manifest-remote-move-before",
            "old-name.txt",
            bytes,
            1,
            10,
            0o644,
        );
        fixture.run(["dropbox-dev", "sync", "start"]).unwrap();
        let root = &fixture.runtime.config.root_paths[0];
        let old_path = root.join("old-name.txt");
        let new_path = root.join("new-name.txt");
        assert_eq!(fs::read(&old_path).unwrap(), bytes);

        fixture.append_remote_move_tombstone(
            "manifest-remote-move-after",
            "old-name.txt",
            "new-name.txt",
            bytes,
            3,
            u64::MAX,
        );
        let controller = DaemonController::new(&fixture.runtime);
        let state = controller.load_state().unwrap();
        let planned = plan_current_worktree(&fixture.runtime, &state).unwrap();
        assert!(planned.plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::DeleteLocal { path } if path == "old-name.txt"
        )));
        assert!(planned.plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::FetchContent { path, .. } if path == "new-name.txt"
        )));
        assert_eq!(
            unsupported_foreground_action_count(&planned.plan, planned.remote_manifest.as_ref()),
            0
        );

        let start = fixture.run(["dropbox-dev", "sync", "start"]).unwrap();

        assert_contains(&start, "sync_start=foreground-complete");
        assert_contains(&start, "foreground_work=ok");
        assert_contains(&start, "foreground_actions_applied=3");
        assert_contains(&start, "foreground_unsupported_actions=0");
        assert_contains(&start, "last_error=none");
        assert_contains(&start, "stale_worktree=clean");
        assert!(!old_path.exists());
        assert_eq!(fs::read(&new_path).unwrap(), bytes);
        let status = fixture.run(["dropbox-dev", "status"]).unwrap();
        assert_contains(&status, "state_stale_worktree=clean");
        assert_contains(&status, "stale_worktree=clean");
        assert_contains(&status, "remote_fetch_actions=0");
        assert_contains(&status, "local_push_actions=0");
        assert_contains(&status, "queued_operations_observed=0");
        assert_contains(&status, "unsupported_actions_observed=0");
        let final_state = controller.load_state().unwrap();
        assert_eq!(final_state.stale_worktree, WorktreeStatus::Clean);
        assert_eq!(final_state.last_error, None);
    }

    #[test]
    fn sync_start_leaves_conflict_sidecar_paths_pending_without_overwriting_loser() {
        let fixture = Fixture::new("foreground-conflict-sidecar");
        let local_bytes = b"local loser contents";
        let remote_bytes = b"remote winner contents";
        fixture.write_file("conflict.txt", local_bytes);
        fixture.seed_remote_file_with_metadata(
            "manifest-conflict",
            "conflict.txt",
            remote_bytes,
            1,
            u64::MAX,
            0o644,
        );

        let start = fixture.run(["dropbox-dev", "sync", "start"]).unwrap();

        assert_contains(&start, "sync_start=foreground-incomplete");
        assert_contains(&start, "foreground_work=unsupported");
        assert_contains(&start, "foreground_actions_applied=0");
        assert_contains(&start, "foreground_unsupported_actions=1");
        assert_contains(&start, "remote_fetch_actions=1");
        assert_eq!(
            fs::read(fixture.runtime.config.root_paths[0].join("conflict.txt")).unwrap(),
            local_bytes
        );
        let state = DaemonController::new(&fixture.runtime).load_state().unwrap();
        assert!(state.last_local_manifest.is_none());
        let planned = plan_current_worktree(&fixture.runtime, &state).unwrap();
        assert!(planned.plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::ConflictSidecar { path, .. } if path == "conflict.txt"
        )));
        assert!(planned.plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::FetchContent { path, .. } if path == "conflict.txt"
        )));
    }

    #[test]
    fn sync_start_verification_fetch_with_temp_suffix_preserves_existing_local_file() {
        let fixture = Fixture::new("foreground-verification-fetch-preserve");
        let local_bytes = b"local edit that must survive verification";
        let remote_bytes = b"remote bytes requiring source verification";
        fixture.write_file("src/lib.rs", local_bytes);
        let remote_blob_id = fixture.seed_remote_store_only_file(
            "manifest-store-only-verification",
            "src/lib.rs",
            remote_bytes,
            1,
        );
        let state = DaemonController::new(&fixture.runtime).load_state().unwrap();
        let planned = plan_current_worktree(&fixture.runtime, &state).unwrap();
        assert!(planned.plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::FetchContent { path, store_blob_id, temp_suffix }
                if path == "src/lib.rs"
                    && store_blob_id.as_deref() == Some(remote_blob_id.as_str())
                    && !temp_suffix.is_empty()
        )));

        let start = fixture.run(["dropbox-dev", "sync", "start"]).unwrap();

        assert_contains(&start, "sync_start=foreground-incomplete");
        assert_contains(&start, "foreground_work=pending");
        assert_contains(&start, "foreground_actions_applied=1");
        assert_contains(&start, "remote_fetch_actions=1");
        assert_eq!(
            fs::read(fixture.runtime.config.root_paths[0].join("src/lib.rs")).unwrap(),
            local_bytes
        );
        let store = connect_transport(&fixture.runtime).unwrap();
        let operations = store.load_operation_log().unwrap();
        let manifest = latest_remote_manifest(&store, &operations).unwrap().unwrap();
        let manifest_entry = manifest
            .entries
            .iter()
            .find(|entry| entry.path == "src/lib.rs")
            .unwrap();
        assert_eq!(manifest_entry.content_hash.as_deref(), Some(remote_blob_id.as_str()));
        let state = DaemonController::new(&fixture.runtime).load_state().unwrap();
        assert_eq!(state.stale_worktree, WorktreeStatus::Stale);
        assert!(state.last_local_manifest.is_none());
    }

    #[test]
    fn sync_start_clean_pass_persists_current_manifest_as_baseline() {
        let fixture = Fixture::new("foreground-clean-baseline");
        let bytes = b"clean baseline contents";
        fixture.seed_remote_file("manifest-clean-baseline", "baseline.txt", bytes, 1);

        let start = fixture.run(["dropbox-dev", "sync", "start"]).unwrap();

        assert_contains(&start, "sync_start=foreground-complete");
        assert_contains(&start, "foreground_work=ok");
        let state = DaemonController::new(&fixture.runtime).load_state().unwrap();
        assert_eq!(state.stale_worktree, WorktreeStatus::Clean);
        assert!(state
            .last_local_manifest
            .as_ref()
            .unwrap()
            .entries
            .iter()
            .any(|entry| entry.path == "baseline.txt"));
    }

    #[test]
    fn recover_stale_noop_clean_persists_baseline_for_later_delete() {
        let fixture = Fixture::new("recover-noop-baseline");
        let bytes = b"already synced contents";
        fixture.seed_remote_file("manifest-recover-noop-baseline", "already.txt", bytes, 1);
        fixture.run(["dropbox-dev", "sync", "start"]).unwrap();
        let controller = DaemonController::new(&fixture.runtime);
        let mut state = controller.load_state().unwrap();
        state.last_local_manifest = None;
        controller.save_state(&state).unwrap();

        let recovered = fixture.run(["dropbox-dev", "sync", "recover-stale"]).unwrap();

        assert_contains(&recovered, "recover_status=ok");
        assert_contains(&recovered, "recovered_paths=0");
        assert_contains(&recovered, "skipped_actions=0");
        assert_contains(&recovered, "state_stale_worktree=clean");
        let state = controller.load_state().unwrap();
        assert_eq!(state.stale_worktree, WorktreeStatus::Clean);
        assert!(state
            .last_local_manifest
            .as_ref()
            .unwrap()
            .entries
            .iter()
            .any(|entry| entry.path == "already.txt"));

        let local_path = fixture.runtime.config.root_paths[0].join("already.txt");
        fs::remove_file(&local_path).unwrap();
        let start = fixture.run(["dropbox-dev", "sync", "start"]).unwrap();

        assert_contains(&start, "sync_start=foreground-complete");
        assert_contains(&start, "foreground_work=ok");
        assert_contains(&start, "foreground_actions_applied=1");
        assert!(!local_path.exists());
        let store = connect_transport(&fixture.runtime).unwrap();
        let operations = store.load_operation_log().unwrap();
        assert!(operations.iter().any(|operation| {
            operation.kind == OperationKind::DeletePath && operation.path == "already.txt"
        }));
        let manifest = latest_remote_manifest(&store, &operations).unwrap().unwrap();
        assert!(!manifest.entries.iter().any(|entry| entry.path == "already.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn sync_start_reports_unsupported_planned_actions_without_claiming_complete() {
        let fixture = Fixture::new("foreground-unsupported");
        std::os::unix::fs::symlink(
            "target.txt",
            fixture.runtime.config.root_paths[0].join("current"),
        )
        .unwrap();

        let start = fixture.run(["dropbox-dev", "sync", "start"]).unwrap();

        assert_contains(&start, "sync_start=foreground-incomplete");
        assert_contains(&start, "foreground_work=unsupported");
        assert_contains(&start, "foreground_unsupported_actions=");
        assert_contains(&start, "last_error=unsupported foreground convergence actions remain");
        assert_contains(&start, "stale_worktree=stale");
        let state = DaemonController::new(&fixture.runtime).load_state().unwrap();
        assert_eq!(state.stale_worktree, WorktreeStatus::Stale);
        assert!(state
            .last_error
            .as_deref()
            .unwrap()
            .starts_with("unsupported foreground convergence actions remain:"));
    }

    #[cfg(unix)]
    #[test]
    fn sync_start_pushes_supported_content_before_reporting_unsupported_symlink_leftovers() {
        let fixture = Fixture::new("foreground-mixed-unsupported");
        let bytes = b"supported foreground contents";
        fixture.write_file("local.txt", bytes);
        std::os::unix::fs::symlink(
            "target.txt",
            fixture.runtime.config.root_paths[0].join("current"),
        )
        .unwrap();

        let start = fixture.run(["dropbox-dev", "sync", "start"]).unwrap();

        assert_contains(&start, "sync_start=foreground-incomplete");
        assert_contains(&start, "foreground_work=unsupported");
        assert_contains(&start, "foreground_actions_applied=1");
        assert_contains(&start, "foreground_unsupported_actions=1");
        assert_contains(&start, "stale_worktree=stale");
        let store = connect_transport(&fixture.runtime).unwrap();
        let operations = store.load_operation_log().unwrap();
        assert!(operations.iter().any(|operation| {
            operation.kind == OperationKind::PutContent
                && operation.path == "local.txt"
                && operation.payload_id.as_deref() == Some(sync_content_hash(bytes).as_str())
        }));
        let manifest = latest_remote_manifest(&store, &operations).unwrap().unwrap();
        assert!(manifest.entries.iter().any(|entry| entry.path == "local.txt"));
        assert!(!manifest.entries.iter().any(|entry| entry.path == "current"));
        assert_eq!(store.fetch_content_blob(sync_content_hash(bytes).as_str()).unwrap(), bytes);
    }

    #[cfg(unix)]
    #[test]
    fn sync_start_applies_remote_permission_only_change_without_republishing_local_mode() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = Fixture::new("foreground-remote-permissions");
        let bytes = b"same foreground contents";
        fixture.write_file("script.sh", bytes);
        let local_path = fixture.runtime.config.root_paths[0].join("script.sh");
        let mut permissions = fs::metadata(&local_path).unwrap().permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(&local_path, permissions).unwrap();
        fixture.seed_remote_file_with_metadata(
            "manifest-remote-permissions",
            "script.sh",
            bytes,
            1,
            u64::MAX,
            0o755,
        );

        let start = fixture.run(["dropbox-dev", "sync", "start"]).unwrap();

        assert_contains(&start, "sync_start=foreground-complete");
        assert_contains(&start, "foreground_work=ok");
        assert_contains(&start, "foreground_actions_applied=1");
        assert_eq!(
            fs::metadata(&local_path).unwrap().permissions().mode() & 0o7777,
            0o755
        );
        let store = connect_transport(&fixture.runtime).unwrap();
        let operations = store.load_operation_log().unwrap();
        assert!(!operations.iter().any(|operation| {
            operation.kind == OperationKind::PermissionChanged && operation.path == "script.sh"
        }));
        assert_eq!(
            operations
                .iter()
                .filter(|operation| operation.kind == OperationKind::PutManifest)
                .count(),
            1
        );
        let manifest = latest_remote_manifest(&store, &operations).unwrap().unwrap();
        let entry = manifest
            .entries
            .iter()
            .find(|entry| entry.path == "script.sh")
            .unwrap();
        assert_eq!(entry.permissions, 0o755);
    }

    #[test]
    fn daemon_state_path_is_namespaced_by_project_root_when_cache_is_shared() {
        let fixture_a = Fixture::new("shared-cache-a");
        let mut fixture_b = Fixture::new("shared-cache-b");
        fixture_b.runtime.config.cache_dir = fixture_a.runtime.config.cache_dir.clone();
        fixture_b.runtime.project_id = fixture_a.runtime.project_id.clone();
        let controller_a = DaemonController::new(&fixture_a.runtime);
        let controller_b = DaemonController::new(&fixture_b.runtime);
        let state_path_a = controller_a.state_path();
        let state_path_b = controller_b.state_path();

        assert_ne!(state_path_a, state_path_b);
        for path in [&state_path_a, &state_path_b] {
            let file_name = path.file_name().and_then(|name| name.to_str()).unwrap();
            assert!(file_name.starts_with("daemon-state-"));
            assert!(file_name.ends_with(".txt"));
            assert!(file_name
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.'));
        }

        controller_a
            .save_state(&DaemonState {
                sync_paused: true,
                queued_operations: 7,
                last_error: Some("project-a-error".to_owned()),
                hydration_placeholders: 5,
                hydration_hydrated: 4,
                env_variables: 3,
                env_conflicts: 2,
                stale_worktree: WorktreeStatus::Stale,
                ..DaemonState::default()
            })
            .unwrap();

        let state_b = controller_b.load_state().unwrap();
        assert!(!state_b.sync_paused);
        assert_eq!(state_b.queued_operations, 0);
        assert_eq!(state_b.last_error, None);
        assert_eq!(state_b.hydration_placeholders, 0);
        assert_eq!(state_b.hydration_hydrated, 0);
        assert_eq!(state_b.env_variables, 0);
        assert_eq!(state_b.env_conflicts, 0);
        assert_eq!(state_b.stale_worktree, WorktreeStatus::Unknown);

        controller_b
            .save_state(&DaemonState {
                queued_operations: 1,
                last_error: Some("project-b-error".to_owned()),
                stale_worktree: WorktreeStatus::Recovered,
                ..DaemonState::default()
            })
            .unwrap();
        let state_a = controller_a.load_state().unwrap();
        assert!(state_a.sync_paused);
        assert_eq!(state_a.queued_operations, 7);
        assert_eq!(state_a.last_error.as_deref(), Some("project-a-error"));
        assert_eq!(state_a.hydration_placeholders, 5);
        assert_eq!(state_a.hydration_hydrated, 4);
        assert_eq!(state_a.env_variables, 3);
        assert_eq!(state_a.env_conflicts, 2);
        assert_eq!(state_a.stale_worktree, WorktreeStatus::Stale);
    }

    #[test]
    fn sync_resume_runs_foreground_pass_and_drains_queued_remote_work() {
        let fixture = Fixture::new("resume-queue");
        let bytes = b"queued remote contents";
        fixture.seed_remote_file("manifest-resume-queue", "queued.txt", bytes, 1);
        let controller = DaemonController::new(&fixture.runtime);
        controller
            .save_state(&DaemonState {
                lifecycle: LifecycleState::Stopped,
                sync_paused: true,
                queued_operations: 1,
                ..DaemonState::default()
            })
            .unwrap();

        let resume = fixture.run(["dropbox-dev", "sync", "resume"]).unwrap();

        assert_contains(&resume, "sync_resume=resumed");
        assert_contains(&resume, "sync_paused=false");
        assert_contains(&resume, "queued_operations=0");
        assert_contains(&resume, "last_error=none");
        assert_eq!(
            fs::read(fixture.runtime.config.root_paths[0].join("queued.txt")).unwrap(),
            bytes
        );
        let state = controller.load_state().unwrap();
        assert!(!state.sync_paused);
        assert_eq!(state.queued_operations, 0);
        assert_eq!(state.last_error, None);
        assert_eq!(state.stale_worktree, WorktreeStatus::Clean);
        assert!(state
            .last_local_manifest
            .as_ref()
            .unwrap()
            .entries
            .iter()
            .any(|entry| entry.path == "queued.txt"));
    }

    #[test]
    fn sync_resume_stale_conflict_pass_does_not_persist_unsynced_manifest_as_baseline() {
        let fixture = Fixture::new("resume-conflict-baseline");
        let local_bytes = b"resume local loser contents";
        let remote_bytes = b"resume remote winner contents";
        fixture.write_file("conflict.txt", local_bytes);
        fixture.seed_remote_file_with_metadata(
            "manifest-resume-conflict",
            "conflict.txt",
            remote_bytes,
            1,
            u64::MAX,
            0o644,
        );
        let controller = DaemonController::new(&fixture.runtime);
        controller
            .save_state(&DaemonState {
                lifecycle: LifecycleState::Stopped,
                sync_paused: true,
                queued_operations: 1,
                ..DaemonState::default()
            })
            .unwrap();

        let resume = fixture.run(["dropbox-dev", "sync", "resume"]).unwrap();

        assert_contains(&resume, "sync_resume=resumed");
        assert_contains(&resume, "last_error=unsupported foreground convergence actions remain");
        assert_eq!(
            fs::read(fixture.runtime.config.root_paths[0].join("conflict.txt")).unwrap(),
            local_bytes
        );
        let state = controller.load_state().unwrap();
        assert_eq!(state.stale_worktree, WorktreeStatus::Stale);
        assert_eq!(state.queued_operations, 0);
        assert!(state.last_local_manifest.is_none());
    }

    #[test]
    fn paused_status_counts_queue_offline_remote_work_as_stale() {
        let fixture = Fixture::new("paused-queue-offline");
        fixture.seed_remote_file("manifest-paused", "remote-only.txt", b"remote", 1);
        fixture.run(["dropbox-dev", "sync", "pause"]).unwrap();

        let status = fixture.run(["dropbox-dev", "status"]).unwrap();

        assert_contains(&status, "sync_paused=true");
        assert_contains(&status, "stale_worktree=stale");
        assert_contains(&status, "remote_fetch_actions=0");
        assert_contains(&status, "queued_operations_observed=1");
        assert_contains(&status, "recovery_command=sync recover-stale");
    }

    #[test]
    fn hydrate_fetches_transport_backed_vfs_content_and_metadata() {
        let fixture = Fixture::new("hydrate-remote");
        let remote_bytes = b"transport bytes";
        let blob_id = fixture.seed_remote_file("manifest-hydrate", "remote.txt", remote_bytes, 1);

        let hydrate = fixture.run(["dropbox-dev", "hydrate", "remote.txt"]).unwrap();

        assert_contains(&hydrate, "hydrate_status=ok");
        assert_contains(&hydrate, "manifest_source=remote");
        assert_contains(&hydrate, &format!("content_hash={blob_id}"));
        assert_contains(&hydrate, &format!("bytes={}", remote_bytes.len()));
        assert_contains(&hydrate, "fetched=true");
        assert_contains(&hydrate, "path_hydration_status=hydrated");
        assert_contains(
            &hydrate,
            &format!("path_source_machine={}", fixture.runtime.config.machine_id),
        );
    }

    #[test]
    fn stale_worktree_is_observable_recoverable_and_new_stale_is_not_masked() {
        let fixture = Fixture::new("stale");
        let bytes = b"remote contents";
        fixture.seed_remote_file("manifest-remote", "src/lib.rs", bytes, 1);

        let status = fixture.run(["dropbox-dev", "status"]).unwrap();
        assert_contains(&status, "stale_worktree=stale");
        assert_contains(&status, "recovery_command=sync recover-stale");
        assert_contains(&status, "remote_fetch_actions=1");

        let recovered = fixture.run(["dropbox-dev", "sync", "recover-stale"]).unwrap();
        assert_contains(&recovered, "recover_status=ok");
        assert_contains(&recovered, "recovered_paths=1");
        assert_eq!(fs::read(fixture.runtime.config.root_paths[0].join("src/lib.rs")).unwrap(), bytes);

        let after = fixture.run(["dropbox-dev", "status"]).unwrap();
        assert_contains(&after, "state_stale_worktree=stale");
        assert_contains(&after, "stale_worktree=stale");
        assert_contains(&after, "remote_fetch_actions=0");
        assert_contains(&after, "recovery_command=sync recover-stale");

        fixture.seed_remote_file("manifest-new-stale", "src/new.rs", b"new remote contents", 3);
        let new_stale = fixture.run(["dropbox-dev", "status"]).unwrap();
        assert_contains(&new_stale, "remote_manifest_id=manifest-new-stale");
        assert_contains(&new_stale, "stale_worktree=stale");
        assert_contains(&new_stale, "recovery_command=sync recover-stale");
    }

    #[test]
    fn recover_stale_preserves_local_loser_for_conflict_sidecar_path() {
        let fixture = Fixture::new("recover-conflict-sidecar");
        let local_bytes = b"local loser contents";
        let remote_bytes = b"remote winner contents";
        fixture.write_file("conflict.txt", local_bytes);
        fixture.seed_remote_file_with_metadata(
            "manifest-recover-conflict",
            "conflict.txt",
            remote_bytes,
            1,
            u64::MAX,
            0o644,
        );

        let recovered = fixture.run(["dropbox-dev", "sync", "recover-stale"]).unwrap();

        assert_contains(&recovered, "recover_status=ok");
        assert_contains(&recovered, "recovered_paths=0");
        assert_eq!(
            fs::read(fixture.runtime.config.root_paths[0].join("conflict.txt")).unwrap(),
            local_bytes
        );
        let state = DaemonController::new(&fixture.runtime).load_state().unwrap();
        assert!(state.last_local_manifest.is_none());
        let planned = plan_current_worktree(&fixture.runtime, &state).unwrap();
        assert!(planned.plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::ConflictSidecar { path, .. } if path == "conflict.txt"
        )));
        assert!(planned.plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::FetchContent { path, .. } if path == "conflict.txt"
        )));
    }

    #[test]
    fn recover_stale_partial_recovery_with_skipped_conflict_does_not_persist_baseline() {
        let fixture = Fixture::new("recover-partial-conflict-baseline");
        let recovered_bytes = b"remote-only bytes recovered safely";
        let local_conflict_bytes = b"local conflict contents must survive";
        let remote_conflict_bytes = b"remote conflict contents";
        fixture.write_file("conflict.txt", local_conflict_bytes);
        fixture.seed_remote_files(
            "manifest-partial-recover",
            &[
                ("remote-only.txt", recovered_bytes as &[u8]),
                ("conflict.txt", remote_conflict_bytes as &[u8]),
            ],
            1,
        );

        let recovered = fixture.run(["dropbox-dev", "sync", "recover-stale"]).unwrap();

        assert_contains(&recovered, "recover_status=ok");
        assert_contains(&recovered, "recovered_paths=1");
        assert!(!recovered.contains("skipped_actions=0\n"));
        assert_eq!(
            fs::read(fixture.runtime.config.root_paths[0].join("remote-only.txt")).unwrap(),
            recovered_bytes
        );
        assert_eq!(
            fs::read(fixture.runtime.config.root_paths[0].join("conflict.txt")).unwrap(),
            local_conflict_bytes
        );
        let state = DaemonController::new(&fixture.runtime).load_state().unwrap();
        assert_eq!(state.stale_worktree, WorktreeStatus::Stale);
        assert!(state.last_local_manifest.is_none());
    }

    #[test]
    fn recover_stale_full_clean_recovery_persists_valid_baseline() {
        let fixture = Fixture::new("recover-full-clean-baseline");
        let bytes = b"remote bytes for clean recovery baseline";
        fixture.seed_remote_file("manifest-full-clean-recover", "clean.txt", bytes, 1);

        let recovered = fixture.run(["dropbox-dev", "sync", "recover-stale"]).unwrap();

        assert_contains(&recovered, "recover_status=ok");
        assert_contains(&recovered, "recovered_paths=1");
        assert_contains(&recovered, "skipped_actions=0");
        assert_eq!(
            fs::read(fixture.runtime.config.root_paths[0].join("clean.txt")).unwrap(),
            bytes
        );
        let state = DaemonController::new(&fixture.runtime).load_state().unwrap();
        assert_eq!(state.stale_worktree, WorktreeStatus::Recovered);
        assert!(state
            .last_local_manifest
            .as_ref()
            .unwrap()
            .entries
            .iter()
            .any(|entry| entry.path == "clean.txt"));
    }

    #[test]
    fn recover_stale_records_manifest_for_next_local_delete_and_rename() {
        let delete_fixture = Fixture::new("recover-delete-baseline");
        let delete_bytes = b"remote contents to recover then delete";
        delete_fixture.seed_remote_file("manifest-recover-delete", "delete-me.txt", delete_bytes, 1);

        let recovered = delete_fixture
            .run(["dropbox-dev", "sync", "recover-stale"])
            .unwrap();
        assert_contains(&recovered, "recover_status=ok");
        assert_contains(&recovered, "recovered_paths=1");
        let state = DaemonController::new(&delete_fixture.runtime)
            .load_state()
            .unwrap();
        assert!(state
            .last_local_manifest
            .as_ref()
            .unwrap()
            .entries
            .iter()
            .any(|entry| entry.path == "delete-me.txt"));
        let delete_path = delete_fixture.runtime.config.root_paths[0].join("delete-me.txt");
        fs::remove_file(&delete_path).unwrap();

        let delete_start = delete_fixture.run(["dropbox-dev", "sync", "start"]).unwrap();

        assert_contains(&delete_start, "sync_start=foreground-complete");
        assert!(!delete_path.exists());
        let store = connect_transport(&delete_fixture.runtime).unwrap();
        let operations = store.load_operation_log().unwrap();
        assert!(operations.iter().any(|operation| {
            operation.kind == OperationKind::DeletePath && operation.path == "delete-me.txt"
        }));
        let manifest = latest_remote_manifest(&store, &operations).unwrap().unwrap();
        assert!(!manifest.entries.iter().any(|entry| entry.path == "delete-me.txt"));

        let rename_fixture = Fixture::new("recover-rename-baseline");
        let rename_bytes = b"remote contents to recover then rename";
        rename_fixture.seed_remote_file("manifest-recover-rename", "old-name.txt", rename_bytes, 1);
        let recovered = rename_fixture
            .run(["dropbox-dev", "sync", "recover-stale"])
            .unwrap();
        assert_contains(&recovered, "recover_status=ok");
        assert_contains(&recovered, "recovered_paths=1");
        let root = &rename_fixture.runtime.config.root_paths[0];
        let old_path = root.join("old-name.txt");
        let new_path = root.join("new-name.txt");
        fs::rename(&old_path, &new_path).unwrap();

        let rename_start = rename_fixture.run(["dropbox-dev", "sync", "start"]).unwrap();

        assert_contains(&rename_start, "sync_start=foreground-complete");
        assert!(!old_path.exists());
        assert_eq!(fs::read(&new_path).unwrap(), rename_bytes);
        let store = connect_transport(&rename_fixture.runtime).unwrap();
        let operations = store.load_operation_log().unwrap();
        assert!(operations.iter().any(|operation| {
            operation.kind == OperationKind::MovePath
                && operation.previous_path.as_deref() == Some("old-name.txt")
                && operation.path == "new-name.txt"
        }));
        let manifest = latest_remote_manifest(&store, &operations).unwrap().unwrap();
        assert!(!manifest.entries.iter().any(|entry| entry.path == "old-name.txt"));
        assert!(manifest.entries.iter().any(|entry| entry.path == "new-name.txt"));
    }

    struct Fixture {
        root: PathBuf,
        runtime: CliRuntime,
    }

    impl Fixture {
        fn new(label: &str) -> Self {
            let root = temp_path(label);
            let project_root = root.join("project");
            let cache_dir = root.join("cache");
            let transport_dir = root.join("transport");
            let config_path = root.join("config").join("config.kv");
            fs::create_dir_all(&project_root).unwrap();
            fs::create_dir_all(&cache_dir).unwrap();
            fs::create_dir_all(&transport_dir).unwrap();
            let machine_id = app_scoped_machine_id(&format!("{label}-machine")).unwrap();
            let project_id = format!("project-{label}");
            let remote_machine_id =
                app_scoped_machine_id(&format!("{project_id}-remote-peer")).unwrap();
            let config = Config {
                machine_id: machine_id.clone(),
                machine_id_provenance: MachineIdProvenance::ConfigFile(config_path.clone()),
                root_paths: vec![project_root.clone()],
                transport_endpoint: transport_dir.display().to_string(),
                cache_dir,
                config_path,
            };
            let platform = test_platform(&machine_id);
            let runtime = CliRuntime::for_test(
                config,
                platform,
                project_root,
                project_id,
                format!("pairing-token-{label}"),
                vec![machine_id, remote_machine_id],
            );
            Self { root, runtime }
        }

        fn run<const N: usize>(&self, args: [&str; N]) -> Result<String, SyncError> {
            let owned = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
            run_with_runtime(owned, &NullLogger, &self.runtime)
        }

        fn write_file(&self, relative: &str, bytes: &[u8]) {
            let path = self.runtime.config.root_paths[0].join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, bytes).unwrap();
        }

        fn seed_remote_file(
            &self,
            manifest_id: &str,
            relative: &str,
            bytes: &[u8],
            sequence: u64,
        ) -> String {
            self.seed_remote_file_with_metadata(manifest_id, relative, bytes, sequence, sequence, 0o644)
        }

        fn seed_remote_store_only_file(
            &self,
            manifest_id: &str,
            relative: &str,
            bytes: &[u8],
            sequence: u64,
        ) -> String {
            let store = connect_transport(&self.runtime).unwrap();
            let blob_id = sync_content_hash(bytes);
            store.put_content_blob(&blob_id, bytes).unwrap();
            let remote_manifest = TreeManifest::new(
                manifest_id,
                self.runtime.project_id.clone(),
                vec![TreeEntry::file(
                    relative,
                    bytes.len() as u64,
                    sequence,
                    0o644,
                    Some(blob_id.clone()),
                )],
            );
            store.put_manifest(&remote_manifest).unwrap();
            let manifest_operation = OperationRecord::from_draft(
                OperationDraft::new(
                    sequence,
                    self.runtime.project_id.clone(),
                    self.runtime.config.machine_id.clone(),
                    OperationKind::PutManifest,
                    "manifest",
                )
                .manifest_id(remote_manifest.id.clone())
                .payload_id(remote_manifest.id.clone())
                .modified_unix_millis(sequence),
            );
            store.append_operation(&manifest_operation).unwrap();
            blob_id
        }

        fn seed_remote_files(
            &self,
            manifest_id: &str,
            files: &[(&str, &[u8])],
            sequence: u64,
        ) -> BTreeMap<String, String> {
            let store = connect_transport(&self.runtime).unwrap();
            let mut entries = Vec::new();
            let mut blob_ids = BTreeMap::new();
            let mut current_sequence = sequence;
            for &(relative, bytes) in files {
                let blob_id = sync_content_hash(bytes);
                store.put_content_blob(&blob_id, bytes).unwrap();
                let source_hash = watcher_content_hash(bytes);
                let content_operation = OperationRecord::from_draft(
                    OperationDraft::new(
                        current_sequence,
                        self.runtime.project_id.clone(),
                        self.runtime.config.machine_id.clone(),
                        OperationKind::PutContent,
                        relative,
                    )
                    .content_hash(source_hash)
                    .payload_id(blob_id.clone())
                    .modified_unix_millis(current_sequence)
                    .permissions(0o644),
                );
                store.append_operation(&content_operation).unwrap();
                entries.push(TreeEntry::file(
                    relative,
                    bytes.len() as u64,
                    current_sequence,
                    0o644,
                    Some(blob_id.clone()),
                ));
                blob_ids.insert(relative.to_owned(), blob_id);
                current_sequence += 1;
            }

            let remote_manifest =
                TreeManifest::new(manifest_id, self.runtime.project_id.clone(), entries);
            store.put_manifest(&remote_manifest).unwrap();
            let manifest_operation = OperationRecord::from_draft(
                OperationDraft::new(
                    current_sequence,
                    self.runtime.project_id.clone(),
                    self.runtime.config.machine_id.clone(),
                    OperationKind::PutManifest,
                    "manifest",
                )
                .manifest_id(remote_manifest.id.clone())
                .payload_id(remote_manifest.id.clone())
                .modified_unix_millis(current_sequence),
            );
            store.append_operation(&manifest_operation).unwrap();
            blob_ids
        }

        fn seed_remote_file_with_metadata(
            &self,
            manifest_id: &str,
            relative: &str,
            bytes: &[u8],
            sequence: u64,
            modified_unix_millis: u64,
            permissions: u32,
        ) -> String {
            let store = connect_transport(&self.runtime).unwrap();
            let blob_id = sync_content_hash(bytes);
            store.put_content_blob(&blob_id, bytes).unwrap();
            let source_hash = watcher_content_hash(bytes);
            let content_operation = OperationRecord::from_draft(
                OperationDraft::new(
                    sequence,
                    self.runtime.project_id.clone(),
                    self.runtime.config.machine_id.clone(),
                    OperationKind::PutContent,
                    relative,
                )
                .content_hash(source_hash)
                .payload_id(blob_id.clone())
                .modified_unix_millis(modified_unix_millis)
                .permissions(permissions),
            );
            store.append_operation(&content_operation).unwrap();
            let remote_manifest = TreeManifest::new(
                manifest_id,
                self.runtime.project_id.clone(),
                vec![TreeEntry::file(
                    relative,
                    bytes.len() as u64,
                    modified_unix_millis,
                    permissions,
                    Some(blob_id.clone()),
                )],
            );
            store.put_manifest(&remote_manifest).unwrap();
            let manifest_operation = OperationRecord::from_draft(
                OperationDraft::new(
                    sequence + 1,
                    self.runtime.project_id.clone(),
                    self.runtime.config.machine_id.clone(),
                    OperationKind::PutManifest,
                    "manifest",
                )
                .manifest_id(remote_manifest.id.clone())
                .payload_id(remote_manifest.id.clone())
                .modified_unix_millis(sequence + 1),
            );
            store.append_operation(&manifest_operation).unwrap();
            blob_id
        }

        fn append_remote_delete_tombstone(
            &self,
            manifest_id: &str,
            relative: &str,
            sequence: u64,
            modified_unix_millis: u64,
        ) {
            let remote_machine_id = self.remote_peer_machine_id();
            let store = self.remote_peer_store(&remote_machine_id);
            let delete_operation = OperationRecord::from_draft(
                OperationDraft::new(
                    sequence,
                    self.runtime.project_id.clone(),
                    remote_machine_id.clone(),
                    OperationKind::DeletePath,
                    relative,
                )
                .modified_unix_millis(modified_unix_millis),
            );
            store.append_operation(&delete_operation).unwrap();
            let remote_manifest = TreeManifest::new(
                manifest_id,
                self.runtime.project_id.clone(),
                Vec::<TreeEntry>::new(),
            );
            store.put_manifest(&remote_manifest).unwrap();
            let manifest_operation = OperationRecord::from_draft(
                OperationDraft::new(
                    sequence + 1,
                    self.runtime.project_id.clone(),
                    remote_machine_id,
                    OperationKind::PutManifest,
                    "manifest",
                )
                .manifest_id(remote_manifest.id.clone())
                .payload_id(remote_manifest.id.clone())
                .modified_unix_millis(sequence + 1),
            );
            store.append_operation(&manifest_operation).unwrap();
        }

        fn append_remote_move_tombstone(
            &self,
            manifest_id: &str,
            from: &str,
            to: &str,
            bytes: &[u8],
            sequence: u64,
            modified_unix_millis: u64,
        ) {
            let remote_machine_id = self.remote_peer_machine_id();
            let store = self.remote_peer_store(&remote_machine_id);
            let blob_id = sync_content_hash(bytes);
            store.put_content_blob(&blob_id, bytes).unwrap();
            let source_hash = watcher_content_hash(bytes);
            let move_operation = OperationRecord::from_draft(
                OperationDraft::new(
                    sequence,
                    self.runtime.project_id.clone(),
                    remote_machine_id.clone(),
                    OperationKind::MovePath,
                    to,
                )
                .previous_path(from)
                .content_hash(source_hash)
                .payload_id(blob_id.clone())
                .modified_unix_millis(modified_unix_millis)
                .permissions(0o644),
            );
            store.append_operation(&move_operation).unwrap();
            let remote_manifest = TreeManifest::new(
                manifest_id,
                self.runtime.project_id.clone(),
                vec![TreeEntry::file(
                    to,
                    bytes.len() as u64,
                    modified_unix_millis,
                    0o644,
                    Some(blob_id),
                )],
            );
            store.put_manifest(&remote_manifest).unwrap();
            let manifest_operation = OperationRecord::from_draft(
                OperationDraft::new(
                    sequence + 1,
                    self.runtime.project_id.clone(),
                    remote_machine_id,
                    OperationKind::PutManifest,
                    "manifest",
                )
                .manifest_id(remote_manifest.id.clone())
                .payload_id(remote_manifest.id.clone())
                .modified_unix_millis(sequence + 1),
            );
            store.append_operation(&manifest_operation).unwrap();
        }

        fn remote_peer_store(&self, remote_machine_id: &str) -> FileBackedSyncStore {
            let mut runtime = self.runtime.clone();
            runtime.config.machine_id = remote_machine_id.to_owned();
            runtime.platform = test_platform(remote_machine_id);
            connect_transport(&runtime).unwrap()
        }

        fn remote_peer_machine_id(&self) -> String {
            app_scoped_machine_id(&format!("{}-remote-peer", self.runtime.project_id)).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn run_with_runtime<I, S>(
        args: I,
        logger: &dyn Logger,
        runtime: &CliRuntime,
    ) -> Result<String, SyncError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut args = args.into_iter().map(Into::into);
        let _binary = args.next();
        let command = args.next();
        let rest = args.collect::<Vec<_>>();
        match command.as_deref() {
            None | Some("--help") | Some("-h") => Ok(help()),
            Some("version") | Some("--version") | Some("-V") => Ok(version()),
            Some("version-info") => Ok(version_info()),
            Some("doctor") => doctor_report(runtime),
            Some(command) => run_command(command, &rest, logger, runtime),
        }
    }

    fn test_platform(machine_id: &str) -> Platform {
        Platform {
            os_family: OsFamily::Linux,
            os_version: Some("test-linux".to_owned()),
            architecture: Architecture::X86_64,
            capabilities: PlatformCapabilities::for_os(&OsFamily::Linux),
            machine_id: MachineId {
                value: machine_id.to_owned(),
                provenance: MachineIdProvenance::ConfigFile(PathBuf::from("test-config")),
            },
        }
    }

    fn temp_path(label: &str) -> PathBuf {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let counter = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "dropbox-dev-cli-{label}-{}-{now}-{counter}",
            std::process::id()
        ))
    }

    fn watcher_content_hash(bytes: &[u8]) -> String {
        const FNV_OFFSET: u64 = 14_695_981_039_346_656_037;
        const FNV_PRIME: u64 = 1_099_511_628_211;
        let mut hash = FNV_OFFSET;
        for byte in b"file-content".iter().copied().chain([0xff]) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        format!("fnv64:{hash:016x}")
    }

    fn assert_contains(haystack: &str, needle: &str) {
        assert!(
            haystack.contains(needle),
            "expected output to contain `{needle}`; output was:\n{haystack}"
        );
    }
}
