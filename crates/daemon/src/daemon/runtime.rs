use std::{collections::BTreeMap, fs, time::Duration};

use anyhow::Context;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use clip_sync_core::{
    clipboard::wayland::WaylandBackend,
    config::{AppPaths, Config},
    crypto::MeshSecret,
    state_keys::{StateKeys, StoreLock},
    storage::HistoryStore,
};

use crate::{
    discovery::InterfaceDiscovery,
    ipc::{self, DaemonState},
    mesh::{MeshHandle, MeshRuntime, MeshRuntimeConfig},
};

use super::{
    activation::{Activation, FetchFinished},
    capture::CaptureLimits,
    clipboard::{handle_clipboard_event, spawn_clipboard_watch},
    commands::handle_daemon_command,
    mesh_persistence::{MeshPersistenceContext, handle_mesh_store_request},
    views::{device_items, history_items},
};

pub(super) const CLIPBOARD_DISABLED_DETAIL: &str =
    "disabled by configuration; this host only stores and relays history";

/// Runs discovery and local IPC until a termination signal is received.
///
/// # Errors
///
/// Returns an error when runtime setup, IPC serving, or signal handling fails.
#[allow(clippy::too_many_lines)]
pub async fn run(paths: AppPaths, config: Config) -> anyhow::Result<()> {
    fs::create_dir_all(&paths.state_dir).context("create state directory")?;
    fs::create_dir_all(&paths.runtime_dir).context("create runtime directory")?;
    make_private_directory(&paths.state_dir).context("secure state directory")?;
    make_private_directory(&paths.runtime_dir).context("secure runtime directory")?;
    let _instance = ipc::DaemonInstance::acquire(&paths.runtime_dir)
        .context("acquire daemon singleton lock")?;

    let store_lock =
        StoreLock::acquire(&paths.state_dir).context("acquire exclusive daemon/store lock")?;
    let mesh_secret = MeshSecret::load(&config.local.mesh_key_file)
        .context("load mesh secret from configured file")?;
    let state_keys =
        StateKeys::open_or_create(&store_lock, &mesh_secret).context("open local state key")?;
    let storage_path = paths.state_dir.join("history.db");
    let mut history = HistoryStore::open(&storage_path, state_keys.storage_key())
        .with_context(|| format!("open encrypted history at {}", storage_path.display()))?;
    remove_retired_directories(&paths);

    let content_key = state_keys.content_identity_key();
    let transport_psk = mesh_secret
        .transport_psk()
        .context("derive mesh transport key")?;
    let limits = CaptureLimits {
        inline_limit_bytes: config.local.inline_limit_bytes,
        history_quota_bytes: config.local.history_quota_bytes,
    };

    let hostname = hostname::get()
        .context("read system hostname")?
        .to_string_lossy()
        .into_owned();
    let (command_tx, mut command_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = DaemonState::new(
        hostname.clone(),
        paths.config.clone(),
        config.clone(),
        command_tx,
    );
    state
        .set_device_names(BTreeMap::from([(
            history.replica().node_id().to_string(),
            hostname.clone(),
        )]))
        .await;
    state.set_history(history_items(history.replica())).await;
    state.set_devices(device_items(&history)).await;
    let shutdown = CancellationToken::new();
    let mut mesh_config = MeshRuntimeConfig::new(
        history.replica().node_id(),
        hostname.clone(),
        config.local.listen_port,
    );
    mesh_config.reconcile_interval = Duration::from_secs(config.local.reconcile_interval_seconds);
    mesh_config.reconnect_min = Duration::from_secs(config.local.reconnect_min_seconds);
    mesh_config.reconnect_max = Duration::from_secs(config.local.reconnect_max_seconds);
    mesh_config.initial_seen = history.projection().seen_ops().clone();
    let acknowledgements = history
        .acknowledgements()
        .context("load durable mesh membership")?;
    mesh_config.known_members = history
        .projection()
        .known_members()
        .chain(acknowledgements.known_members())
        .chain(std::iter::once(history.replica().node_id()))
        .collect();
    mesh_config.forgotten_devices = history.projection().forgotten_devices().collect();
    let (mesh, mut mesh_rx) = MeshRuntime::spawn(mesh_config, transport_psk, shutdown.clone())
        .context("start mesh runtime")?;
    let mesh_handle = mesh.handle();
    state.set_mesh(mesh_handle.clone()).await;
    let discovery = spawn_discovery(
        state.clone(),
        mesh_handle.clone(),
        hostname,
        shutdown.clone(),
    );

    let clipboard = WaylandBackend::new();
    clipboard
        .set_capture_threshold(config.local.max_capture_bytes)
        .context("apply clipboard capture limit")?;
    let (clipboard_tx, mut clipboard_rx) = tokio::sync::mpsc::channel(128);
    let mut clipboard_finished = !config.local.clipboard;
    let mut clipboard_watch = if config.local.clipboard {
        spawn_clipboard_watch(
            clipboard.clone(),
            state.clone(),
            clipboard_tx,
            shutdown.clone(),
        )
    } else {
        state
            .set_clipboard_status(true, CLIPBOARD_DISABLED_DETAIL)
            .await;
        tracing::info!("clipboard disabled by configuration; storing and relaying history only");
        tokio::spawn(async {})
    };
    let (fetch_tx, mut fetch_rx) = tokio::sync::mpsc::unbounded_channel::<FetchFinished>();

    tracing::info!(socket = %paths.socket.display(), "clip-sync daemon started");
    let server = ipc::serve(&paths.socket, state.clone(), shutdown.clone());
    let termination = shutdown_signal();
    tokio::pin!(server);
    tokio::pin!(termination);
    let mut server_finished = false;

    loop {
        tokio::select! {
            result = &mut server, if !server_finished => {
                server_finished = true;
                result.context("serve local IPC")?;
                break;
            }
            result = &mut clipboard_watch, if !clipboard_finished => {
                clipboard_finished = true;
                if let Err(error) = result {
                    state.set_clipboard_status(false, error.to_string()).await;
                    tracing::warn!(%error, "Wayland clipboard supervisor failed");
                } else if !shutdown.is_cancelled() {
                    state
                        .set_clipboard_status(false, "Wayland clipboard supervisor stopped")
                        .await;
                }
            }
            command = command_rx.recv() => {
                if let Some(command) = command {
                    let mut activation = Activation {
                        clipboard: &clipboard,
                        history: &mut history,
                        state: &state,
                        mesh: &mesh_handle,
                        cache_dir: &paths.cache_dir,
                        fetches: &fetch_tx,
                    };
                    handle_daemon_command(command, &mut activation, config.local.clipboard).await;
                }
            }
            finished = fetch_rx.recv() => {
                if let Some(finished) = finished {
                    Activation {
                        clipboard: &clipboard,
                        history: &mut history,
                        state: &state,
                        mesh: &mesh_handle,
                        cache_dir: &paths.cache_dir,
                        fetches: &fetch_tx,
                    }
                    .finish_fetch(finished)
                    .await;
                }
            }
            event = clipboard_rx.recv() => {
                if let Some(event) = event {
                    handle_clipboard_event(
                        event,
                        &mut history,
                        &state,
                        content_key,
                        limits,
                        &mesh_handle,
                    ).await;
                }
            }
            request = mesh_rx.recv() => {
                if let Some(request) = request {
                    let mut context = MeshPersistenceContext {
                        history: &mut history,
                        state: &state,
                        content_key,
                        mesh: &mesh_handle,
                    };
                    handle_mesh_store_request(request, &mut context).await;
                }
            }
            result = &mut termination => {
                result.context("listen for shutdown signal")?;
                break;
            }
        }
    }

    shutdown.cancel();
    drop(clipboard_rx);
    if !server_finished {
        server.await.context("stop local IPC")?;
    }
    if !clipboard_finished && let Err(error) = clipboard_watch.await {
        tracing::warn!(%error, "Wayland clipboard supervisor failed");
    }
    finish_task(discovery).await;
    mesh.wait().await;
    tracing::info!("clip-sync daemon stopped");
    Ok(())
}

/// 0.3 kept an encrypted chunk store and materialized transfers; neither is
/// used any more.
fn remove_retired_directories(paths: &AppPaths) {
    for directory in [
        paths.state_dir.join("chunks"),
        paths.runtime_dir.join("materialized"),
    ] {
        if directory.exists()
            && let Err(error) = fs::remove_dir_all(&directory)
        {
            tracing::warn!(%error, path = %directory.display(), "could not remove a retired directory");
        }
    }
}

pub(super) fn unix_time_millis() -> anyhow::Result<u64> {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_millis();
    u64::try_from(millis).context("system clock milliseconds exceed u64")
}

/// Re-reads interface addresses on the configured interval, so a VPN that
/// reconnects with a new address is picked up without restarting.
fn spawn_discovery(
    state: DaemonState,
    mesh: MeshHandle,
    hostname: String,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let config = state.config().await;
            let interval = Duration::from_secs(config.local.discovery_interval_seconds);
            let discovery = InterfaceDiscovery::new(
                config.local.peer_interfaces,
                config.local.peer_addresses,
                hostname.clone(),
                config.local.listen_port,
            );
            match discovery.discover().await {
                Ok(snapshot) => {
                    mesh.update_discovery(snapshot.clone());
                    state.set_discovery(snapshot).await;
                }
                Err(error) => {
                    mesh.clear_discovery();
                    state.set_discovery_error(error.to_string()).await;
                    tracing::warn!(%error, "peer interfaces are unavailable");
                }
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
#[cfg(unix)]
fn make_private_directory(path: &std::path::Path) -> anyhow::Result<()> {
    use rustix::fs::{FileType, Mode, OFlags, fchmod, fstat, open};

    let fd = open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )?;
    let stat = fstat(&fd)?;
    anyhow::ensure!(
        FileType::from_raw_mode(stat.st_mode).is_dir(),
        "{} is not a directory",
        path.display()
    );
    anyhow::ensure!(
        stat.st_uid == rustix::process::getuid().as_raw(),
        "{} is not owned by the current user",
        path.display()
    );
    fchmod(&fd, Mode::RUSR | Mode::WUSR | Mode::XUSR)?;
    Ok(())
}

#[cfg(not(unix))]
fn make_private_directory(_path: &std::path::Path) -> anyhow::Result<()> {
    Ok(())
}
