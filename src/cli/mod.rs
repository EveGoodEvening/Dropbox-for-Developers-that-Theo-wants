//! Minimal CHUNK-01 CLI smoke shim.
//!
//! CHUNK-08 owns the full user-facing command surface after this skeleton
//! merges. CHUNK-01 intentionally exposes only `version` and `info`.

use crate::foundation::{
    Config, InMemoryMigrationStore, LogEvent, LogLevel, Logger, MigrationRunner, StderrLogger,
    SyncError,
};

pub const APP_NAME: &str = "dropbox-dev";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

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

    match command.as_deref() {
        None | Some("--help") | Some("-h") => Ok(help()),
        Some("version") | Some("--version") | Some("-V") => Ok(version()),
        Some("info") => info(logger),
        Some(other) => Err(SyncError::cli(format!("unknown command `{other}`"))),
    }
}

fn version() -> String {
    format!("{APP_NAME} {VERSION}\n")
}

fn info(logger: &dyn Logger) -> Result<String, SyncError> {
    let config = Config::load_default()?;
    let mut store = InMemoryMigrationStore::new();
    let migration_report = MigrationRunner::empty_v0().apply(&mut store)?;

    logger.emit(
        &LogEvent::new(LogLevel::Info, "cli info smoke", config.machine_id.clone())
            .with_field("machine_id_provenance", config.machine_id_provenance.to_string())
            .with_correlation_id("cli-info")
            .with_field("version", VERSION)
            .with_field("config_path", config.config_path.display().to_string())
            .with_field("schema_version", migration_report.schema_version.clone()),
    )?;

    Ok(format!(
        "machine_id={}\nversion={}\nconfig_path={}\nschema_version={}\nproduct_tables={}\n",
        config.machine_id,
        VERSION,
        config.config_path.display(),
        migration_report.schema_version,
        migration_report.product_table_count()
    ))
}

fn help() -> String {
    format!(
        "{APP_NAME} {VERSION}\n\nCommands:\n  version    Print application version\n  info       Print machine id, version, config path, and baseline schema info\n"
    )
}

