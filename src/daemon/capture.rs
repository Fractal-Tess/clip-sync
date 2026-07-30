use anyhow::Context;
use tokio_util::sync::CancellationToken;

use crate::{
    clipboard::{
        backend::ClipboardBackend,
        types::{ClipboardContent, CurrentClipboardInspection},
    },
    mesh::MeshHandle,
    model::{Payload, Representation},
    payload::{ExplicitShareInspection, FileSnapshotLimits, parse_file_uri_list},
    storage::HistoryStore,
    transfer::{TransferCoordinator, TransferId},
};

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
