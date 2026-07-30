mod activation;
mod sharing;
mod synchronization;

use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;

use crate::{
    clipboard::types::{ClipboardContent, ClipboardContentError, MimeTypeError},
    model::{ContentError, ContentId, Operation, Payload, StampedOperation},
    payload::{
        ChunkId, ChunkStore, ChunkStoreError, ExplicitShareError, ExplicitShareInspection,
        ExplicitSharePolicy, FileSnapshotError, ManifestId, MaterializationError, Materializer,
        StoredManifest,
    },
    storage::HistoryError,
};

use super::{TransferError, TransferId, TransferPhase, TransferRecord, TransferStateLimits};

const MAX_REPLICATED_MANIFEST_BYTES: usize = 1024 * 1024;

/// Bounded request passed from the authenticated mesh to the chunk owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransferChunk {
    pub transfer_id: TransferId,
    pub manifest_id: ManifestId,
    pub chunk_id: ChunkId,
    pub logical_size: u32,
}

/// Daemon-facing aggregate transfer state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransferProgress {
    pub transfer_id: TransferId,
    pub manifest_id: ManifestId,
    pub phase: TransferPhase,
    pub logical_size: u64,
    pub verified_bytes: u64,
    pub verified_chunks: usize,
    pub expected_chunks: usize,
    pub quota_exempt: bool,
}

/// Result of activating a manifest-backed clipboard item.
#[derive(Clone, Debug)]
pub struct ActivatedClipboard {
    content: ClipboardContent,
    materialized_manifest: Option<ManifestId>,
}

impl ActivatedClipboard {
    #[must_use]
    pub const fn content(&self) -> &ClipboardContent {
        &self.content
    }

    #[must_use]
    pub const fn materialized_manifest(&self) -> Option<ManifestId> {
        self.materialized_manifest
    }

    #[must_use]
    pub fn into_content(self) -> ClipboardContent {
        self.content
    }
}

/// Single-owner daemon coordinator for durable chunk state and transfer state.
pub struct TransferCoordinator {
    store: ChunkStore,
    materializer: Materializer,
    policy: ExplicitSharePolicy,
    limits: TransferStateLimits,
    records: BTreeMap<TransferId, TransferRecord>,
    declared_complete: BTreeSet<TransferId>,
}

impl TransferCoordinator {
    #[must_use]
    pub fn new(
        store: ChunkStore,
        materializer: Materializer,
        policy: ExplicitSharePolicy,
        limits: TransferStateLimits,
    ) -> Self {
        Self {
            store,
            materializer,
            policy,
            limits,
            records: BTreeMap::new(),
            declared_complete: BTreeSet::new(),
        }
    }

    #[must_use]
    pub const fn store(&self) -> &ChunkStore {
        &self.store
    }

    pub fn store_mut(&mut self) -> &mut ChunkStore {
        &mut self.store
    }

    /// Returns the effective replicated limit for automatic clipboard capture.
    #[must_use]
    pub const fn automatic_capture_threshold_bytes(&self) -> u64 {
        self.policy.automatic_capture_threshold_bytes
    }

    /// Replaces the resource policy used for future explicit shares.
    ///
    /// In-flight transfers retain their already-durable quota classification.
    ///
    /// # Errors
    ///
    /// Returns an error when the replacement policy is internally inconsistent.
    pub fn update_policy(
        &mut self,
        policy: ExplicitSharePolicy,
    ) -> Result<(), TransferCoordinatorError> {
        policy.validate()?;
        self.policy = policy;
        Ok(())
    }

    /// Inspects an already streamed live snapshot without allocating chunks.
    ///
    /// # Errors
    ///
    /// Returns policy, hard-limit, or free-space errors.
    pub fn inspect_payload(
        &self,
        payload: &Payload,
        available_space: u64,
    ) -> Result<ExplicitShareInspection, TransferCoordinatorError> {
        self.policy
            .inspect(payload.descriptor().logical_size(), available_space)
            .map_err(Into::into)
    }

    /// Inspects a size-only live offer before the confirmed re-read.
    ///
    /// # Errors
    ///
    /// Returns policy, hard-limit, or free-space errors.
    pub fn inspect_size(
        &self,
        logical_size: u64,
        available_space: u64,
    ) -> Result<ExplicitShareInspection, TransferCoordinatorError> {
        self.policy
            .inspect(logical_size, available_space)
            .map_err(Into::into)
    }

    /// Verifies confirmation before the live offer is re-read or chunked.
    ///
    /// # Errors
    ///
    /// Returns [`ExplicitShareError::ConfirmationRequired`] for an oversized
    /// unconfirmed inspection.
    pub fn require_confirmation(
        &self,
        inspection: ExplicitShareInspection,
        confirmed: bool,
    ) -> Result<(), TransferCoordinatorError> {
        self.policy.authorize(inspection, confirmed)?;
        Ok(())
    }

    /// Validates replicated manifest metadata before it enters durable history.
    ///
    /// # Errors
    ///
    /// Returns malformed, oversized, or keyed-ID mismatch errors.
    pub fn validate_manifest(
        &self,
        manifest_id: ManifestId,
        manifest: &StoredManifest,
    ) -> Result<(), TransferCoordinatorError> {
        self.store.validate_manifest_id(manifest_id, manifest)?;
        let encoded = serde_json::to_vec(manifest)?;
        if encoded.len() > MAX_REPLICATED_MANIFEST_BYTES {
            return Err(TransferCoordinatorError::ManifestMetadataTooLarge {
                observed: encoded.len(),
                maximum: MAX_REPLICATED_MANIFEST_BYTES,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum TransferCoordinatorError {
    #[error("clipboard source changed after inspection")]
    SourceChanged,
    #[error("transfer {0} is unknown")]
    UnknownTransfer(TransferId),
    #[error("transfer {0} is cancelled")]
    Cancelled(TransferId),
    #[error("chunk {0} does not belong to this transfer")]
    UnknownChunk(ChunkId),
    #[error("manifest {0} conflicts with retained state")]
    ManifestConflict(ManifestId),
    #[error("replicated manifest metadata is {observed} bytes, exceeding {maximum}")]
    ManifestMetadataTooLarge { observed: usize, maximum: usize },
    #[error("content {0} is not locally available")]
    ContentUnavailable(ContentId),
    #[error("reconstructed payload identity does not match history")]
    ContentIdentityMismatch,
    #[error("encrypted chunk object has an invalid fixed size")]
    InvalidEncryptedSize,
    #[error("request limit must be nonzero")]
    InvalidLimit,
    #[error("manifest kind cannot be activated as clipboard content")]
    UnsupportedManifest,
    #[error(transparent)]
    ExplicitShare(#[from] ExplicitShareError),
    #[error(transparent)]
    ChunkStore(#[from] ChunkStoreError),
    #[error(transparent)]
    Transfer(#[from] TransferError),
    #[error(transparent)]
    History(#[from] HistoryError),
    #[error(transparent)]
    Content(#[from] ContentError),
    #[error(transparent)]
    Mime(#[from] MimeTypeError),
    #[error(transparent)]
    Clipboard(#[from] ClipboardContentError),
    #[error(transparent)]
    Materialization(#[from] MaterializationError),
    #[error(transparent)]
    FileSnapshot(#[from] FileSnapshotError),
    #[error(transparent)]
    ManifestEncoding(#[from] serde_json::Error),
}

/// Extracts transfer metadata from a replicated operation for mesh wakeups.
#[must_use]
pub fn operation_transfer_id(operation: &StampedOperation) -> Option<TransferId> {
    match operation.operation() {
        Operation::BeginShare { transfer_id, .. }
        | Operation::CompleteShare { transfer_id, .. }
        | Operation::CancelShare { transfer_id, .. } => Some(*transfer_id),
        _ => None,
    }
}
