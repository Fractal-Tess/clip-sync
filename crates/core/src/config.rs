use std::{
    env, fs,
    io::Read,
    net::IpAddr,
    path::{Path, PathBuf},
};

use directories::BaseDirs;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const DEFAULT_LISTEN_PORT: u16 = 24_892;
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MIB: u64 = 1024 * 1024;

/// Per-host settings, read once at start. Nothing here replicates; keys from
/// older versions (the `[shared]` section, share and transfer limits) are
/// ignored so an old file still loads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
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

    /// Validates resource limits and required settings.
    ///
    /// # Errors
    ///
    /// Returns an error when a required value is zero, empty, or out of range.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let local = &self.local;
        if local.inline_limit_bytes == 0 {
            return Err(ConfigError::Invalid(
                "local.inline_limit_bytes must be greater than zero",
            ));
        }
        if local.max_capture_bytes < local.inline_limit_bytes {
            return Err(ConfigError::Invalid(
                "local.max_capture_bytes must be at least local.inline_limit_bytes",
            ));
        }
        if local.history_quota_bytes == 0 {
            return Err(ConfigError::Invalid(
                "local.history_quota_bytes must be greater than zero",
            ));
        }
        if local.discovery_interval_seconds == 0 {
            return Err(ConfigError::Invalid(
                "local.discovery_interval_seconds must be greater than zero",
            ));
        }
        if local.peer_interfaces.len() > 32 {
            return Err(ConfigError::Invalid(
                "local.peer_interfaces must contain at most 32 interface names",
            ));
        }
        let mut interfaces = std::collections::BTreeSet::new();
        for interface in &local.peer_interfaces {
            if interface.is_empty()
                || interface.len() > 15
                || !interface
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_.:-".contains(&byte))
            {
                return Err(ConfigError::Invalid(
                    "local.peer_interfaces contains an invalid Linux interface name",
                ));
            }
            if !interfaces.insert(interface) {
                return Err(ConfigError::Invalid(
                    "local.peer_interfaces must not contain duplicates",
                ));
            }
        }
        if local.peer_addresses.len() > 512 {
            return Err(ConfigError::Invalid(
                "local.peer_addresses must contain at most 512 IP addresses",
            ));
        }
        let mut peer_addresses = std::collections::BTreeSet::new();
        for address in &local.peer_addresses {
            if address.is_unspecified() || address.is_multicast() || !peer_addresses.insert(address)
            {
                return Err(ConfigError::Invalid(
                    "local.peer_addresses must contain unique unicast IP addresses",
                ));
            }
        }
        if local.listen_port == 0 {
            return Err(ConfigError::Invalid(
                "local.listen_port must be greater than zero",
            ));
        }
        if local.reconcile_interval_seconds == 0 {
            return Err(ConfigError::Invalid(
                "local.reconcile_interval_seconds must be greater than zero",
            ));
        }
        if local.reconnect_max_seconds < local.reconnect_min_seconds
            || local.reconnect_min_seconds == 0
        {
            return Err(ConfigError::Invalid(
                "local reconnect bounds must be nonzero and ordered",
            ));
        }
        if local.mesh_key_file.as_os_str().is_empty() {
            return Err(ConfigError::Invalid(
                "local.mesh_key_file must not be empty",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalConfig {
    pub mesh_key_file: PathBuf,
    pub listen_port: u16,
    /// How often the configured interfaces' addresses are re-read.
    pub discovery_interval_seconds: u64,
    pub reconcile_interval_seconds: u64,
    pub reconnect_min_seconds: u64,
    pub reconnect_max_seconds: u64,
    /// Linux interfaces mesh listeners bind to. An empty list disables
    /// incoming connections and dialling.
    pub peer_interfaces: Vec<String>,
    /// The other hosts' addresses; every connection still authenticates.
    pub peer_addresses: Vec<IpAddr>,
    /// Copies up to this size replicate to every host. Larger copies stay on
    /// this host and are fetched by a peer only when it pastes them.
    pub inline_limit_bytes: u64,
    /// The largest non-file copy this host keeps at all. Copied files are
    /// never read at copy time, so they have no size limit.
    pub max_capture_bytes: u64,
    /// Once inline history exceeds this, its oldest unpinned items are
    /// deleted on every host.
    pub history_quota_bytes: u64,
    /// Space for copies fetched from other hosts; the oldest are removed
    /// first, since the originals remain on their hosts.
    pub fetch_cache_bytes: u64,
    /// Set to `false` on headless hosts. The daemon then never connects to
    /// Wayland and only stores and relays history for the other devices.
    pub clipboard: bool,
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
            peer_interfaces: Vec::new(),
            peer_addresses: Vec::new(),
            inline_limit_bytes: 5 * MIB,
            max_capture_bytes: 512 * MIB,
            history_quota_bytes: 1024 * MIB,
            fetch_cache_bytes: 10 * 1024 * MIB,
            clipboard: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    pub config: PathBuf,
    pub state_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub socket: PathBuf,
}

impl AppPaths {
    /// Resolves configuration, state, cache, runtime, and IPC paths from XDG
    /// variables.
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
        let cache_root =
            xdg_path("XDG_CACHE_HOME")?.unwrap_or_else(|| base.home_dir().join(".cache"));
        let runtime_root = xdg_path("XDG_RUNTIME_DIR")?.ok_or(ConfigError::MissingRuntime)?;
        let runtime_dir = runtime_root.join("clip-sync");
        let socket = runtime_dir.join("daemon.sock");

        Ok(Self {
            config,
            state_dir: state_root.join("clip-sync"),
            runtime_dir,
            cache_dir: cache_root.join("clip-sync"),
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
    #[error("could not read the config: {0}")]
    Read(std::io::Error),
    #[error("config exceeds the 1 MiB size limit")]
    TooLarge,
    #[error("invalid TOML: {0}")]
    TomlDecode(#[from] toml::de::Error),
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
        let local = Config::default().local;
        assert_eq!(local.inline_limit_bytes, 5 * MIB);
        assert_eq!(local.history_quota_bytes, 1024 * MIB);
        assert_eq!(local.fetch_cache_bytes, 10 * 1024 * MIB);
        assert_eq!(local.listen_port, 24_892);
        assert!(local.clipboard);
    }

    #[test]
    fn a_0_3_config_still_loads() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        fs::write(
            &path,
            r#"
            [shared]
            mesh_quota_bytes = 1073741824
            capture_threshold_bytes = 20971520
            revision = "5f69"

            [local]
            listen_port = 24892
            peer_interfaces = ["wt0"]
            maximum_explicit_share_bytes = 4294967296
            max_concurrent_chunk_streams = 4
            "#,
        )
        .unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.local.peer_interfaces, ["wt0"]);
        assert_eq!(config.local.inline_limit_bytes, 5 * MIB);
    }

    #[test]
    fn rejects_zero_quota_and_inverted_capture_limits() {
        let mut config = Config::default();
        config.local.history_quota_bytes = 0;
        assert!(matches!(config.validate(), Err(ConfigError::Invalid(_))));

        let mut config = Config::default();
        config.local.max_capture_bytes = config.local.inline_limit_bytes - 1;
        assert!(matches!(config.validate(), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn rejects_oversized_config_before_parsing() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        fs::write(&path, vec![b'#'; 1024 * 1024 + 1]).unwrap();
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

    #[test]
    fn peer_interfaces_are_bounded_unique_linux_names() {
        let mut config = Config::default();
        config.local.peer_interfaces = vec!["wt0".to_owned(), "tun0".to_owned()];
        assert!(config.validate().is_ok());

        config.local.peer_interfaces.push("wt0".to_owned());
        assert!(matches!(config.validate(), Err(ConfigError::Invalid(_))));

        config.local.peer_interfaces = vec!["interface-name-is-too-long".to_owned()];
        assert!(matches!(config.validate(), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn peer_addresses_are_bounded_unique_unicast_addresses() {
        let mut config = Config::default();
        config.local.peer_addresses = vec![
            "100.91.0.2".parse().expect("IP"),
            "100.91.126.8".parse().expect("IP"),
        ];
        assert!(config.validate().is_ok());

        config
            .local
            .peer_addresses
            .push("100.91.0.2".parse().expect("IP"));
        assert!(matches!(config.validate(), Err(ConfigError::Invalid(_))));

        config.local.peer_addresses = vec!["239.255.67.83".parse().expect("IP")];
        assert!(matches!(config.validate(), Err(ConfigError::Invalid(_))));
    }
}
