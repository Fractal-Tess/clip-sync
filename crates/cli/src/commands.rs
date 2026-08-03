use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "clip-sync", version, about)]
pub(super) struct Cli {
    /// Override the XDG config path.
    #[arg(long, global = true, value_name = "PATH")]
    pub(super) config: Option<PathBuf>,
    #[command(subcommand)]
    pub(super) command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub(super) enum Command {
    /// Launch the desktop control window.
    Desktop,
    /// Run the background daemon in the foreground.
    Daemon,
    /// Query the running daemon.
    Status(OutputArgs),
    /// List peers with live authenticated mesh connections.
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
    /// Inspect and explicitly share the current clipboard.
    ShareClipboard(ShareArgs),
    /// Query and cancel payload transfers.
    Transfer {
        #[command(subcommand)]
        command: TransferCommand,
    },
    /// Manage remembered mesh devices.
    Device {
        #[command(subcommand)]
        command: DeviceCommand,
    },
    /// Rotate the mesh secret wrapping local encrypted-store data keys.
    Rekey(RekeyArgs),
}

#[derive(Debug, Clone, Copy, Args)]
pub(super) struct OutputArgs {
    /// Emit stable machine-readable JSON.
    #[arg(long)]
    pub(super) json: bool,
}

#[derive(Debug, Clone, Copy, Args)]
pub(super) struct ShareArgs {
    /// Confirm sharing when the inspected offer exceeds the capture threshold.
    #[arg(long)]
    pub(super) confirm: bool,
    /// Emit stable machine-readable JSON.
    #[arg(long)]
    pub(super) json: bool,
}

#[derive(Debug, Subcommand)]
pub(super) enum HistoryCommand {
    /// List newest history entries, optionally matching a query.
    List {
        /// Free text and comma/space-separated filters: d:, t:, p:, before:, min-size:, max-size:.
        #[arg(value_name = "QUERY")]
        query: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: u32,
        #[arg(long)]
        json: bool,
    },
    /// Search retained history.
    Search {
        /// Free text and filters; quote the complete query when it contains spaces.
        #[arg(value_name = "QUERY")]
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
pub(super) struct MutationArgs {
    pub(super) content_id: String,
    #[arg(long)]
    pub(super) json: bool,
}

#[derive(Debug, Args)]
pub(super) struct RekeyArgs {
    /// Owner-only file containing the currently active mesh secret.
    #[arg(long, value_name = "PATH")]
    pub(super) old_key_file: PathBuf,
    /// Owner-only file containing the replacement mesh secret.
    #[arg(long, value_name = "PATH")]
    pub(super) new_key_file: PathBuf,
}

#[derive(Debug, Subcommand)]
pub(super) enum ConfigCommand {
    /// Show the daemon's effective configuration with secret paths redacted.
    Show(OutputArgs),
    /// Write a documented default config file.
    Init {
        /// Replace an existing config file.
        #[arg(long)]
        force: bool,
    },
    /// Replicate and apply one shared mesh setting.
    Set {
        #[arg(value_enum)]
        setting: ConfigSetting,
        value: u64,
        #[arg(long)]
        json: bool,
    },
    /// Select Linux interfaces for discovery and mesh connections; no values disables networking.
    SetPeerInterfaces {
        #[arg(value_name = "INTERFACE", num_args = 0..)]
        interfaces: Vec<String>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(super) enum ConfigSetting {
    MeshQuota,
    CaptureThreshold,
}

#[derive(Debug, Subcommand)]
pub(super) enum TransferCommand {
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
pub(super) enum DeviceCommand {
    /// Replicate rejection of a remembered mesh identity.
    Forget {
        device_id: String,
        #[arg(long)]
        json: bool,
    },
}
