use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{
    clipboard::types::{
        ClipboardContent, ClipboardContentError, ClipboardRepresentation, MimeType, MimeTypeError,
    },
    model::{
        ContentError, ContentId, Operation, Payload, Projection, Representation, StampedOperation,
    },
    payload::{
        ChunkId, ChunkStore, ChunkStoreError, ExplicitShareError, ExplicitShareInspection,
        ExplicitSharePolicy, FileSnapshotError, FileSnapshotLimits, ManifestId,
        MaterializationError, Materializer, StoredManifest, inspect_file_uris, snapshot_file_uris,
    },
    storage::{HistoryError, HistoryStore},
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

    /// Encrypts and commits a confirmed current clipboard, then persists the
    /// pending replicated operation. Completion remains explicit so callers can
    /// publish the begin operation before publishing completion.
    ///
    /// # Errors
    ///
    /// Returns confirmation, source-size, chunk-store, model, or history errors.
    pub fn begin_payload_share(
        &mut self,
        payload: &Payload,
        inspection: ExplicitShareInspection,
        confirmed: bool,
        history: &mut HistoryStore,
        now_millis: u64,
        cancellation: &CancellationToken,
    ) -> Result<(TransferId, StampedOperation), TransferCoordinatorError> {
        let decision = self.policy.authorize(inspection, confirmed)?;
        if decision.logical_size() != payload.descriptor().logical_size() {
            return Err(TransferCoordinatorError::SourceChanged);
        }
        let mut representations = payload
            .representations()
            .iter()
            .map(|representation| (representation.mime().to_owned(), representation.bytes()))
            .collect::<Vec<_>>();
        let bundle = match self.store.stage_mime_bundle(
            &mut representations,
            decision.logical_size(),
            cancellation,
        ) {
            Ok(bundle) => bundle,
            Err(error) => {
                let _ = self.store.cleanup_unreferenced();
                return Err(error.into());
            }
        };
        let manifest = StoredManifest::MimeBundle(bundle);
        let manifest_id = match self.store.manifest_id_for(&manifest) {
            Ok(manifest_id) => manifest_id,
            Err(error) => {
                let _ = self.store.cleanup_unreferenced();
                return Err(error.into());
            }
        };
        if let Err(error) = self.validate_manifest(manifest_id, &manifest) {
            let _ = self.store.cleanup_unreferenced();
            return Err(error);
        }
        let committed_id = match self.store.commit_manifest(&manifest) {
            Ok(committed_id) => committed_id,
            Err(error) => {
                let _ = self.store.cleanup_unreferenced();
                return Err(error.into());
            }
        };
        debug_assert_eq!(committed_id, manifest_id);
        let transfer_id = TransferId::new();
        let record = match self.complete_local_record(
            transfer_id,
            manifest_id,
            &manifest,
            decision.quota_exempt(),
        ) {
            Ok(record) => record,
            Err(error) => {
                let _ = self.store.remove_manifest(manifest_id);
                return Err(error);
            }
        };
        let begin = match history.begin_manifest_share(
            transfer_id,
            payload.descriptor().content_id(),
            manifest_id,
            manifest.clone(),
            decision.quota_exempt(),
            now_millis,
        ) {
            Ok(operation) => operation,
            Err(error) => {
                let _ = self.store.remove_manifest(manifest_id);
                return Err(error.into());
            }
        };
        self.records.insert(transfer_id, record);
        Ok((transfer_id, begin))
    }

    /// Registers an already committed safe manifest (for example a file
    /// snapshot) as a pending replicated share.
    ///
    /// # Errors
    ///
    /// Returns manifest mismatch, chunk authentication, history, or state errors.
    pub fn begin_committed_manifest_share(
        &mut self,
        content_id: ContentId,
        manifest_id: ManifestId,
        manifest: &StoredManifest,
        quota_exempt: bool,
        history: &mut HistoryStore,
        now_millis: u64,
    ) -> Result<(TransferId, StampedOperation), TransferCoordinatorError> {
        if self.store.manifest(manifest_id)? != *manifest {
            return Err(TransferCoordinatorError::ManifestConflict(manifest_id));
        }
        self.validate_manifest(manifest_id, manifest)?;
        let transfer_id = TransferId::new();
        let record =
            self.complete_local_record(transfer_id, manifest_id, manifest, quota_exempt)?;
        let operation = history.begin_manifest_share(
            transfer_id,
            content_id,
            manifest_id,
            manifest.clone(),
            quota_exempt,
            now_millis,
        )?;
        self.records.insert(transfer_id, record);
        Ok((transfer_id, operation))
    }

    /// Preflights safe file roots without reading or chunking their bytes.
    ///
    /// # Errors
    ///
    /// Returns path-safety, resource-policy, hard-limit, or free-space errors.
    pub fn inspect_files(
        &self,
        paths: &[PathBuf],
        limits: FileSnapshotLimits,
        available_space: u64,
        cancellation: &CancellationToken,
    ) -> Result<ExplicitShareInspection, TransferCoordinatorError> {
        let logical_size = inspect_file_uris(paths, limits, cancellation)?;
        self.inspect_size(logical_size, available_space)
    }

    /// Snapshots and publishes confirmed local files using the inspected size.
    ///
    /// # Errors
    ///
    /// Returns confirmation, source mutation, path safety, chunk-store,
    /// manifest, history, or state errors.
    #[allow(clippy::too_many_arguments)]
    pub fn begin_file_share(
        &mut self,
        paths: &[PathBuf],
        content_id: ContentId,
        inspection: ExplicitShareInspection,
        confirmed: bool,
        limits: FileSnapshotLimits,
        history: &mut HistoryStore,
        now_millis: u64,
        cancellation: &CancellationToken,
    ) -> Result<(TransferId, StampedOperation), TransferCoordinatorError> {
        let decision = self.policy.authorize(inspection, confirmed)?;
        let snapshot = match snapshot_file_uris(paths, &mut self.store, limits, cancellation) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let _ = self.store.cleanup_unreferenced();
                return Err(error.into());
            }
        };
        if snapshot.logical_size() != decision.logical_size() {
            self.store.cleanup_unreferenced()?;
            return Err(TransferCoordinatorError::SourceChanged);
        }
        let manifest = StoredManifest::Files(snapshot);
        let manifest_id = match self.store.manifest_id_for(&manifest) {
            Ok(manifest_id) => manifest_id,
            Err(error) => {
                let _ = self.store.cleanup_unreferenced();
                return Err(error.into());
            }
        };
        if let Err(error) = self.validate_manifest(manifest_id, &manifest) {
            let _ = self.store.cleanup_unreferenced();
            return Err(error);
        }
        let committed = match self.store.commit_manifest(&manifest) {
            Ok(committed) => committed,
            Err(error) => {
                let _ = self.store.cleanup_unreferenced();
                return Err(error.into());
            }
        };
        debug_assert_eq!(committed, manifest_id);
        let result = self.begin_committed_manifest_share(
            content_id,
            manifest_id,
            &manifest,
            decision.quota_exempt(),
            history,
            now_millis,
        );
        if result.is_err() {
            let _ = self.store.remove_manifest(manifest_id);
        }
        result
    }

    /// Persists origin completion after the begin operation has been exposed.
    ///
    /// # Errors
    ///
    /// Returns transfer-state or history errors.
    pub fn complete_payload_share(
        &mut self,
        transfer_id: TransferId,
        history: &mut HistoryStore,
        now_millis: u64,
    ) -> Result<StampedOperation, TransferCoordinatorError> {
        let mut completed = self
            .records
            .get(&transfer_id)
            .cloned()
            .ok_or(TransferCoordinatorError::UnknownTransfer(transfer_id))?;
        completed.complete()?;
        let operation = history.complete_manifest_share(transfer_id, now_millis)?;
        self.records.insert(transfer_id, completed);
        self.declared_complete.insert(transfer_id);
        Ok(operation)
    }

    /// Persists dominating cancellation and removes local staging/chunks.
    ///
    /// # Errors
    ///
    /// Returns transfer-state, history, or cleanup errors.
    pub fn cancel(
        &mut self,
        transfer_id: TransferId,
        history: &mut HistoryStore,
        now_millis: u64,
    ) -> Result<StampedOperation, TransferCoordinatorError> {
        let operation = history.cancel_manifest_share(transfer_id, now_millis)?;
        self.apply_cancellation(transfer_id)?;
        Ok(operation)
    }

    /// Rebuilds resumable local state from the durable replicated projection.
    ///
    /// # Errors
    ///
    /// Returns malformed-state, chunk-store, authentication, or transition errors.
    pub fn reconcile_projection(
        &mut self,
        projection: &Projection,
    ) -> Result<(), TransferCoordinatorError> {
        let mut views = projection.transfers();
        views.sort_by_key(|view| view.phase() == TransferPhase::Cancelled);
        for view in views {
            let (Some(content_id), Some(manifest_id), Some(manifest)) =
                (view.content_id(), view.manifest_id(), view.manifest())
            else {
                continue;
            };
            self.validate_manifest(manifest_id, manifest)?;
            let _ = content_id;
            if view.phase() == TransferPhase::Cancelled {
                if !self.records.contains_key(&view.transfer_id()) {
                    let record = TransferRecord::new(
                        view.transfer_id(),
                        manifest_id,
                        manifest.logical_size(),
                        &manifest.chunks(),
                        view.quota_exempt(),
                        self.limits,
                    )?;
                    self.records.insert(view.transfer_id(), record);
                }
                self.apply_cancellation(view.transfer_id())?;
                continue;
            }

            match self.store.manifest(manifest_id) {
                Ok(stored) if stored == *manifest => {
                    self.store.abandon_staged_manifest(manifest_id)?;
                }
                Ok(_) => return Err(TransferCoordinatorError::ManifestConflict(manifest_id)),
                Err(ChunkStoreError::MissingManifest(_)) => {
                    self.store.stage_incoming_manifest(manifest_id, manifest)?;
                }
                Err(error) => return Err(error.into()),
            }
            if !self.records.contains_key(&view.transfer_id()) {
                let mut record = TransferRecord::new(
                    view.transfer_id(),
                    manifest_id,
                    manifest.logical_size(),
                    &manifest.chunks(),
                    view.quota_exempt(),
                    self.limits,
                )?;
                record.begin_staging()?;
                for chunk in record.expected_chunks().collect::<Vec<_>>() {
                    if self.store.has_chunk(chunk.id()) {
                        match self.store.verify_chunk(&chunk) {
                            Ok(()) => {
                                record.mark_chunk_verified(chunk.id(), chunk.logical_size())?;
                            }
                            Err(_) => {
                                self.store.discard_unretained_chunk(chunk.id())?;
                            }
                        }
                    }
                }
                record.begin_replication()?;
                self.records.insert(view.transfer_id(), record);
            }
            if view.phase() == TransferPhase::Complete {
                self.declared_complete.insert(view.transfer_id());
                self.promote_if_ready(view.transfer_id())?;
            }
        }
        Ok(())
    }

    /// Returns deterministic bounded missing chunk work for any connected peer.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero bound.
    pub fn missing_chunks(
        &self,
        maximum: usize,
    ) -> Result<Vec<TransferChunk>, TransferCoordinatorError> {
        if maximum == 0 {
            return Err(TransferCoordinatorError::InvalidLimit);
        }
        let mut requests = Vec::new();
        for record in self.records.values() {
            if matches!(
                record.phase(),
                TransferPhase::Cancelled | TransferPhase::Failed
            ) {
                continue;
            }
            for chunk_id in record.missing_chunks(maximum - requests.len())? {
                let logical_size = record
                    .chunk_logical_size(chunk_id)
                    .ok_or(TransferCoordinatorError::UnknownChunk(chunk_id))?;
                requests.push(TransferChunk {
                    transfer_id: record.id(),
                    manifest_id: record.manifest_id(),
                    chunk_id,
                    logical_size,
                });
                if requests.len() == maximum {
                    return Ok(requests);
                }
            }
        }
        Ok(requests)
    }

    /// Exports one fixed-size encrypted object for a dedicated QUIC stream.
    ///
    /// # Errors
    ///
    /// Returns unknown/cancelled transfer, chunk validation, storage, or I/O errors.
    pub fn export_chunk(
        &self,
        request: TransferChunk,
        cancellation: &CancellationToken,
    ) -> Result<Vec<u8>, TransferCoordinatorError> {
        let record = self.records.get(&request.transfer_id).ok_or(
            TransferCoordinatorError::UnknownTransfer(request.transfer_id),
        )?;
        if record.phase() == TransferPhase::Cancelled {
            return Err(TransferCoordinatorError::Cancelled(request.transfer_id));
        }
        if record.manifest_id() != request.manifest_id
            || record.chunk_logical_size(request.chunk_id) != Some(request.logical_size)
        {
            return Err(TransferCoordinatorError::UnknownChunk(request.chunk_id));
        }
        let capacity = usize::try_from(self.store.encrypted_chunk_bytes())
            .map_err(|_| TransferCoordinatorError::InvalidLimit)?;
        let mut encrypted = Vec::with_capacity(capacity);
        self.store
            .export_encrypted_chunk(request.chunk_id, &mut encrypted, cancellation)?;
        Ok(encrypted)
    }

    /// Authenticates and installs one encrypted object, preserving verified
    /// progress across restart and promoting a fully available manifest.
    ///
    /// # Errors
    ///
    /// Returns unknown/cancelled transfer, malformed data, authentication, or
    /// catalog errors. Corrupt input never advances progress.
    pub fn import_chunk(
        &mut self,
        request: TransferChunk,
        encrypted: &[u8],
        cancellation: &CancellationToken,
    ) -> Result<bool, TransferCoordinatorError> {
        let record = self.records.get_mut(&request.transfer_id).ok_or(
            TransferCoordinatorError::UnknownTransfer(request.transfer_id),
        )?;
        if record.phase() == TransferPhase::Cancelled {
            return Err(TransferCoordinatorError::Cancelled(request.transfer_id));
        }
        if record.manifest_id() != request.manifest_id
            || record.chunk_logical_size(request.chunk_id) != Some(request.logical_size)
        {
            return Err(TransferCoordinatorError::UnknownChunk(request.chunk_id));
        }
        if u64::try_from(encrypted.len()).ok() != Some(self.store.encrypted_chunk_bytes()) {
            return Err(TransferCoordinatorError::InvalidEncryptedSize);
        }
        let imported = self.store.import_encrypted_chunk(
            request.chunk_id,
            request.logical_size,
            &mut std::io::Cursor::new(encrypted),
            cancellation,
        );
        if let Err(error) = imported {
            let _ = self.store.discard_unretained_chunk(request.chunk_id);
            return Err(error.into());
        }
        let changed = record.mark_chunk_verified(request.chunk_id, request.logical_size)?;
        self.promote_if_ready(request.transfer_id)?;
        Ok(changed)
    }

    #[must_use]
    pub fn progress(&self) -> Vec<TransferProgress> {
        self.records
            .values()
            .map(|record| TransferProgress {
                transfer_id: record.id(),
                manifest_id: record.manifest_id(),
                phase: record.phase(),
                logical_size: record.logical_size(),
                verified_bytes: record.verified_bytes(),
                verified_chunks: record.verified_chunk_count(),
                expected_chunks: record.expected_chunk_count(),
                quota_exempt: record.quota_exempt(),
            })
            .collect()
    }

    /// Reconstructs an authenticated MIME bundle or safe file snapshot.
    ///
    /// # Errors
    ///
    /// Returns unavailable/cancelled content, authentication, identity,
    /// clipboard validation, free-space, or materialization errors.
    pub fn activate(
        &self,
        content_id: ContentId,
        projection: &Projection,
        content_key: &[u8; 32],
        maximum_bytes: u64,
        cancellation: &CancellationToken,
    ) -> Result<ActivatedClipboard, TransferCoordinatorError> {
        let (_, manifest_id, _) = projection
            .completed_manifest_for_content(content_id)
            .ok_or(TransferCoordinatorError::ContentUnavailable(content_id))?;
        let manifest = self.store.manifest(manifest_id)?;
        match manifest {
            StoredManifest::MimeBundle(bundle) => {
                let representations = self
                    .store
                    .read_mime_bundle(&bundle, cancellation)?
                    .into_iter()
                    .map(|(mime, bytes)| Representation::new(mime, bytes))
                    .collect::<Vec<_>>();
                let payload = Payload::new(content_key, representations)?;
                if payload.descriptor().content_id() != content_id {
                    return Err(TransferCoordinatorError::ContentIdentityMismatch);
                }
                let content = payload
                    .representations()
                    .iter()
                    .map(|representation| {
                        Ok(ClipboardRepresentation::new(
                            MimeType::new(representation.mime())?,
                            representation.bytes(),
                        ))
                    })
                    .collect::<Result<Vec<_>, MimeTypeError>>()?;
                Ok(ActivatedClipboard {
                    content: ClipboardContent::new_with_limit(content, maximum_bytes)?,
                    materialized_manifest: None,
                })
            }
            StoredManifest::Files(_) => {
                let materialization =
                    self.materializer
                        .materialize(&self.store, manifest_id, cancellation)?;
                let content = ClipboardContent::new_with_limit(
                    vec![ClipboardRepresentation::new(
                        MimeType::new("text/uri-list")?,
                        materialization.uri_list(),
                    )],
                    maximum_bytes,
                )?;
                Ok(ActivatedClipboard {
                    content,
                    materialized_manifest: Some(manifest_id),
                })
            }
            StoredManifest::Blob(_) => Err(TransferCoordinatorError::UnsupportedManifest),
        }
    }

    /// Cleans a prior file activation after clipboard ownership changes.
    ///
    /// # Errors
    ///
    /// Returns safe materialization cleanup errors.
    pub fn cleanup_materialization(
        &self,
        manifest_id: ManifestId,
    ) -> Result<bool, TransferCoordinatorError> {
        self.materializer.cleanup(manifest_id).map_err(Into::into)
    }

    fn complete_local_record(
        &self,
        transfer_id: TransferId,
        manifest_id: ManifestId,
        manifest: &StoredManifest,
        quota_exempt: bool,
    ) -> Result<TransferRecord, TransferCoordinatorError> {
        let mut record = TransferRecord::new(
            transfer_id,
            manifest_id,
            manifest.logical_size(),
            &manifest.chunks(),
            quota_exempt,
            self.limits,
        )?;
        record.begin_staging()?;
        for chunk in record.expected_chunks().collect::<Vec<_>>() {
            self.store.verify_chunk(&chunk)?;
            record.mark_chunk_verified(chunk.id(), chunk.logical_size())?;
        }
        record.begin_replication()?;
        Ok(record)
    }

    fn promote_if_ready(
        &mut self,
        transfer_id: TransferId,
    ) -> Result<(), TransferCoordinatorError> {
        if !self.declared_complete.contains(&transfer_id) {
            return Ok(());
        }
        let record = self
            .records
            .get_mut(&transfer_id)
            .ok_or(TransferCoordinatorError::UnknownTransfer(transfer_id))?;
        if !record.missing_chunks(1)?.is_empty() || record.phase() == TransferPhase::Complete {
            return Ok(());
        }
        match self.store.manifest(record.manifest_id()) {
            Ok(_) => {}
            Err(ChunkStoreError::MissingManifest(_)) => {
                self.store.promote_staged_manifest(record.manifest_id())?;
            }
            Err(error) => return Err(error.into()),
        }
        record.complete()?;
        Ok(())
    }

    fn apply_cancellation(
        &mut self,
        transfer_id: TransferId,
    ) -> Result<(), TransferCoordinatorError> {
        let record = self
            .records
            .get_mut(&transfer_id)
            .ok_or(TransferCoordinatorError::UnknownTransfer(transfer_id))?;
        let manifest_id = record.manifest_id();
        if record.phase() == TransferPhase::Complete {
            // Replicated cancellation dominates local availability even if
            // completion arrived first due to operation reordering.
            let mut replacement = TransferRecord::new(
                record.id(),
                record.manifest_id(),
                record.logical_size(),
                &record.expected_chunks().collect::<Vec<_>>(),
                record.quota_exempt(),
                self.limits,
            )?;
            replacement.cancel()?;
            *record = replacement;
        } else {
            record.cancel()?;
        }
        self.declared_complete.remove(&transfer_id);
        let shared_elsewhere = self.records.iter().any(|(id, other)| {
            *id != transfer_id
                && other.manifest_id() == manifest_id
                && other.phase() != TransferPhase::Cancelled
        });
        if !shared_elsewhere {
            self.store.abandon_staged_manifest(manifest_id)?;
            let _ = self.store.remove_manifest(manifest_id)?;
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
