use std::{fs, time::Duration};

use anyhow::Context;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{
    config::{AppPaths, Config},
    discovery::{NetbirdDiscovery, PeerDiscovery},
    ipc::{self, DaemonState},
};

/// Runs discovery and local IPC until a termination signal is received.
///
/// # Errors
///
/// Returns an error when runtime setup, IPC serving, or signal handling fails.
pub async fn run(paths: AppPaths, config: Config) -> anyhow::Result<()> {
    fs::create_dir_all(&paths.state_dir).context("create state directory")?;
    fs::create_dir_all(&paths.runtime_dir).context("create runtime directory")?;

    let hostname = hostname::get()
        .context("read system hostname")?
        .to_string_lossy()
        .into_owned();
    let state = DaemonState::new(hostname, paths.config.clone(), config.clone());
    let shutdown = CancellationToken::new();
    let discovery = spawn_discovery(config, state.clone(), shutdown.clone());

    tracing::info!(socket = %paths.socket.display(), "clip-sync daemon started");
    let server = ipc::serve(&paths.socket, state, shutdown.clone());
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => result.context("serve local IPC")?,
        result = shutdown_signal() => {
            result.context("listen for shutdown signal")?;
            shutdown.cancel();
            server.await.context("stop local IPC")?;
        }
    }

    shutdown.cancel();
    finish_task(discovery).await;
    tracing::info!("clip-sync daemon stopped");
    Ok(())
}

fn spawn_discovery(
    config: Config,
    state: DaemonState,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let discovery = NetbirdDiscovery::new(config.local.netbird_command);
        let interval = Duration::from_secs(config.local.discovery_interval_seconds);

        loop {
            match discovery.discover().await {
                Ok(snapshot) => {
                    tracing::debug!(
                        peer_count = snapshot.peers.len(),
                        "NetBird discovery updated"
                    );
                    state.set_discovery(snapshot).await;
                }
                Err(error) => tracing::warn!(%error, "NetBird discovery is unavailable"),
            }

            tokio::select! {
                () = shutdown.cancelled() => break,
                () = tokio::time::sleep(interval) => {}
            }
        }
    })
}

async fn finish_task(task: JoinHandle<()>) {
    if let Err(error) = task.await {
        tracing::warn!(%error, "background task did not stop cleanly");
    }
}

async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }

    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}
