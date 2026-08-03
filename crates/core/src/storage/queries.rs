use zeroize::Zeroizing;

use crate::{
    model::{Projection, StampedOperation},
    replica::Replica,
};

use super::{
    EncryptedStorage, Result, StorageError, error::sqlite_integer,
    metadata::update_replica_metadata, operations::OPERATION_ENCODING_VERSION,
};

impl EncryptedStorage {
    /// Loads all operations in deterministic event-key order.
    ///
    /// Serialized buffers are zeroized after each operation is reconstructed.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed rows, unsupported encodings, or SQL.
    pub fn load_operations(&self) -> Result<Vec<StampedOperation>> {
        let mut statement = self.connection.prepare(
            "SELECT origin_node, counter, hlc_physical_millis, hlc_logical,
                    encoding_version, payload
             FROM operations
             ORDER BY hlc_physical_millis ASC, hlc_logical ASC,
                      origin_node ASC, counter ASC",
        )?;
        let mut rows = statement.query([])?;
        let mut operations = Vec::new();

        while let Some(row) = rows.next()? {
            let origin_node: Vec<u8> = row.get(0)?;
            let counter: i64 = row.get(1)?;
            let physical: i64 = row.get(2)?;
            let logical: i64 = row.get(3)?;
            let encoding_version: i64 = row.get(4)?;
            let payload = Zeroizing::new(row.get::<_, Vec<u8>>(5)?);

            if encoding_version != OPERATION_ENCODING_VERSION {
                return Err(StorageError::CorruptOperation(format!(
                    "unsupported encoding version {encoding_version}"
                )));
            }

            let operation: StampedOperation = serde_json::from_slice(payload.as_slice())
                .map_err(StorageError::OperationDeserialization)?;
            validate_operation_row(&operation, &origin_node, counter, physical, logical)?;
            operations.push(operation);
        }

        Ok(operations)
    }

    /// Deterministically rebuilds the materialized model from the operation log.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid persisted operations or model validation.
    pub fn rebuild_projection(&self) -> Result<Projection> {
        let operations = self.load_operations()?;
        let compacted_seen = self.load_compacted_seen()?.unwrap_or_default();
        let mut projection = Projection::default();
        projection.apply_all(&operations)?;
        projection.merge_compacted_seen(&compacted_seen);
        Ok(projection)
    }

    /// Reconstructs the complete in-memory replica from the immutable log and
    /// durable authoring metadata, validating local counter continuity.
    ///
    /// # Errors
    ///
    /// Returns an error for corrupt operations, inconsistent local metadata,
    /// or a database failure while repairing an older database's persisted HLC.
    pub fn load_replica(&mut self) -> Result<Replica> {
        let operations = self.load_operations()?;
        let compacted_seen = self.load_compacted_seen()?.unwrap_or_default();
        let mut projection = Projection::default();
        projection.apply_all(&operations)?;
        projection.merge_compacted_seen(&compacted_seen);
        let mut metadata = self.replica_metadata()?;
        let last_counter = metadata
            .next_operation_counter
            .checked_sub(1)
            .ok_or_else(|| {
                StorageError::CorruptReplicaMetadata("next counter is zero".to_owned())
            })?;
        let local_frontier = projection.seen_ops().frontier(metadata.node_id);
        let local_has_gaps = projection
            .seen_ops()
            .gaps(metadata.node_id)
            .next()
            .is_some();
        if local_frontier != last_counter || local_has_gaps {
            return Err(StorageError::LocalOperationLogMismatch(format!(
                "metadata last counter is {last_counter}, log frontier is {local_frontier}"
            )));
        }

        if let Some(max_timestamp) = operations.iter().map(StampedOperation::timestamp).max()
            && max_timestamp > metadata.last_hlc
        {
            metadata.last_hlc = max_timestamp;
            update_replica_metadata(&self.connection, metadata)?;
        }

        Ok(Replica::restore(
            metadata.node_id,
            last_counter,
            metadata.last_hlc,
            projection,
        ))
    }
}

fn validate_operation_row(
    operation: &StampedOperation,
    origin_node: &[u8],
    counter: i64,
    physical: i64,
    logical: i64,
) -> Result<()> {
    let expected_node = operation.id().node().as_uuid();
    let expected_counter = sqlite_integer("operation counter", operation.id().counter())?;
    let expected_physical = sqlite_integer(
        "operation HLC physical milliseconds",
        operation.timestamp().physical_millis(),
    )?;
    let expected_logical = i64::from(operation.timestamp().logical());

    if origin_node != expected_node.as_bytes()
        || counter != expected_counter
        || physical != expected_physical
        || logical != expected_logical
    {
        return Err(StorageError::CorruptOperation(format!(
            "indexed fields do not match serialized operation {}",
            operation.id()
        )));
    }
    Ok(())
}
