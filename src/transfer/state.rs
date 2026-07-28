use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    str::FromStr,
};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    model::NodeId,
    payload::{ChunkId, ChunkRef, ManifestId},
};

const DEFAULT_MAX_CHUNKS: usize = 1_048_576;
const DEFAULT_MAX_PEERS: usize = 256;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TransferId(Uuid);

impl TransferId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    #[must_use]
    pub const fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    #[must_use]
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Default for TransferId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for TransferId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("TransferId")
            .field(&self.to_string())
            .finish()
    }
}

impl fmt::Display for TransferId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for TransferId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(value).map(Self)
    }
}

impl Serialize for TransferId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for TransferId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferPhase {
    Pending,
    Staging,
    Replicating,
    Paused,
    Complete,
    Cancelled,
    Failed,
}

impl TransferPhase {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Complete | Self::Cancelled | Self::Failed)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransferStateLimits {
    pub max_chunks: usize,
    pub max_peers: usize,
}

impl Default for TransferStateLimits {
    fn default() -> Self {
        Self {
            max_chunks: DEFAULT_MAX_CHUNKS,
            max_peers: DEFAULT_MAX_PEERS,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerProgress {
    verified_bytes: u64,
    complete: bool,
}

impl PeerProgress {
    #[must_use]
    pub const fn verified_bytes(self) -> u64 {
        self.verified_bytes
    }

    #[must_use]
    pub const fn complete(self) -> bool {
        self.complete
    }
}

/// Persistable transfer state. The runtime cancellation token is recreated on
/// deserialization and is intentionally excluded from persistence.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransferRecord {
    id: TransferId,
    manifest_id: ManifestId,
    phase: TransferPhase,
    logical_size: u64,
    quota_exempt: bool,
    expected_chunks: BTreeMap<ChunkId, u32>,
    verified_chunks: BTreeSet<ChunkId>,
    peers: BTreeMap<NodeId, PeerProgress>,
    #[serde(skip, default)]
    cancellation: CancellationToken,
}

impl TransferRecord {
    /// Creates bounded resumable state from a committed manifest's chunk list.
    ///
    /// # Errors
    ///
    /// Returns an error for zero/overflowing sizes, conflicting duplicate IDs,
    /// excessive chunks, or invalid limits.
    pub fn new(
        id: TransferId,
        manifest_id: ManifestId,
        logical_size: u64,
        chunks: &[ChunkRef],
        quota_exempt: bool,
        limits: TransferStateLimits,
    ) -> Result<Self, TransferError> {
        if limits.max_chunks == 0 || limits.max_peers == 0 {
            return Err(TransferError::InvalidLimits);
        }
        if chunks.len() > limits.max_chunks {
            return Err(TransferError::TooManyChunks {
                maximum: limits.max_chunks,
            });
        }
        let mut expected_chunks = BTreeMap::new();
        for chunk in chunks {
            if chunk.logical_size() == 0 {
                return Err(TransferError::InvalidChunk);
            }
            if expected_chunks
                .insert(chunk.id(), chunk.logical_size())
                .is_some_and(|prior| prior != chunk.logical_size())
            {
                return Err(TransferError::ConflictingChunk(chunk.id()));
            }
        }
        let unique_bytes = expected_chunks.values().try_fold(0_u64, |total, size| {
            total
                .checked_add(u64::from(*size))
                .ok_or(TransferError::SizeOverflow)
        })?;
        if logical_size > 0 && unique_bytes == 0 || logical_size == 0 && !chunks.is_empty() {
            return Err(TransferError::InvalidChunk);
        }
        Ok(Self {
            id,
            manifest_id,
            phase: TransferPhase::Pending,
            logical_size,
            quota_exempt,
            expected_chunks,
            verified_chunks: BTreeSet::new(),
            peers: BTreeMap::new(),
            cancellation: CancellationToken::new(),
        })
    }

    #[must_use]
    pub const fn id(&self) -> TransferId {
        self.id
    }

    #[must_use]
    pub const fn manifest_id(&self) -> ManifestId {
        self.manifest_id
    }

    #[must_use]
    pub const fn phase(&self) -> TransferPhase {
        self.phase
    }

    #[must_use]
    pub const fn logical_size(&self) -> u64 {
        self.logical_size
    }

    #[must_use]
    pub const fn quota_exempt(&self) -> bool {
        self.quota_exempt
    }

    #[must_use]
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    #[must_use]
    pub fn verified_bytes(&self) -> u64 {
        self.verified_chunks
            .iter()
            .filter_map(|id| self.expected_chunks.get(id))
            .map(|size| u64::from(*size))
            .sum()
    }

    #[must_use]
    pub fn unique_transfer_bytes(&self) -> u64 {
        self.expected_chunks
            .values()
            .map(|size| u64::from(*size))
            .sum()
    }

    #[must_use]
    pub fn verified_chunk_count(&self) -> usize {
        self.verified_chunks.len()
    }

    #[must_use]
    pub fn expected_chunk_count(&self) -> usize {
        self.expected_chunks.len()
    }

    #[must_use]
    pub fn peers(&self) -> &BTreeMap<NodeId, PeerProgress> {
        &self.peers
    }

    /// Validates deserialized state and restores terminal cancellation into the
    /// process-local token.
    ///
    /// # Errors
    ///
    /// Returns an error for excessive collections, invalid chunks, impossible
    /// progress, or a completed record that still lacks chunks.
    pub fn validate_and_restore(
        &mut self,
        limits: TransferStateLimits,
    ) -> Result<(), TransferError> {
        if limits.max_chunks == 0 || limits.max_peers == 0 {
            return Err(TransferError::InvalidLimits);
        }
        if self.expected_chunks.len() > limits.max_chunks {
            return Err(TransferError::TooManyChunks {
                maximum: limits.max_chunks,
            });
        }
        if self.peers.len() > limits.max_peers {
            return Err(TransferError::TooManyPeers {
                maximum: limits.max_peers,
            });
        }
        if self.expected_chunks.values().any(|size| *size == 0)
            || !self
                .verified_chunks
                .iter()
                .all(|id| self.expected_chunks.contains_key(id))
        {
            return Err(TransferError::InvalidChunk);
        }
        let transfer_bytes = self.unique_transfer_bytes();
        if self
            .peers
            .values()
            .any(|peer| peer.verified_bytes > transfer_bytes)
        {
            return Err(TransferError::InvalidPeerProgress);
        }
        if self.phase == TransferPhase::Complete
            && self.verified_chunks.len() != self.expected_chunks.len()
        {
            return Err(TransferError::Incomplete {
                missing: self.expected_chunks.len() - self.verified_chunks.len(),
            });
        }
        if self.phase == TransferPhase::Cancelled {
            self.cancellation.cancel();
        }
        Ok(())
    }

    /// Decodes persisted JSON only after applying an encoded-size bound, then
    /// validates all collection and progress limits.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero/exceeded byte bound, malformed JSON, or
    /// invalid transfer state.
    pub fn decode_bounded_json(
        bytes: &[u8],
        limits: TransferStateLimits,
        maximum_encoded_bytes: usize,
    ) -> Result<Self, TransferError> {
        if maximum_encoded_bytes == 0 {
            return Err(TransferError::InvalidLimits);
        }
        if bytes.len() > maximum_encoded_bytes {
            return Err(TransferError::StateTooLarge {
                maximum: maximum_encoded_bytes,
            });
        }
        let mut record: Self =
            serde_json::from_slice(bytes).map_err(|_| TransferError::MalformedState)?;
        record.validate_and_restore(limits)?;
        Ok(record)
    }

    /// Serializes validated state and enforces a caller-provided byte bound.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid state, serialization failure, or an
    /// exceeded/zero byte bound.
    pub fn encode_bounded_json(
        &mut self,
        limits: TransferStateLimits,
        maximum_encoded_bytes: usize,
    ) -> Result<Vec<u8>, TransferError> {
        if maximum_encoded_bytes == 0 {
            return Err(TransferError::InvalidLimits);
        }
        self.validate_and_restore(limits)?;
        let encoded = serde_json::to_vec(self).map_err(|_| TransferError::MalformedState)?;
        if encoded.len() > maximum_encoded_bytes {
            return Err(TransferError::StateTooLarge {
                maximum: maximum_encoded_bytes,
            });
        }
        Ok(encoded)
    }

    /// Starts local chunking after explicit confirmation.
    ///
    /// # Errors
    ///
    /// Returns an error unless the record is pending.
    pub fn begin_staging(&mut self) -> Result<(), TransferError> {
        self.transition(TransferPhase::Pending, TransferPhase::Staging)
    }

    /// Marks the manifest ready for peer replication.
    ///
    /// # Errors
    ///
    /// Returns an error unless currently staging or paused.
    pub fn begin_replication(&mut self) -> Result<(), TransferError> {
        match self.phase {
            TransferPhase::Staging | TransferPhase::Paused => {
                self.phase = TransferPhase::Replicating;
                Ok(())
            }
            _ => Err(TransferError::InvalidTransition {
                from: self.phase,
                to: TransferPhase::Replicating,
            }),
        }
    }

    /// Pauses an interrupted active transfer without losing verified chunks.
    ///
    /// # Errors
    ///
    /// Returns an error unless staging or replicating.
    pub fn pause(&mut self) -> Result<(), TransferError> {
        match self.phase {
            TransferPhase::Staging | TransferPhase::Replicating => {
                self.phase = TransferPhase::Paused;
                Ok(())
            }
            _ => Err(TransferError::InvalidTransition {
                from: self.phase,
                to: TransferPhase::Paused,
            }),
        }
    }

    /// Records a locally authenticated chunk. Duplicate acknowledgements are
    /// idempotent and unknown/mismatched chunks are rejected.
    ///
    /// # Errors
    ///
    /// Returns an error for terminal state, unknown ID, or size mismatch.
    pub fn mark_chunk_verified(
        &mut self,
        id: ChunkId,
        logical_size: u32,
    ) -> Result<bool, TransferError> {
        if self.phase.is_terminal() {
            return Err(TransferError::Terminal(self.phase));
        }
        let expected = self
            .expected_chunks
            .get(&id)
            .ok_or(TransferError::UnknownChunk(id))?;
        if *expected != logical_size {
            return Err(TransferError::ChunkSizeMismatch {
                id,
                expected: *expected,
                actual: logical_size,
            });
        }
        Ok(self.verified_chunks.insert(id))
    }

    /// Returns at most `limit` missing chunk IDs in deterministic order.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero request bound.
    pub fn missing_chunks(&self, limit: usize) -> Result<Vec<ChunkId>, TransferError> {
        if limit == 0 {
            return Err(TransferError::InvalidRequestLimit);
        }
        Ok(self
            .expected_chunks
            .keys()
            .filter(|id| !self.verified_chunks.contains(id))
            .copied()
            .take(limit)
            .collect())
    }

    /// Updates bounded per-peer progress monotonically.
    ///
    /// # Errors
    ///
    /// Returns an error for regression, excess peers, or impossible byte counts.
    pub fn update_peer(
        &mut self,
        node: NodeId,
        verified_bytes: u64,
        complete: bool,
        limits: TransferStateLimits,
    ) -> Result<(), TransferError> {
        if verified_bytes > self.unique_transfer_bytes() {
            return Err(TransferError::InvalidPeerProgress);
        }
        if !self.peers.contains_key(&node) && self.peers.len() == limits.max_peers {
            return Err(TransferError::TooManyPeers {
                maximum: limits.max_peers,
            });
        }
        if let Some(prior) = self.peers.get(&node)
            && (verified_bytes < prior.verified_bytes || prior.complete && !complete)
        {
            return Err(TransferError::PeerProgressRegression);
        }
        self.peers.insert(
            node,
            PeerProgress {
                verified_bytes,
                complete,
            },
        );
        Ok(())
    }

    /// Completes only after every expected chunk has authenticated.
    ///
    /// # Errors
    ///
    /// Returns an error when chunks are missing or the state is not active.
    pub fn complete(&mut self) -> Result<(), TransferError> {
        if self.verified_chunks.len() != self.expected_chunks.len() {
            return Err(TransferError::Incomplete {
                missing: self.expected_chunks.len() - self.verified_chunks.len(),
            });
        }
        match self.phase {
            TransferPhase::Staging | TransferPhase::Replicating | TransferPhase::Paused => {
                self.phase = TransferPhase::Complete;
                Ok(())
            }
            _ => Err(TransferError::InvalidTransition {
                from: self.phase,
                to: TransferPhase::Complete,
            }),
        }
    }

    /// Cancellation dominates every incomplete phase and triggers all cloned
    /// runtime tokens. Repeated cancellation is idempotent.
    ///
    /// # Errors
    ///
    /// A completed share must be deleted through history semantics instead.
    pub fn cancel(&mut self) -> Result<bool, TransferError> {
        if self.phase == TransferPhase::Complete {
            return Err(TransferError::AlreadyComplete);
        }
        if self.phase == TransferPhase::Cancelled {
            return Ok(false);
        }
        self.phase = TransferPhase::Cancelled;
        self.cancellation.cancel();
        Ok(true)
    }

    /// Marks an unrecoverable local failure. Cancellation can still dominate
    /// this state when its mesh tombstone arrives later.
    ///
    /// # Errors
    ///
    /// Returns an error for complete/cancelled records.
    pub fn fail(&mut self) -> Result<(), TransferError> {
        if matches!(
            self.phase,
            TransferPhase::Complete | TransferPhase::Cancelled
        ) {
            return Err(TransferError::Terminal(self.phase));
        }
        self.phase = TransferPhase::Failed;
        Ok(())
    }

    fn transition(
        &mut self,
        expected: TransferPhase,
        next: TransferPhase,
    ) -> Result<(), TransferError> {
        if self.phase != expected {
            return Err(TransferError::InvalidTransition {
                from: self.phase,
                to: next,
            });
        }
        self.phase = next;
        Ok(())
    }
}

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
