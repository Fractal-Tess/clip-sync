use std::{fs, path::PathBuf, time::Duration};

use anyhow::{Context, bail};
use clap::{Args, Parser, Subcommand};
use serde::Serialize;

use crate::{
    config::{AppPaths, Config},
    daemon,
    discovery::{NetbirdDiscovery, PeerDiscovery},
    ipc::{
        self,
        protocol::{IPC_PROTOCOL_VERSION, Request, StatusRequest, request, response},
    },
};

#[derive(Debug, Parser)]
#[command(version, about)]
pub struct Cli {
    /// Override the XDG config path.
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the background daemon in the foreground.
    Daemon,
    /// Query the running daemon.
    Status(OutputArgs),
    /// Inspect or initialize configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Check local configuration, secret provisioning, and `NetBird` discovery.
    Doctor(OutputArgs),
}

#[derive(Debug, Clone, Args)]
struct OutputArgs {
    /// Emit stable machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Show the effective local configuration with the secret path redacted.
    Show(OutputArgs),
    /// Write a documented default config file.
    Init {
        /// Replace an existing config file.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Serialize)]
struct StatusOutput<'a> {
    version: &'a str,
    hostname: &'a str,
    uptime_seconds: u64,
    config_path: &'a str,
    netbird_address: &'a Option<String>,
    discovered_peers: u32,
}

#[derive(Serialize)]
struct SafeLocal<'a> {
    listen_port: u16,
    discovery_interval_seconds: u64,
    netbird_command: String,
    mesh_key_file_configured: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    config_path: Option<&'a str>,
}

#[derive(Serialize)]
struct SafeConfig<'a> {
    shared: &'a crate::config::SharedConfig,
    local: SafeLocal<'a>,
}

#[derive(Serialize)]
struct Check {
    ok: bool,
    detail: String,
}

#[derive(Serialize)]
struct DoctorOutput {
    config: Check,
    mesh_key_file: Check,
    netbird: Check,
}

impl Cli {
    /// Executes the selected command.
    ///
    /// # Errors
    ///
    /// Returns an error when configuration, local IPC, discovery, or daemon startup fails.
    pub async fn run(self) -> anyhow::Result<()> {
        let paths = AppPaths::discover(self.config)?;
        match self.command {
            Command::Daemon => {
                let config = Config::load(&paths.config)?;
                daemon::run(paths, config).await
            }
            Command::Status(output) => status(&paths, output).await,
            Command::Config { command } => config_command(&paths, command),
            Command::Doctor(output) => doctor(&paths, output).await,
        }
    }
}

async fn status(paths: &AppPaths, output: OutputArgs) -> anyhow::Result<()> {
    let response = ipc::request(
        &paths.socket,
        Request {
            protocol_version: IPC_PROTOCOL_VERSION,
            request_id: 1,
            body: Some(request::Body::Status(StatusRequest {})),
        },
    )
    .await
    .with_context(|| format!("connect to daemon at {}", paths.socket.display()))?;

    match response.body {
        Some(response::Body::Status(status)) => {
            let value = StatusOutput {
                version: &status.version,
                hostname: &status.hostname,
                uptime_seconds: status.uptime_seconds,
                config_path: &status.config_path,
                netbird_address: &status.netbird_address,
                discovered_peers: status.discovered_peers,
            };
            if output.json {
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                println!("clip-sync {} on {}", value.version, value.hostname);
                println!("uptime: {}s", value.uptime_seconds);
                println!("peers discovered: {}", value.discovered_peers);
                println!(
                    "NetBird address: {}",
                    value.netbird_address.as_deref().unwrap_or("unavailable")
                );
                println!("config: {}", value.config_path);
            }
            Ok(())
        }
        Some(response::Body::Error(error)) => bail!("{}: {}", error.code, error.message),
        _ => bail!("daemon returned an unexpected response"),
    }
}

fn config_command(paths: &AppPaths, command: ConfigCommand) -> anyhow::Result<()> {
    match command {
        ConfigCommand::Show(output) => {
            let config = Config::load(&paths.config)?;
            let display_path = paths.config.to_str();
            let safe = SafeConfig {
                shared: &config.shared,
                local: SafeLocal {
                    listen_port: config.local.listen_port,
                    discovery_interval_seconds: config.local.discovery_interval_seconds,
                    netbird_command: config.local.netbird_command.display().to_string(),
                    mesh_key_file_configured: !config.local.mesh_key_file.as_os_str().is_empty(),
                    config_path: display_path,
                },
            };
            if output.json {
                println!("{}", serde_json::to_string_pretty(&safe)?);
            } else {
                println!("{}", toml::to_string_pretty(&safe)?);
            }
            Ok(())
        }
        ConfigCommand::Init { force } => {
            if paths.config.exists() && !force {
                bail!(
                    "{} already exists; pass --force to replace it",
                    paths.config.display()
                );
            }
            Config::default().save(&paths.config)?;
            println!("wrote {}", paths.config.display());
            Ok(())
        }
    }
}

async fn doctor(paths: &AppPaths, output: OutputArgs) -> anyhow::Result<()> {
    let config = Config::load(&paths.config)?;
    let key_metadata = fs::metadata(&config.local.mesh_key_file);
    let discovery = NetbirdDiscovery::new(&config.local.netbird_command)
        .with_timeout(Duration::from_secs(5))
        .discover()
        .await;

    let report = DoctorOutput {
        config: Check {
            ok: true,
            detail: paths.config.display().to_string(),
        },
        mesh_key_file: match key_metadata {
            Ok(metadata) => Check {
                ok: metadata.is_file(),
                detail: if metadata.is_file() {
                    "provisioned".to_owned()
                } else {
                    "path is not a regular file".to_owned()
                },
            },
            Err(error) => Check {
                ok: false,
                detail: format!("unavailable ({})", error.kind()),
            },
        },
        netbird: match discovery {
            Ok(snapshot) => Check {
                ok: true,
                detail: format!("connected; {} peers visible", snapshot.peers.len()),
            },
            Err(error) => Check {
                ok: false,
                detail: error.to_string(),
            },
        },
    };

    if output.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "config: {} ({})",
            check_mark(report.config.ok),
            report.config.detail
        );
        println!(
            "mesh key: {} ({})",
            check_mark(report.mesh_key_file.ok),
            report.mesh_key_file.detail
        );
        println!(
            "NetBird: {} ({})",
            check_mark(report.netbird.ok),
            report.netbird.detail
        );
    }

    if report.config.ok && report.mesh_key_file.ok && report.netbird.ok {
        Ok(())
    } else {
        bail!("one or more checks failed")
    }
}

fn check_mark(ok: bool) -> &'static str {
    if ok { "ok" } else { "failed" }
}
