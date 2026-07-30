use thiserror::Error;

use crate::{mesh::protocol::PROTOCOL_VERSION, model::NodeId};

use super::super::protocol::ProtocolError;

#[derive(Debug, Error)]
pub enum MeshError {
    #[error("mesh runtime configuration is invalid")]
    InvalidConfig,
    #[error("mesh hostname is invalid")]
    InvalidHostname,
    #[error("peer node identity is invalid")]
    InvalidNodeId,
    #[error(
        "peer protocol range {minimum}..={maximum} does not include local version {PROTOCOL_VERSION}"
    )]
    UnsupportedProtocol { minimum: u32, maximum: u32 },
    #[error("peer duplicated the local active node identity {0}")]
    DuplicateNodeIdentity(NodeId),
    #[error("forgotten peer node identity {0} was rejected")]
    ForgottenNodeIdentity(NodeId),
    #[error("peer {0} already has a canonical active connection")]
    DuplicateConnection(NodeId),
    #[error("identity handshake timed out")]
    HandshakeTimeout,
    #[error("replication exchange timed out")]
    ExchangeTimeout,
    #[error("daemon persistence timed out")]
    PersistenceTimeout,
    #[error("daemon persistence service is unavailable")]
    PersistenceUnavailable,
    #[error("daemon rejected a remote operation batch: {0}")]
    PersistenceRejected(String),
    #[error("authenticated chunk broker timed out")]
    ChunkBrokerTimeout,
    #[error("authenticated chunk broker is unavailable")]
    ChunkBrokerUnavailable,
    #[error("daemon rejected chunk work: {0}")]
    ChunkBrokerRejected(String),
    #[error("unknown authenticated stream kind {0}")]
    UnknownStreamKind(u8),
    #[error("chunk stream frame is too large ({0} bytes)")]
    ChunkFrameTooLarge(usize),
    #[error("chunk request is invalid")]
    InvalidChunkRequest,
    #[error("chunk response is invalid")]
    InvalidChunkResponse,
    #[error("frontier is malformed: {0}")]
    Frontier(serde_json::Error),
    #[error("frontier serialization failed: {0}")]
    FrontierSerialization(#[from] serde_json::Error),
    #[error("membership advertisement is malformed: {0}")]
    Membership(serde_json::Error),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error(transparent)]
    Codec(#[from] crate::replication::CodecError),
    #[error(transparent)]
    Replication(#[from] crate::replication::AntiEntropyError),
    #[error("QUIC connection failed: {0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error("could not finish a QUIC stream: {0}")]
    Finish(#[from] quinn::ClosedStream),
    #[error("could not write a QUIC stream: {0}")]
    StreamWrite(#[from] quinn::WriteError),
    #[error("could not read a QUIC stream: {0}")]
    StreamRead(#[from] quinn::ReadExactError),
}
