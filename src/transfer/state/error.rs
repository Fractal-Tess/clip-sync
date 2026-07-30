use thiserror::Error;

use crate::payload::ChunkId;

use super::TransferPhase;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TransferError {
    #[error("transfer limits must be nonzero")]
    InvalidLimits,
    #[error("transfer exceeds the {maximum}-chunk limit")]
    TooManyChunks { maximum: usize },
    #[error("transfer exceeds the {maximum}-peer limit")]
    TooManyPeers { maximum: usize },
    #[error("invalid empty chunk description")]
    InvalidChunk,
    #[error("chunk {0} appears with conflicting sizes")]
    ConflictingChunk(ChunkId),
    #[error("transfer size overflow")]
    SizeOverflow,
    #[error("invalid transition from {from:?} to {to:?}")]
    InvalidTransition {
        from: TransferPhase,
        to: TransferPhase,
    },
    #[error("transfer is already terminal in {0:?}")]
    Terminal(TransferPhase),
    #[error("transfer is already complete; delete the completed history item instead")]
    AlreadyComplete,
    #[error("unknown transfer chunk {0}")]
    UnknownChunk(ChunkId),
    #[error("chunk {id} size mismatch: expected {expected}, got {actual}")]
    ChunkSizeMismatch {
        id: ChunkId,
        expected: u32,
        actual: u32,
    },
    #[error("missing-chunk request limit must be nonzero")]
    InvalidRequestLimit,
    #[error("transfer still has {missing} missing chunks")]
    Incomplete { missing: usize },
    #[error("peer progress exceeds the transfer size")]
    InvalidPeerProgress,
    #[error("peer progress must be monotonic")]
    PeerProgressRegression,
    #[error("persisted transfer state exceeds the {maximum}-byte limit")]
    StateTooLarge { maximum: usize },
    #[error("persisted transfer state is malformed")]
    MalformedState,
}
