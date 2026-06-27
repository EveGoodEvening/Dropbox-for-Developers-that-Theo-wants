use std::error::Error;
use std::fmt;
use std::path::PathBuf;

/// Stable string-coded error classes used across module boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    ConfigInvalid,
    ConfigIo,
    LoggingIo,
    MigrationFailed,
    WatchUnsupported,
    VfsUnsupported,
    EnvInvalid,
    CliInvalidCommand,
    PlatformProbeFailed,
}

impl ErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ConfigInvalid => "CONFIG_INVALID",
            Self::ConfigIo => "CONFIG_IO",
            Self::LoggingIo => "LOGGING_IO",
            Self::MigrationFailed => "MIGRATION_FAILED",
            Self::WatchUnsupported => "WATCH_UNSUPPORTED",
            Self::VfsUnsupported => "VFS_UNSUPPORTED",
            Self::EnvInvalid => "ENV_INVALID",
            Self::CliInvalidCommand => "CLI_INVALID_COMMAND",
            Self::PlatformProbeFailed => "PLATFORM_PROBE_FAILED",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    code: ErrorCode,
    message: String,
    path: Option<PathBuf>,
}

impl ConfigError {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: ErrorCode::ConfigInvalid,
            message: message.into(),
            path: None,
        }
    }

    pub fn invalid_at(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Self {
            code: ErrorCode::ConfigInvalid,
            message: message.into(),
            path: Some(path.into()),
        }
    }

    pub fn io(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Self {
            code: ErrorCode::ConfigIo,
            message: message.into(),
            path: Some(path.into()),
        }
    }

    pub fn code(&self) -> &'static str {
        self.code.as_str()
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.path {
            Some(path) => write!(formatter, "{} at {}: {}", self.code, path.display(), self.message),
            None => write!(formatter, "{}: {}", self.code, self.message),
        }
    }
}

impl Error for ConfigError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationError {
    code: ErrorCode,
    message: String,
}

impl MigrationError {
    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            code: ErrorCode::MigrationFailed,
            message: message.into(),
        }
    }

    pub fn code(&self) -> &'static str {
        self.code.as_str()
    }
}

impl fmt::Display for MigrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl Error for MigrationError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchError {
    message: String,
}

impl WatchError {
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }

    pub fn code(&self) -> &'static str {
        ErrorCode::WatchUnsupported.as_str()
    }
}

impl fmt::Display for WatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code(), self.message)
    }
}

impl Error for WatchError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VfsError {
    message: String,
}

impl VfsError {
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }

    pub fn code(&self) -> &'static str {
        ErrorCode::VfsUnsupported.as_str()
    }
}

impl fmt::Display for VfsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code(), self.message)
    }
}

impl Error for VfsError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvError {
    message: String,
}

impl EnvError {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }

    pub fn code(&self) -> &'static str {
        ErrorCode::EnvInvalid.as_str()
    }
}

impl fmt::Display for EnvError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code(), self.message)
    }
}

impl Error for EnvError {}

/// Top-level shared error used by the CLI smoke path and future modules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncError {
    Config(ConfigError),
    Migration(MigrationError),
    Watch(WatchError),
    Vfs(VfsError),
    Env(EnvError),
    Logging { message: String },
    Cli { message: String },
    Platform { message: String },
}

impl SyncError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Config(error) => error.code(),
            Self::Migration(error) => error.code(),
            Self::Watch(error) => error.code(),
            Self::Vfs(error) => error.code(),
            Self::Env(error) => error.code(),
            Self::Logging { .. } => ErrorCode::LoggingIo.as_str(),
            Self::Cli { .. } => ErrorCode::CliInvalidCommand.as_str(),
            Self::Platform { .. } => ErrorCode::PlatformProbeFailed.as_str(),
        }
    }

    pub fn cli(message: impl Into<String>) -> Self {
        Self::Cli { message: message.into() }
    }

    pub fn logging(message: impl Into<String>) -> Self {
        Self::Logging { message: message.into() }
    }

    pub fn platform(message: impl Into<String>) -> Self {
        Self::Platform { message: message.into() }
    }
}

impl fmt::Display for SyncError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(error) => write!(formatter, "{error}"),
            Self::Migration(error) => write!(formatter, "{error}"),
            Self::Watch(error) => write!(formatter, "{error}"),
            Self::Vfs(error) => write!(formatter, "{error}"),
            Self::Env(error) => write!(formatter, "{error}"),
            Self::Logging { message } => write!(formatter, "{}: {message}", ErrorCode::LoggingIo),
            Self::Cli { message } => write!(formatter, "{}: {message}", ErrorCode::CliInvalidCommand),
            Self::Platform { message } => write!(formatter, "{}: {message}", ErrorCode::PlatformProbeFailed),
        }
    }
}

impl Error for SyncError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Config(error) => Some(error),
            Self::Migration(error) => Some(error),
            Self::Watch(error) => Some(error),
            Self::Vfs(error) => Some(error),
            Self::Env(error) => Some(error),
            Self::Logging { .. } | Self::Cli { .. } | Self::Platform { .. } => None,
        }
    }
}

impl From<ConfigError> for SyncError {
    fn from(error: ConfigError) -> Self {
        Self::Config(error)
    }
}

impl From<MigrationError> for SyncError {
    fn from(error: MigrationError) -> Self {
        Self::Migration(error)
    }
}

impl From<WatchError> for SyncError {
    fn from(error: WatchError) -> Self {
        Self::Watch(error)
    }
}

impl From<VfsError> for SyncError {
    fn from(error: VfsError) -> Self {
        Self::Vfs(error)
    }
}

impl From<EnvError> for SyncError {
    fn from(error: EnvError) -> Self {
        Self::Env(error)
    }
}
