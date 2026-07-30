use std::collections::BTreeSet;

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior};
use zeroize::Zeroizing;

use crate::model::{HlcTimestamp, NodeId, SeenOps, StampedOperation};

use super::{
    EncryptedStorage, Result, StorageError,
    error::sqlite_integer,
    frontier::{record_acknowledgement, record_known_member},
    metadata::{read_replica_metadata, update_replica_metadata},
};

pub(super) const OPERATION_ENCODING_VERSION: i64 = 1;
const MAX_SQLITE_INTEGER: u64 = i64::MAX as u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppendOutcome {
    Inserted,
    AlreadyPresent,
}

impl EncryptedStorage {
    /// Appends an immutable operation transactionally.
    ///
    /// An exact serialized replay is idempotent. Reusing an operation ID with
    /// different serialized bytes is rejected.
    ///
    /// # Errors
    ///
    /// Returns a typed conflict, integer-bound, serialization, or database error.
    pub fn append_operation(&mut self, operation: &StampedOperation) -> Result<AppendOutcome> {
        let serialized = serialize_operation(operation)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let outcome = insert_serialized_operation(&transaction, operation, &serialized)?;
        transaction.commit()?;
        Ok(outcome)
    }

    /// Appends a remote batch and advances the durable observed HLC atomically.
    ///
    /// Every operation is inserted in one immediate transaction. A conflict or
    /// malformed bound rolls the complete batch back. Exact replays are
    /// idempotent, but the supplied observed clock may still advance the local
    /// durable HLC so a restart cannot author an event behind a remote event.
    ///
    /// # Errors
    ///
    /// Returns an error for operation conflicts, invalid integer bounds, an HLC
    /// regression, serialization failures, or database failures.
    pub fn append_remote_operations(
        &mut self,
        operations: &[StampedOperation],
        observed_hlc: HlcTimestamp,
    ) -> Result<usize> {
        let compacted_seen = self.load_compacted_seen()?.unwrap_or_default();
        let serialized = operations
            .iter()
            .map(serialize_operation)
            .collect::<Result<Vec<_>>>()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let metadata = read_replica_metadata(&transaction)?;
        let mut inserted = 0;
        for (operation, bytes) in operations.iter().zip(&serialized) {
            if insert_remote_operation(&transaction, operation, bytes, &compacted_seen)?
                == AppendOutcome::Inserted
            {
                if operation.id().node() == metadata.node_id {
                    return Err(StorageError::RemoteOperationClaimsLocalIdentity(
                        metadata.node_id,
                    ));
                }
                inserted += 1;
            }
        }

        if observed_hlc < metadata.last_hlc {
            return Err(StorageError::HlcRegression {
                operation: observed_hlc,
                last: metadata.last_hlc,
            });
        }
        if observed_hlc > metadata.last_hlc {
            let physical =
                sqlite_integer("last_hlc.physical_millis", observed_hlc.physical_millis())?;
            let logical = i64::from(observed_hlc.logical());
            let changed = transaction.execute(
                "UPDATE local_replica
                 SET last_hlc_physical_millis = ?1,
                     last_hlc_logical = ?2
                 WHERE singleton = 1",
                (physical, logical),
            )?;
            if changed != 1 {
                return Err(StorageError::CorruptReplicaMetadata(
                    "singleton row disappeared during transaction".to_owned(),
                ));
            }
        }

        transaction.commit()?;
        Ok(inserted)
    }

    /// Atomically appends an authenticated peer batch, advances the observed
    /// HLC, records the peer's monotonic acknowledgement frontier, and unions
    /// its membership advertisement.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid operations/frontiers, identity conflicts,
    /// serialization failures, or database failures. No part commits alone.
    pub fn append_authenticated_peer_batch(
        &mut self,
        peer: NodeId,
        peer_frontier: &SeenOps,
        known_members: &BTreeSet<NodeId>,
        operations: &[StampedOperation],
        observed_hlc: HlcTimestamp,
    ) -> Result<usize> {
        let compacted_seen = self.load_compacted_seen()?.unwrap_or_default();
        let serialized = operations
            .iter()
            .map(serialize_operation)
            .collect::<Result<Vec<_>>>()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let metadata = read_replica_metadata(&transaction)?;
        let mut inserted = 0;
        for (operation, bytes) in operations.iter().zip(&serialized) {
            if insert_remote_operation(&transaction, operation, bytes, &compacted_seen)?
                == AppendOutcome::Inserted
            {
                if operation.id().node() == metadata.node_id {
                    return Err(StorageError::RemoteOperationClaimsLocalIdentity(
                        metadata.node_id,
                    ));
                }
                inserted += 1;
            }
        }

        if observed_hlc < metadata.last_hlc {
            return Err(StorageError::HlcRegression {
                operation: observed_hlc,
                last: metadata.last_hlc,
            });
        }
        if observed_hlc > metadata.last_hlc {
            let physical =
                sqlite_integer("last_hlc.physical_millis", observed_hlc.physical_millis())?;
            let logical = i64::from(observed_hlc.logical());
            let changed = transaction.execute(
                "UPDATE local_replica
                 SET last_hlc_physical_millis = ?1,
                     last_hlc_logical = ?2
                 WHERE singleton = 1",
                (physical, logical),
            )?;
            if changed != 1 {
                return Err(StorageError::CorruptReplicaMetadata(
                    "singleton row disappeared during transaction".to_owned(),
                ));
            }
        }

        record_acknowledgement(&transaction, peer, peer_frontier)?;
        record_known_member(&transaction, metadata.node_id)?;
        record_known_member(&transaction, peer)?;
        for member in known_members {
            record_known_member(&transaction, *member)?;
        }
        transaction.commit()?;
        Ok(inserted)
    }

    /// Appends an operation created by this replica and atomically advances the
    /// next operation counter and persisted HLC in the same transaction.
    ///
    /// Exact retries return [`AppendOutcome::AlreadyPresent`] without advancing
    /// metadata a second time.
    ///
    /// # Errors
    ///
    /// Returns an error for conflicts, a non-local ID, an unexpected counter,
    /// an HLC regression, exhausted/bounded integers, serialization, or SQL.
    pub fn append_local_operation(
        &mut self,
        operation: &StampedOperation,
    ) -> Result<AppendOutcome> {
        let outcomes = self.append_local_operations(std::slice::from_ref(operation))?;
        Ok(outcomes[0])
    }

    /// Appends a sequence of locally authored operations and advances replica
    /// metadata in one transaction.
    ///
    /// This is used for quota enforcement, where a setting change and several
    /// deterministic delete operations must either all survive a restart or
    /// none of them may.
    ///
    /// # Errors
    ///
    /// Returns the same validation errors as [`Self::append_local_operation`].
    pub fn append_local_operations(
        &mut self,
        operations: &[StampedOperation],
    ) -> Result<Vec<AppendOutcome>> {
        if operations.is_empty() {
            return Ok(Vec::new());
        }
        let serialized = operations
            .iter()
            .map(serialize_operation)
            .collect::<Result<Vec<_>>>()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut metadata = read_replica_metadata(&transaction)?;
        let mut outcomes = Vec::with_capacity(operations.len());
        let mut metadata_changed = false;

        for (operation, serialized) in operations.iter().zip(&serialized) {
            if operation.id().node() != metadata.node_id {
                return Err(StorageError::ReplicaNodeMismatch {
                    operation: operation.id().node(),
                    local: metadata.node_id,
                });
            }
            let outcome =
                insert_serialized_operation(&transaction, operation, serialized.as_slice())?;
            match outcome {
                AppendOutcome::Inserted => {
                    if operation.id().counter() != metadata.next_operation_counter {
                        return Err(StorageError::UnexpectedOperationCounter {
                            expected: metadata.next_operation_counter,
                            actual: operation.id().counter(),
                        });
                    }
                    if operation.timestamp() <= metadata.last_hlc {
                        return Err(StorageError::HlcRegression {
                            operation: operation.timestamp(),
                            last: metadata.last_hlc,
                        });
                    }
                    metadata.next_operation_counter = metadata
                        .next_operation_counter
                        .checked_add(1)
                        .filter(|counter| *counter <= MAX_SQLITE_INTEGER)
                        .ok_or(StorageError::CounterExhausted)?;
                    metadata.last_hlc = operation.timestamp();
                    metadata_changed = true;
                }
                AppendOutcome::AlreadyPresent => {
                    if operation.id().counter() >= metadata.next_operation_counter {
                        return Err(StorageError::LocalOperationLogMismatch(format!(
                            "operation {} exists but next counter is {}",
                            operation.id(),
                            metadata.next_operation_counter
                        )));
                    }
                }
            }
            outcomes.push(outcome);
        }

        if metadata_changed {
            update_replica_metadata(&transaction, metadata)?;
        }
        transaction.commit()?;
        Ok(outcomes)
    }

    /// Persists a newly ingested peer operation together with the HLC produced
    /// by observing it. This prevents a restart from authoring operations that
    /// sort before already observed remote events.
    ///
    /// # Errors
    ///
    /// Rejects local-origin spoofing, invalid HLC advancement, conflicts, and
    /// database failures.
    pub fn append_ingested_operation(
        &mut self,
        operation: &StampedOperation,
        observed_hlc: HlcTimestamp,
    ) -> Result<AppendOutcome> {
        let serialized = serialize_operation(operation)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut metadata = read_replica_metadata(&transaction)?;
        if operation.id().node() == metadata.node_id {
            return Err(StorageError::LocalOriginIngest(metadata.node_id));
        }
        let outcome = insert_serialized_operation(&transaction, operation, &serialized)?;
        if outcome == AppendOutcome::Inserted {
            if observed_hlc <= operation.timestamp() || observed_hlc <= metadata.last_hlc {
                return Err(StorageError::InvalidObservedHlc {
                    observed: observed_hlc,
                    operation: operation.timestamp(),
                    last: metadata.last_hlc,
                });
            }
            metadata.last_hlc = observed_hlc;
            update_replica_metadata(&transaction, metadata)?;
        }
        transaction.commit()?;
        Ok(outcome)
    }
}

fn serialize_operation(operation: &StampedOperation) -> Result<Zeroizing<Vec<u8>>> {
    sqlite_integer("operation counter", operation.id().counter())?;
    sqlite_integer(
        "operation HLC physical milliseconds",
        operation.timestamp().physical_millis(),
    )?;
    serde_json::to_vec(operation)
        .map(Zeroizing::new)
        .map_err(StorageError::OperationSerialization)
}

fn insert_serialized_operation(
    transaction: &Transaction<'_>,
    operation: &StampedOperation,
    serialized: &[u8],
) -> Result<AppendOutcome> {
    let node_bytes = *operation.id().node().as_uuid().as_bytes();
    let counter = sqlite_integer("operation counter", operation.id().counter())?;
    let physical = sqlite_integer(
        "operation HLC physical milliseconds",
        operation.timestamp().physical_millis(),
    )?;
    let logical = i64::from(operation.timestamp().logical());
    let changed = transaction.execute(
        "INSERT INTO operations (
             origin_node, counter, hlc_physical_millis, hlc_logical,
             encoding_version, payload
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(origin_node, counter) DO NOTHING",
        (
            &node_bytes[..],
            counter,
            physical,
            logical,
            OPERATION_ENCODING_VERSION,
            serialized,
        ),
    )?;

    if changed == 1 {
        return Ok(AppendOutcome::Inserted);
    }

    let existing = transaction.query_row(
        "SELECT payload FROM operations WHERE origin_node = ?1 AND counter = ?2",
        (&node_bytes[..], counter),
        |row| row.get::<_, Vec<u8>>(0),
    )?;
    let existing = Zeroizing::new(existing);
    if existing.as_slice() == serialized {
        Ok(AppendOutcome::AlreadyPresent)
    } else {
        Err(StorageError::OperationConflict(operation.id()))
    }
}

fn insert_remote_operation(
    transaction: &Transaction<'_>,
    operation: &StampedOperation,
    serialized: &[u8],
    compacted_seen: &SeenOps,
) -> Result<AppendOutcome> {
    if !compacted_seen.contains(operation.id()) {
        return insert_serialized_operation(transaction, operation, serialized);
    }

    let node_bytes = *operation.id().node().as_uuid().as_bytes();
    let counter = sqlite_integer("operation counter", operation.id().counter())?;
    let existing = transaction
        .query_row(
            "SELECT payload FROM operations WHERE origin_node = ?1 AND counter = ?2",
            (&node_bytes[..], counter),
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?;
    let Some(existing) = existing else {
        return Ok(AppendOutcome::AlreadyPresent);
    };
    let existing = Zeroizing::new(existing);
    if existing.as_slice() == serialized {
        Ok(AppendOutcome::AlreadyPresent)
    } else {
        Err(StorageError::OperationConflict(operation.id()))
    }
}
