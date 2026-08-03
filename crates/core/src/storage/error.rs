use thiserror::Error;

use crate::model::{HlcTimestamp, NodeId, OpId, ProjectionError};

pub type Result<T> = std::result::Result<T, StorageError>;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("storage key must be 32 bytes")]
    InvalidKeyLength,

    #[error("storage key derivation failed")]
    KeyDerivation,

    #[error("SQLCipher is not available")]
    CipherUnavailable,

    #[error("SQLite FTS5 is not available")]
    Fts5Unavailable,

    #[error("storage database could not be opened with the supplied key")]
    InvalidKey,

    #[error("storage database is not a regular, non-symlink file owned by the current user")]
    UnsafeDatabaseFile,

    #[error("storage schema is incompatible: {0}")]
    IncompatibleSchema(String),

    #[error("operation {0} already exists with different serialized bytes")]
    OperationConflict(OpId),

    #[error("{field} value {value} exceeds SQLite's signed integer range")]
    IntegerOutOfRange { field: &'static str, value: u64 },

    #[error("operation counter is exhausted")]
    CounterExhausted,

    #[error("local operation belongs to node {operation}, not local node {local}")]
    ReplicaNodeMismatch { operation: NodeId, local: NodeId },

    #[error("new remote operation claims the local node identity {0}")]
    RemoteOperationClaimsLocalIdentity(NodeId),

    #[error("local operation counter must be {expected}, got {actual}")]
    UnexpectedOperationCounter { expected: u64, actual: u64 },

    #[error("local operation timestamp {operation:?} does not advance past persisted HLC {last:?}")]
    HlcRegression {
        operation: HlcTimestamp,
        last: HlcTimestamp,
    },

    #[error(
        "observed HLC {observed:?} must advance past operation {operation:?} and persisted HLC {last:?}"
    )]
    InvalidObservedHlc {
        observed: HlcTimestamp,
        operation: HlcTimestamp,
        last: HlcTimestamp,
    },

    #[error("received a new peer operation claiming the local node identity {0}")]
    LocalOriginIngest(NodeId),

    #[error("persisted local operation log does not match replica metadata: {0}")]
    LocalOperationLogMismatch(String),

    #[error("serialized operation is invalid: {0}")]
    OperationDeserialization(#[source] serde_json::Error),

    #[error("stored operation is inconsistent: {0}")]
    CorruptOperation(String),

    #[error("stored local replica metadata is invalid: {0}")]
    CorruptReplicaMetadata(String),

    #[error("operation serialization failed: {0}")]
    OperationSerialization(#[source] serde_json::Error),

    #[error("acknowledgement serialization failed: {0}")]
    AcknowledgementSerialization(#[source] serde_json::Error),

    #[error("stored acknowledgement is invalid: {0}")]
    AcknowledgementDeserialization(#[source] serde_json::Error),

    #[error("compacted seen summary serialization failed: {0}")]
    CompactedSeenSerialization(#[source] serde_json::Error),

    #[error("stored compacted seen summary is invalid: {0}")]
    CompactedSeenDeserialization(#[source] serde_json::Error),

    #[error(transparent)]
    Projection(#[from] ProjectionError),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
}

pub(super) fn sqlite_integer(field: &'static str, value: u64) -> Result<i64> {
    value
        .try_into()
        .map_err(|_| StorageError::IntegerOutOfRange { field, value })
}
