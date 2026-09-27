use thiserror::Error;

use clip_sync_core::model::NodeId;

use crate::mesh::protocol::PROTOCOL_VERSION;

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
    #[error("unknown authenticated stream kind {0}")]
    UnknownStreamKind(u8),
    #[error("the device that copied this item is not connected")]
    OriginOffline,
    #[error("the device that copied this item cannot provide it: {0}")]
    SourceUnavailable(String),
    #[error("fetch request is invalid")]
    InvalidFetchRequest,
    #[error("the fetched item did not match its description")]
    FetchMismatch,
    #[error("could not write the fetched item: {0}")]
    FetchIo(#[from] std::io::Error),
    #[error("could not stop a QUIC stream: {0}")]
    Stopped(#[from] quinn::StoppedError),
    #[error("could not read a QUIC stream: {0}")]
    StreamReadChunk(#[from] quinn::ReadError),
    #[error("frontier is malformed: {0}")]
    Frontier(serde_json::Error),
    #[error("frontier serialization failed: {0}")]
    FrontierSerialization(#[from] serde_json::Error),
    #[error("membership advertisement is malformed: {0}")]
    Membership(serde_json::Error),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error(transparent)]
    Codec(#[from] clip_sync_core::replication::CodecError),
    #[error("QUIC connection failed: {0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error("could not finish a QUIC stream: {0}")]
    Finish(#[from] quinn::ClosedStream),
    #[error("could not write a QUIC stream: {0}")]
    StreamWrite(#[from] quinn::WriteError),
    #[error("could not read a QUIC stream: {0}")]
    StreamRead(#[from] quinn::ReadExactError),
}
