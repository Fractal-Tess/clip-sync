use std::{collections::BTreeMap, fs, time::Duration};

use anyhow::Context;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{
    clipboard::{backend::ClipboardEvent, wayland::WaylandBackend},
    config::{AppPaths, Config},
    crypto::MeshSecret,
    discovery::{NetbirdDiscovery, PeerDiscovery},
    envelope::{StateKeys, StoreLock},
    ipc::{self, DaemonState},
    mesh::{MeshHandle, MeshRuntime, MeshRuntimeConfig},
    payload::{
        ChunkStore, ChunkStoreConfig, ExplicitSharePolicy, Materializer, MaterializerConfig,
    },
    storage::HistoryStore,
    transfer::{TransferCoordinator, TransferStateLimits},
};

use super::{
    activation::schedule_materialization_cleanup,
    clipboard::{handle_clipboard_event, spawn_clipboard_watch},
    commands::handle_daemon_command,
    config_supervision::{apply_config_reload, initialize_shared_settings, spawn_config_watch},
    mesh_persistence::{MeshPersistenceContext, handle_mesh_batch, handle_mesh_chunk_command},
    views::{device_items, history_items},
};

/// Runs discovery and local IPC until a termination signal is received.
///
/// # Errors
///
/// Returns an error when runtime setup, IPC serving, or signal handling fails.
#[allow(clippy::too_many_lines)]
pub async fn run(paths: AppPaths, mut config: Config) -> anyhow::Result<()> {
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
        StateKeys::open_or_create(&store_lock, &mesh_secret).context("open encrypted keyslot")?;
    let storage_path = paths.state_dir.join("history.db");
    let mut history = HistoryStore::open(&storage_path, state_keys.storage_key())
        .with_context(|| format!("open encrypted history at {}", storage_path.display()))?;
    initialize_shared_settings(&mut history, &paths.config, &mut config)
        .context("reconcile shared mesh settings with config")?;
    let persisted_operations = history
        .storage()
        .load_operations()
        .context("load mesh operation log")?;

    let content_key = state_keys.content_identity_key();
    let transport_psk = mesh_secret
        .transport_psk()
        .context("derive mesh transport key")?;
    let chunk_store = ChunkStore::open(
        paths.state_dir.join("chunks"),
        state_keys.chunk_store_key(),
        ChunkStoreConfig {
            max_payload_bytes: config.local.maximum_explicit_share_bytes,
            max_chunks_per_manifest: 65_536,
            ..ChunkStoreConfig::default()
        },
    )
    .context("open encrypted chunk store")?;
    let materializer = Materializer::new(
        paths.runtime_dir.join("materialized"),
        MaterializerConfig {
            free_space_reserve_bytes: config.local.materialization_free_space_reserve_bytes,
        },
    )
    .context("open runtime materializer")?;
    let abandoned_materializations = materializer
        .cleanup_abandoned()
        .context("clean materializations left by a previous daemon")?;
    if abandoned_materializations != 0 {
        tracing::info!(
            removed = abandoned_materializations,
            "removed abandoned runtime materializations"
        );
    }
    let mut transfers = TransferCoordinator::new(
        chunk_store,
        materializer,
        ExplicitSharePolicy {
            automatic_capture_threshold_bytes: config.shared.capture_threshold_bytes,
            mesh_quota_bytes: config.shared.mesh_quota_bytes,
            maximum_explicit_share_bytes: config.local.maximum_explicit_share_bytes,
            free_space_reserve_bytes: config.local.transfer_free_space_reserve_bytes,
        },
        TransferStateLimits::default(),
    );
    transfers
        .reconcile_projection(history.projection())
        .context("recover transfer state")?;

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
    mesh_config.max_concurrent_chunk_streams = config.local.max_concurrent_chunk_streams;
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
    let (mesh, mut mesh_rx, mut mesh_chunk_rx) = MeshRuntime::spawn_with_transfers(
        mesh_config,
        transport_psk,
        &persisted_operations,
        shutdown.clone(),
    )
    .context("start mesh runtime")?;
    let mesh_handle = mesh.handle();
    state.set_mesh(mesh_handle.clone()).await;
    let discovery = spawn_discovery(
        config.clone(),
        state.clone(),
        mesh_handle.clone(),
        shutdown.clone(),
    );
    let (config_tx, mut config_rx) = tokio::sync::mpsc::unbounded_channel();
    let config_watch = spawn_config_watch(
        paths.config.clone(),
        config.clone(),
        config_tx,
        shutdown.clone(),
    );

    let clipboard = WaylandBackend::new();
    clipboard
        .set_capture_threshold(
            history
                .projection()
                .effective_shared_settings()
                .capture_threshold_bytes,
        )
        .context("apply effective clipboard capture threshold")?;
    let (clipboard_tx, mut clipboard_rx) = tokio::sync::mpsc::channel(128);
    let mut clipboard_watch = spawn_clipboard_watch(
        clipboard.clone(),
        state.clone(),
        clipboard_tx,
        shutdown.clone(),
    );

    tracing::info!(socket = %paths.socket.display(), "clip-sync daemon started");
    let server = ipc::serve(&paths.socket, state.clone(), shutdown.clone());
    let termination = shutdown_signal();
    tokio::pin!(server);
    tokio::pin!(termination);
    let mut server_finished = false;
    let mut clipboard_finished = false;
    let mut active_materialization = None;
    let mut pending_materialization_cleanup = None;
    let materialization_root = paths.runtime_dir.join("materialized");

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
                    handle_daemon_command(
                        command,
                        &clipboard,
                        &mut history,
                        &state,
                        &mesh_handle,
                        &mut transfers,
                        content_key,
                        &paths.config,
                        &mut config,
                        &mut active_materialization,
                        &mut pending_materialization_cleanup,
                        &materialization_root,
                    ).await;
                }
            }
            event = clipboard_rx.recv() => {
                if let Some(event) = event {
                    if matches!(
                        event,
                        ClipboardEvent::NewOffer { .. }
                            | ClipboardEvent::Captured { .. }
                            | ClipboardEvent::Cleared { .. }
                    ) && let Some(manifest_id) = active_materialization.take()
                    {
                        let cancellation = CancellationToken::new();
                        schedule_materialization_cleanup(
                            materialization_root.clone(),
                            manifest_id,
                            cancellation.clone(),
                        );
                        pending_materialization_cleanup = Some((manifest_id, cancellation));
                    }
                    handle_clipboard_event(
                        event,
                        &mut history,
                        &state,
                        content_key,
                        &mesh_handle,
                        &mut transfers,
                    ).await?;
                }
            }
            batch = mesh_rx.recv() => {
                if let Some(batch) = batch {
                    let mut context = MeshPersistenceContext {
                        history: &mut history,
                        state: &state,
                        content_key,
                        clipboard: &clipboard,
                        mesh: &mesh_handle,
                        config_path: &paths.config,
                        config: &mut config,
                        transfers: &mut transfers,
                    };
                    handle_mesh_batch(batch, &mut context).await;
                }
            }
            changed = config_rx.recv() => {
                if let Some(changed) = changed
                    && let Err(error) = apply_config_reload(
                        changed,
                        &paths.config,
                        &mut config,
                        &mut history,
                        &clipboard,
                        &mesh_handle,
                        &state,
                        &mut transfers,
                    ).await
                {
                    tracing::warn!(%error, "config reload was rejected");
                }
            }
            command = mesh_chunk_rx.recv() => {
                if let Some(command) = command {
                    handle_mesh_chunk_command(command, &mut transfers, &mesh_handle);
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
    finish_task(config_watch).await;
    mesh.wait().await;
    tracing::info!("clip-sync daemon stopped");
    Ok(())
}
pub(super) fn unix_time_millis() -> anyhow::Result<u64> {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_millis();
    u64::try_from(millis).context("system clock milliseconds exceed u64")
}

fn spawn_discovery(
    config: Config,
    state: DaemonState,
    mesh: MeshHandle,
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
                    mesh.update_discovery(snapshot.clone());
                    state.set_discovery(snapshot).await;
                }
                Err(error) => {
                    mesh.clear_discovery();
                    state.set_discovery_error(error.to_string()).await;
                    tracing::warn!(%error, "NetBird discovery is unavailable");
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
