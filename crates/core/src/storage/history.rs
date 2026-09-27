use std::path::Path;

use thiserror::Error;

use crate::{
    model::{
        ContentId, NodeId, Operation, Payload, Projection, Reference, SeenOps, StampedOperation,
    },
    replica::{Replica, ReplicaError},
    replication::BatchLimits,
};

use super::{EncryptedStorage, LocalSource, OperationBatch, Result, StorageError, StorageKey};

/// Crash-consistent owner of encrypted storage and its in-memory replica.
///
/// Mutations are applied to the live replica and then persisted in one
/// transaction. If authoring or persistence fails part-way, the replica is
/// rebuilt from storage, so memory never disagrees with what survived on disk.
/// Peer ingest persists the HLC merge alongside the operation.
pub struct HistoryStore {
    pub(super) storage: EncryptedStorage,
    pub(super) replica: Replica,
}

impl HistoryStore {
    /// Opens storage and reconstructs the replica from the operation log.
    ///
    /// # Errors
    ///
    /// Returns storage validation, encryption, migration, or I/O errors.
    pub fn open(path: impl AsRef<Path>, key: &StorageKey) -> Result<Self> {
        Self::from_storage(EncryptedStorage::open(path, key)?)
    }

    /// Reconstructs a history owner from already-open storage.
    ///
    /// # Errors
    ///
    /// Returns an error if the log and durable replica metadata disagree.
    pub fn from_storage(mut storage: EncryptedStorage) -> Result<Self> {
        let replica = storage.load_replica()?;
        Ok(Self { storage, replica })
    }

    #[must_use]
    pub const fn replica(&self) -> &Replica {
        &self.replica
    }

    #[must_use]
    pub const fn projection(&self) -> &Projection {
        self.replica.projection()
    }

    #[must_use]
    pub const fn storage(&self) -> &EncryptedStorage {
        &self.storage
    }

    #[must_use]
    pub fn into_storage(self) -> EncryptedStorage {
        self.storage
    }

    /// Resets the local replica as a new empty device identity.
    ///
    /// This maintenance action does not rotate or revoke the shared mesh key.
    /// It is the supported way for a forgotten machine to join again.
    ///
    /// # Errors
    ///
    /// Returns an error without changing live state when the encrypted reset
    /// transaction fails.
    pub fn reset_identity(&mut self) -> std::result::Result<NodeId, HistoryError> {
        let last_hlc = self.replica.last_timestamp();
        let node_id = self.storage.reset_replica_identity()?;
        self.replica = Replica::restore(node_id, 0, last_hlc, Projection::default());
        Ok(node_id)
    }

    /// Adds locally captured payload or touches an exact visible duplicate.
    ///
    /// # Errors
    ///
    /// Returns authoring or durable-storage errors without changing live state.
    pub fn copy(
        &mut self,
        payload: Payload,
        now_millis: u64,
    ) -> std::result::Result<StampedOperation, HistoryError> {
        self.commit_one(|replica| replica.copy(payload, now_millis))
    }

    /// Publishes a large item whose bytes stay on this device. Where they
    /// live is recorded before the operation, so a crash between the two
    /// never leaves peers told about an item this device cannot serve.
    ///
    /// # Errors
    ///
    /// Returns authoring or durable-storage errors without changing live state.
    pub fn add_reference(
        &mut self,
        content_id: ContentId,
        reference: Reference,
        source: &LocalSource,
        now_millis: u64,
    ) -> std::result::Result<StampedOperation, HistoryError> {
        self.storage.put_local_source(content_id, source)?;
        self.commit_one(|replica| replica.add_reference(content_id, reference, now_millis))
    }

    /// Captures payload and atomically persists its deterministic quota
    /// evictions in the same local operation transaction.
    ///
    /// # Errors
    ///
    /// Returns authoring or storage errors without changing live state.
    pub fn copy_and_enforce(
        &mut self,
        payload: Payload,
        quota_bytes: u64,
        now_millis: u64,
    ) -> std::result::Result<Vec<StampedOperation>, HistoryError> {
        self.commit_many(|replica| replica.copy_and_enforce(payload, quota_bytes, now_millis))
    }

    /// Touches a visible history entry after activation.
    ///
    /// # Errors
    ///
    /// Returns visibility, authoring, or durable-storage errors.
    pub fn activate(
        &mut self,
        content_id: ContentId,
        now_millis: u64,
    ) -> std::result::Result<StampedOperation, HistoryError> {
        self.commit_one(|replica| replica.activate(content_id, now_millis))
    }

    /// Parses and activates an externally supplied content ID.
    ///
    /// # Errors
    ///
    /// Returns a typed invalid-ID, visibility, authoring, or storage error.
    pub fn activate_by_id(
        &mut self,
        content_id: &str,
        now_millis: u64,
    ) -> std::result::Result<StampedOperation, HistoryError> {
        self.activate(parse_history_content_id(content_id)?, now_millis)
    }

    /// Pins a visible item mesh-wide.
    ///
    /// # Errors
    ///
    /// Returns visibility, authoring, or durable-storage errors.
    pub fn pin(
        &mut self,
        content_id: ContentId,
        now_millis: u64,
    ) -> std::result::Result<StampedOperation, HistoryError> {
        self.commit_one(|replica| replica.pin(content_id, now_millis))
    }

    /// Parses and pins an externally supplied content ID.
    ///
    /// # Errors
    ///
    /// Returns a typed invalid-ID, visibility, authoring, or storage error.
    pub fn pin_by_id(
        &mut self,
        content_id: &str,
        now_millis: u64,
    ) -> std::result::Result<StampedOperation, HistoryError> {
        self.pin(parse_history_content_id(content_id)?, now_millis)
    }

    /// Unpins a visible item mesh-wide.
    ///
    /// # Errors
    ///
    /// Returns visibility, authoring, or durable-storage errors.
    pub fn unpin(
        &mut self,
        content_id: ContentId,
        now_millis: u64,
    ) -> std::result::Result<StampedOperation, HistoryError> {
        self.commit_one(|replica| replica.unpin(content_id, now_millis))
    }

    /// Parses and unpins an externally supplied content ID.
    ///
    /// # Errors
    ///
    /// Returns a typed invalid-ID, visibility, authoring, or storage error.
    pub fn unpin_by_id(
        &mut self,
        content_id: &str,
        now_millis: u64,
    ) -> std::result::Result<StampedOperation, HistoryError> {
        self.unpin(parse_history_content_id(content_id)?, now_millis)
    }

    /// Deletes a visible item mesh-wide.
    ///
    /// # Errors
    ///
    /// Returns visibility, authoring, or durable-storage errors.
    pub fn delete(
        &mut self,
        content_id: ContentId,
        now_millis: u64,
    ) -> std::result::Result<StampedOperation, HistoryError> {
        self.commit_one(|replica| replica.delete(content_id, now_millis))
    }

    /// Parses and deletes an externally supplied content ID.
    ///
    /// # Errors
    ///
    /// Returns a typed invalid-ID, visibility, authoring, or storage error.
    pub fn delete_by_id(
        &mut self,
        content_id: &str,
        now_millis: u64,
    ) -> std::result::Result<StampedOperation, HistoryError> {
        self.delete(parse_history_content_id(content_id)?, now_millis)
    }

    /// Replicates and persists a device-forget decision.
    ///
    /// # Errors
    ///
    /// Returns local-device validation, authoring, or durable-storage errors.
    pub fn forget_device(
        &mut self,
        node_id: NodeId,
        now_millis: u64,
    ) -> std::result::Result<StampedOperation, HistoryError> {
        self.commit_one(|replica| replica.forget_device(node_id, now_millis))
    }

    /// Authors and atomically persists all deterministic quota deletions.
    ///
    /// # Errors
    ///
    /// Returns authoring or durable-storage errors.
    pub fn enforce_quota(
        &mut self,
        quota_bytes: u64,
        now_millis: u64,
    ) -> std::result::Result<Vec<StampedOperation>, HistoryError> {
        self.commit_many(|replica| replica.enforce_quota(quota_bytes, now_millis))
    }

    fn commit_one(
        &mut self,
        author: impl FnOnce(&mut Replica) -> std::result::Result<StampedOperation, ReplicaError>,
    ) -> std::result::Result<StampedOperation, HistoryError> {
        self.commit_many(|replica| author(replica).map(|operation| vec![operation]))
            .map(|mut operations| operations.remove(0))
    }

    fn commit_many(
        &mut self,
        author: impl FnOnce(&mut Replica) -> std::result::Result<Vec<StampedOperation>, ReplicaError>,
    ) -> std::result::Result<Vec<StampedOperation>, HistoryError> {
        let before = (self.replica.last_counter(), self.replica.last_timestamp());
        let result = author(&mut self.replica)
            .map_err(HistoryError::from)
            .and_then(|operations| {
                self.storage.append_local_operations(&operations)?;
                Ok(operations)
            });
        if result.is_err() && (self.replica.last_counter(), self.replica.last_timestamp()) != before
        {
            self.restore_from_storage()?;
        }
        result
    }

    /// Replaces the live replica with the durable one after a mutation that
    /// changed memory but did not commit.
    pub(super) fn restore_from_storage(&mut self) -> std::result::Result<(), HistoryError> {
        self.replica = self.storage.load_replica()?;
        Ok(())
    }

    /// Loads the bytes of a retained inline item from the operation that
    /// carried them. Returns `None` for items whose add has not arrived and
    /// for references.
    ///
    /// # Errors
    ///
    /// Returns storage or decoding errors, or a corruption error when the
    /// referenced operation no longer carries the expected payload.
    pub fn load_payload(
        &self,
        content_id: ContentId,
    ) -> std::result::Result<Option<Payload>, HistoryError> {
        let Some(operation_id) = self.projection().item_operation(content_id) else {
            return Ok(None);
        };
        let operation = self.storage.load_operation(operation_id)?.ok_or_else(|| {
            StorageError::CorruptOperation(format!(
                "payload operation {operation_id} is missing from the log"
            ))
        })?;
        match operation.into_operation() {
            Operation::Add {
                content_id: carried,
                payload,
            } if carried == content_id => Ok(Some(payload)),
            Operation::AddReference {
                content_id: carried,
                ..
            } if carried == content_id => Ok(None),
            _ => Err(StorageError::CorruptOperation(format!(
                "operation {operation_id} does not carry the payload of {content_id}"
            ))
            .into()),
        }
    }

    /// Where this device reads a reference's bytes from, if it authored it.
    ///
    /// # Errors
    ///
    /// Returns storage or decoding errors.
    pub fn local_source(
        &self,
        content_id: ContentId,
    ) -> std::result::Result<Option<LocalSource>, HistoryError> {
        Ok(self.storage.local_source(content_id)?)
    }

    /// Operations the peer described by `remote` has not seen yet.
    ///
    /// # Errors
    ///
    /// Returns storage or decoding errors.
    pub fn operation_batch(
        &self,
        remote: &SeenOps,
        limits: &BatchLimits,
    ) -> std::result::Result<OperationBatch, HistoryError> {
        Ok(self
            .storage
            .operation_batch(self.projection().seen_ops(), remote, limits)?)
    }
}

#[derive(Debug, Error)]
pub enum HistoryError {
    #[error("content ID is invalid: {0}")]
    InvalidContentId(String),
    #[error("new operation from forgotten device identity {0} was rejected")]
    ForgottenOperationOrigin(NodeId),
    #[error(transparent)]
    Replica(#[from] ReplicaError),
    #[error(transparent)]
    Storage(#[from] StorageError),
}

fn parse_history_content_id(content_id: &str) -> std::result::Result<ContentId, HistoryError> {
    content_id
        .parse()
        .map_err(|error: crate::model::ContentIdParseError| {
            HistoryError::InvalidContentId(error.to_string())
        })
}
