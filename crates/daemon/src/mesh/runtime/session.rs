use std::{cmp::Ordering, sync::Arc};

use quinn::Connection;
use tokio::{sync::Mutex, time::timeout};
use tokio_util::sync::CancellationToken;

use clip_sync_core::model::{NodeId, SeenOps};

use super::super::protocol::{STREAM_KIND_FETCH, STREAM_KIND_SYNC};
use super::{
    ActiveConnection, CLOSE_DUPLICATE, CLOSE_FORGOTTEN, CLOSE_PROTOCOL, CLOSE_SHUTDOWN, Direction,
    EXCHANGE_TIMEOUT, HANDSHAKE_TIMEOUT, MAX_CONCURRENT_FETCHES, MeshError, RuntimeContext,
    control::{answer_sync, initiate_sync_streams, persist_and_record},
    fetch::serve_fetch,
    handshake::exchange_identity,
};

pub(super) async fn run_connection(
    connection: Connection,
    direction: Direction,
    context: Arc<RuntimeContext>,
    shutdown: CancellationToken,
    _session_permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<(), MeshError> {
    let peer = match timeout(
        HANDSHAKE_TIMEOUT,
        exchange_identity(&connection, direction, &context),
    )
    .await
    {
        Ok(Ok(peer)) => peer,
        Ok(Err(error)) => {
            connection.close(CLOSE_PROTOCOL.into(), b"incompatible mesh protocol");
            return Err(error);
        }
        Err(_) => {
            connection.close(CLOSE_PROTOCOL.into(), b"mesh handshake timed out");
            return Err(MeshError::HandshakeTimeout);
        }
    };

    if peer.node_id == context.config.node_id {
        connection.close(CLOSE_DUPLICATE.into(), b"duplicate node identity");
        return Err(MeshError::DuplicateNodeIdentity(peer.node_id));
    }
    if context
        .forgotten_devices
        .read()
        .await
        .contains(&peer.node_id)
    {
        connection.close(CLOSE_FORGOTTEN.into(), b"device identity forgotten");
        return Err(MeshError::ForgottenNodeIdentity(peer.node_id));
    }

    context
        .device_hostnames
        .write()
        .await
        .insert(peer.node_id, peer.hostname.clone());

    persist_and_record(
        &context,
        peer.node_id,
        peer.frontier.clone(),
        peer.known_members.clone(),
        Vec::new(),
    )
    .await?;

    // A forget may become durable while this handshake is waiting on the
    // daemon. Recheck before registering so that race cannot establish a
    // session for the retired identity.
    if context
        .forgotten_devices
        .read()
        .await
        .contains(&peer.node_id)
    {
        connection.close(CLOSE_FORGOTTEN.into(), b"device identity forgotten");
        return Err(MeshError::ForgottenNodeIdentity(peer.node_id));
    }

    let stable_id = connection.stable_id();
    if !register_connection(&context, peer.node_id, direction, &connection).await {
        connection.close(CLOSE_DUPLICATE.into(), b"duplicate connection");
        return Err(MeshError::DuplicateConnection(peer.node_id));
    }

    tracing::info!(
        peer_id = %peer.node_id,
        peer_hostname = %peer.hostname,
        ?direction,
        "authenticated mesh peer connected"
    );
    let peer_frontier = Arc::new(Mutex::new(peer.frontier));
    let result = session_loop(&connection, &context, peer.node_id, peer_frontier, shutdown).await;
    remove_connection(&context, peer.node_id, stable_id).await;
    connection.close(CLOSE_SHUTDOWN.into(), b"mesh session ended");
    tracing::info!(peer_id = %peer.node_id, "mesh peer disconnected");
    result
}

async fn register_connection(
    context: &RuntimeContext,
    peer: NodeId,
    direction: Direction,
    connection: &Connection,
) -> bool {
    let preferred = preferred_direction(context.config.node_id, peer) == direction;
    let mut registry = context.registry.lock().await;
    if let Some(existing) = registry.get(&peer) {
        if existing.preferred || !preferred {
            return false;
        }
        existing
            .connection
            .close(CLOSE_DUPLICATE.into(), b"replaced by canonical connection");
    }
    registry.insert(
        peer,
        ActiveConnection {
            stable_id: connection.stable_id(),
            preferred,
            connection: connection.clone(),
        },
    );
    context.status.send_modify(|status| {
        status.active_connections = registry.len();
    });
    true
}

async fn remove_connection(context: &RuntimeContext, peer: NodeId, stable_id: usize) {
    let mut registry = context.registry.lock().await;
    if registry
        .get(&peer)
        .is_some_and(|active| active.stable_id == stable_id)
    {
        registry.remove(&peer);
        context.status.send_modify(|status| {
            status.active_connections = registry.len();
        });
    }
}

fn preferred_direction(local: NodeId, peer: NodeId) -> Direction {
    match local.cmp(&peer) {
        Ordering::Less => Direction::Outbound,
        Ordering::Equal | Ordering::Greater => Direction::Inbound,
    }
}

async fn session_loop(
    connection: &Connection,
    context: &Arc<RuntimeContext>,
    peer: NodeId,
    peer_frontier: Arc<Mutex<SeenOps>>,
    shutdown: CancellationToken,
) -> Result<(), MeshError> {
    let inbound = accept_session_streams(
        connection,
        context,
        peer,
        peer_frontier.clone(),
        shutdown.clone(),
    );
    let outbound =
        initiate_sync_streams(connection, context, peer, &peer_frontier, shutdown.clone());
    tokio::pin!(inbound);
    tokio::pin!(outbound);

    tokio::select! {
        result = &mut inbound => result,
        result = &mut outbound => result,
        error = connection.closed() => Err(MeshError::Connection(error)),
        () = shutdown.cancelled() => Ok(()),
    }
}

/// Sync exchanges are answered one at a time and bounded by the exchange
/// timeout. Fetches can move gigabytes, so each runs on its own task without
/// that timeout; QUIC's idle timeout still ends one whose peer vanished.
async fn accept_session_streams(
    connection: &Connection,
    context: &Arc<RuntimeContext>,
    peer: NodeId,
    peer_frontier: Arc<Mutex<SeenOps>>,
    shutdown: CancellationToken,
) -> Result<(), MeshError> {
    let fetches = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_FETCHES));
    let mut serving = tokio::task::JoinSet::new();
    loop {
        let (mut send, mut recv) = tokio::select! {
            () = shutdown.cancelled() => return Ok(()),
            Some(_) = serving.join_next(), if !serving.is_empty() => continue,
            streams = connection.accept_bi() => streams?,
        };
        let mut kind = [0_u8; 1];
        timeout(EXCHANGE_TIMEOUT, recv.read_exact(&mut kind))
            .await
            .map_err(|_| MeshError::ExchangeTimeout)??;
        match kind[0] {
            STREAM_KIND_SYNC => timeout(
                EXCHANGE_TIMEOUT,
                answer_sync(&mut send, &mut recv, context, peer, &peer_frontier),
            )
            .await
            .map_err(|_| MeshError::ExchangeTimeout)??,
            STREAM_KIND_FETCH => {
                let Ok(permit) = fetches.clone().try_acquire_owned() else {
                    send.reset(0_u32.into()).ok();
                    continue;
                };
                let context = context.clone();
                serving.spawn(async move {
                    let _permit = permit;
                    if let Err(error) = serve_fetch(send, recv, &context).await {
                        tracing::debug!(%error, "fetch was not served");
                    }
                });
            }
            kind => return Err(MeshError::UnknownStreamKind(kind)),
        }
    }
}
