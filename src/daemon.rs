use std::{fs, time::Duration};

use anyhow::Context;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{
    clipboard::{
        backend::{ClipboardBackend, ClipboardEvent},
        types::{ClipboardContent, ClipboardRepresentation, CurrentClipboardInspection, MimeType},
        wayland::WaylandBackend,
    },
    config::{AppPaths, Config},
    crypto::MeshSecret,
    discovery::{NetbirdDiscovery, PeerDiscovery},
    ipc::{self, DaemonCommand, DaemonState, protocol::HistoryItem},
    mesh::{MeshChunkCommand, MeshHandle, MeshRuntime, MeshRuntimeConfig, PersistBatch},
    model::{Operation, Payload, Representation, StampedOperation},
    payload::{
        ChunkStore, ChunkStoreConfig, ExplicitShareInspection, ExplicitSharePolicy,
        FileSnapshotLimits, Materializer, MaterializerConfig, parse_file_uri_list,
    },
    replica::Replica,
    replication::{Codec, JsonV1Codec},
    storage::HistoryStore,
    transfer::{TransferCoordinator, TransferId, TransferStateLimits},
};

/// Stable daemon-layer result consumed by future IPC/UI adapters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShareCurrentClipboardResult {
    pub transfer_id: TransferId,
    pub content_id: crate::model::ContentId,
}

/// Two-pass live-offer inspection: size/MIME metadata plus policy warning.
#[derive(Clone, Debug)]
pub struct LiveClipboardShareInspection {
    pub current: CurrentClipboardInspection,
    pub policy: ExplicitShareInspection,
    file_paths: Option<Vec<std::path::PathBuf>>,
}

/// Inspects the real current Wayland offer without retaining its bytes.
///
/// # Errors
///
/// Returns live-offer, hard-limit, or free-space errors.
pub async fn inspect_live_current_clipboard(
    clipboard: &impl ClipboardBackend,
    transfers: &TransferCoordinator,
    maximum_explicit_share_bytes: u64,
) -> anyhow::Result<LiveClipboardShareInspection> {
    let current = clipboard
        .inspect_current_clipboard(maximum_explicit_share_bytes)
        .await
        .context("inspect current live clipboard")?;
    let available =
        fs2::available_space(transfers.store().root()).context("inspect chunk-store free space")?;
    let mut policy = transfers
        .inspect_size(current.logical_size(), available)
        .context("inspect explicit clipboard policy")?;
    let mut file_paths = None;
    if current.logical_size() <= 1024 * 1024
        && current
            .mime_list()
            .types()
            .iter()
            .any(|mime| mime.as_str() == "text/uri-list")
    {
        let metadata = clipboard
            .capture_current_clipboard(&current)
            .await
            .context("inspect current file URI metadata")?;
        if let Some(uri_list) = metadata.bytes_for_mime("text/uri-list") {
            let limits = FileSnapshotLimits {
                max_logical_bytes: maximum_explicit_share_bytes,
                ..FileSnapshotLimits::default()
            };
            let paths = parse_file_uri_list(&uri_list, limits)
                .context("validate current clipboard file URIs")?;
            policy = transfers
                .inspect_files(&paths, limits, available, &CancellationToken::new())
                .context("inspect current clipboard file snapshot")?;
            file_paths = Some(paths);
        }
    }
    Ok(LiveClipboardShareInspection {
        current,
        policy,
        file_paths,
    })
}

/// Confirms, re-reads, chunks, and publishes the exact inspected live offer.
///
/// Confirmation is checked before the second read and before chunk allocation.
///
/// # Errors
///
/// Returns confirmation, generation-change, capture, storage, or mesh errors.
#[allow(clippy::too_many_arguments)]
pub async fn share_live_current_clipboard(
    clipboard: &impl ClipboardBackend,
    inspection: &LiveClipboardShareInspection,
    confirmed: bool,
    content_key: &[u8; 32],
    transfers: &mut TransferCoordinator,
    history: &mut HistoryStore,
    mesh: &MeshHandle,
    now_millis: u64,
    cancellation: &CancellationToken,
) -> anyhow::Result<ShareCurrentClipboardResult> {
    transfers
        .require_confirmation(inspection.policy, confirmed)
        .context("confirm explicit clipboard share")?;
    let content = clipboard
        .capture_current_clipboard(&inspection.current)
        .await
        .context("capture inspected current clipboard")?;
    if let Some(expected_paths) = &inspection.file_paths {
        let uri_list = content
            .bytes_for_mime("text/uri-list")
            .context("inspected file clipboard no longer offers text/uri-list")?;
        let limits = FileSnapshotLimits {
            max_logical_bytes: inspection.policy.logical_size(),
            ..FileSnapshotLimits::default()
        };
        let paths = parse_file_uri_list(&uri_list, limits)
            .context("validate confirmed clipboard file URIs")?;
        if paths != *expected_paths {
            anyhow::bail!("clipboard file list changed after inspection");
        }
        let payload = payload_from_clipboard(&content, content_key)?;
        let (transfer_id, begin) = transfers
            .begin_file_share(
                &paths,
                payload.descriptor().content_id(),
                inspection.policy,
                confirmed,
                limits,
                history,
                now_millis,
                cancellation,
            )
            .context("begin explicit clipboard file snapshot")?;
        mesh.record_local(&begin)
            .await
            .context("publish pending clipboard file snapshot")?;
        let complete = transfers
            .complete_payload_share(transfer_id, history, now_millis)
            .context("complete explicit clipboard file snapshot")?;
        mesh.record_local(&complete)
            .await
            .context("publish completed clipboard file snapshot")?;
        enforce_and_publish_quota(history, mesh, now_millis).await?;
        mesh.notify_transfers();
        return Ok(ShareCurrentClipboardResult {
            transfer_id,
            content_id: payload.descriptor().content_id(),
        });
    }
    share_current_clipboard(
        &content,
        confirmed,
        content_key,
        transfers,
        history,
        mesh,
        now_millis,
        cancellation,
    )
    .await
}

/// Inspects a live clipboard snapshot using the daemon's effective policy.
///
/// # Errors
///
/// Returns content validation, free-space, or explicit-share policy errors.
pub fn inspect_current_clipboard(
    content: &ClipboardContent,
    content_key: &[u8; 32],
    transfers: &TransferCoordinator,
) -> anyhow::Result<ExplicitShareInspection> {
    let payload = payload_from_clipboard(content, content_key)?;
    let available =
        fs2::available_space(transfers.store().root()).context("inspect chunk-store free space")?;
    transfers
        .inspect_payload(&payload, available)
        .context("inspect explicit clipboard share")
}

/// Secure explicit-share vertical slice used by the daemon and future local
/// API adapters. Oversized content cannot begin without confirmation.
///
/// Begin and completion operations become durable before each mesh wakeup.
/// Remote peers only receive history/transfer state; this function never
/// changes a remote active clipboard.
///
/// # Errors
///
/// Returns validation, confirmation, storage, authoring, or mesh errors.
#[allow(clippy::too_many_arguments)]
pub async fn share_current_clipboard(
    content: &ClipboardContent,
    confirmed: bool,
    content_key: &[u8; 32],
    transfers: &mut TransferCoordinator,
    history: &mut HistoryStore,
    mesh: &MeshHandle,
    now_millis: u64,
    cancellation: &CancellationToken,
) -> anyhow::Result<ShareCurrentClipboardResult> {
    let payload = payload_from_clipboard(content, content_key)?;
    let available =
        fs2::available_space(transfers.store().root()).context("inspect chunk-store free space")?;
    let inspection = transfers
        .inspect_payload(&payload, available)
        .context("inspect explicit clipboard share")?;
    let (transfer_id, begin) = transfers
        .begin_payload_share(
            &payload,
            inspection,
            confirmed,
            history,
            now_millis,
            cancellation,
        )
        .context("begin explicit clipboard share")?;
    mesh.record_local(&begin)
        .await
        .context("publish pending clipboard share")?;
    let complete = transfers
        .complete_payload_share(transfer_id, history, now_millis)
        .context("complete explicit clipboard share")?;
    mesh.record_local(&complete)
        .await
        .context("publish completed clipboard share")?;
    enforce_and_publish_quota(history, mesh, now_millis).await?;
    mesh.notify_transfers();
    Ok(ShareCurrentClipboardResult {
        transfer_id,
        content_id: payload.descriptor().content_id(),
    })
}

/// Cancels a transfer mesh-wide and synchronously cleans local staging.
///
/// # Errors
///
/// Returns unknown-transfer, persistence, cleanup, or mesh publication errors.
pub async fn cancel_transfer(
    transfer_id: TransferId,
    transfers: &mut TransferCoordinator,
    history: &mut HistoryStore,
    mesh: &MeshHandle,
    now_millis: u64,
) -> anyhow::Result<()> {
    let operation = transfers
        .cancel(transfer_id, history, now_millis)
        .context("persist transfer cancellation")?;
    mesh.record_local(&operation)
        .await
        .context("publish transfer cancellation")?;
    mesh.notify_transfers();
    Ok(())
}

fn payload_from_clipboard(
    content: &ClipboardContent,
    content_key: &[u8; 32],
) -> anyhow::Result<Payload> {
    let representations = content
        .representations()
        .iter()
        .map(|representation| {
            Representation::new(representation.mime_type().as_str(), representation.bytes())
        })
        .collect();
    Payload::new(content_key, representations).context("build explicit clipboard payload")
}

async fn enforce_and_publish_quota(
    history: &mut HistoryStore,
    mesh: &MeshHandle,
    now_millis: u64,
) -> anyhow::Result<()> {
    let evictions = history
        .enforce_quota(now_millis)
        .context("persist explicit-share quota evictions")?;
    for eviction in &evictions {
        mesh.record_local(eviction)
            .await
            .context("publish explicit-share quota eviction")?;
    }
    Ok(())
}

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

    let mesh_secret = MeshSecret::load(&config.local.mesh_key_file)
        .context("load mesh secret from configured file")?;
    let storage_key = mesh_secret.storage_key().context("derive storage key")?;
    let storage_path = paths.state_dir.join("history.db");
    let mut history = HistoryStore::open(&storage_path, &storage_key)
        .with_context(|| format!("open encrypted history at {}", storage_path.display()))?;
    let persisted_operations = history
        .storage()
        .load_operations()
        .context("load mesh operation log")?;

    let content_key = mesh_secret
        .content_key()
        .context("derive content identity key")?;
    let transport_psk = mesh_secret
        .transport_psk()
        .context("derive mesh transport key")?;
    let chunk_store_key = mesh_secret
        .chunk_store_key()
        .context("derive chunk-store key")?;
    let chunk_store = ChunkStore::open(
        paths.state_dir.join("chunks"),
        &chunk_store_key,
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
    state.set_history(history_items(history.replica())).await;
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
    let maximum_explicit_share_bytes = config.local.maximum_explicit_share_bytes;
    let (mesh, mut mesh_rx, mut mesh_chunk_rx) = MeshRuntime::spawn_with_transfers(
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
    state
        .set_clipboard_status(true, "Wayland clipboard monitoring is active")
        .await;

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
                match result {
                    Ok(Ok(())) => {
                        state
                            .set_clipboard_status(false, "Wayland clipboard monitoring stopped")
                            .await;
                    }
                    Ok(Err(error)) => {
                        state.set_clipboard_status(false, error.to_string()).await;
                        tracing::warn!(%error, "Wayland clipboard monitoring stopped");
                    }
                    Err(error) => {
                        state.set_clipboard_status(false, error.to_string()).await;
                        tracing::warn!(%error, "Wayland clipboard task failed");
                    }
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
                        &content_key,
                        maximum_explicit_share_bytes,
                        &mut active_materialization,
                        &mut pending_materialization_cleanup,
                        &materialization_root,
                    ).await;
                }
            }
            event = clipboard_rx.recv(), if !clipboard_finished => {
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
                        &content_key,
                        &mesh_handle,
                    ).await?;
                }
            }
            batch = mesh_rx.recv() => {
                if let Some(batch) = batch {
                    handle_mesh_batch(
                        batch,
                        &mut history,
                        &state,
                        &content_key,
                        &mut transfers,
                        &mesh_handle,
                    ).await;
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

#[allow(clippy::too_many_arguments)]
async fn handle_daemon_command(
    command: DaemonCommand,
    clipboard: &WaylandBackend,
    history: &mut HistoryStore,
    state: &DaemonState,
    mesh: &MeshHandle,
    transfers: &mut TransferCoordinator,
    content_key: &[u8; 32],
    maximum_explicit_share_bytes: u64,
    active_materialization: &mut Option<crate::payload::ManifestId>,
    pending_cleanup: &mut Option<(crate::payload::ManifestId, CancellationToken)>,
    materialization_root: &std::path::Path,
) {
    match command {
        DaemonCommand::Activate { content_id, reply } => {
            let result = activate_history_item(
                &content_id,
                clipboard,
                history,
                state,
                mesh,
                transfers,
                content_key,
                maximum_explicit_share_bytes,
            )
            .await;
            let result = result
                .map(|materialized| {
                    if let Some(manifest_id) = materialized
                        && pending_cleanup
                            .as_ref()
                            .is_some_and(|(pending, _)| *pending == manifest_id)
                        && let Some((_, cancellation)) = pending_cleanup.take()
                    {
                        cancellation.cancel();
                    }
                    if let Some(previous) = std::mem::replace(active_materialization, materialized)
                        && Some(previous) != materialized
                    {
                        let cancellation = CancellationToken::new();
                        schedule_materialization_cleanup(
                            materialization_root.to_path_buf(),
                            previous,
                            cancellation.clone(),
                        );
                        *pending_cleanup = Some((previous, cancellation));
                    }
                })
                .map_err(|error| error.to_string());
            let _ = reply.send(result);
        }
        DaemonCommand::SetPinned {
            content_id,
            pinned,
            reply,
        } => {
            let result = update_history_item(
                &content_id,
                HistoryMutation::SetPinned(pinned),
                history,
                state,
                mesh,
            )
            .await
            .map_err(|error| error.to_string());
            let _ = reply.send(result);
        }
        DaemonCommand::Delete { content_id, reply } => {
            let result =
                update_history_item(&content_id, HistoryMutation::Delete, history, state, mesh)
                    .await
                    .map_err(|error| error.to_string());
            let _ = reply.send(result);
        }
    }
}

#[derive(Clone, Copy)]
enum HistoryMutation {
    SetPinned(bool),
    Delete,
}

async fn update_history_item(
    encoded_content_id: &str,
    mutation: HistoryMutation,
    history: &mut HistoryStore,
    state: &DaemonState,
    mesh: &MeshHandle,
) -> anyhow::Result<()> {
    let now_millis = unix_time_millis()?;
    let operation = match mutation {
        HistoryMutation::SetPinned(true) => history
            .pin_by_id(encoded_content_id, now_millis)
            .context("persist clipboard history pin")?,
        HistoryMutation::SetPinned(false) => history
            .unpin_by_id(encoded_content_id, now_millis)
            .context("persist clipboard history unpin")?,
        HistoryMutation::Delete => history
            .delete_by_id(encoded_content_id, now_millis)
            .context("persist clipboard history deletion")?,
    };
    mesh.record_local(&operation)
        .await
        .context("publish clipboard history update to mesh")?;
    state.set_history(history_items(history.replica())).await;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn activate_history_item(
    encoded_content_id: &str,
    clipboard: &WaylandBackend,
    history: &mut HistoryStore,
    state: &DaemonState,
    mesh: &MeshHandle,
    transfers: &mut TransferCoordinator,
    content_key: &[u8; 32],
    maximum_explicit_share_bytes: u64,
) -> anyhow::Result<Option<crate::payload::ManifestId>> {
    let content_id = encoded_content_id
        .parse()
        .context("content ID is invalid")?;
    if !history.projection().is_visible(content_id) {
        anyhow::bail!("history item is deleted");
    }

    let (clipboard_content, materialized_manifest) =
        if let Some(payload) = history.projection().payload(content_id) {
            let representations = payload
                .representations()
                .iter()
                .map(|representation| {
                    let mime = MimeType::new(representation.mime())
                        .context("stored MIME type cannot be served")?;
                    Ok(ClipboardRepresentation::new(mime, representation.bytes()))
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            (
                ClipboardContent::new(representations)
                    .context("stored history item cannot be served")?,
                None,
            )
        } else {
            let activated = transfers
                .activate(
                    content_id,
                    history.projection(),
                    content_key,
                    maximum_explicit_share_bytes,
                    &CancellationToken::new(),
                )
                .context("materialize transferred history item")?;
            let materialized = activated.materialized_manifest();
            (activated.into_content(), materialized)
        };
    clipboard
        .set_clipboard_content(clipboard_content)
        .await
        .context("set active Wayland clipboard")?;

    let operation = history
        .activate(content_id, unix_time_millis()?)
        .context("persist clipboard activation")?;
    mesh.record_local(&operation)
        .await
        .context("publish clipboard activation to mesh")?;
    state.set_history(history_items(history.replica())).await;
    Ok(materialized_manifest)
}

fn schedule_materialization_cleanup(
    root: std::path::PathBuf,
    manifest_id: crate::payload::ManifestId,
    cancellation: CancellationToken,
) {
    tokio::spawn(async move {
        tokio::select! {
            () = tokio::time::sleep(Duration::from_secs(2)) => {}
            () = cancellation.cancelled() => return,
        }
        match Materializer::new(root, MaterializerConfig::default())
            .and_then(|materializer| materializer.cleanup(manifest_id))
        {
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(%error, "could not clean clipboard file materialization");
            }
        }
    });
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
            let transfer = replica.projection().manifest_for_content(view.content_id());
            let mime_types = if mime_types.is_empty() {
                transfer.map_or_else(Vec::new, |(_, _, manifest)| match manifest {
                    crate::payload::StoredManifest::MimeBundle(bundle) => bundle
                        .representations()
                        .iter()
                        .map(|representation| representation.mime().to_owned())
                        .collect(),
                    crate::payload::StoredManifest::Files(_) => {
                        vec!["text/uri-list".to_owned()]
                    }
                    crate::payload::StoredManifest::Blob(_) => {
                        vec!["application/octet-stream".to_owned()]
                    }
                })
            } else {
                mime_types
            };
            let logical_size = payload.map_or_else(
                || transfer.map_or(0, |(_, _, manifest)| manifest.logical_size()),
                |payload| payload.descriptor().logical_size(),
            );
            HistoryItem {
                content_id: view.content_id().to_string(),
                preview: payload.map_or_else(
                    || {
                        transfer.map_or_else(
                            || "Unavailable payload".to_owned(),
                            |(transfer_id, _, _)| {
                                let phase = replica
                                    .projection()
                                    .transfer(transfer_id)
                                    .map_or(crate::transfer::TransferPhase::Pending, |view| {
                                        view.phase()
                                    });
                                format!("Transferred payload · {phase:?}")
                            },
                        )
                    },
                    history_preview,
                ),
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
    history: &mut HistoryStore,
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
            let operations = history
                .copy_and_enforce(payload, unix_time_millis()?)
                .context("persist clipboard history and quota operations")?;
            for operation in &operations {
                mesh.record_local(operation)
                    .await
                    .context("publish clipboard history operation to mesh")?;
            }
            state.set_history(history_items(history.replica())).await;
            tracing::debug!(
                history_entries = history.projection().visible_items().len(),
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
    history: &mut HistoryStore,
    state: &DaemonState,
    content_key: &[u8; 32],
    transfers: &mut TransferCoordinator,
    mesh: &MeshHandle,
) {
    let result = persist_mesh_batch(batch.operations(), history, content_key, transfers);
    let result = result.and_then(|()| {
        transfers
            .reconcile_projection(history.projection())
            .context("reconcile received transfer state")
    });
    if result.is_ok() {
        state.set_history(history_items(history.replica())).await;
        mesh.notify_transfers();
    }
    batch.complete(result.map_err(|error| error.to_string()));
}

fn handle_mesh_chunk_command(
    command: MeshChunkCommand,
    transfers: &mut TransferCoordinator,
    mesh: &MeshHandle,
) {
    let cancellation = CancellationToken::new();
    match command {
        MeshChunkCommand::Missing { maximum, reply } => {
            let result = transfers
                .missing_chunks(maximum)
                .map_err(|error| error.to_string());
            let _ = reply.send(result);
        }
        MeshChunkCommand::Export { request, reply } => {
            let result = transfers
                .export_chunk(request, &cancellation)
                .map_err(|error| error.to_string());
            let _ = reply.send(result);
        }
        MeshChunkCommand::Import {
            request,
            encrypted,
            reply,
        } => {
            let result = transfers
                .import_chunk(request, &encrypted, &cancellation)
                .map(|_| ())
                .map_err(|error| error.to_string());
            if result.is_ok() {
                mesh.notify_transfers();
            }
            let _ = reply.send(result);
        }
    }
}

fn persist_mesh_batch(
    raw_operations: &[Vec<u8>],
    history: &mut HistoryStore,
    content_key: &[u8; 32],
    transfers: &TransferCoordinator,
) -> anyhow::Result<()> {
    let codec = JsonV1Codec;
    let operations = raw_operations
        .iter()
        .map(|raw| codec.decode_op(raw).context("decode remote operation"))
        .collect::<anyhow::Result<Vec<StampedOperation>>>()?;
    for operation in &operations {
        if let Operation::Add { payload, .. } | Operation::AddQuotaExempt { payload, .. } =
            operation.operation()
        {
            payload
                .validate(content_key)
                .context("validate remote clipboard payload identity")?;
        }
        if let Operation::BeginShare {
            manifest_id,
            manifest,
            ..
        } = operation.operation()
        {
            transfers
                .validate_manifest(*manifest_id, manifest)
                .context("validate remote transfer manifest")?;
        }
    }

    history
        .ingest_batch(&operations, unix_time_millis()?)
        .context("persist remote operation batch")?;
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
                Err(error) => {
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
