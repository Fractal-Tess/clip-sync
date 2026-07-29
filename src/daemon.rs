use std::{fs, io::Cursor, time::Duration};

use anyhow::Context;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{
    clipboard::{
        backend::{ClipboardBackend, ClipboardEvent},
        types::{ClipboardContent, ClipboardRepresentation, CurrentClipboardInspection, MimeType},
        wayland::WaylandBackend,
    },
    config::{AppPaths, Config, SharedConfig},
    crypto::MeshSecret,
    discovery::{NetbirdDiscovery, PeerDiscovery},
    envelope::{StateKeys, StoreLock},
    ipc::{
        self, DaemonCommand, DaemonState,
        protocol::{
            DeviceItem, HistoryItem, ImagePreviewResponse, ShareClipboardResponse,
            SharedSettingKind, TransferItem,
        },
    },
    mesh::{
        MeshChunkCommand, MeshHandle, MeshRuntime, MeshRuntimeConfig, PersistBatch, PersistResult,
    },
    model::{NodeId, Operation, Payload, Representation, SharedSetting, StampedOperation},
    payload::{
        ChunkStore, ChunkStoreConfig, ExplicitShareInspection, ExplicitSharePolicy,
        FileSnapshotLimits, Materializer, MaterializerConfig, parse_file_uri_list,
    },
    replica::Replica,
    replication::{Codec, JsonV1Codec},
    storage::{CompactionReport, HistoryStore},
    transfer::{TransferCoordinator, TransferId, TransferStateLimits},
};

const IMAGE_PREVIEW_WIDTH: u32 = 320;
const IMAGE_PREVIEW_HEIGHT: u32 = 180;
const MAX_IMAGE_PREVIEW_SOURCE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_IMAGE_PREVIEW_DIMENSION: u32 = 8192;
const MAX_IMAGE_PREVIEW_DECODE_BYTES: u64 = 128 * 1024 * 1024;

/// Stable daemon-layer result consumed by future IPC/UI adapters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShareCurrentClipboardResult {
    pub transfer_id: TransferId,
    pub content_id: crate::model::ContentId,
}

/// Result of processing one backend-approved automatic clipboard capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutomaticClipboardCaptureResult {
    /// An ordinary MIME offer was retained inline in replicated history.
    Payload { content_id: crate::model::ContentId },
    /// A local file offer was retained as an encrypted file manifest.
    Files {
        transfer_id: TransferId,
        content_id: crate::model::ContentId,
    },
    /// A file offer failed URI, path, type, mutation, size, or resource checks.
    RejectedFiles,
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

/// Safely persists and publishes one automatic clipboard capture.
///
/// Offers containing `text/uri-list` never enter inline history. Their local
/// roots are parsed, recursively preflighted against the effective automatic
/// threshold, revalidated while being streamed, and published through the
/// file-manifest transfer path. Safety and resource-policy failures return
/// [`AutomaticClipboardCaptureResult::RejectedFiles`] without retaining the
/// URI bytes.
///
/// Non-file offers retain every captured MIME representation.
///
/// # Errors
///
/// Returns payload, storage, chunk-store, operation publication, or quota
/// errors. File-offer safety/policy rejection is reported as a successful
/// [`AutomaticClipboardCaptureResult::RejectedFiles`] outcome.
pub async fn capture_automatic_clipboard(
    content: &ClipboardContent,
    content_key: &[u8; 32],
    transfers: &mut TransferCoordinator,
    history: &mut HistoryStore,
    mesh: &MeshHandle,
    now_millis: u64,
    cancellation: &CancellationToken,
) -> anyhow::Result<AutomaticClipboardCaptureResult> {
    let Some(uri_list) = content.bytes_for_mime("text/uri-list") else {
        let payload = payload_from_clipboard(content, content_key)?;
        let content_id = payload.descriptor().content_id();
        let operations = history
            .copy_and_enforce(payload, now_millis)
            .context("persist clipboard history and quota operations")?;
        for operation in &operations {
            mesh.record_local(operation)
                .await
                .context("publish clipboard history operation to mesh")?;
        }
        return Ok(AutomaticClipboardCaptureResult::Payload { content_id });
    };

    let limits = FileSnapshotLimits {
        max_logical_bytes: transfers.automatic_capture_threshold_bytes(),
        ..FileSnapshotLimits::default()
    };
    let paths = match parse_file_uri_list(&uri_list, limits) {
        Ok(paths) => paths,
        Err(error) => {
            tracing::debug!(%error, "automatic clipboard file URI list was rejected");
            return Ok(AutomaticClipboardCaptureResult::RejectedFiles);
        }
    };
    let available =
        fs2::available_space(transfers.store().root()).context("inspect chunk-store free space")?;
    let inspection = match transfers.inspect_files(&paths, limits, available, cancellation) {
        Ok(inspection) => inspection,
        Err(error) => {
            tracing::debug!(%error, "automatic clipboard file preflight was rejected");
            return Ok(AutomaticClipboardCaptureResult::RejectedFiles);
        }
    };
    let payload = payload_from_clipboard(content, content_key)?;
    let content_id = payload.descriptor().content_id();
    let (transfer_id, begin) = match transfers.begin_file_share(
        &paths,
        content_id,
        inspection,
        false,
        limits,
        history,
        now_millis,
        cancellation,
    ) {
        Ok(begin) => begin,
        Err(crate::transfer::TransferCoordinatorError::FileSnapshot(error)) => {
            tracing::debug!(%error, "automatic clipboard file snapshot was rejected");
            return Ok(AutomaticClipboardCaptureResult::RejectedFiles);
        }
        Err(crate::transfer::TransferCoordinatorError::SourceChanged) => {
            tracing::debug!("automatic clipboard files changed during snapshot");
            return Ok(AutomaticClipboardCaptureResult::RejectedFiles);
        }
        Err(error) => return Err(error).context("begin automatic clipboard file snapshot"),
    };
    mesh.record_local(&begin)
        .await
        .context("publish pending automatic clipboard file snapshot")?;
    let complete = transfers
        .complete_payload_share(transfer_id, history, now_millis)
        .context("complete automatic clipboard file snapshot")?;
    mesh.record_local(&complete)
        .await
        .context("publish completed automatic clipboard file snapshot")?;
    enforce_and_publish_quota(history, mesh, now_millis).await?;
    mesh.notify_transfers();
    Ok(AutomaticClipboardCaptureResult::Files {
        transfer_id,
        content_id,
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

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the single-owner daemon dispatcher keeps command replies next to their mutations"
)]
async fn handle_daemon_command(
    command: DaemonCommand,
    clipboard: &WaylandBackend,
    history: &mut HistoryStore,
    state: &DaemonState,
    mesh: &MeshHandle,
    transfers: &mut TransferCoordinator,
    content_key: &[u8; 32],
    config_path: &std::path::Path,
    config: &mut Config,
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
                config.local.maximum_explicit_share_bytes,
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
        DaemonCommand::ShareClipboard { confirmed, reply } => {
            let result = share_clipboard_command(
                clipboard,
                confirmed,
                content_key,
                transfers,
                history,
                mesh,
                config.local.maximum_explicit_share_bytes,
            )
            .await
            .map_err(|error| error.to_string());
            if result.as_ref().is_ok_and(|result| result.shared) {
                state.set_history(history_items(history.replica())).await;
            }
            let _ = reply.send(result);
        }
        DaemonCommand::ListTransfers { reply } => {
            let _ = reply.send(Ok(transfer_items(history, transfers)));
        }
        DaemonCommand::CancelTransfer { transfer_id, reply } => {
            let result = async {
                let transfer_id = transfer_id.parse().context("transfer ID is invalid")?;
                cancel_transfer(transfer_id, transfers, history, mesh, unix_time_millis()?).await?;
                state.set_history(history_items(history.replica())).await;
                Ok(())
            }
            .await
            .map_err(|error: anyhow::Error| error.to_string());
            let _ = reply.send(result);
        }
        DaemonCommand::ForgetDevice { device_id, reply } => {
            let result = forget_device(&device_id, history, mesh)
                .await
                .map_err(|error| error.to_string());
            if result.is_ok() {
                state.set_devices(device_items(history)).await;
            }
            let _ = reply.send(result);
        }
        DaemonCommand::UpdateSharedSetting {
            setting,
            value,
            reply,
        } => {
            let result = update_shared_setting(
                setting,
                value,
                history,
                clipboard,
                transfers,
                mesh,
                config_path,
                config,
            )
            .await
            .map_err(|error| error.to_string());
            if result.is_ok() {
                state.set_config(config.clone()).await;
                state.set_history(history_items(history.replica())).await;
            }
            let _ = reply.send(result);
        }
        DaemonCommand::ImagePreview { content_id, reply } => {
            let result =
                image_preview(&content_id, history, transfers).map_err(|error| error.to_string());
            let _ = reply.send(result);
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn share_clipboard_command(
    clipboard: &impl ClipboardBackend,
    confirmed: bool,
    content_key: &[u8; 32],
    transfers: &mut TransferCoordinator,
    history: &mut HistoryStore,
    mesh: &MeshHandle,
    maximum_explicit_share_bytes: u64,
) -> anyhow::Result<ShareClipboardResponse> {
    let inspection =
        inspect_live_current_clipboard(clipboard, transfers, maximum_explicit_share_bytes).await?;
    let logical_size = inspection.policy.logical_size();
    let mime_types = inspection
        .current
        .mime_list()
        .types()
        .iter()
        .map(ToString::to_string)
        .collect();
    if inspection.policy.confirmation_required() && !confirmed {
        return Ok(ShareClipboardResponse {
            shared: false,
            confirmation_required: true,
            logical_size,
            mime_types,
            quota_exempt: inspection.policy.quota_exempt(),
            transfer_id: None,
            content_id: None,
            message: format!(
                "sharing {} requires confirmation; repeat with --confirm",
                inspection.policy.human_size()
            ),
        });
    }

    let result = share_live_current_clipboard(
        clipboard,
        &inspection,
        confirmed,
        content_key,
        transfers,
        history,
        mesh,
        unix_time_millis()?,
        &CancellationToken::new(),
    )
    .await?;
    Ok(ShareClipboardResponse {
        shared: true,
        confirmation_required: inspection.policy.confirmation_required(),
        logical_size,
        mime_types,
        quota_exempt: inspection.policy.quota_exempt(),
        transfer_id: Some(result.transfer_id.to_string()),
        content_id: Some(result.content_id.to_string()),
        message: "clipboard shared".to_owned(),
    })
}

fn transfer_items(history: &HistoryStore, transfers: &TransferCoordinator) -> Vec<TransferItem> {
    let projection = history.projection();
    transfers
        .progress()
        .into_iter()
        .map(|progress| {
            let view = projection.transfer(progress.transfer_id);
            TransferItem {
                transfer_id: progress.transfer_id.to_string(),
                content_id: view
                    .and_then(crate::model::TransferView::content_id)
                    .map_or_else(String::new, |content_id| content_id.to_string()),
                peer: view
                    .and_then(crate::model::TransferView::source_node)
                    .map_or_else(String::new, |node_id| node_id.to_string()),
                state: format!("{:?}", progress.phase).to_lowercase(),
                completed_bytes: progress.verified_bytes,
                total_bytes: progress.logical_size,
            }
        })
        .collect()
}

async fn forget_device(
    encoded_node_id: &str,
    history: &mut HistoryStore,
    mesh: &MeshHandle,
) -> anyhow::Result<()> {
    let node_id: NodeId = encoded_node_id.parse().context("device ID is invalid")?;
    let acknowledgements = history
        .acknowledgements()
        .context("load durable mesh membership")?;
    let known = history
        .projection()
        .known_members()
        .chain(acknowledgements.known_members())
        .any(|member| member == node_id);
    anyhow::ensure!(known, "device is not a known mesh member");
    anyhow::ensure!(
        !history.projection().is_device_forgotten(node_id),
        "device is already forgotten"
    );
    let operation = history
        .forget_device(node_id, unix_time_millis()?)
        .context("persist device-forget operation")?;
    mesh.record_local(&operation)
        .await
        .context("publish device-forget operation")?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn update_shared_setting(
    setting: SharedSettingKind,
    value: u64,
    history: &mut HistoryStore,
    clipboard: &WaylandBackend,
    transfers: &mut TransferCoordinator,
    mesh: &MeshHandle,
    config_path: &std::path::Path,
    config: &mut Config,
) -> anyhow::Result<()> {
    anyhow::ensure!(value > 0, "shared setting value must be greater than zero");
    if setting == SharedSettingKind::CaptureThresholdBytes {
        anyhow::ensure!(
            value <= config.local.maximum_explicit_share_bytes,
            "capture threshold exceeds the local explicit-share hard limit"
        );
    }

    let now = unix_time_millis()?;
    let operations = match setting {
        SharedSettingKind::MeshQuotaBytes => history
            .set_mesh_quota_and_enforce(value, now)
            .context("persist mesh quota and deterministic evictions")?,
        SharedSettingKind::CaptureThresholdBytes => vec![
            history
                .set_shared_setting(SharedSetting::CaptureThresholdBytes, value, now)
                .context("persist shared capture threshold")?,
        ],
        SharedSettingKind::Unspecified => anyhow::bail!("shared setting is missing"),
    };
    for operation in &operations {
        mesh.record_local(operation)
            .await
            .context("publish shared setting operation")?;
    }

    let effective = history.projection().effective_shared_settings();
    clipboard
        .set_capture_threshold(effective.capture_threshold_bytes)
        .context("apply shared capture threshold")?;
    transfers
        .update_policy(ExplicitSharePolicy {
            automatic_capture_threshold_bytes: effective.capture_threshold_bytes,
            mesh_quota_bytes: effective.mesh_quota_bytes,
            maximum_explicit_share_bytes: config.local.maximum_explicit_share_bytes,
            free_space_reserve_bytes: config.local.transfer_free_space_reserve_bytes,
        })
        .context("apply shared transfer policy")?;
    let rewritten = Config::rewrite_shared(
        config_path,
        effective,
        history.projection().shared_settings_revision(),
    )
    .context("atomically mirror shared settings to config")?;
    config.shared = rewritten.shared;
    Ok(())
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
                ClipboardContent::new_with_max(representations, u64::MAX)
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

fn image_preview(
    encoded_content_id: &str,
    history: &HistoryStore,
    transfers: &TransferCoordinator,
) -> anyhow::Result<ImagePreviewResponse> {
    let content_id = encoded_content_id
        .parse()
        .context("content ID is invalid")?;
    if !history.projection().is_visible(content_id) {
        anyhow::bail!("history item is deleted");
    }

    let (mime_type, bytes) = if let Some(payload) = history.projection().payload(content_id) {
        let representation = payload
            .representations()
            .iter()
            .find(|representation| image_format_for_mime(representation.mime()).is_some())
            .context("history item has no supported raster image")?;
        let source_size = u64::try_from(representation.bytes().len())
            .context("image preview source size does not fit in u64")?;
        if source_size > MAX_IMAGE_PREVIEW_SOURCE_BYTES {
            anyhow::bail!("image is too large to preview safely");
        }
        (
            representation.mime().to_owned(),
            representation.bytes().to_vec(),
        )
    } else {
        let (_, _, manifest) = history
            .projection()
            .completed_manifest_for_content(content_id)
            .context("image payload is not available locally")?;
        let crate::payload::StoredManifest::MimeBundle(bundle) = manifest else {
            anyhow::bail!("history item has no supported raster image");
        };
        let representation = bundle
            .representations()
            .iter()
            .find(|representation| image_format_for_mime(representation.mime()).is_some())
            .context("history item has no supported raster image")?;
        if representation.blob().logical_size() > MAX_IMAGE_PREVIEW_SOURCE_BYTES {
            anyhow::bail!("image is too large to preview safely");
        }
        let capacity = usize::try_from(representation.blob().logical_size())
            .context("image preview source size does not fit in memory")?;
        let mut bytes = Vec::with_capacity(capacity);
        transfers
            .store()
            .read_blob(representation.blob(), &mut bytes, &CancellationToken::new())
            .context("read encrypted image preview source")?;
        (representation.mime().to_owned(), bytes)
    };

    decode_image_preview(encoded_content_id, mime_type, bytes)
}

fn decode_image_preview(
    content_id: &str,
    mime_type: String,
    bytes: Vec<u8>,
) -> anyhow::Result<ImagePreviewResponse> {
    let format = image_format_for_mime(&mime_type).context("unsupported raster image type")?;
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_PREVIEW_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_PREVIEW_DIMENSION);
    limits.max_alloc = Some(MAX_IMAGE_PREVIEW_DECODE_BYTES);
    reader.limits(limits);
    let image = reader.decode().context("decode clipboard image preview")?;
    let thumbnail = image
        .thumbnail(IMAGE_PREVIEW_WIDTH, IMAGE_PREVIEW_HEIGHT)
        .to_rgba8();
    let (width, height) = thumbnail.dimensions();
    Ok(ImagePreviewResponse {
        content_id: content_id.to_owned(),
        mime_type,
        width,
        height,
        rgba: thumbnail.into_raw(),
    })
}

fn image_format_for_mime(mime: &str) -> Option<image::ImageFormat> {
    match mime
        .split(';')
        .next()
        .unwrap_or(mime)
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "image/png" => Some(image::ImageFormat::Png),
        "image/jpeg" | "image/jpg" => Some(image::ImageFormat::Jpeg),
        "image/gif" => Some(image::ImageFormat::Gif),
        "image/webp" => Some(image::ImageFormat::WebP),
        "image/bmp" | "image/x-ms-bmp" => Some(image::ImageFormat::Bmp),
        "image/tiff" => Some(image::ImageFormat::Tiff),
        _ => None,
    }
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

fn device_items(history: &HistoryStore) -> Vec<DeviceItem> {
    let local = history.replica().node_id();
    let mut members = history
        .projection()
        .known_members()
        .chain(history.projection().forgotten_devices())
        .collect::<std::collections::BTreeSet<_>>();
    if let Ok(acknowledgements) = history.acknowledgements() {
        members.extend(acknowledgements.known_members());
    }
    members.insert(local);
    members
        .into_iter()
        .map(|node_id| DeviceItem {
            device_id: node_id.to_string(),
            local: node_id == local,
            forgotten: history.projection().is_device_forgotten(node_id),
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
    transfers: &mut TransferCoordinator,
) -> anyhow::Result<()> {
    match event {
        ClipboardEvent::Ready => {
            state
                .set_clipboard_status(true, "Wayland clipboard monitoring is active")
                .await;
        }
        ClipboardEvent::Captured { content, .. } => {
            let result = capture_automatic_clipboard(
                &content,
                content_key,
                transfers,
                history,
                mesh,
                unix_time_millis()?,
                &CancellationToken::new(),
            )
            .await?;
            state.set_history(history_items(history.replica())).await;
            match result {
                AutomaticClipboardCaptureResult::Payload { .. } => {
                    tracing::debug!(
                        history_entries = history.projection().visible_items().len(),
                        "captured clipboard history entry"
                    );
                }
                AutomaticClipboardCaptureResult::Files { transfer_id, .. } => {
                    tracing::debug!(
                        %transfer_id,
                        history_entries = history.projection().visible_items().len(),
                        "captured clipboard file snapshot"
                    );
                }
                AutomaticClipboardCaptureResult::RejectedFiles => {
                    tracing::debug!("clipboard file offer was not captured");
                }
            }
        }
        ClipboardEvent::CaptureRejected { reason, .. } => {
            tracing::debug!(?reason, "clipboard offer was not captured");
        }
        ClipboardEvent::Finished => {
            state
                .set_clipboard_status(false, "Wayland clipboard device was removed; reconnecting")
                .await;
            tracing::warn!("Wayland compositor finished the clipboard data-control device");
        }
        ClipboardEvent::NewOffer { .. }
        | ClipboardEvent::OwnContent { .. }
        | ClipboardEvent::Cleared { .. } => {}
    }
    Ok(())
}

fn spawn_clipboard_watch<B>(
    clipboard: B,
    state: DaemonState,
    events: tokio::sync::mpsc::Sender<ClipboardEvent>,
    shutdown: CancellationToken,
) -> JoinHandle<()>
where
    B: ClipboardBackend + Clone + 'static,
{
    tokio::spawn(async move {
        let mut retry_delay = Duration::from_secs(1);
        loop {
            state
                .set_clipboard_status(false, "connecting to the Wayland clipboard")
                .await;
            let callback_events = events.clone();
            let ready = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let callback_ready = ready.clone();
            let result = clipboard
                .watch(
                    shutdown.clone(),
                    Box::new(move |event| {
                        if matches!(event, ClipboardEvent::Ready) {
                            callback_ready.store(true, std::sync::atomic::Ordering::SeqCst);
                        }
                        // Keep compositor callbacks non-blocking: this callback
                        // runs inside the dedicated current-thread Tokio runtime,
                        // where `blocking_send` would panic. A bounded queue
                        // prevents suspend/resume storms from growing memory.
                        if callback_events.try_send(event).is_err() {
                            tracing::warn!("dropping Wayland clipboard event because the daemon queue is full or closed");
                        }
                    }),
                )
                .await;
            if shutdown.is_cancelled() {
                break;
            }
            let detail = match result {
                Ok(()) => "Wayland clipboard device stopped; retrying".to_owned(),
                Err(error) => error.to_string(),
            };
            state.set_clipboard_status(false, detail.clone()).await;
            tracing::warn!(error = %detail, "Wayland clipboard monitoring is unavailable");
            if ready.load(std::sync::atomic::Ordering::SeqCst) {
                retry_delay = Duration::from_secs(1);
            } else {
                retry_delay = retry_delay.saturating_mul(2).min(Duration::from_secs(30));
            }
            tokio::select! {
                () = shutdown.cancelled() => break,
                () = tokio::time::sleep(retry_delay) => {}
            }
        }
    })
}

struct MeshPersistenceContext<'a> {
    history: &'a mut HistoryStore,
    state: &'a DaemonState,
    content_key: &'a [u8; 32],
    clipboard: &'a WaylandBackend,
    mesh: &'a MeshHandle,
    config_path: &'a std::path::Path,
    config: &'a mut Config,
    transfers: &'a mut TransferCoordinator,
}

async fn handle_mesh_batch(batch: PersistBatch, context: &mut MeshPersistenceContext<'_>) {
    let result = persist_mesh_batch(&batch, context).await;
    if result.is_ok() {
        context
            .state
            .set_history(history_items(context.history.replica()))
            .await;
        context
            .state
            .set_devices(device_items(context.history))
            .await;
        context.state.set_config(context.config.clone()).await;
        context.mesh.notify_transfers();
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

#[allow(
    clippy::too_many_lines,
    reason = "remote persistence, policy application, and config mirroring form one transaction boundary"
)]
async fn persist_mesh_batch(
    batch: &PersistBatch,
    context: &mut MeshPersistenceContext<'_>,
) -> anyhow::Result<PersistResult> {
    let codec = JsonV1Codec;
    let operations = batch
        .operations()
        .iter()
        .map(|raw| codec.decode_op(raw).context("decode remote operation"))
        .collect::<anyhow::Result<Vec<StampedOperation>>>()?;
    for operation in &operations {
        if let Operation::Add { payload, .. } | Operation::AddQuotaExempt { payload, .. } =
            operation.operation()
        {
            payload
                .validate(context.content_key)
                .context("validate remote clipboard payload identity")?;
        }
        if let Operation::BeginShare {
            manifest_id,
            manifest,
            ..
        } = operation.operation()
        {
            context
                .transfers
                .validate_manifest(*manifest_id, manifest)
                .context("validate remote transfer manifest")?;
        }
    }

    let before = context.history.projection().effective_shared_settings();
    context
        .history
        .ingest_authenticated_batch(
            batch.peer(),
            batch.peer_frontier(),
            batch.known_members(),
            &operations,
            unix_time_millis()?,
        )
        .context("persist authenticated remote operation batch and frontier")?;
    context
        .transfers
        .reconcile_projection(context.history.projection())
        .context("reconcile received transfer state")?;
    let after = context.history.projection().effective_shared_settings();

    if before != after {
        if let Err(error) = context
            .clipboard
            .set_capture_threshold(after.capture_threshold_bytes)
        {
            tracing::warn!(
                %error,
                "durable shared setting could not be applied to clipboard capture"
            );
        }

        if before.mesh_quota_bytes != after.mesh_quota_bytes {
            match unix_time_millis()
                .context("read wall clock for quota enforcement")
                .and_then(|now| {
                    context
                        .history
                        .enforce_quota(now)
                        .context("persist deterministic quota evictions")
                }) {
                Ok(evictions) => {
                    for operation in &evictions {
                        if let Err(error) = context.mesh.record_local(operation).await {
                            tracing::warn!(
                                %error,
                                operation_id = %operation.id(),
                                "durable quota eviction could not be queued for replication"
                            );
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "quota enforcement deferred until all visible payloads are available"
                    );
                }
            }
        }
    }
    let revision = context.history.projection().shared_settings_revision();
    if !context.config.shared.matches(after, &revision) {
        match Config::rewrite_shared(context.config_path, after, revision) {
            Ok(config) => context.config.shared = config.shared,
            Err(error) => {
                context.config.shared = SharedConfig {
                    mesh_quota_bytes: after.mesh_quota_bytes,
                    capture_threshold_bytes: after.capture_threshold_bytes,
                    revision: context.history.projection().shared_settings_revision(),
                };
                tracing::warn!(
                    %error,
                    "durable replicated settings could not be mirrored to config"
                );
            }
        }
    }
    if let Err(error) = context.transfers.update_policy(ExplicitSharePolicy {
        automatic_capture_threshold_bytes: after.capture_threshold_bytes,
        mesh_quota_bytes: after.mesh_quota_bytes,
        maximum_explicit_share_bytes: context.config.local.maximum_explicit_share_bytes,
        free_space_reserve_bytes: context.config.local.transfer_free_space_reserve_bytes,
    }) {
        tracing::warn!(
            %error,
            "durable shared settings could not be applied to explicit-share policy"
        );
    }

    let compacted = match context.history.compact_acknowledged_tombstones() {
        Ok(compacted) => compacted,
        Err(error) => {
            tracing::warn!(
                %error,
                "acknowledged tombstone compaction will be retried later"
            );
            CompactionReport::default()
        }
    };
    Ok(PersistResult::new(compacted.operations().to_vec()))
}

fn initialize_shared_settings(
    history: &mut HistoryStore,
    config_path: &std::path::Path,
    config: &mut Config,
) -> anyhow::Result<()> {
    let projection = history.projection();
    let effective = projection.effective_shared_settings();
    let revision = projection.shared_settings_revision();
    let has_replicated_settings = projection
        .setting_event(SharedSetting::MeshQuotaBytes.key())
        .is_some()
        || projection
            .setting_event(SharedSetting::CaptureThresholdBytes.key())
            .is_some();
    let external_edit = if has_replicated_settings {
        !config.shared.revision.is_empty() && !config.shared.matches(effective, &revision)
    } else {
        config.shared.mesh_quota_bytes != effective.mesh_quota_bytes
            || config.shared.capture_threshold_bytes != effective.capture_threshold_bytes
    };

    if external_edit {
        let now = unix_time_millis()?;
        if config.shared.mesh_quota_bytes != effective.mesh_quota_bytes {
            history
                .set_mesh_quota_and_enforce(config.shared.mesh_quota_bytes, now)
                .context("apply configured shared mesh quota")?;
        }
        let current = history.projection().effective_shared_settings();
        if config.shared.capture_threshold_bytes != current.capture_threshold_bytes {
            history
                .set_shared_setting(
                    SharedSetting::CaptureThresholdBytes,
                    config.shared.capture_threshold_bytes,
                    now,
                )
                .context("apply configured shared capture threshold")?;
        }
    }

    let effective = history.projection().effective_shared_settings();
    let revision = history.projection().shared_settings_revision();
    if !config.shared.matches(effective, &revision) {
        config.shared = SharedConfig {
            mesh_quota_bytes: effective.mesh_quota_bytes,
            capture_threshold_bytes: effective.capture_threshold_bytes,
            revision,
        };
        config
            .save(config_path)
            .context("atomically save effective shared settings")?;
    }
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "config reload updates every daemon-owned policy surface atomically"
)]
async fn apply_config_reload(
    mut changed: Config,
    config_path: &std::path::Path,
    current: &mut Config,
    history: &mut HistoryStore,
    clipboard: &WaylandBackend,
    mesh: &MeshHandle,
    state: &DaemonState,
    transfers: &mut TransferCoordinator,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !restart_required_local_change(&current.local, &changed.local),
        "changed local bootstrap settings require a daemon restart"
    );
    let before = history.projection().effective_shared_settings();
    let current_revision = history.projection().shared_settings_revision();
    if changed.shared.matches(before, &current_revision) {
        transfers
            .update_policy(ExplicitSharePolicy {
                automatic_capture_threshold_bytes: before.capture_threshold_bytes,
                mesh_quota_bytes: before.mesh_quota_bytes,
                maximum_explicit_share_bytes: changed.local.maximum_explicit_share_bytes,
                free_space_reserve_bytes: changed.local.transfer_free_space_reserve_bytes,
            })
            .context("apply reloaded explicit-share policy")?;
        *current = changed;
        state.set_config(current.clone()).await;
        return Ok(());
    }

    let now = unix_time_millis()?;
    let mut authored = Vec::new();
    if changed.shared.mesh_quota_bytes != before.mesh_quota_bytes {
        authored.extend(
            history
                .set_mesh_quota_and_enforce(changed.shared.mesh_quota_bytes, now)
                .context("apply reloaded shared mesh quota")?,
        );
    }
    let effective = history.projection().effective_shared_settings();
    if changed.shared.capture_threshold_bytes != effective.capture_threshold_bytes {
        authored.push(
            history
                .set_shared_setting(
                    SharedSetting::CaptureThresholdBytes,
                    changed.shared.capture_threshold_bytes,
                    now,
                )
                .context("apply reloaded shared capture threshold")?,
        );
    }
    for operation in &authored {
        mesh.record_local(operation)
            .await
            .context("publish config-authored shared setting")?;
    }

    let effective = history.projection().effective_shared_settings();
    clipboard
        .set_capture_threshold(effective.capture_threshold_bytes)
        .context("apply config-authored capture threshold")?;
    transfers
        .update_policy(ExplicitSharePolicy {
            automatic_capture_threshold_bytes: effective.capture_threshold_bytes,
            mesh_quota_bytes: effective.mesh_quota_bytes,
            maximum_explicit_share_bytes: changed.local.maximum_explicit_share_bytes,
            free_space_reserve_bytes: changed.local.transfer_free_space_reserve_bytes,
        })
        .context("apply config-authored explicit-share policy")?;
    changed.shared = SharedConfig {
        mesh_quota_bytes: effective.mesh_quota_bytes,
        capture_threshold_bytes: effective.capture_threshold_bytes,
        revision: history.projection().shared_settings_revision(),
    };
    changed
        .save(config_path)
        .context("atomically save config-authored shared settings")?;
    *current = changed;
    state.set_config(current.clone()).await;
    state.set_history(history_items(history.replica())).await;
    Ok(())
}

fn restart_required_local_change(
    current: &crate::config::LocalConfig,
    changed: &crate::config::LocalConfig,
) -> bool {
    current.mesh_key_file != changed.mesh_key_file
        || current.listen_port != changed.listen_port
        || current.discovery_interval_seconds != changed.discovery_interval_seconds
        || current.reconcile_interval_seconds != changed.reconcile_interval_seconds
        || current.reconnect_min_seconds != changed.reconnect_min_seconds
        || current.reconnect_max_seconds != changed.reconnect_max_seconds
        || current.netbird_command != changed.netbird_command
        || current.materialization_free_space_reserve_bytes
            != changed.materialization_free_space_reserve_bytes
        || current.max_concurrent_chunk_streams != changed.max_concurrent_chunk_streams
}

fn spawn_config_watch(
    path: std::path::PathBuf,
    initial: Config,
    updates: tokio::sync::mpsc::UnboundedSender<Config>,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut observed = initial;
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                () = tokio::time::sleep(Duration::from_millis(500)) => {}
            }
            match Config::load(&path) {
                Ok(config) if config != observed => {
                    let reload = config_change_requires_reload(&observed, &config);
                    observed = config.clone();
                    if reload && updates.send(config).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::debug!(%error, "waiting for config to become valid");
                }
            }
        }
    })
}

fn config_change_requires_reload(observed: &Config, changed: &Config) -> bool {
    if observed.local != changed.local {
        return true;
    }
    if observed.shared == changed.shared {
        return false;
    }

    // Daemon-authored shared-setting mirrors always advance the replicated
    // register fingerprint. A human edit changes values while retaining (or
    // clearing) the last fingerprint, so it still becomes a mesh operation.
    changed.shared.revision.is_empty() || changed.shared.revision == observed.shared.revision
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

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use std::{
        collections::BTreeSet,
        net::UdpSocket,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use async_trait::async_trait;

    use super::*;
    use crate::{
        clipboard::{
            backend::BackendError,
            types::{FeedbackMarker, Generation, OfferMimeList, ProbeResult},
        },
        mesh::MeshRuntimeConfig,
        payload::{ChunkStoreKey, MaterializerConfig},
        storage::StorageKey,
        transport::Psk,
    };

    const STORAGE_KEY: [u8; 32] = [0x31; 32];
    const CHUNK_KEY: [u8; 32] = [0x42; 32];
    const CONTENT_KEY: [u8; 32] = [0x53; 32];
    const PSK: [u8; 32] = [0x64; 32];

    #[derive(Clone)]
    struct TestClipboard {
        inspection: CurrentClipboardInspection,
        content: ClipboardContent,
    }

    #[async_trait]
    impl ClipboardBackend for TestClipboard {
        async fn probe(&self) -> Result<ProbeResult, BackendError> {
            Err(BackendError::NoDisplay)
        }

        async fn watch(
            &self,
            _shutdown: CancellationToken,
            _on_event: Box<dyn Fn(ClipboardEvent) + Send + Sync>,
        ) -> Result<(), BackendError> {
            Err(BackendError::NoDisplay)
        }

        async fn set_clipboard_content(
            &self,
            _content: ClipboardContent,
        ) -> Result<FeedbackMarker, BackendError> {
            Err(BackendError::WatchNotRunning)
        }

        async fn inspect_current_clipboard(
            &self,
            _maximum_bytes: u64,
        ) -> Result<CurrentClipboardInspection, BackendError> {
            Ok(self.inspection.clone())
        }

        async fn capture_current_clipboard(
            &self,
            inspection: &CurrentClipboardInspection,
        ) -> Result<ClipboardContent, BackendError> {
            if inspection.generation() != self.inspection.generation() {
                return Err(BackendError::CurrentOfferChanged);
            }
            Ok(self.content.clone())
        }
    }

    #[derive(Clone, Default)]
    struct RecoveringClipboard {
        watches: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ClipboardBackend for RecoveringClipboard {
        async fn probe(&self) -> Result<ProbeResult, BackendError> {
            Err(BackendError::NoDisplay)
        }

        async fn watch(
            &self,
            shutdown: CancellationToken,
            on_event: Box<dyn Fn(ClipboardEvent) + Send + Sync>,
        ) -> Result<(), BackendError> {
            let attempt = self.watches.fetch_add(1, Ordering::SeqCst);
            std::thread::spawn(move || on_event(ClipboardEvent::Ready))
                .join()
                .expect("scripted clipboard callback");
            if attempt == 0 {
                return Err(BackendError::Connection("injected disconnect".to_owned()));
            }
            shutdown.cancelled().await;
            Ok(())
        }

        async fn set_clipboard_content(
            &self,
            _content: ClipboardContent,
        ) -> Result<FeedbackMarker, BackendError> {
            Err(BackendError::WatchNotRunning)
        }

        async fn inspect_current_clipboard(
            &self,
            _maximum_bytes: u64,
        ) -> Result<CurrentClipboardInspection, BackendError> {
            Err(BackendError::WatchNotRunning)
        }

        async fn capture_current_clipboard(
            &self,
            _inspection: &CurrentClipboardInspection,
        ) -> Result<ClipboardContent, BackendError> {
            Err(BackendError::WatchNotRunning)
        }
    }

    fn clipboard(bytes: &[u8]) -> TestClipboard {
        let mime = MimeType::new("text/plain").unwrap();
        let content = ClipboardContent::new_with_max(
            vec![ClipboardRepresentation::new(mime.clone(), bytes.to_vec())],
            u64::MAX,
        )
        .unwrap();
        let inspection = CurrentClipboardInspection::new(
            Generation::from_value(7),
            OfferMimeList::new(vec![mime]).unwrap(),
            u64::try_from(bytes.len()).unwrap(),
        );
        TestClipboard {
            inspection,
            content,
        }
    }

    #[tokio::test]
    async fn clipboard_supervisor_reconnects_after_injected_disconnect() {
        let temporary = tempfile::tempdir().unwrap();
        let (commands, _command_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = DaemonState::new(
            "test".to_owned(),
            temporary.path().join("config.toml"),
            Config::default(),
            commands,
        );
        let backend = RecoveringClipboard::default();
        let watches = backend.watches.clone();
        let shutdown = CancellationToken::new();
        let (events, mut received) = tokio::sync::mpsc::channel(4);
        let task = spawn_clipboard_watch(backend, state, events, shutdown.clone());

        tokio::time::timeout(Duration::from_secs(3), async {
            while watches.load(Ordering::SeqCst) < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("clipboard watcher did not reconnect");
        assert!(matches!(received.recv().await, Some(ClipboardEvent::Ready)));
        assert!(matches!(received.recv().await, Some(ClipboardEvent::Ready)));

        shutdown.cancel();
        drop(received);
        task.await.expect("clipboard supervisor");
    }

    #[test]
    fn config_watcher_suppresses_daemon_revisions_but_not_external_edits() {
        let mut observed = Config::default();
        observed.shared.revision = "a1".to_owned();

        for revision in 2..10_000 {
            let mut daemon_write = observed.clone();
            daemon_write.shared.mesh_quota_bytes += 1;
            daemon_write.shared.revision = format!("{revision:x}");
            assert!(!config_change_requires_reload(&observed, &daemon_write));
            observed = daemon_write;
        }

        let mut external_shared = observed.clone();
        external_shared.shared.mesh_quota_bytes += 1;
        assert!(config_change_requires_reload(&observed, &external_shared));

        let mut external_local = observed.clone();
        external_local.local.listen_port += 1;
        external_local.shared.revision = "ffff".to_owned();
        assert!(config_change_requires_reload(&observed, &external_local));
        assert!(restart_required_local_change(
            &observed.local,
            &external_local.local
        ));

        let mut live_policy = observed.local.clone();
        live_policy.maximum_explicit_share_bytes += 1;
        live_policy.transfer_free_space_reserve_bytes += 1;
        assert!(!restart_required_local_change(
            &observed.local,
            &live_policy
        ));
    }

    fn open_state(
        root: &std::path::Path,
        threshold: u64,
        quota: u64,
    ) -> (HistoryStore, TransferCoordinator) {
        let history = HistoryStore::open(
            root.join("history.db"),
            &StorageKey::from_bytes(STORAGE_KEY),
        )
        .unwrap();
        let store = ChunkStore::open(
            root.join("chunks"),
            &ChunkStoreKey::from_bytes(CHUNK_KEY),
            ChunkStoreConfig {
                chunk_bytes: 64 * 1024,
                max_payload_bytes: 1024 * 1024,
                max_chunks_per_manifest: 1024,
            },
        )
        .unwrap();
        let materializer =
            Materializer::new(root.join("materialized"), MaterializerConfig::default()).unwrap();
        let transfers = TransferCoordinator::new(
            store,
            materializer,
            ExplicitSharePolicy {
                automatic_capture_threshold_bytes: threshold,
                mesh_quota_bytes: quota,
                maximum_explicit_share_bytes: 1024 * 1024,
                free_space_reserve_bytes: 0,
            },
            TransferStateLimits::default(),
        );
        (history, transfers)
    }

    fn spawn_mesh(node_id: NodeId) -> (MeshRuntime, MeshHandle, CancellationToken) {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = socket.local_addr().unwrap().port();
        drop(socket);
        let shutdown = CancellationToken::new();
        let config = MeshRuntimeConfig::new(node_id, "test-node".to_owned(), port);
        let (runtime, _persist, _chunks) = MeshRuntime::spawn_with_transfers(
            config,
            Psk::new(&PSK).unwrap(),
            &[],
            shutdown.clone(),
        )
        .unwrap();
        let mesh = runtime.handle();
        (runtime, mesh, shutdown)
    }

    #[tokio::test]
    async fn share_command_inspects_then_requires_confirmation_and_returns_ids() {
        let temporary = tempfile::tempdir().unwrap();
        let (mut history, mut transfers) = open_state(temporary.path(), 4, 1024);
        let (runtime, mesh, shutdown) = spawn_mesh(history.replica().node_id());
        let clipboard = clipboard(b"explicit payload");

        let inspection = share_clipboard_command(
            &clipboard,
            false,
            &CONTENT_KEY,
            &mut transfers,
            &mut history,
            &mesh,
            1024 * 1024,
        )
        .await
        .unwrap();
        assert!(!inspection.shared);
        assert!(inspection.confirmation_required);
        assert!(transfers.progress().is_empty());

        let shared = share_clipboard_command(
            &clipboard,
            true,
            &CONTENT_KEY,
            &mut transfers,
            &mut history,
            &mesh,
            1024 * 1024,
        )
        .await
        .unwrap();
        assert!(shared.shared);
        assert!(shared.transfer_id.is_some());
        assert!(shared.content_id.is_some());
        assert_eq!(transfers.progress().len(), 1);
        assert_eq!(history.projection().transfers().len(), 1);

        shutdown.cancel();
        runtime.wait().await;
    }

    #[tokio::test]
    async fn transfer_cancel_mutates_history_and_coordinator() {
        let temporary = tempfile::tempdir().unwrap();
        let (mut history, mut transfers) = open_state(temporary.path(), 1024, 4096);
        let (runtime, mesh, shutdown) = spawn_mesh(history.replica().node_id());
        let payload = Payload::new(
            &CONTENT_KEY,
            vec![Representation::new("text/plain", b"pending".to_vec())],
        )
        .unwrap();
        let available = fs2::available_space(transfers.store().root()).unwrap();
        let inspection = transfers.inspect_payload(&payload, available).unwrap();
        let (transfer_id, begin) = transfers
            .begin_payload_share(
                &payload,
                inspection,
                false,
                &mut history,
                10,
                &CancellationToken::new(),
            )
            .unwrap();
        mesh.record_local(&begin).await.unwrap();
        let listed = transfer_items(&history, &transfers);
        assert_eq!(listed[0].transfer_id, transfer_id.to_string());
        assert_eq!(
            listed[0].content_id,
            payload.descriptor().content_id().to_string()
        );
        assert_eq!(listed[0].peer, history.replica().node_id().to_string());
        assert_eq!(listed[0].total_bytes, 7);

        cancel_transfer(transfer_id, &mut transfers, &mut history, &mesh, 11)
            .await
            .unwrap();
        assert_eq!(
            history.projection().transfer(transfer_id).unwrap().phase(),
            crate::transfer::TransferPhase::Cancelled
        );
        assert_eq!(
            transfers.progress()[0].phase,
            crate::transfer::TransferPhase::Cancelled
        );

        shutdown.cancel();
        runtime.wait().await;
    }

    #[tokio::test]
    async fn forget_device_persists_and_publishes_known_member_rejection() {
        let temporary = tempfile::tempdir().unwrap();
        let (mut history, _transfers) = open_state(temporary.path(), 1024, 4096);
        let remote = NodeId::new();
        history
            .ingest_authenticated_batch(
                remote,
                &crate::model::SeenOps::default(),
                &BTreeSet::from([remote]),
                &[],
                1,
            )
            .unwrap();
        let (runtime, mesh, shutdown) = spawn_mesh(history.replica().node_id());

        forget_device(&remote.to_string(), &mut history, &mesh)
            .await
            .unwrap();
        assert!(history.projection().is_device_forgotten(remote));
        assert!(
            device_items(&history)
                .iter()
                .any(|device| device.device_id == remote.to_string() && device.forgotten)
        );

        shutdown.cancel();
        runtime.wait().await;
    }

    #[test]
    fn raster_image_preview_is_bounded_rgba() {
        let source = image::RgbaImage::from_pixel(640, 360, image::Rgba([20, 80, 140, 255]));
        let mut encoded = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(source)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();

        let preview =
            decode_image_preview("content-id", "image/png".to_owned(), encoded.into_inner())
                .unwrap();

        assert_eq!(preview.content_id, "content-id");
        assert_eq!(preview.mime_type, "image/png");
        assert_eq!((preview.width, preview.height), (320, 180));
        assert_eq!(preview.rgba.len(), 320 * 180 * 4);
    }

    #[test]
    fn vector_images_are_not_offered_as_raster_previews() {
        assert_eq!(image_format_for_mime("image/svg+xml"), None);
        assert_eq!(
            image_format_for_mime("image/jpeg; charset=binary"),
            Some(image::ImageFormat::Jpeg)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn setting_update_replicates_enforces_quota_and_preserves_config_symlink() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let (mut history, mut transfers) = open_state(temporary.path(), 1024, 4096);
        let payload = Payload::new(
            &CONTENT_KEY,
            vec![Representation::new("text/plain", vec![7; 64])],
        )
        .unwrap();
        history.copy_and_enforce(payload, 1).unwrap();
        let target = temporary.path().join("managed.toml");
        let link = temporary.path().join("config.toml");
        let mut config = Config::default();
        config.local.maximum_explicit_share_bytes = 32 * 1024 * 1024;
        config.local.transfer_free_space_reserve_bytes = 0;
        config.save(&target).unwrap();
        symlink("managed.toml", &link).unwrap();
        let clipboard = WaylandBackend::new();
        let (runtime, mesh, shutdown) = spawn_mesh(history.replica().node_id());

        update_shared_setting(
            SharedSettingKind::MeshQuotaBytes,
            1,
            &mut history,
            &clipboard,
            &mut transfers,
            &mesh,
            &link,
            &mut config,
        )
        .await
        .unwrap();
        assert_eq!(
            history
                .projection()
                .effective_shared_settings()
                .mesh_quota_bytes,
            1
        );
        assert!(history.projection().visible_items().is_empty());
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(Config::load(&target).unwrap().shared.mesh_quota_bytes, 1);

        update_shared_setting(
            SharedSettingKind::CaptureThresholdBytes,
            512,
            &mut history,
            &clipboard,
            &mut transfers,
            &mesh,
            &link,
            &mut config,
        )
        .await
        .unwrap();
        assert_eq!(config.shared.capture_threshold_bytes, 512);

        shutdown.cancel();
        runtime.wait().await;
    }
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
