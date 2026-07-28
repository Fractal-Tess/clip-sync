use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
};

use directories::BaseDirs;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::model::EffectiveSharedSettings;
pub use crate::model::{DEFAULT_CAPTURE_THRESHOLD_BYTES, DEFAULT_MESH_QUOTA_BYTES};
pub const DEFAULT_LISTEN_PORT: u16 = 24_892;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub shared: SharedConfig,
    pub local: LocalConfig,
}

impl Config {
    /// Loads TOML from `path`, or returns defaults when the file does not exist.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read, decoded, or validated.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let source = match fs::read_to_string(path) {
            Ok(source) => source,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(source) => return Err(ConfigError::Read(source)),
        };

        let config: Self = toml::from_str(&source)?;
        config.validate()?;
        Ok(config)
    }

    /// Atomically saves validated configuration as TOML.
    ///
    /// # Errors
    ///
    /// Returns an error when validation, encoding, or file replacement fails.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        self.validate()?;
        let parent = path.parent().ok_or(ConfigError::MissingParent)?;
        fs::create_dir_all(parent)?;

        let encoded = toml::to_string_pretty(self)?;
        let temporary = path.with_extension("toml.tmp");
        let mut file = fs::File::create(&temporary)?;
        file.write_all(encoded.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(temporary, path)?;
        Ok(())
    }

    /// Validates resource limits and required local settings.
    ///
    /// # Errors
    ///
    /// Returns an error when a required value is zero or empty.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.shared.mesh_quota_bytes == 0 {
            return Err(ConfigError::Invalid(
                "shared.mesh_quota_bytes must be greater than zero",
            ));
        }
        if self.shared.capture_threshold_bytes == 0 {
            return Err(ConfigError::Invalid(
                "shared.capture_threshold_bytes must be greater than zero",
            ));
        }
        if self.local.discovery_interval_seconds == 0 {
            return Err(ConfigError::Invalid(
                "local.discovery_interval_seconds must be greater than zero",
            ));
        }
        if self.local.mesh_key_file.as_os_str().is_empty() {
            return Err(ConfigError::Invalid(
                "local.mesh_key_file must not be empty",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SharedConfig {
    pub mesh_quota_bytes: u64,
    pub capture_threshold_bytes: u64,
}

impl Default for SharedConfig {
    fn default() -> Self {
        Self {
            mesh_quota_bytes: DEFAULT_MESH_QUOTA_BYTES,
            capture_threshold_bytes: DEFAULT_CAPTURE_THRESHOLD_BYTES,
        }
    }
}

impl From<EffectiveSharedSettings> for SharedConfig {
    fn from(settings: EffectiveSharedSettings) -> Self {
        Self {
            mesh_quota_bytes: settings.mesh_quota_bytes,
            capture_threshold_bytes: settings.capture_threshold_bytes,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalConfig {
    pub mesh_key_file: PathBuf,
    pub listen_port: u16,
    pub discovery_interval_seconds: u64,
    pub netbird_command: PathBuf,
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            mesh_key_file: PathBuf::from("/run/secrets/clip-sync-mesh-key"),
            listen_port: DEFAULT_LISTEN_PORT,
            discovery_interval_seconds: 15,
            netbird_command: PathBuf::from("netbird"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    pub config: PathBuf,
    pub state_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub socket: PathBuf,
}

impl AppPaths {
    /// Resolves configuration, state, runtime, and IPC paths from XDG variables.
    ///
    /// # Errors
    ///
    /// Returns an error when the home or runtime directory cannot be determined.
    pub fn discover(config_override: Option<PathBuf>) -> Result<Self, ConfigError> {
        let base = BaseDirs::new().ok_or(ConfigError::MissingHome)?;
        let config =
            config_override.unwrap_or_else(|| base.config_dir().join("clip-sync/config.toml"));
        let state_dir = env::var_os("XDG_STATE_HOME").map_or_else(
            || base.home_dir().join(".local/state/clip-sync"),
            |path| PathBuf::from(path).join("clip-sync"),
        );
        let runtime_root = env::var_os("XDG_RUNTIME_DIR").ok_or(ConfigError::MissingRuntime)?;
        let runtime_dir = PathBuf::from(runtime_root).join("clip-sync");
        let socket = runtime_dir.join("daemon.sock");

        Ok(Self {
            config,
            state_dir,
            runtime_dir,
            socket,
        })
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not determine the user's home directory")]
    MissingHome,
    #[error("XDG_RUNTIME_DIR is not set")]
    MissingRuntime,
    #[error("the config path has no parent directory")]
    MissingParent,
    #[error("could not read the config: {0}")]
    Read(std::io::Error),
    #[error("invalid TOML: {0}")]
    TomlDecode(#[from] toml::de::Error),
    #[error("could not encode TOML: {0}")]
    TomlEncode(#[from] toml::ser::Error),
    #[error("config is invalid: {0}")]
    Invalid(&'static str),
    #[error("config I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_product_decisions() {
        let config = Config::default();
        assert_eq!(config.shared.mesh_quota_bytes, 1024 * 1024 * 1024);
        assert_eq!(config.shared.capture_threshold_bytes, 20 * 1024 * 1024);
        assert_eq!(config.local.listen_port, 24_892);
    }

    #[test]
    fn config_round_trips() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let path = temp.path().join("config.toml");
        let config = Config::default();

        config.save(&path).expect("save config");
        let loaded = Config::load(&path).expect("load config");

        assert_eq!(loaded, config);
    }

    #[test]
    fn rejects_zero_quota() {
        let config = Config {
            shared: SharedConfig {
                mesh_quota_bytes: 0,
                ..SharedConfig::default()
            },
            ..Config::default()
        };

        assert!(matches!(config.validate(), Err(ConfigError::Invalid(_))));
    }
}
