mod commands;
mod devices;
mod diagnostics;
mod history;
mod support;
mod views;

#[cfg(test)]
mod tests;

use std::path::Path;

use anyhow::bail;
use clap::Parser;
use clip_sync_core::config::AppPaths;

use commands::{Cli, Command};
use devices::{device_command, peers};
use diagnostics::{doctor, status};
use history::history_command;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchKind {
    Desktop,
    Daemon,
    Client,
}

pub struct ParsedInvocation {
    cli: Cli,
}

#[must_use]
pub fn parse() -> ParsedInvocation {
    ParsedInvocation { cli: Cli::parse() }
}

impl ParsedInvocation {
    #[must_use]
    pub fn kind(&self) -> LaunchKind {
        match self.cli.command.as_ref() {
            None | Some(Command::Desktop(_)) => LaunchKind::Desktop,
            Some(Command::Daemon) => LaunchKind::Daemon,
            Some(_) => LaunchKind::Client,
        }
    }

    /// Whether the window should open on the control centre rather than the
    /// picker.
    #[must_use]
    pub fn desktop_control(&self) -> bool {
        matches!(
            self.cli.command.as_ref(),
            Some(Command::Desktop(commands::DesktopArgs { control: true }))
        )
    }

    #[must_use]
    pub fn config_override(&self) -> Option<&Path> {
        self.cli.config.as_deref()
    }

    /// Executes an invocation routed to client mode.
    ///
    /// # Errors
    ///
    /// Returns an error when the invocation is not a client command or when
    /// configuration, local IPC, or an offline maintenance operation fails.
    pub async fn run_client(self) -> anyhow::Result<()> {
        if self.kind() != LaunchKind::Client {
            bail!("invocation is not a client command");
        }

        let paths = AppPaths::discover(self.cli.config)?;
        match self.cli.command {
            Some(Command::Status(output)) => status(&paths, output).await,
            Some(Command::Peers(output)) => peers(&paths, output).await,
            Some(Command::History { command }) => history_command(&paths, command).await,
            Some(Command::Doctor(output)) => doctor(&paths, output).await,
            Some(Command::Device { command }) => device_command(&paths, command).await,
            None | Some(Command::Desktop(_) | Command::Daemon) => {
                bail!("invocation is not a client command")
            }
        }
    }
}
