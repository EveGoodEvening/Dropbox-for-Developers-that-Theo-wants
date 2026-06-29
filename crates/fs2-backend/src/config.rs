//! Backend configuration.

use serde::{Deserialize, Serialize};

/// Backend server configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendConfig {
    /// Bind address for the HTTP server.
    #[serde(default = "default_bind")]
    pub bind: String,
    /// Database URL. If empty, an in-memory store is used.
    #[serde(default)]
    pub database_url: String,
    /// Object store configuration (local path for dev, S3 config for prod).
    #[serde(default)]
    pub object_store: ObjectStoreConfig,
    /// JWT signing secret for access tokens.
    #[serde(default = "default_jwt_secret")]
    pub jwt_secret: String,
    /// Whether dev-only auth endpoints are enabled.
    #[serde(default = "default_dev_auth")]
    pub dev_auth: bool,
}

/// Object store configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObjectStoreConfig {
    /// Local filesystem blob store (for development).
    Local {
        /// Root directory for blob storage.
        path: String,
    },
    /// S3-compatible blob store.
    S3 {
        /// Endpoint URL.
        endpoint: String,
        /// Bucket name.
        bucket: String,
        /// Region.
        region: String,
        /// Access key ID.
        access_key_id: String,
        /// Secret access key.
        secret_access_key: String,
    },
}

impl Default for ObjectStoreConfig {
    fn default() -> Self {
        Self::Local {
            path: "./.fs2-backend/blobs".to_owned(),
        }
    }
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            database_url: String::new(),
            object_store: ObjectStoreConfig::default(),
            jwt_secret: default_jwt_secret(),
            dev_auth: true,
        }
    }
}

impl BackendConfig {
    /// Load config from environment variables, falling back to defaults.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            bind: std::env::var("FS2_BACKEND_BIND").unwrap_or_else(|_| default_bind()),
            database_url: std::env::var("FS2_DATABASE_URL").unwrap_or_default(),
            object_store: ObjectStoreConfig::default(),
            jwt_secret: std::env::var("FS2_JWT_SECRET").unwrap_or_else(|_| default_jwt_secret()),
            dev_auth: std::env::var("FS2_DEV_AUTH").map_or(true, |v| v != "false" && v != "0"),
        }
    }
}

fn default_bind() -> String {
    "127.0.0.1:8787".to_owned()
}

fn default_jwt_secret() -> String {
    "dev-only-secret-change-me".to_owned()
}

fn default_dev_auth() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let cfg = BackendConfig::default();
        assert_eq!(cfg.bind, "127.0.0.1:8787");
        assert!(cfg.dev_auth);
        assert!(cfg.database_url.is_empty());
    }

    #[test]
    fn from_env_uses_defaults() {
        // Clear any existing env vars to avoid interference.
        std::env::remove_var("FS2_BACKEND_BIND");
        let cfg = BackendConfig::from_env();
        assert_eq!(cfg.bind, "127.0.0.1:8787");
    }
}
