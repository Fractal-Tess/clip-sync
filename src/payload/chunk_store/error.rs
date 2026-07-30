use std::io;

use thiserror::Error;

use super::{ChunkId, ManifestId};

#[derive(Debug, Error)]
pub enum ChunkStoreError {
    #[error("chunk size must be a power of two between 64 KiB and 4 MiB")]
    InvalidChunkSize,
    #[error("chunk-store limits must be nonzero")]
    InvalidLimits,
    #[error("chunk-store key derivation failed")]
    KeyDerivation,
    #[error("secure randomness is unavailable")]
    Randomness,
    #[error("SQLCipher is unavailable for the chunk catalog")]
    CipherUnavailable,
    #[error("chunk catalog could not be opened with this key")]
    InvalidKey,
    #[error("payload reached {observed} bytes, exceeding its {maximum}-byte limit")]
    PayloadTooLarge { observed: u64, maximum: u64 },
    #[error("manifest exceeds the {maximum}-chunk limit")]
    TooManyChunks { maximum: usize },
    #[error("payload size overflow")]
    SizeOverflow,
    #[error("operation cancelled")]
    Cancelled,
    #[error("invalid keyed identifier")]
    InvalidIdentifier,
    #[error("keyed identifier collision")]
    IdentifierCollision,
    #[error("chunk identifier did not match authenticated plaintext")]
    IdentifierMismatch,
    #[error("chunk encryption failed")]
    Encryption,
    #[error("chunk {0} failed authentication")]
    Authentication(ChunkId),
    #[error("chunk {0} is missing")]
    MissingChunk(ChunkId),
    #[error("chunk {0} is corrupt")]
    CorruptChunk(ChunkId),
    #[error("chunk {0} is retained and cannot be discarded")]
    ChunkRetained(ChunkId),
    #[error("manifest {0} is missing")]
    MissingManifest(ManifestId),
    #[error("manifest {0} is corrupt")]
    CorruptManifest(ManifestId),
    #[error("manifest is internally inconsistent")]
    MalformedManifest,
    #[error("chunk {0} refcount would underflow")]
    RefcountUnderflow(ChunkId),
    #[error("chunk catalog is corrupt")]
    CorruptCatalog,
    #[error("incoming chunk was truncated")]
    TruncatedChunk,
    #[error("incoming chunk exceeded the fixed object size")]
    OversizedChunk,
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
