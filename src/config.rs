use std::{
    env, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

use directories::BaseDirs;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::model::EffectiveSharedSettings;
pub use crate::model::{DEFAULT_CAPTURE_THRESHOLD_BYTES, DEFAULT_MESH_QUOTA_BYTES};
pub const DEFAULT_LISTEN_PORT: u16 = 24_892;
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

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
        let file = match fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(source) => return Err(ConfigError::Read(source)),
        };
        if file.metadata()?.len() > MAX_CONFIG_BYTES {
            return Err(ConfigError::TooLarge);
        }
        let mut source = String::new();
        file.take(MAX_CONFIG_BYTES + 1)
            .read_to_string(&mut source)?;
        if u64::try_from(source.len()).unwrap_or(u64::MAX) > MAX_CONFIG_BYTES {
            return Err(ConfigError::TooLarge);
        }

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
        let temporary = parent.join(format!(".config.toml.{}.tmp", uuid::Uuid::new_v4()));
        let result: Result<(), ConfigError> = (|| {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temporary)?;
            file.write_all(encoded.as_bytes())?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, path)?;
            fs::File::open(parent)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result?;
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
        if self.local.listen_port == 0 {
            return Err(ConfigError::Invalid(
                "local.listen_port must be greater than zero",
            ));
        }
        if self.local.reconcile_interval_seconds == 0 {
            return Err(ConfigError::Invalid(
                "local.reconcile_interval_seconds must be greater than zero",
            ));
        }
        if self.local.reconnect_max_seconds < self.local.reconnect_min_seconds
            || self.local.reconnect_min_seconds == 0
        {
            return Err(ConfigError::Invalid(
                "local reconnect bounds must be nonzero and ordered",
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
    pub reconcile_interval_seconds: u64,
    pub reconnect_min_seconds: u64,
    pub reconnect_max_seconds: u64,
    pub netbird_command: PathBuf,
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            mesh_key_file: PathBuf::from("/run/secrets/clip-sync-mesh-key"),
            listen_port: DEFAULT_LISTEN_PORT,
            discovery_interval_seconds: 15,
            reconcile_interval_seconds: 5,
            reconnect_min_seconds: 1,
            reconnect_max_seconds: 60,
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
        let state_root =
            xdg_path("XDG_STATE_HOME")?.unwrap_or_else(|| base.home_dir().join(".local/state"));
        let state_dir = state_root.join("clip-sync");
        let runtime_root = xdg_path("XDG_RUNTIME_DIR")?.ok_or(ConfigError::MissingRuntime)?;
        let runtime_dir = runtime_root.join("clip-sync");
        let socket = runtime_dir.join("daemon.sock");

        Ok(Self {
            config,
            state_dir,
            runtime_dir,
            socket,
        })
    }
}

fn xdg_path(variable: &'static str) -> Result<Option<PathBuf>, ConfigError> {
    xdg_path_value(variable, env::var_os(variable))
}

fn xdg_path_value(
    variable: &'static str,
    value: Option<std::ffi::OsString>,
) -> Result<Option<PathBuf>, ConfigError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_empty() {
        return Ok(None);
    }
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(ConfigError::RelativeXdgPath { variable, path });
    }
    Ok(Some(path))
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not determine the user's home directory")]
    MissingHome,
    #[error("XDG_RUNTIME_DIR is not set")]
    MissingRuntime,
    #[error("{variable} must be an absolute path, got {path:?}")]
    RelativeXdgPath {
        variable: &'static str,
        path: PathBuf,
    },
    #[error("the config path has no parent directory")]
    MissingParent,
    #[error("could not read the config: {0}")]
    Read(std::io::Error),
    #[error("config exceeds the 1 MiB size limit")]
    TooLarge,
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
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
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

    #[test]
    fn rejects_oversized_config_before_parsing() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let path = temp.path().join("config.toml");
        fs::write(
            &path,
            vec![b'x'; usize::try_from(MAX_CONFIG_BYTES + 1).unwrap()],
        )
        .unwrap();
        assert!(matches!(Config::load(&path), Err(ConfigError::TooLarge)));
    }

    #[test]
    fn xdg_paths_must_be_absolute() {
        assert_eq!(
            xdg_path_value("XDG_STATE_HOME", Some(std::ffi::OsString::new())).expect("empty path"),
            None
        );
        let error =
            xdg_path_value("XDG_RUNTIME_DIR", Some("relative".into())).expect_err("relative path");
        assert!(matches!(
            error,
            ConfigError::RelativeXdgPath {
                variable: "XDG_RUNTIME_DIR",
                ..
            }
        ));
    }
}
