mod commands;
mod devices;
mod diagnostics;
mod history;
mod rekey;
mod settings;
mod share;
mod support;
mod transfer;
mod views;

#[cfg(test)]
mod tests;

use crate::{
    config::{AppPaths, Config},
    daemon,
};

pub use commands::Cli;
use commands::Command;
#[cfg(feature = "ui")]
use commands::UiCommand;
use devices::{device_command, peers};
use diagnostics::{doctor, status};
use history::history_command;
use rekey::rekey_command;
use settings::config_command;
use share::share_clipboard;
use transfer::transfer_command;

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
            Command::Rekey(args) => rekey_command(&paths, &args),
            #[cfg(feature = "ui")]
            Command::Ui { command } => match command {
                UiCommand::Switcher => {
                    crate::ui::run(crate::ui::UiMode::Switcher, paths).map_err(anyhow::Error::msg)
                }
                UiCommand::Control => {
                    crate::ui::run(crate::ui::UiMode::Control, paths).map_err(anyhow::Error::msg)
                }
                UiCommand::CloseQuick => crate::ui::close_quick(&paths).map_err(anyhow::Error::msg),
                UiCommand::Tray => crate::tray::run(paths).await.map_err(anyhow::Error::msg),
            },
        }
    }
}
