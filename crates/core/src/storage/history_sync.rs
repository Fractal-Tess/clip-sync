use std::collections::BTreeSet;

use crate::{
    model::{
        Acknowledgements, ApplyOutcome, ContentId, HlcTimestamp, NodeId, OpId, SeenOps,
        StampedOperation,
    },
    replica::Replica,
};

use super::{
    EncryptedStorage, Result as StorageResult,
    history::{HistoryError, HistoryStore},
};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompactionReport {
    pub(super) compacted_tombstones: Vec<ContentId>,
    pub(super) compacted_operations: Vec<OpId>,
    pub(super) removed_acknowledgements: Vec<NodeId>,
}

impl CompactionReport {
    #[must_use]
    pub fn tombstones(&self) -> &[ContentId] {
        &self.compacted_tombstones
    }

    #[must_use]
    pub fn operations(&self) -> &[OpId] {
        &self.compacted_operations
    }

    #[must_use]
    pub fn removed_acknowledgements(&self) -> &[NodeId] {
        &self.removed_acknowledgements
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.compacted_tombstones.is_empty()
            && self.compacted_operations.is_empty()
            && self.removed_acknowledgements.is_empty()
    }
}

impl HistoryStore {
    /// Ingests and durably stores one peer operation and its HLC observation.
    ///
    /// # Errors
    ///
    /// Returns projection, clock, identity-conflict, or storage errors without
    /// changing live state.
    pub fn ingest(
        &mut self,
        operation: &StampedOperation,
        now_millis: u64,
    ) -> std::result::Result<ApplyOutcome, HistoryError> {
        let outcome = self.replica.ingest(operation, now_millis)?;
        if outcome == ApplyOutcome::Duplicate {
            return Ok(outcome);
        }
        let persisted = self
            .storage
            .append_ingested_operation(operation, self.replica.last_timestamp());
        if let Err(error) = persisted {
            self.restore_from_storage()?;
            return Err(error.into());
        }
        Ok(outcome)
    }

    /// Ingests a peer batch atomically and publishes the reconstructed state
    /// only after the complete encrypted transaction commits.
    ///
    /// # Errors
    ///
    /// Returns projection, clock, identity-conflict, or storage errors without
    /// changing live state.
    pub fn ingest_batch(
        &mut self,
        operations: &[StampedOperation],
        now_millis: u64,
    ) -> std::result::Result<Vec<ApplyOutcome>, HistoryError> {
        self.ingest_then_persist(operations, now_millis, |storage, observed| {
            storage.append_remote_operations(operations, observed)
        })
    }

    /// Ingests operations and the frontier/membership advertisement from the
    /// authenticated session peer in one durable transaction.
    ///
    /// New operations authored by an already-forgotten identity are rejected;
    /// forwarding operations from other active members remains allowed.
    ///
    /// # Errors
    ///
    /// Returns a model, identity, serialization, or storage error without
    /// changing live state or acknowledgement metadata.
    pub fn ingest_authenticated_batch(
        &mut self,
        peer: NodeId,
        peer_frontier: &SeenOps,
        known_members: &BTreeSet<NodeId>,
        operations: &[StampedOperation],
        now_millis: u64,
    ) -> std::result::Result<Vec<ApplyOutcome>, HistoryError> {
        for operation in operations {
            if self
                .replica
                .projection()
                .is_device_forgotten(operation.id().node())
                && !self
                    .replica
                    .projection()
                    .seen_ops()
                    .contains(operation.id())
            {
                return Err(HistoryError::ForgottenOperationOrigin(
                    operation.id().node(),
                ));
            }
        }

        self.ingest_then_persist(operations, now_millis, |storage, observed| {
            storage.append_authenticated_peer_batch(
                peer,
                peer_frontier,
                known_members,
                operations,
                observed,
            )
        })
    }

    /// Applies a batch to the live replica, then persists it. Every operation
    /// is vetted before the first is applied, so a malformed or clock-skewed
    /// peer cannot force a rebuild; only a failed write or an exhausted clock
    /// falls back to reloading the replica from storage.
    fn ingest_then_persist<T>(
        &mut self,
        operations: &[StampedOperation],
        now_millis: u64,
        persist: impl FnOnce(&mut EncryptedStorage, HlcTimestamp) -> StorageResult<T>,
    ) -> std::result::Result<Vec<ApplyOutcome>, HistoryError> {
        for operation in operations {
            Replica::check_remote(operation, now_millis)?;
        }
        let outcomes = operations
            .iter()
            .map(|operation| self.replica.ingest(operation, now_millis))
            .collect::<std::result::Result<Vec<_>, _>>();
        let result = outcomes.map_err(HistoryError::from).and_then(|outcomes| {
            persist(&mut self.storage, self.replica.last_timestamp())?;
            Ok(outcomes)
        });
        if result.is_err() {
            self.restore_from_storage()?;
        }
        result
    }

    /// Monotonically persists a peer's anti-entropy acknowledgement.
    ///
    /// # Errors
    ///
    /// Returns serialization, corruption, or database errors.
    pub fn record_peer_acknowledgement(
        &mut self,
        peer: NodeId,
        seen: &SeenOps,
    ) -> std::result::Result<(), HistoryError> {
        self.storage.record_peer_acknowledgement(peer, seen)?;
        Ok(())
    }

    /// Loads persisted acknowledgement frontiers.
    ///
    /// # Errors
    ///
    /// Returns deserialization, corruption, or database errors.
    pub fn acknowledgements(&self) -> std::result::Result<Acknowledgements, HistoryError> {
        self.storage.acknowledgements().map_err(Into::into)
    }

    /// Compacts only currently-deleted content whose tombstone is covered by
    /// every active known member's durable acknowledgement. The compacted seen
    /// summary, operation deletion, and forgotten-peer acknowledgement cleanup
    /// commit atomically.
    ///
    /// # Errors
    ///
    /// Returns an error without changing live state when acknowledgement
    /// loading, seen-summary serialization, or storage mutation fails.
    pub fn compact_acknowledged_tombstones(
        &mut self,
    ) -> std::result::Result<CompactionReport, HistoryError> {
        let acknowledgements = self.storage.acknowledgements()?;
        let local = self.replica.node_id();
        let compacted_tombstones = self
            .replica
            .projection()
            .collectable_tombstones(local, &acknowledgements)
            .into_iter()
            .map(crate::model::TombstoneView::content_id)
            .collect::<Vec<_>>();
        let content_ids = compacted_tombstones
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let forgotten_members = self
            .replica
            .projection()
            .stably_forgotten_devices(local, &acknowledgements)
            .into_iter()
            .filter(|member| acknowledgements.peer(*member).is_some())
            .collect::<BTreeSet<_>>();

        if content_ids.is_empty() && forgotten_members.is_empty() {
            return Ok(CompactionReport::default());
        }

        let mut projection = self.replica.projection().clone();
        projection.remove_compacted_tombstones(&content_ids);
        let compacted_operations =
            self.storage
                .compact_tombstones(&projection, &content_ids, &forgotten_members)?;
        self.replica = Replica::restore(
            self.replica.node_id(),
            self.replica.last_counter(),
            self.replica.last_timestamp(),
            projection,
        );
        Ok(CompactionReport {
            compacted_tombstones,
            compacted_operations,
            removed_acknowledgements: forgotten_members.into_iter().collect(),
        })
    }
}
