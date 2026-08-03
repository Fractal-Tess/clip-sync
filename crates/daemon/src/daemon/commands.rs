use anyhow::Context;
use tokio_util::sync::CancellationToken;

use clip_sync_core::{
    clipboard::{backend::ClipboardBackend, wayland::WaylandBackend},
    config::Config,
    model::NodeId,
    storage::HistoryStore,
    transfer::{TransferCoordinator, TransferId},
};
use clip_sync_ipc::protocol::{ShareClipboardResponse, TransferItem};

use crate::{
    ipc::{DaemonCommand, DaemonState},
    mesh::MeshHandle,
};

use super::{
    activation::{activate_history_item, schedule_materialization_cleanup},
    capture::{inspect_live_current_clipboard, share_live_current_clipboard},
    config_supervision::update_shared_setting,
    preview::image_preview,
    runtime::unix_time_millis,
    views::{device_items, history_items},
};

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
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the single-owner daemon dispatcher keeps command replies next to their mutations"
)]
pub(super) async fn handle_daemon_command(
    command: DaemonCommand,
    clipboard: &WaylandBackend,
    history: &mut HistoryStore,
    state: &DaemonState,
    mesh: &MeshHandle,
    transfers: &mut TransferCoordinator,
    content_key: &[u8; 32],
    config_path: &std::path::Path,
    config: &mut Config,
    active_materialization: &mut Option<clip_sync_core::payload::ManifestId>,
    pending_cleanup: &mut Option<(clip_sync_core::payload::ManifestId, CancellationToken)>,
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
                .map_err(|error| format!("{error:#}"));
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
        DaemonCommand::UpdatePeerInterfaces { interfaces, reply } => {
            let result = (|| {
                let mut changed = config.clone();
                changed.local.peer_interfaces = interfaces;
                changed
                    .save(config_path)
                    .context("save peer interface configuration")?;
                *config = changed;
                Ok(())
            })()
            .map_err(|error: anyhow::Error| error.to_string());
            if result.is_ok() {
                state.set_config(config.clone()).await;
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
pub(super) async fn share_clipboard_command(
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

pub(super) fn transfer_items(
    history: &HistoryStore,
    transfers: &TransferCoordinator,
) -> Vec<TransferItem> {
    let projection = history.projection();
    transfers
        .progress()
        .into_iter()
        .map(|progress| {
            let view = projection.transfer(progress.transfer_id);
            TransferItem {
                transfer_id: progress.transfer_id.to_string(),
                content_id: view
                    .and_then(clip_sync_core::model::TransferView::content_id)
                    .map_or_else(String::new, |content_id| content_id.to_string()),
                peer: view
                    .and_then(clip_sync_core::model::TransferView::source_node)
                    .map_or_else(String::new, |node_id| node_id.to_string()),
                state: format!("{:?}", progress.phase).to_lowercase(),
                completed_bytes: progress.verified_bytes,
                total_bytes: progress.logical_size,
            }
        })
        .collect()
}

pub(super) async fn forget_device(
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
