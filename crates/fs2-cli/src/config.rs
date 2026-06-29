//! CLI configuration: stores backend URL, token, and user/device IDs.
//!
//! Config is stored in `~/.fs2/config.json`. Tokens are stored here for
//! development convenience; in production they should go in the OS keychain.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// CLI configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliConfig {
    /// Backend URL.
    pub backend_url: String,
    /// JWT access token.
    pub token: String,
    /// User ID.
    pub user_id: uuid::Uuid,
    /// Device ID.
    pub device_id: uuid::Uuid,
}

impl CliConfig {
    /// Get the path to the config file.
    fn config_path() -> anyhow::Result<PathBuf> {
        let home = dirs_or_env()?;
        let dir = home.join(".fs2");
        Ok(dir.join("config.json"))
    }

    /// Load config from disk.
    ///
    /// # Errors
    /// Returns an error if the config file cannot be read or parsed.
    pub fn load() -> anyhow::Result<Self> {
        let path = Self::config_path()?;
        let data = std::fs::read_to_string(&path)?;
        let cfg: Self = serde_json::from_str(&data)?;
        Ok(cfg)
    }

    /// Save config to disk.
    ///
    /// # Errors
    /// Returns an error if the config file cannot be written.
    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::config_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, data)?;
        Ok(())
    }

    /// Clear saved config (logout).
    ///
    /// # Errors
    /// Returns an error if the config file cannot be removed.
    pub fn clear() -> anyhow::Result<()> {
        let path = Self::config_path()?;
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        Ok(())
    }
}

/// Get the home directory.
fn dirs_or_env() -> anyhow::Result<PathBuf> {
    if let Ok(home) = std::env::var("HOME") {
        return Ok(PathBuf::from(home));
    }
    anyhow::bail!("cannot determine home directory (HOME not set)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_roundtrip() {
        let cfg = CliConfig {
            backend_url: "http://localhost:8787".to_owned(),
            token: "test-token".to_owned(),
            user_id: uuid::Uuid::new_v4(),
            device_id: uuid::Uuid::new_v4(),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: CliConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg.backend_url, back.backend_url);
        assert_eq!(cfg.token, back.token);
        assert_eq!(cfg.user_id, back.user_id);
        assert_eq!(cfg.device_id, back.device_id);
    }
}
