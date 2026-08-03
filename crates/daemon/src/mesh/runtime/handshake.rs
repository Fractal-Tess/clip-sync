use std::collections::BTreeSet;

use quinn::Connection;
use uuid::Uuid;

use clip_sync_core::model::{NodeId, SeenOps};

use super::super::protocol::{
    IdentityHello, MAX_BATCH_OPERATIONS, MAX_FRONTIER_BYTES, MAX_HOSTNAME_BYTES,
    MAX_MEMBERSHIP_BYTES, PROTOCOL_VERSION, ProtocolError, ProtocolHello, read_message,
    write_message,
};
use super::{
    Direction, MeshError, MeshRuntimeConfig, RuntimeContext,
    control::{decode_frontier, decode_membership, encode_frontier, encode_membership},
};

#[derive(Debug)]
pub(super) struct PeerIdentity {
    pub(super) node_id: NodeId,
    pub(super) hostname: String,
    pub(super) frontier: SeenOps,
    pub(super) known_members: BTreeSet<NodeId>,
}

pub(super) async fn exchange_identity(
    connection: &Connection,
    direction: Direction,
    context: &RuntimeContext,
) -> Result<PeerIdentity, MeshError> {
    let local_protocol = ProtocolHello {
        minimum_version: PROTOCOL_VERSION,
        maximum_version: PROTOCOL_VERSION,
    };
    let peer = match direction {
        Direction::Outbound => {
            let (mut send, mut recv) = connection.open_bi().await?;
            write_message(&mut send, &local_protocol).await?;
            let peer_protocol: ProtocolHello = read_message(&mut recv).await?;
            validate_protocol(&peer_protocol)?;
            let local = local_identity(context).await?;
            write_message(&mut send, &local).await?;
            let peer: IdentityHello = read_message(&mut recv).await?;
            send.finish()?;
            peer
        }
        Direction::Inbound => {
            let (mut send, mut recv) = connection.accept_bi().await?;
            let peer_protocol: ProtocolHello = read_message(&mut recv).await?;
            write_message(&mut send, &local_protocol).await?;
            validate_protocol(&peer_protocol)?;
            let peer: IdentityHello = read_message(&mut recv).await?;
            let local = local_identity(context).await?;
            write_message(&mut send, &local).await?;
            send.finish()?;
            peer
        }
    };
    parse_identity(peer)
}

async fn local_identity(context: &RuntimeContext) -> Result<IdentityHello, MeshError> {
    let frontier = encode_frontier(context.state.read().await.seen())?;
    let members = context.known_members.read().await;
    let known_members = encode_membership(&members)?;
    Ok(IdentityHello {
        node_id: context.config.node_id.as_uuid().as_bytes().to_vec(),
        hostname: context.config.hostname.clone(),
        frontier,
        known_members,
    })
}

pub(super) fn parse_identity(hello: IdentityHello) -> Result<PeerIdentity, MeshError> {
    if !valid_hostname(&hello.hostname) {
        return Err(MeshError::InvalidHostname);
    }
    if hello.frontier.len() > MAX_FRONTIER_BYTES {
        return Err(MeshError::Protocol(ProtocolError::FrontierTooLarge(
            hello.frontier.len(),
        )));
    }
    if hello.known_members.len() > MAX_MEMBERSHIP_BYTES {
        return Err(MeshError::Protocol(ProtocolError::MembershipTooLarge(
            hello.known_members.len(),
        )));
    }
    let uuid = Uuid::from_slice(&hello.node_id).map_err(|_| MeshError::InvalidNodeId)?;
    let frontier = decode_frontier(&hello.frontier)?;
    let known_members = decode_membership(&hello.known_members)?;
    Ok(PeerIdentity {
        node_id: NodeId::from_uuid(uuid),
        hostname: hello.hostname,
        frontier,
        known_members,
    })
}

pub(super) fn validate_protocol(hello: &ProtocolHello) -> Result<(), MeshError> {
    if hello.minimum_version > hello.maximum_version
        || PROTOCOL_VERSION < hello.minimum_version
        || PROTOCOL_VERSION > hello.maximum_version
    {
        return Err(MeshError::UnsupportedProtocol {
            minimum: hello.minimum_version,
            maximum: hello.maximum_version,
        });
    }
    Ok(())
}

pub(super) fn validate_local_config(config: &MeshRuntimeConfig) -> Result<(), MeshError> {
    if !valid_hostname(&config.hostname) {
        return Err(MeshError::InvalidHostname);
    }
    if config.reconcile_interval.is_zero()
        || config.reconnect_min.is_zero()
        || config.reconnect_max < config.reconnect_min
        || config.listen_port == 0
        || config.batch_limits.max_ops == 0
        || config.batch_limits.max_ops > MAX_BATCH_OPERATIONS
        || config.batch_limits.max_bytes == 0
        || config.batch_limits.max_bytes > super::super::protocol::MAX_CONTROL_FRAME_BYTES
        || config.max_concurrent_chunk_streams == 0
        || config.max_concurrent_chunk_streams > 32
    {
        return Err(MeshError::InvalidConfig);
    }
    Ok(())
}

fn valid_hostname(hostname: &str) -> bool {
    !hostname.is_empty()
        && hostname.len() <= MAX_HOSTNAME_BYTES
        && !hostname.chars().any(char::is_control)
}
