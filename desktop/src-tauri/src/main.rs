// Prevents an additional console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::Path;

use clip_sync_cli::{LaunchKind, parse};
use clip_sync_core::config::{AppPaths, Config};
use tracing_subscriber::EnvFilter;

fn main() -> anyhow::Result<()> {
    let invocation = parse();
    let kind = invocation.kind();
    init_tracing();

    match kind {
        LaunchKind::Desktop => {
            let config_override = invocation.config_override().map(Path::to_path_buf);
            clip_sync_app::run(config_override);
            Ok(())
        }
        LaunchKind::Daemon => {
            let paths = AppPaths::discover(invocation.config_override().map(Path::to_path_buf))?;
            let config = Config::load(&paths.config)?;
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(clip_sync_daemon::run(paths, config))
        }
        LaunchKind::Client => tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(invocation.run_client()),
    }
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("clip_sync=info")),
        )
        .with_target(false)
        .compact()
        .init();
}
