use std::collections::BTreeSet;

use quinn::Connection;
use tokio::{
    sync::{Mutex, oneshot},
    time::{MissedTickBehavior, timeout},
};
use tokio_util::sync::CancellationToken;

use clip_sync_core::{
    model::{NodeId, OpId, Operation, SeenOps, StampedOperation},
    replication::decode_operation,
};

use super::super::protocol::{
    MAX_FRONTIER_BYTES, MAX_MEMBERSHIP_BYTES, ProtocolError, STREAM_KIND_SYNC, SyncRequest,
    SyncResponse, read_message, validate_batch, write_message,
};
use super::{
    BatchRequest, CLOSE_FORGOTTEN, EXCHANGE_TIMEOUT, MAX_RECONCILE_ROUNDS, MeshError,
    MeshStoreRequest, PERSIST_TIMEOUT, PersistBatch, RuntimeContext, bump_revision,
};

pub(super) async fn answer_sync(
    send: &mut quinn::SendStream,
    recv: &mut quinn::RecvStream,
    context: &RuntimeContext,
    peer: NodeId,
    peer_frontier: &Mutex<SeenOps>,
) -> Result<(), MeshError> {
    let request: SyncRequest = read_message(recv).await?;
    validate_batch(
        &request.frontier,
        &request.known_members,
        &request.operations,
    )?;
    tracing::debug!(
        received_operations = request.operations.len(),
        "answering mesh reconciliation request"
    );
    let advertised = decode_frontier(&request.frontier)?;
    let known_members = decode_membership(&request.known_members)?;
    persist_and_record(
        context,
        peer,
        advertised.clone(),
        known_members,
        request.operations,
    )
    .await?;
    *peer_frontier.lock().await = advertised.clone();

    let (frontier, known_members, operations, has_more) =
        batch_for_peer(context, &advertised).await?;
    let response = SyncResponse {
        frontier,
        operations,
        has_more,
        known_members,
    };
    write_message(send, &response).await?;
    send.finish()?;
    Ok(())
}

pub(super) async fn initiate_sync_streams(
    connection: &Connection,
    context: &RuntimeContext,
    peer: NodeId,
    peer_frontier: &Mutex<SeenOps>,
    shutdown: CancellationToken,
) -> Result<(), MeshError> {
    let mut interval = tokio::time::interval(context.config.reconcile_interval);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut revision = context.revision.subscribe();

    loop {
        tokio::select! {
            () = shutdown.cancelled() => return Ok(()),
            _ = interval.tick() => {}
            result = revision.changed() => {
                if result.is_err() {
                    return Ok(());
                }
            }
        }

        for _ in 0..MAX_RECONCILE_ROUNDS {
            let more = timeout(
                EXCHANGE_TIMEOUT,
                initiate_sync(connection, context, peer, peer_frontier),
            )
            .await
            .map_err(|_| MeshError::ExchangeTimeout)??;
            if !more {
                break;
            }
        }
    }
}

async fn initiate_sync(
    connection: &Connection,
    context: &RuntimeContext,
    peer: NodeId,
    peer_frontier: &Mutex<SeenOps>,
) -> Result<bool, MeshError> {
    let advertised = peer_frontier.lock().await.clone();
    let (frontier, known_members, operations, request_has_more) =
        batch_for_peer(context, &advertised).await?;
    let request = SyncRequest {
        frontier,
        operations,
        has_more: request_has_more,
        known_members,
    };
    let (mut send, mut recv) = connection.open_bi().await?;
    send.write_all(&[STREAM_KIND_SYNC]).await?;
    write_message(&mut send, &request).await?;
    let response: SyncResponse = read_message(&mut recv).await?;
    validate_batch(
        &response.frontier,
        &response.known_members,
        &response.operations,
    )?;
    tracing::debug!(
        pushed_operations = request.operations.len(),
        received_operations = response.operations.len(),
        "completed mesh reconciliation exchange"
    );
    let response_frontier = decode_frontier(&response.frontier)?;
    let known_members = decode_membership(&response.known_members)?;
    persist_and_record(
        context,
        peer,
        response_frontier.clone(),
        known_members,
        response.operations,
    )
    .await?;
    *peer_frontier.lock().await = response_frontier;
    send.finish()?;
    Ok(request_has_more || response.has_more)
}

async fn batch_for_peer(
    context: &RuntimeContext,
    peer_frontier: &SeenOps,
) -> Result<(Vec<u8>, Vec<u8>, Vec<Vec<u8>>, bool), MeshError> {
    let (reply, completed) = oneshot::channel();
    context
        .store_tx
        .send(MeshStoreRequest::Batch(BatchRequest {
            remote: peer_frontier.clone(),
            limits: context.config.batch_limits,
            reply,
        }))
        .await
        .map_err(|_| MeshError::PersistenceUnavailable)?;
    let batch = timeout(PERSIST_TIMEOUT, completed)
        .await
        .map_err(|_| MeshError::PersistenceTimeout)?
        .map_err(|_| MeshError::PersistenceUnavailable)?
        .map_err(MeshError::PersistenceRejected)?;
    let frontier = encode_frontier(&*context.seen.read().await)?;
    let known_members = encode_membership(&*context.known_members.read().await)?;
    Ok((frontier, known_members, batch.operations, batch.has_more))
}

pub(super) async fn persist_and_record(
    context: &RuntimeContext,
    peer: NodeId,
    peer_frontier: SeenOps,
    known_members: BTreeSet<NodeId>,
    operations: Vec<Vec<u8>>,
) -> Result<(), MeshError> {
    let operations = operations
        .iter()
        .map(|operation| decode_operation(operation))
        .collect::<Result<Vec<_>, _>>()?;
    // The batch itself moves to the store; only what the runtime needs
    // afterwards is kept, so large payloads are never held twice.
    let received = operations
        .iter()
        .map(StampedOperation::id)
        .collect::<Vec<OpId>>();
    let forgotten = operations
        .iter()
        .filter_map(|operation| match operation.operation() {
            Operation::ForgetDevice { node_id } => Some(*node_id),
            _ => None,
        })
        .collect::<Vec<NodeId>>();

    let (reply, completed) = oneshot::channel();
    context
        .store_tx
        .send(MeshStoreRequest::Persist(PersistBatch {
            peer,
            peer_frontier,
            known_members: known_members.clone(),
            operations,
            reply,
        }))
        .await
        .map_err(|_| MeshError::PersistenceUnavailable)?;
    timeout(PERSIST_TIMEOUT, completed)
        .await
        .map_err(|_| MeshError::PersistenceTimeout)?
        .map_err(|_| MeshError::PersistenceUnavailable)?
        .map_err(MeshError::PersistenceRejected)?;

    let mut seen = context.seen.write().await;
    for id in &received {
        seen.record(*id);
    }
    drop(seen);

    let mut members = context.known_members.write().await;
    let mut changed = members.insert(peer);
    for member in known_members {
        changed |= members.insert(member);
    }
    for id in &received {
        changed |= members.insert(id.node());
    }
    drop(members);

    for node_id in forgotten {
        changed |= context.forgotten_devices.write().await.insert(node_id);
        if let Some(active) = context.registry.lock().await.remove(&node_id) {
            active
                .connection
                .close(CLOSE_FORGOTTEN.into(), b"device identity forgotten");
        }
    }

    // The reconciliation loop wakes on this revision, so bumping it after an
    // exchange that carried nothing makes every exchange schedule the next one
    // and the mesh spins at full speed while idle. Each peer's task also wakes
    // the others, so the cost scales with the mesh. Signalling only real change
    // leaves the periodic tick as the floor for convergence.
    if !received.is_empty() || changed {
        bump_revision(&context.revision);
    }
    Ok(())
}

pub(super) fn encode_frontier(frontier: &SeenOps) -> Result<Vec<u8>, MeshError> {
    let encoded = serde_json::to_vec(frontier)?;
    if encoded.len() > MAX_FRONTIER_BYTES {
        return Err(MeshError::Protocol(ProtocolError::FrontierTooLarge(
            encoded.len(),
        )));
    }
    Ok(encoded)
}

pub(super) fn decode_frontier(encoded: &[u8]) -> Result<SeenOps, MeshError> {
    if encoded.len() > MAX_FRONTIER_BYTES {
        return Err(MeshError::Protocol(ProtocolError::FrontierTooLarge(
            encoded.len(),
        )));
    }
    serde_json::from_slice(encoded).map_err(MeshError::Frontier)
}

pub(super) fn encode_membership(members: &BTreeSet<NodeId>) -> Result<Vec<u8>, MeshError> {
    let encoded = serde_json::to_vec(members)?;
    if encoded.len() > MAX_MEMBERSHIP_BYTES {
        return Err(MeshError::Protocol(ProtocolError::MembershipTooLarge(
            encoded.len(),
        )));
    }
    Ok(encoded)
}

pub(super) fn decode_membership(encoded: &[u8]) -> Result<BTreeSet<NodeId>, MeshError> {
    if encoded.len() > MAX_MEMBERSHIP_BYTES {
        return Err(MeshError::Protocol(ProtocolError::MembershipTooLarge(
            encoded.len(),
        )));
    }
    serde_json::from_slice(encoded).map_err(MeshError::Membership)
}
