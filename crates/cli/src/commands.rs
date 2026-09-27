use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

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
    /// Launch the clipboard picker window.
    Desktop(DesktopArgs),
    /// Run the background daemon in the foreground.
    Daemon,
    /// Query the running daemon.
    Status(OutputArgs),
    /// List peers with live authenticated mesh connections.
    Peers(OutputArgs),
    /// Search and manage retained clipboard history.
    History {
        #[command(subcommand)]
        command: HistoryCommand,
    },
    /// Report live daemon, storage, clipboard, and discovery diagnostics.
    Doctor(OutputArgs),
    /// Manage remembered mesh devices.
    Device {
        #[command(subcommand)]
        command: DeviceCommand,
    },
}

#[derive(Debug, Clone, Copy, Args)]
pub(super) struct DesktopArgs {
    /// Open the control centre instead of the clipboard picker.
    #[arg(long)]
    pub(super) control: bool,
}

#[derive(Debug, Clone, Copy, Args)]
pub(super) struct OutputArgs {
    /// Emit stable machine-readable JSON.
    #[arg(long)]
    pub(super) json: bool,
}

#[derive(Debug, Subcommand)]
pub(super) enum HistoryCommand {
    /// List newest history entries, optionally matching a query.
    List {
        /// Words that must all appear in an item's preview, type, or device.
        #[arg(value_name = "QUERY")]
        query: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: u32,
        #[arg(long)]
        json: bool,
    },
    /// Search retained history.
    Search {
        /// Words that must all appear in an item's preview, type, or device.
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

#[derive(Debug, Subcommand)]
pub(super) enum DeviceCommand {
    /// Replicate rejection of a remembered mesh identity.
    Forget {
        device_id: String,
        #[arg(long)]
        json: bool,
    },
}
