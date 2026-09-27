use anyhow::Context;

use clip_sync_core::{
    model::{ItemKind, Operation},
    storage::HistoryStore,
};

use crate::{
    ipc::DaemonState,
    mesh::{BatchRequest, MeshHandle, MeshStoreRequest, PersistBatch, SourceRequest},
};

use super::{
    runtime::unix_time_millis,
    views::{device_items, history_items},
};

pub(super) struct MeshPersistenceContext<'a> {
    pub(super) history: &'a mut HistoryStore,
    pub(super) state: &'a DaemonState,
    pub(super) content_key: &'a [u8; 32],
    pub(super) mesh: &'a MeshHandle,
}

pub(super) async fn handle_mesh_store_request(
    request: MeshStoreRequest,
    context: &mut MeshPersistenceContext<'_>,
) {
    match request {
        MeshStoreRequest::Persist(batch) => handle_mesh_batch(batch, context).await,
        MeshStoreRequest::Batch(request) => answer_batch_request(request, context.history),
        MeshStoreRequest::Source(request) => answer_source_request(request, context.history),
    }
}

fn answer_batch_request(request: BatchRequest, history: &HistoryStore) {
    let result = history
        .operation_batch(request.remote(), request.limits())
        .map_err(|error| error.to_string());
    request.complete(result);
}

/// Serves only references this device authored, and only the exact version
/// the requesting peer holds.
fn answer_source_request(request: SourceRequest, history: &HistoryStore) {
    let content_id = request.content_id();
    let projection = history.projection();
    let current = projection.is_visible(content_id)
        && matches!(projection.item(content_id), Some(ItemKind::Reference(_)))
        && projection.item_operation(content_id) == Some(request.operation());
    let result = if current {
        match history.local_source(content_id) {
            Ok(Some(source)) => Ok(source),
            Ok(None) => Err("this device no longer has the copied item".to_owned()),
            Err(error) => Err(error.to_string()),
        }
    } else {
        Err("the item was deleted or copied again since this device last synced".to_owned())
    };
    request.complete(result);
}

async fn handle_mesh_batch(batch: PersistBatch, context: &mut MeshPersistenceContext<'_>) {
    let carried_operations = !batch.operations().is_empty();
    let result = persist_mesh_batch(&batch, context);
    if result.is_ok() {
        // Every handshake ends in a batch, usually an empty one. Names are
        // learned at the handshake, so they are refreshed here rather than
        // only when operations arrive, or a quiet peer would stay a raw ID
        // after a restart.
        let names = context
            .mesh
            .device_hostnames()
            .await
            .into_iter()
            .map(|(node_id, hostname)| (node_id.to_string(), hostname))
            .collect();
        let renamed = context.state.set_device_names(names).await;
        // An exchange that carried no operations and taught no names leaves
        // history exactly as it was, so there is nothing to republish.
        if carried_operations || renamed {
            context
                .state
                .set_history(history_items(context.history.replica()))
                .await;
        }
        if carried_operations {
            context
                .state
                .set_devices(device_items(context.history))
                .await;
        }
    }
    batch.complete(result.map_err(|error| format!("{error:#}")));
}

fn persist_mesh_batch(
    batch: &PersistBatch,
    context: &mut MeshPersistenceContext<'_>,
) -> anyhow::Result<()> {
    let operations = batch.operations();
    for operation in operations {
        if let Operation::Add { payload, .. } = operation.operation() {
            payload
                .validate(context.content_key)
                .context("validate remote clipboard payload identity")?;
        }
    }
    context
        .history
        .ingest_authenticated_batch(
            batch.peer(),
            batch.peer_frontier(),
            batch.known_members(),
            operations,
            unix_time_millis()?,
        )
        .context("persist authenticated remote operation batch and frontier")?;
    if let Err(error) = context.history.compact_acknowledged_tombstones() {
        tracing::warn!(
            %error,
            "acknowledged tombstone compaction will be retried later"
        );
    }
    Ok(())
}
