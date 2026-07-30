use anyhow::Context;
use tokio_util::sync::CancellationToken;

use crate::{
    clipboard::wayland::WaylandBackend,
    config::{Config, SharedConfig},
    ipc::DaemonState,
    mesh::{MeshChunkCommand, MeshHandle, PersistBatch, PersistResult},
    model::{Operation, StampedOperation},
    payload::ExplicitSharePolicy,
    replication::{Codec, JsonV1Codec},
    storage::{CompactionReport, HistoryStore},
    transfer::TransferCoordinator,
};

use super::{
    runtime::unix_time_millis,
    views::{device_items, history_items},
};

pub(super) struct MeshPersistenceContext<'a> {
    pub(super) history: &'a mut HistoryStore,
    pub(super) state: &'a DaemonState,
    pub(super) content_key: &'a [u8; 32],
    pub(super) clipboard: &'a WaylandBackend,
    pub(super) mesh: &'a MeshHandle,
    pub(super) config_path: &'a std::path::Path,
    pub(super) config: &'a mut Config,
    pub(super) transfers: &'a mut TransferCoordinator,
}

pub(super) async fn handle_mesh_batch(
    batch: PersistBatch,
    context: &mut MeshPersistenceContext<'_>,
) {
    let result = persist_mesh_batch(&batch, context).await;
    if result.is_ok() {
        context
            .state
            .set_device_names(
                context
                    .mesh
                    .device_hostnames()
                    .await
                    .into_iter()
                    .map(|(node_id, hostname)| (node_id.to_string(), hostname))
                    .collect(),
            )
            .await;
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

pub(super) fn handle_mesh_chunk_command(
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
