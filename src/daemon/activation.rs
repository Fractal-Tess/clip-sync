use std::time::Duration;

use anyhow::Context;
use tokio_util::sync::CancellationToken;

use crate::{
    clipboard::{
        backend::ClipboardBackend,
        types::{ClipboardContent, ClipboardRepresentation, MimeType},
        wayland::WaylandBackend,
    },
    ipc::DaemonState,
    mesh::MeshHandle,
    payload::{Materializer, MaterializerConfig},
    storage::HistoryStore,
    transfer::TransferCoordinator,
};

use super::{runtime::unix_time_millis, views::history_items};

#[allow(clippy::too_many_arguments)]
pub(super) async fn activate_history_item(
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
    let clipboard_content = image_focused_activation_content(clipboard_content)
        .context("prepare history item for clipboard activation")?;
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

pub(super) fn image_focused_activation_content(
    content: ClipboardContent,
) -> Result<ClipboardContent, crate::clipboard::types::ClipboardContentError> {
    if !content
        .representations()
        .iter()
        .any(|representation| clipboard_mime_is_image(representation.mime_type().as_str()))
    {
        return Ok(content);
    }

    let representations = content
        .representations()
        .iter()
        .filter(|representation| clipboard_mime_is_image(representation.mime_type().as_str()))
        .cloned()
        .collect();
    ClipboardContent::new_with_max(representations, u64::MAX)
}

fn clipboard_mime_is_image(mime: &str) -> bool {
    let essence = mime.split(';').next().unwrap_or(mime).trim();
    essence
        .get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("image/"))
}

pub(super) fn schedule_materialization_cleanup(
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
