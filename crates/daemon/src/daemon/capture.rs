use anyhow::Context;

use clip_sync_core::{
    clipboard::types::ClipboardContent,
    files,
    model::{ContentId, Payload, Reference, Representation, StampedOperation},
    storage::{HistoryStore, LocalSource},
};

use crate::mesh::MeshHandle;

/// Per-host capture limits from configuration.
#[derive(Clone, Copy, Debug)]
pub struct CaptureLimits {
    pub inline_limit_bytes: u64,
    pub history_quota_bytes: u64,
}

/// Records one captured clipboard offer and publishes it to peers.
///
/// Copied files are never read: they are described and stay where they are,
/// and a peer fetches them when it pastes. Other content up to the inline
/// limit replicates to every host; anything larger stays in this host's
/// local store and is fetched the same way.
///
/// # Errors
///
/// Returns payload, file-inspection, storage, or publication errors.
pub async fn capture_clipboard(
    content: &ClipboardContent,
    content_key: &[u8; 32],
    limits: CaptureLimits,
    history: &mut HistoryStore,
    mesh: &MeshHandle,
    now_millis: u64,
) -> anyhow::Result<ContentId> {
    if let Some(uri_list) = content.bytes_for_mime("text/uri-list") {
        return capture_files(&uri_list, content_key, history, mesh, now_millis).await;
    }

    let payload = payload_from_clipboard(content, content_key)?;
    let content_id = payload.descriptor().content_id();
    let operations = if payload.descriptor().logical_size() <= limits.inline_limit_bytes {
        history
            .copy_and_enforce(payload, limits.history_quota_bytes, now_millis)
            .context("persist clipboard history and quota operations")?
    } else if history.projection().is_visible(content_id) {
        vec![
            history
                .activate(content_id, now_millis)
                .context("persist repeated large clipboard copy")?,
        ]
    } else {
        let reference = Reference::Data(payload.descriptor().representations().to_vec());
        vec![
            history
                .add_reference(
                    content_id,
                    reference,
                    &LocalSource::Data(payload),
                    now_millis,
                )
                .context("persist large clipboard copy")?,
        ]
    };
    publish(mesh, &operations).await?;
    Ok(content_id)
}

async fn capture_files(
    uri_list: &[u8],
    content_key: &[u8; 32],
    history: &mut HistoryStore,
    mesh: &MeshHandle,
    now_millis: u64,
) -> anyhow::Result<ContentId> {
    let paths = files::parse_file_uri_list(uri_list).context("read copied file list")?;
    let (reference, sources) = tokio::task::spawn_blocking(move || files::describe_files(&paths))
        .await
        .context("inspect copied files")?
        .context("inspect copied files")?;
    let content_id =
        ContentId::from_file_reference(content_key, history.replica().node_id(), uri_list);
    // Always re-publish: a file edited in place keeps its path, and so its
    // content ID, but its size and contents may have changed.
    let operation = history
        .add_reference(
            content_id,
            reference,
            &LocalSource::Files(sources),
            now_millis,
        )
        .context("persist copied files")?;
    publish(mesh, std::slice::from_ref(&operation)).await?;
    Ok(content_id)
}

async fn publish(mesh: &MeshHandle, operations: &[StampedOperation]) -> anyhow::Result<()> {
    for operation in operations {
        mesh.record_local(operation)
            .await
            .context("publish clipboard history operation to mesh")?;
    }
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
    Payload::new(content_key, representations).context("build clipboard payload")
}
