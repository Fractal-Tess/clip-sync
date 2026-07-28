use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};

use crate::{
    config::{AppPaths, Config},
    daemon,
    ipc::{
        self,
        protocol::{
            ActivateRequest, ConfigRequest, DiagnosticsRequest, ForgetDeviceRequest,
            HistoryRequest, HistoryUpdateAction, HistoryUpdateRequest, IPC_PROTOCOL_VERSION,
            PeersRequest, Request, ShareClipboardRequest, StatusRequest, TransferCancelRequest,
            TransfersRequest, request, response,
        },
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
    /// List peers currently visible through `NetBird` discovery.
    Peers(OutputArgs),
    /// Inspect or initialize configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Search and manage retained clipboard history.
    History {
        #[command(subcommand)]
        command: HistoryCommand,
    },
    /// Report live daemon, storage, clipboard, and discovery diagnostics.
    Doctor(OutputArgs),
    /// Request explicit capture; the current Wayland backend reports this as unsupported.
    ShareClipboard(OutputArgs),
    /// Query transfer controls; daemon wiring is not available yet.
    Transfer {
        #[command(subcommand)]
        command: TransferCommand,
    },
    /// Manage remembered mesh devices.
    Device {
        #[command(subcommand)]
        command: DeviceCommand,
    },
    /// Open the optional egui interface.
    #[cfg(feature = "ui")]
    Ui {
        #[command(subcommand)]
        command: UiCommand,
    },
}

#[derive(Debug, Clone, Copy, Args)]
struct OutputArgs {
    /// Emit stable machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[cfg(feature = "ui")]
#[derive(Debug, Subcommand)]
enum UiCommand {
    /// Open the compact keyboard-first history switcher.
    Switcher,
    /// Open the full control center.
    Control,
}

#[derive(Debug, Subcommand)]
enum HistoryCommand {
    /// List newest history entries, optionally matching a query.
    List {
        query: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: u32,
        #[arg(long)]
        json: bool,
    },
    /// Search retained history.
    Search {
        query: String,
        #[arg(long, default_value_t = 100)]
        limit: u32,
        #[arg(long)]
        json: bool,
    },
    /// Set one retained item as the active clipboard and move it to the top.
    Activate(MutationArgs),
    /// Replicate a pin for one retained item.
    Pin(MutationArgs),
    /// Replicate removal of a pin from one retained item.
    Unpin(MutationArgs),
    /// Replicate deletion of one retained item.
    Delete(MutationArgs),
}

#[derive(Debug, Args)]
struct MutationArgs {
    content_id: String,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Show the daemon's effective configuration with secret paths redacted.
    Show(OutputArgs),
    /// Write a documented default config file.
    Init {
        /// Replace an existing config file.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Subcommand)]
enum TransferCommand {
    /// Request transfer state from the daemon.
    List(OutputArgs),
    /// Request cancellation of a transfer.
    Cancel {
        transfer_id: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum DeviceCommand {
    /// Request rejection of a device identity; the current replica backend is unsupported.
    Forget {
        device_id: String,
        #[arg(long)]
        json: bool,
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

#[derive(Debug, Deserialize, Serialize)]
struct SafeConfig {
    shared: crate::config::SharedConfig,
    local: SafeLocal,
}

#[derive(Debug, Deserialize, Serialize)]
struct SafeLocal {
    listen_port: u16,
    discovery_interval_seconds: u64,
    reconcile_interval_seconds: u64,
    reconnect_min_seconds: u64,
    reconnect_max_seconds: u64,
    netbird_command: String,
    mesh_key_file_configured: bool,
    config_path: String,
}

impl Cli {
    /// Executes the selected command.
    ///
    /// # Errors
    ///
    /// Returns an error when configuration, local IPC, or daemon startup fails.
    pub async fn run(self) -> anyhow::Result<()> {
        let paths = AppPaths::discover(self.config)?;
        match self.command {
            Command::Daemon => {
                let config = Config::load(&paths.config)?;
                daemon::run(paths, config).await
            }
            Command::Status(output) => status(&paths, output).await,
            Command::Peers(output) => peers(&paths, output).await,
            Command::Config { command } => config_command(&paths, command).await,
            Command::History { command } => history_command(&paths, command).await,
            Command::Doctor(output) => doctor(&paths, output).await,
            Command::ShareClipboard(output) => share_clipboard(&paths, output).await,
            Command::Transfer { command } => transfer_command(&paths, command).await,
            Command::Device { command } => device_command(&paths, command).await,
            #[cfg(feature = "ui")]
            Command::Ui { command } => {
                let mode = match command {
                    UiCommand::Switcher => crate::ui::UiMode::Switcher,
                    UiCommand::Control => crate::ui::UiMode::Control,
                };
                crate::ui::run(mode, paths).map_err(anyhow::Error::msg)
            }
        }
    }
}

async fn status(paths: &AppPaths, output: OutputArgs) -> anyhow::Result<()> {
    let response = daemon_request(
        paths,
        1,
        request::Body::Status(StatusRequest {}),
        output.json,
    )
    .await?;
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
                print_json(&value)?;
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
        Some(response::Body::Error(error)) => Err(daemon_response_error(&error, output.json)),
        _ => Err(unexpected_response(output.json, "status")),
    }
}

async fn peers(paths: &AppPaths, output: OutputArgs) -> anyhow::Result<()> {
    let response =
        daemon_request(paths, 2, request::Body::Peers(PeersRequest {}), output.json).await?;
    match response.body {
        Some(response::Body::Peers(peers)) if output.json => {
            let peer_items = peers
                .peers
                .into_iter()
                .map(|peer| {
                    serde_json::json!({
                        "hostname": peer.hostname,
                        "address": peer.address,
                        "connected": peer.connected,
                    })
                })
                .collect::<Vec<_>>();
            print_json(&serde_json::json!({
                "local_hostname": peers.local_hostname,
                "local_address": peers.local_address,
                "peers": peer_items,
                "discovery_error": peers.discovery_error,
            }))
        }
        Some(response::Body::Peers(peers)) => {
            println!(
                "local: {} ({})",
                peers.local_hostname,
                peers
                    .local_address
                    .as_deref()
                    .unwrap_or("address unavailable")
            );
            if let Some(error) = peers.discovery_error {
                println!("discovery: unavailable ({error})");
            }
            if peers.peers.is_empty() {
                println!("no peers discovered");
            }
            for peer in peers.peers {
                let status = if peer.connected {
                    "connected"
                } else {
                    "offline"
                };
                println!("{}  {}  {status}", peer.hostname, peer.address);
            }
            Ok(())
        }
        Some(response::Body::Error(error)) => Err(daemon_response_error(&error, output.json)),
        _ => Err(unexpected_response(output.json, "peers")),
    }
}

async fn history_command(paths: &AppPaths, command: HistoryCommand) -> anyhow::Result<()> {
    match command {
        HistoryCommand::List { query, limit, json } => {
            history_query(paths, query.unwrap_or_default(), limit, json).await
        }
        HistoryCommand::Search { query, limit, json } => {
            history_query(paths, query, limit, json).await
        }
        HistoryCommand::Activate(args) => {
            let response = daemon_request(
                paths,
                4,
                request::Body::Activate(ActivateRequest {
                    content_id: args.content_id,
                }),
                args.json,
            )
            .await?;
            mutation_response(response, args.json, "clipboard activated")
        }
        HistoryCommand::Pin(args) => history_update(paths, args, HistoryUpdateAction::Pin).await,
        HistoryCommand::Unpin(args) => {
            history_update(paths, args, HistoryUpdateAction::Unpin).await
        }
        HistoryCommand::Delete(args) => {
            history_update(paths, args, HistoryUpdateAction::Delete).await
        }
    }
}

async fn history_query(
    paths: &AppPaths,
    query: String,
    limit: u32,
    json: bool,
) -> anyhow::Result<()> {
    let response = daemon_request(
        paths,
        3,
        request::Body::History(HistoryRequest { query, limit }),
        json,
    )
    .await?;
    match response.body {
        Some(response::Body::History(history)) if json => {
            let items = history
                .items
                .into_iter()
                .map(|item| {
                    serde_json::json!({
                        "content_id": item.content_id,
                        "preview": item.preview,
                        "mime_types": item.mime_types,
                        "logical_size": item.logical_size,
                        "source_node": item.source_node,
                        "pinned": item.pinned,
                        "physical_millis": item.physical_millis,
                    })
                })
                .collect::<Vec<_>>();
            print_json(&items)
        }
        Some(response::Body::History(history)) => {
            for item in history.items {
                let short_id = item.content_id.chars().take(12).collect::<String>();
                let pin = if item.pinned { "pin" } else { "   " };
                println!(
                    "{short_id}  {pin}  {:>8} B  {}",
                    item.logical_size, item.preview
                );
            }
            Ok(())
        }
        Some(response::Body::Error(error)) => Err(daemon_response_error(&error, json)),
        _ => Err(unexpected_response(json, "history query")),
    }
}

async fn history_update(
    paths: &AppPaths,
    args: MutationArgs,
    action: HistoryUpdateAction,
) -> anyhow::Result<()> {
    let response = daemon_request(
        paths,
        5,
        request::Body::HistoryUpdate(HistoryUpdateRequest {
            content_id: args.content_id,
            action: action as i32,
        }),
        args.json,
    )
    .await?;
    mutation_response(response, args.json, "history updated")
}

async fn config_command(paths: &AppPaths, command: ConfigCommand) -> anyhow::Result<()> {
    match command {
        ConfigCommand::Show(output) => {
            let response = daemon_request(
                paths,
                6,
                request::Body::Config(ConfigRequest {}),
                output.json,
            )
            .await?;
            match response.body {
                Some(response::Body::Config(config)) => {
                    let config: SafeConfig = serde_json::from_slice(&config.redacted_json)
                        .context("decode daemon config response")?;
                    if output.json {
                        print_json(&config)?;
                    } else {
                        println!("{}", toml::to_string_pretty(&config)?);
                    }
                    Ok(())
                }
                Some(response::Body::Error(error)) => {
                    Err(daemon_response_error(&error, output.json))
                }
                _ => Err(unexpected_response(output.json, "config show")),
            }
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
    let response = daemon_request(
        paths,
        7,
        request::Body::Diagnostics(DiagnosticsRequest {}),
        output.json,
    )
    .await?;
    match response.body {
        Some(response::Body::Diagnostics(diagnostics)) => {
            let ok = diagnostics.checks.iter().all(|check| check.ok);
            if output.json {
                let checks = diagnostics
                    .checks
                    .iter()
                    .map(|check| {
                        serde_json::json!({
                            "name": check.name,
                            "ok": check.ok,
                            "detail": check.detail,
                        })
                    })
                    .collect::<Vec<_>>();
                print_json(&serde_json::json!({ "ok": ok, "checks": checks }))?;
            } else {
                for check in diagnostics.checks {
                    println!(
                        "{}: {} ({})",
                        check.name,
                        if check.ok { "ok" } else { "failed" },
                        check.detail
                    );
                }
            }
            if ok {
                Ok(())
            } else {
                bail!("one or more daemon checks failed")
            }
        }
        Some(response::Body::Error(error)) => Err(daemon_response_error(&error, output.json)),
        _ => Err(unexpected_response(output.json, "diagnostics")),
    }
}

async fn share_clipboard(paths: &AppPaths, output: OutputArgs) -> anyhow::Result<()> {
    let response = daemon_request(
        paths,
        8,
        request::Body::ShareClipboard(ShareClipboardRequest {}),
        output.json,
    )
    .await?;
    mutation_response(response, output.json, "clipboard shared")
}

async fn transfer_command(paths: &AppPaths, command: TransferCommand) -> anyhow::Result<()> {
    match command {
        TransferCommand::List(output) => {
            let response = daemon_request(
                paths,
                9,
                request::Body::Transfers(TransfersRequest {}),
                output.json,
            )
            .await?;
            match response.body {
                Some(response::Body::Transfers(transfers)) if output.json => {
                    let transfers = transfers
                        .transfers
                        .into_iter()
                        .map(|transfer| {
                            serde_json::json!({
                                "transfer_id": transfer.transfer_id,
                                "content_id": transfer.content_id,
                                "peer": transfer.peer,
                                "state": transfer.state,
                                "completed_bytes": transfer.completed_bytes,
                                "total_bytes": transfer.total_bytes,
                            })
                        })
                        .collect::<Vec<_>>();
                    print_json(&transfers)
                }
                Some(response::Body::Transfers(transfers)) => {
                    if transfers.transfers.is_empty() {
                        println!("no transfers");
                    }
                    for transfer in transfers.transfers {
                        println!(
                            "{}  {}  {}/{} B  {}",
                            transfer.transfer_id,
                            transfer.state,
                            transfer.completed_bytes,
                            transfer.total_bytes,
                            transfer.peer
                        );
                    }
                    Ok(())
                }
                Some(response::Body::Error(error)) => {
                    Err(daemon_response_error(&error, output.json))
                }
                _ => Err(unexpected_response(output.json, "transfers")),
            }
        }
        TransferCommand::Cancel { transfer_id, json } => {
            let response = daemon_request(
                paths,
                10,
                request::Body::TransferCancel(TransferCancelRequest { transfer_id }),
                json,
            )
            .await?;
            mutation_response(response, json, "transfer cancelled")
        }
    }
}

async fn device_command(paths: &AppPaths, command: DeviceCommand) -> anyhow::Result<()> {
    match command {
        DeviceCommand::Forget { device_id, json } => {
            let response = daemon_request(
                paths,
                11,
                request::Body::ForgetDevice(ForgetDeviceRequest { device_id }),
                json,
            )
            .await?;
            mutation_response(response, json, "device forgotten")
        }
    }
}

async fn daemon_request(
    paths: &AppPaths,
    request_id: u64,
    body: request::Body,
    json: bool,
) -> anyhow::Result<crate::ipc::protocol::Response> {
    let result = ipc::request(
        &paths.socket,
        Request {
            protocol_version: IPC_PROTOCOL_VERSION,
            request_id,
            body: Some(body),
        },
    )
    .await;
    match result {
        Ok(response) => Ok(response),
        Err(error) => {
            let message = format!(
                "clip-sync daemon is unavailable at {}; start clip-sync.service or run `clip-sync daemon`",
                paths.socket.display()
            );
            if json {
                print_json(&error_json("daemon_unavailable", &message))?;
            }
            Err(anyhow::Error::new(error).context(message))
        }
    }
}

fn mutation_response(
    response: crate::ipc::protocol::Response,
    json: bool,
    fallback_message: &str,
) -> anyhow::Result<()> {
    match response.body {
        Some(response::Body::Mutation(result)) if result.ok => {
            if json {
                print_json(&serde_json::json!({
                    "ok": true,
                    "message": result.message,
                    "resource_id": result.resource_id,
                }))?;
            } else if result.message.is_empty() {
                println!("{fallback_message}");
            } else {
                println!("{}", result.message);
            }
            Ok(())
        }
        Some(response::Body::Mutation(_)) => Err(operation_error(
            json,
            "mutation_failed",
            "daemon reported an unsuccessful mutation",
        )),
        Some(response::Body::Error(error)) => Err(daemon_response_error(&error, json)),
        _ => Err(unexpected_response(json, "mutation")),
    }
}

fn daemon_response_error(error: &crate::ipc::protocol::ErrorResponse, json: bool) -> anyhow::Error {
    operation_error(json, &error.code, &error.message)
}

fn unexpected_response(json: bool, operation: &str) -> anyhow::Error {
    operation_error(
        json,
        "protocol_error",
        &format!("daemon returned an unexpected response to {operation}"),
    )
}

fn operation_error(json: bool, code: &str, message: &str) -> anyhow::Error {
    if json {
        let value = error_json(code, message);
        if let Err(serialization_error) = print_json(&value) {
            return serialization_error;
        }
    }
    anyhow::anyhow!("{code}: {message}")
}

fn error_json(code: &str, message: &str) -> serde_json::Value {
    serde_json::json!({
        "ok": false,
        "error": {
            "code": code,
            "message": message,
        }
    })
}

fn print_json(value: &impl Serialize) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_exposes_history_search_and_mutations() {
        for arguments in [
            vec!["clip-sync", "history", "search", "needle", "--json"],
            vec!["clip-sync", "history", "pin", "content", "--json"],
            vec!["clip-sync", "history", "unpin", "content"],
            vec!["clip-sync", "history", "delete", "content"],
            vec!["clip-sync", "transfer", "cancel", "transfer-id", "--json"],
            vec!["clip-sync", "device", "forget", "device-id", "--json"],
        ] {
            Cli::try_parse_from(arguments).expect("command should parse");
        }
    }

    #[test]
    fn json_error_shape_is_stable() {
        assert_eq!(
            error_json("daemon_unavailable", "not running"),
            serde_json::json!({
                "ok": false,
                "error": {
                    "code": "daemon_unavailable",
                    "message": "not running",
                }
            })
        );
    }

    #[test]
    fn status_json_fields_are_stable() {
        let address = Some("100.64.0.1".to_owned());
        let output = StatusOutput {
            version: "0.1.0",
            hostname: "kiwi",
            uptime_seconds: 42,
            config_path: "/config.toml",
            netbird_address: &address,
            discovered_peers: 2,
        };

        assert_eq!(
            serde_json::to_value(output).expect("serialize status"),
            serde_json::json!({
                "version": "0.1.0",
                "hostname": "kiwi",
                "uptime_seconds": 42,
                "config_path": "/config.toml",
                "netbird_address": "100.64.0.1",
                "discovered_peers": 2,
            })
        );
    }
}
