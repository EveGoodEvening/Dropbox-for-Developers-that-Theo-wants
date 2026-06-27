//! Foundation contracts shared by all implementation chunks.

pub mod config;
pub mod errors;
pub mod logging;
pub mod migration;
pub mod platform;

pub use config::{Config, ConfigPaths};
pub use errors::{ConfigError, EnvError, ErrorCode, MigrationError, SyncError, VfsError, WatchError};
pub use logging::{LogEvent, LogField, LogLevel, Logger, StderrLogger};
pub use migration::{
    InMemoryMigrationStore, Migration, MigrationReport, MigrationRunner, MigrationStore,
    BASELINE_SCHEMA_VERSION, SCHEMA_VERSION_TABLE, SCHEMA_VERSION_TABLE_SQL,
};
pub use platform::{
    Architecture, MachineId, MachineIdProvenance, OsFamily, Platform, PlatformCapabilities,
};
