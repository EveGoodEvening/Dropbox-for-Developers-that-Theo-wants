//! Backend configuration loader.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Backend configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendConfig {
    /// Bind address, e.g. `"127.0.0.1:8080"`.
    #[serde(default = "default_bind")]
    pub bind: String,
    /// Database URL. If empty, the in-memory store is used (dev/test only).
    #[serde(default)]
    pub database_url: String,
    /// Object store configuration.
    #[serde(default)]
    pub object_store: ObjectStoreConfig,
    /// JWT/session signing secret. If empty in dev mode, a random secret is
    /// generated with a clear warning.
    #[serde(default)]
    pub jwt_secret: String,
    /// Whether to run in dev mode (enables dev-only auth endpoints).
    #[serde(default = "default_true")]
    pub dev_mode: bool,
}

/// Object store configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ObjectStoreConfig {
    /// Backend type: `"local"`, `"s3"`, or `"r2"`.
    #[serde(default = "default_local")]
    pub backend: String,
    /// Local filesystem root for the `"local"` backend.
    #[serde(default)]
    pub local_root: String,
    /// S3/R2 endpoint URL.
    #[serde(default)]
    pub endpoint: String,
    /// S3/R2 bucket name.
    #[serde(default)]
    pub bucket: String,
    /// S3/R2 access key.
    #[serde(default)]
    pub access_key: String,
    /// S3/R2 secret key.
    #[serde(default)]
    pub secret_key: String,
}

fn default_bind() -> String {
    "127.0.0.1:8080".to_owned()
}
fn default_true() -> bool {
    true
}
fn default_local() -> String {
    "local".to_owned()
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            database_url: String::new(),
            object_store: ObjectStoreConfig {
                backend: default_local(),
                local_root: String::new(),
                endpoint: String::new(),
                bucket: String::new(),
                access_key: String::new(),
                secret_key: String::new(),
            },
            jwt_secret: String::new(),
            dev_mode: true,
        }
    }
}

/// Error returned when config loading fails.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Environment variable error.
    #[error("config error: {0}")]
    Env(String),
}

impl BackendConfig {
    /// Load configuration from environment variables, with defaults.
    ///
    /// Supported variables:
    /// - `FS2_BACKEND_BIND` — bind address
    /// - `FS2_DATABASE_URL` — Postgres URL (empty = in-memory store)
    /// - `FS2_JWT_SECRET` — JWT signing secret
    /// - `FS2_DEV_MODE` — `"true"`/`"false"`
    /// - `FS2_OBJECT_STORE_BACKEND` — `"local"`/`"s3"`/`"r2"`
    /// - `FS2_OBJECT_STORE_LOCAL_ROOT` — local root path
    pub fn from_env() -> Result<Self, ConfigError> {
        let mut cfg = Self::default();
        if let Ok(v) = std::env::var("FS2_BACKEND_BIND") {
            cfg.bind = v;
        }
        if let Ok(v) = std::env::var("FS2_DATABASE_URL") {
            cfg.database_url = v;
        }
        if let Ok(v) = std::env::var("FS2_JWT_SECRET") {
            cfg.jwt_secret = v;
        }
        if let Ok(v) = std::env::var("FS2_DEV_MODE") {
            cfg.dev_mode = v == "true" || v == "1";
        }
        if let Ok(v) = std::env::var("FS2_OBJECT_STORE_BACKEND") {
            cfg.object_store.backend = v;
        }
        if let Ok(v) = std::env::var("FS2_OBJECT_STORE_LOCAL_ROOT") {
            cfg.object_store.local_root = v;
        }
        Ok(cfg)
    }

    /// Whether the in-memory store should be used (no database URL configured).
    #[must_use]
    pub fn use_memory_store(&self) -> bool {
        self.database_url.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sensible() {
        let cfg = BackendConfig::default();
        assert_eq!(cfg.bind, "127.0.0.1:8080");
        assert!(cfg.use_memory_store());
        assert!(cfg.dev_mode);
        assert_eq!(cfg.object_store.backend, "local");
    }

    #[test]
    fn from_env_reads_variables() {
        std::env::set_var("FS2_BACKEND_BIND", "0.0.0.0:9090");
        std::env::set_var("FS2_DEV_MODE", "false");
        let cfg = BackendConfig::from_env().unwrap();
        assert_eq!(cfg.bind, "0.0.0.0:9090");
        assert!(!cfg.dev_mode);
        std::env::remove_var("FS2_BACKEND_BIND");
        std::env::remove_var("FS2_DEV_MODE");
    }
}
