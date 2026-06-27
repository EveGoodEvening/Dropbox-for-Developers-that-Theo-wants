use crate::foundation::platform::{MachineId, MachineIdProvenance, Platform};
use crate::foundation::ConfigError;
use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub machine_id: String,
    pub machine_id_provenance: MachineIdProvenance,
    pub root_paths: Vec<PathBuf>,
    pub transport_endpoint: String,
    pub cache_dir: PathBuf,
    pub config_path: PathBuf,
}

impl Config {
    pub fn load_default() -> Result<Self, ConfigError> {
        Self::load_or_default(Self::default_config_path()?)
    }

    pub fn load_or_default(path: impl Into<PathBuf>) -> Result<Self, ConfigError> {
        let path = path.into();
        let mut config = Self::default_with_path(path.clone())?;

        if let Some(contents) = read_config_contents_if_present(&path)? {
            config.apply_kv_contents(&contents)?;
        }

        config.validate()?;
        Ok(config)
    }

    pub fn defaults() -> Result<Self, ConfigError> {
        let config = Self::default_with_path(Self::default_config_path()?)?;
        config.validate()?;
        Ok(config)
    }

    pub fn paths(&self) -> ConfigPaths {
        ConfigPaths {
            config_path: self.config_path.clone(),
            cache_dir: self.cache_dir.clone(),
        }
    }

    pub fn default_config_path() -> Result<PathBuf, ConfigError> {
        default_config_path_from_env(
            env_path("DROPBOX_DEV_CONFIG"),
            env_path("XDG_CONFIG_HOME"),
            env_path("APPDATA"),
            env_path("HOME"),
        )
    }

    pub fn default_cache_dir() -> PathBuf {
        if let Some(path) = absolute_env_path("DROPBOX_DEV_CACHE_DIR") {
            return path;
        }

        let base = absolute_env_path("XDG_CACHE_HOME")
            .or_else(|| absolute_env_path("LOCALAPPDATA"))
            .or_else(|| absolute_env_path("HOME").map(|home| home.join(".cache")))
            .unwrap_or_else(env::temp_dir);

        base.join("dropbox-dev")
    }

    fn default_with_path(config_path: PathBuf) -> Result<Self, ConfigError> {
        ensure_absolute_config_path(&config_path)?;
        let platform = Platform::detect();
        Ok(Self {
            machine_id: platform.machine_id.value,
            machine_id_provenance: platform.machine_id.provenance,
            root_paths: Vec::new(),
            transport_endpoint: "unconfigured".to_owned(),
            cache_dir: Self::default_cache_dir(),
            config_path,
        })
    }

    fn apply_kv_contents(&mut self, contents: &str) -> Result<(), ConfigError> {
        for (index, line) in contents.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let Some((key, value)) = line.split_once('=') else {
                return Err(ConfigError::invalid_at(
                    self.config_path.clone(),
                    format!("line {} is not key=value", index + 1),
                ));
            };

            let key = key.trim();
            let value = strip_optional_quotes(value.trim());
            match key {
                "machine_id" => {
                    self.machine_id = value.trim().to_owned();
                    self.machine_id_provenance =
                        MachineIdProvenance::ConfigFile(self.config_path.clone());
                }
                "transport_endpoint" => self.transport_endpoint = value,
                "cache_dir" => self.cache_dir = PathBuf::from(value),
                "root_path" => self.root_paths.push(PathBuf::from(value)),
                "root_paths" => {
                    self.root_paths = value
                        .split(',')
                        .map(str::trim)
                        .filter(|entry| !entry.is_empty())
                        .map(PathBuf::from)
                        .collect();
                }
                other => {
                    return Err(ConfigError::invalid_at(
                        self.config_path.clone(),
                        format!("unknown config key `{other}`"),
                    ));
                }
            }
        }

        Ok(())
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        ensure_absolute_config_path(&self.config_path)?;

        let machine_id = self.machine_id.trim();
        if machine_id.is_empty() {
            return Err(ConfigError::invalid_at(
                self.config_path.clone(),
                "machine_id must not be empty",
            ));
        }

        if !MachineId::is_app_scoped_value(machine_id) {
            return Err(ConfigError::invalid_at(
                self.config_path.clone(),
                "machine_id must be a dropbox-dev app-scoped id",
            ));
        }

        if matches!(&self.machine_id_provenance, MachineIdProvenance::Fallback) {
            return Err(ConfigError::invalid_at(
                self.config_path.clone(),
                "machine_id source is unavailable; set a dropbox-dev app-scoped machine_id in config",
            ));
        }

        if self.transport_endpoint.trim().is_empty() {
            return Err(ConfigError::invalid_at(
                self.config_path.clone(),
                "transport_endpoint must not be empty",
            ));
        }

        if self.cache_dir.as_os_str().is_empty() {
            return Err(ConfigError::invalid_at(
                self.config_path.clone(),
                "cache_dir must not be empty",
            ));
        }

        for root_path in &self.root_paths {
            if root_path.as_os_str().is_empty() {
                return Err(ConfigError::invalid_at(
                    self.config_path.clone(),
                    "root_paths entries must not be empty",
                ));
            }
        }

        Ok(())
    }
}

fn read_config_contents_if_present(path: &Path) -> Result<Option<String>, ConfigError> {
    match fs::metadata(path) {
        Ok(_) => fs::read_to_string(path)
            .map(Some)
            .map_err(|error| ConfigError::io(path.to_path_buf(), error.to_string())),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(ConfigError::io(path.to_path_buf(), error.to_string())),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPaths {
    pub config_path: PathBuf,
    pub cache_dir: PathBuf,
}

fn default_config_path_from_env(
    dropbox_config: Option<PathBuf>,
    xdg_config_home: Option<PathBuf>,
    appdata: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Result<PathBuf, ConfigError> {
    if let Some(path) = dropbox_config {
        return require_absolute_env_path("DROPBOX_DEV_CONFIG", path);
    }

    if let Some(path) = xdg_config_home {
        return Ok(require_absolute_env_path("XDG_CONFIG_HOME", path)?
            .join("dropbox-dev")
            .join("config.conf"));
    }

    if let Some(path) = appdata {
        return Ok(require_absolute_env_path("APPDATA", path)?
            .join("dropbox-dev")
            .join("config.conf"));
    }

    if let Some(path) = home {
        return Ok(require_absolute_env_path("HOME", path)?
            .join(".config")
            .join("dropbox-dev")
            .join("config.conf"));
    }

    Err(ConfigError::invalid(
        "config home is unavailable; set DROPBOX_DEV_CONFIG, XDG_CONFIG_HOME, APPDATA, or HOME to an absolute path",
    ))
}

fn require_absolute_env_path(key: &str, path: PathBuf) -> Result<PathBuf, ConfigError> {
    if path.is_absolute() {
        return Ok(path);
    }

    Err(ConfigError::invalid(format!(
        "{key} must be an absolute path; refusing to resolve config from the current working directory"
    )))
}

fn ensure_absolute_config_path(path: &Path) -> Result<(), ConfigError> {
    if path.is_absolute() {
        return Ok(());
    }

    Err(ConfigError::invalid_at(
        path.to_path_buf(),
        "config_path must be absolute; refusing to read config from the current working directory",
    ))
}

fn absolute_env_path(key: &str) -> Option<PathBuf> {
    env_path(key).filter(|path| path.is_absolute())
}

fn env_path(key: &str) -> Option<PathBuf> {
    env::var_os(key)
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

fn strip_optional_quotes(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        if (bytes[0] == b'\"' && bytes[value.len() - 1] == b'\"')
            || (bytes[0] == b'\'' && bytes[value.len() - 1] == b'\'')
        {
            return value[1..value.len() - 1].to_owned();
        }
    }
    value.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_path_requires_config_home() {
        let error = default_config_path_from_env(None, None, None, None).unwrap_err();

        assert_eq!(error.code(), "CONFIG_INVALID");
    }

    #[test]
    fn default_config_path_rejects_relative_explicit_path() {
        let error = default_config_path_from_env(
            Some(PathBuf::from("dropbox-dev/config.conf")),
            None,
            None,
            None,
        )
        .unwrap_err();

        assert_eq!(error.code(), "CONFIG_INVALID");
    }

    #[test]
    fn default_config_path_uses_absolute_xdg_home() {
        let xdg_home = std::env::current_dir().unwrap().join("xdg-config-home");
        let path = default_config_path_from_env(None, Some(xdg_home.clone()), None, None).unwrap();

        assert_eq!(path, xdg_home.join("dropbox-dev").join("config.conf"));
    }

    #[test]
    fn validation_rejects_raw_machine_id() {
        let mut config = Config::default_with_path(
            std::env::current_dir().unwrap().join("dropbox-dev/config.conf"),
        )
        .unwrap();
        config.machine_id = "0123456789abcdef0123456789abcdef".to_owned();

        assert!(config.validate().is_err());
    }

    #[test]
    fn validation_rejects_unstable_fallback_machine_id() {
        let mut config = Config::default_with_path(
            std::env::current_dir().unwrap().join("dropbox-dev/config.conf"),
        )
        .unwrap();
        config.machine_id_provenance = MachineIdProvenance::Fallback;

        assert!(config.validate().is_err());
    }

    fn unique_temp_path(label: &str) -> PathBuf {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "dropbox-dev-{label}-{}-{timestamp}",
            std::process::id()
        ))
    }

    #[test]
    fn optional_config_reader_skips_missing_files() {
        let path = unique_temp_path("missing-config");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&path);

        assert_eq!(read_config_contents_if_present(&path).unwrap(), None);
    }

    #[test]
    fn optional_config_reader_returns_config_io_for_read_errors() {
        let path = unique_temp_path("config-dir");
        std::fs::create_dir_all(&path).unwrap();

        let error = read_config_contents_if_present(&path).unwrap_err();

        std::fs::remove_dir_all(&path).unwrap();
        assert_eq!(error.code(), "CONFIG_IO");
    }

    #[cfg(unix)]
    #[test]
    fn optional_config_reader_returns_config_io_for_metadata_errors() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let path = PathBuf::from(OsString::from_vec(b"/tmp/dropbox-dev-\0config.conf".to_vec()));
        let error = read_config_contents_if_present(&path).unwrap_err();

        assert_eq!(error.code(), "CONFIG_IO");
    }
}
