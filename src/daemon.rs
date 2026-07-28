use std::{fs, time::Duration};

use anyhow::Context;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{
    clipboard::{
        backend::{ClipboardBackend, ClipboardEvent},
        types::{ClipboardContent, ClipboardRepresentation, MimeType},
        wayland::WaylandBackend,
    },
    config::{AppPaths, Config},
    crypto::MeshSecret,
    discovery::{NetbirdDiscovery, PeerDiscovery},
    ipc::{self, DaemonCommand, DaemonState, protocol::HistoryItem},
    mesh::{MeshHandle, MeshRuntime, MeshRuntimeConfig, PersistBatch},
    model::{Operation, Payload, Representation, StampedOperation},
    replica::Replica,
    replication::{Codec, JsonV1Codec},
    storage::EncryptedStorage,
};

/// Runs discovery and local IPC until a termination signal is received.
///
/// # Errors
///
/// Returns an error when runtime setup, IPC serving, or signal handling fails.
#[allow(clippy::too_many_lines)]
pub async fn run(paths: AppPaths, config: Config) -> anyhow::Result<()> {
    fs::create_dir_all(&paths.state_dir).context("create state directory")?;
    fs::create_dir_all(&paths.runtime_dir).context("create runtime directory")?;

    let mesh_secret = MeshSecret::load(&config.local.mesh_key_file)
        .context("load mesh secret from configured file")?;
    let storage_key = mesh_secret.storage_key().context("derive storage key")?;
    let storage_path = paths.state_dir.join("history.db");
    let mut storage = EncryptedStorage::open(&storage_path, &storage_key)
        .with_context(|| format!("open encrypted history at {}", storage_path.display()))?;
    let metadata = storage
        .local_replica_metadata()
        .context("load local replica identity")?;
    let projection = storage
        .rebuild_projection()
        .context("rebuild history projection")?;
    let persisted_operations = storage
        .load_operations()
        .context("load mesh operation log")?;
    let mut replica = Replica::restore(
        metadata.node_id(),
        metadata.next_operation_counter().saturating_sub(1),
        metadata.last_hlc(),
        projection,
    );

    let content_key = mesh_secret
        .content_key()
        .context("derive content identity key")?;
    let transport_psk = mesh_secret
        .transport_psk()
        .context("derive mesh transport key")?;

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
    state.set_history(history_items(&replica)).await;
    let shutdown = CancellationToken::new();
    let mut mesh_config = MeshRuntimeConfig::new(
        replica.node_id(),
        hostname.clone(),
        config.local.listen_port,
    );
    mesh_config.reconcile_interval = Duration::from_secs(config.local.reconcile_interval_seconds);
    mesh_config.reconnect_min = Duration::from_secs(config.local.reconnect_min_seconds);
    mesh_config.reconnect_max = Duration::from_secs(config.local.reconnect_max_seconds);
    let (mesh, mut mesh_rx) = MeshRuntime::spawn(
        mesh_config,
        transport_psk,
        &persisted_operations,
        shutdown.clone(),
    )
    .context("start mesh runtime")?;
    let mesh_handle = mesh.handle();
    let discovery = spawn_discovery(config, state.clone(), mesh_handle.clone(), shutdown.clone());

    let clipboard = WaylandBackend::new();
    let (clipboard_tx, mut clipboard_rx) = tokio::sync::mpsc::unbounded_channel();
    let clipboard_events = clipboard_tx.clone();
    let clipboard_backend = clipboard.clone();
    let clipboard_shutdown = shutdown.clone();
    let mut clipboard_watch = tokio::spawn(async move {
        clipboard_backend
            .watch(
                clipboard_shutdown,
                Box::new(move |event| {
                    let _ = clipboard_events.send(event);
                }),
            )
            .await
    });

    tracing::info!(socket = %paths.socket.display(), "clip-sync daemon started");
    let server = ipc::serve(&paths.socket, state.clone(), shutdown.clone());
    let termination = shutdown_signal();
    tokio::pin!(server);
    tokio::pin!(termination);
    let mut server_finished = false;
    let mut clipboard_finished = false;

    loop {
        tokio::select! {
            result = &mut server, if !server_finished => {
                server_finished = true;
                result.context("serve local IPC")?;
                break;
            }
            result = &mut clipboard_watch, if !clipboard_finished => {
                clipboard_finished = true;
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => tracing::warn!(%error, "Wayland clipboard monitoring stopped"),
                    Err(error) => tracing::warn!(%error, "Wayland clipboard task failed"),
                }
            }
            command = command_rx.recv() => {
                if let Some(command) = command {
                    handle_daemon_command(
                        command,
                        &clipboard,
                        &mut replica,
                        &mut storage,
                        &state,
                        &mesh_handle,
                    ).await;
                }
            }
            event = clipboard_rx.recv(), if !clipboard_finished => {
                if let Some(event) = event {
                    handle_clipboard_event(
                        event,
                        &mut replica,
                        &mut storage,
                        &state,
                        &content_key,
                        &mesh_handle,
                    ).await?;
                }
            }
            batch = mesh_rx.recv() => {
                if let Some(batch) = batch {
                    handle_mesh_batch(
                        batch,
                        &mut replica,
                        &mut storage,
                        &state,
                        &content_key,
                    ).await;
                }
            }
            result = &mut termination => {
                result.context("listen for shutdown signal")?;
                break;
            }
        }
    }

    shutdown.cancel();
    if !server_finished {
        server.await.context("stop local IPC")?;
    }
    if !clipboard_finished {
        match clipboard_watch.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(%error, "Wayland clipboard monitoring stopped"),
            Err(error) => tracing::warn!(%error, "Wayland clipboard task failed"),
        }
    }
    finish_task(discovery).await;
    mesh.wait().await;
    tracing::info!("clip-sync daemon stopped");
    Ok(())
}

async fn handle_daemon_command(
    command: DaemonCommand,
    clipboard: &WaylandBackend,
    replica: &mut Replica,
    storage: &mut EncryptedStorage,
    state: &DaemonState,
    mesh: &MeshHandle,
) {
    match command {
        DaemonCommand::Activate { content_id, reply } => {
            let result =
                activate_history_item(&content_id, clipboard, replica, storage, state, mesh)
                    .await
                    .map_err(|error| error.to_string());
            let _ = reply.send(result);
        }
    }
}

async fn activate_history_item(
    encoded_content_id: &str,
    clipboard: &WaylandBackend,
    replica: &mut Replica,
    storage: &mut EncryptedStorage,
    state: &DaemonState,
    mesh: &MeshHandle,
) -> anyhow::Result<()> {
    let content_id = encoded_content_id
        .parse()
        .context("content ID is invalid")?;
    let payload = replica
        .projection()
        .payload(content_id)
        .cloned()
        .context("history item is unavailable")?;
    if !replica.projection().is_visible(content_id) {
        anyhow::bail!("history item is deleted");
    }

    let representations = payload
        .representations()
        .iter()
        .map(|representation| {
            let mime = MimeType::new(representation.mime())
                .context("stored MIME type cannot be served")?;
            Ok(ClipboardRepresentation::new(mime, representation.bytes()))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let clipboard_content =
        ClipboardContent::new(representations).context("stored history item cannot be served")?;
    clipboard
        .set_clipboard_content(clipboard_content)
        .await
        .context("set active Wayland clipboard")?;

    let mut next = replica.clone();
    let operation = next
        .activate(content_id, unix_time_millis()?)
        .context("author clipboard activation")?;
    storage
        .append_local_operation(&operation)
        .context("persist clipboard activation")?;
    *replica = next;
    mesh.record_local(&operation)
        .await
        .context("publish clipboard activation to mesh")?;
    state.set_history(history_items(replica)).await;
    Ok(())
}

fn history_items(replica: &Replica) -> Vec<HistoryItem> {
    replica
        .projection()
        .visible_items()
        .into_iter()
        .map(|view| {
            let payload = view.payload();
            let mime_types = payload.map_or_else(Vec::new, |payload| {
                payload
                    .representations()
                    .iter()
                    .map(|representation| representation.mime().to_owned())
                    .collect()
            });
            let logical_size = payload.map_or(0, |payload| payload.descriptor().logical_size());
            HistoryItem {
                content_id: view.content_id().to_string(),
                preview: payload.map_or_else(|| "Unavailable payload".to_owned(), history_preview),
                mime_types,
                logical_size,
                source_node: view.last_activity().operation_id().node().to_string(),
                pinned: view.pinned(),
                physical_millis: view.last_activity().timestamp().physical_millis(),
            }
        })
        .collect()
}

fn history_preview(payload: &Payload) -> String {
    if let Some(text) = payload
        .representations()
        .iter()
        .find(|representation| representation.mime().starts_with("text/plain"))
    {
        let decoded = String::from_utf8_lossy(text.bytes());
        let mut preview = decoded
            .chars()
            .map(|character| {
                if character.is_control() {
                    ' '
                } else {
                    character
                }
            })
            .take(160)
            .collect::<String>();
        if decoded.chars().count() > 160 {
            preview.push('…');
        }
        return preview;
    }

    let mime = payload
        .representations()
        .first()
        .map_or("unknown", |representation| representation.mime());
    format!("{mime} · {} bytes", payload.descriptor().logical_size())
}

async fn handle_clipboard_event(
    event: ClipboardEvent,
    replica: &mut Replica,
    storage: &mut EncryptedStorage,
    state: &DaemonState,
    content_key: &[u8; 32],
    mesh: &MeshHandle,
) -> anyhow::Result<()> {
    match event {
        ClipboardEvent::Captured { content, .. } => {
            let representations = content
                .representations()
                .iter()
                .map(|representation| {
                    Representation::new(representation.mime_type().as_str(), representation.bytes())
                })
                .collect::<Vec<_>>();
            let payload = Payload::new(content_key, representations)
                .context("build captured clipboard payload")?;
            let mut next = replica.clone();
            let operation = next
                .copy(payload, unix_time_millis()?)
                .context("author clipboard history operation")?;
            storage
                .append_local_operation(&operation)
                .context("persist clipboard history operation")?;
            *replica = next;
            mesh.record_local(&operation)
                .await
                .context("publish clipboard history operation to mesh")?;
            state.set_history(history_items(replica)).await;
            tracing::debug!(
                history_entries = replica.projection().visible_items().len(),
                "captured clipboard history entry"
            );
        }
        ClipboardEvent::CaptureRejected { reason, .. } => {
            tracing::debug!(?reason, "clipboard offer was not captured");
        }
        ClipboardEvent::Finished => {
            tracing::warn!("Wayland compositor finished the clipboard data-control device");
        }
        ClipboardEvent::NewOffer { .. }
        | ClipboardEvent::OwnContent { .. }
        | ClipboardEvent::Cleared { .. } => {}
    }
    Ok(())
}

async fn handle_mesh_batch(
    batch: PersistBatch,
    replica: &mut Replica,
    storage: &mut EncryptedStorage,
    state: &DaemonState,
    content_key: &[u8; 32],
) {
    let result = persist_mesh_batch(batch.operations(), replica, storage, content_key);
    if result.is_ok() {
        state.set_history(history_items(replica)).await;
    }
    batch.complete(result.map_err(|error| error.to_string()));
}

fn persist_mesh_batch(
    raw_operations: &[Vec<u8>],
    replica: &mut Replica,
    storage: &mut EncryptedStorage,
    content_key: &[u8; 32],
) -> anyhow::Result<()> {
    let codec = JsonV1Codec;
    let operations = raw_operations
        .iter()
        .map(|raw| codec.decode_op(raw).context("decode remote operation"))
        .collect::<anyhow::Result<Vec<StampedOperation>>>()?;
    for operation in &operations {
        if let Operation::Add { payload, .. } = operation.operation() {
            payload
                .validate(content_key)
                .context("validate remote clipboard payload identity")?;
        }
    }

    let now_millis = unix_time_millis()?;
    let mut next = replica.clone();
    for operation in &operations {
        next.ingest(operation, now_millis)
            .context("apply remote history operation")?;
    }
    storage
        .append_remote_operations(&operations, next.last_timestamp())
        .context("persist remote operation batch")?;
    *replica = next;
    Ok(())
}

fn unix_time_millis() -> anyhow::Result<u64> {
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
