use std::collections::BTreeSet;

use tokio_util::sync::CancellationToken;

use crate::{
    model::Projection,
    payload::ChunkStoreError,
    transfer::{TransferId, TransferPhase, TransferRecord},
};

use super::{TransferChunk, TransferCoordinator, TransferCoordinatorError, TransferProgress};

impl TransferCoordinator {
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
        let retained_manifests = views
            .iter()
            .filter_map(|view| view.manifest_id())
            .collect::<BTreeSet<_>>();
        self.store
            .cleanup_untracked_manifests(&retained_manifests)?;
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

    pub(super) fn apply_cancellation(
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
