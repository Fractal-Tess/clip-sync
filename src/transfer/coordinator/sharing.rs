use std::path::PathBuf;

use tokio_util::sync::CancellationToken;

use crate::{
    model::{ContentId, Payload, StampedOperation},
    payload::{
        ExplicitShareInspection, FileSnapshotLimits, ManifestId, StoredManifest, inspect_file_uris,
        snapshot_file_uris,
    },
    storage::HistoryStore,
    transfer::{TransferId, TransferRecord},
};

use super::{TransferCoordinator, TransferCoordinatorError};

impl TransferCoordinator {
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
}
