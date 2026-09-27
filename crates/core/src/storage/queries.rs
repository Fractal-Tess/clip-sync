use rusqlite::OptionalExtension;
use zeroize::Zeroizing;

use crate::{
    model::{NodeId, OpId, Projection, SeenOps, StampedOperation},
    replica::Replica,
    replication::BatchLimits,
};

use super::{
    EncryptedStorage, Result, StorageError,
    error::sqlite_integer,
    metadata::update_replica_metadata,
    operations::{OPERATION_ENCODING_VERSION, decode_stored_operation},
};

/// Operations a peer is missing, bounded by [`BatchLimits`]. They are the
/// stored encoding, which is also the wire encoding, so they are sent as-is.
#[derive(Debug, Default)]
pub struct OperationBatch {
    pub operations: Vec<Vec<u8>>,
    pub has_more: bool,
}

impl EncryptedStorage {
    /// Loads all operations in deterministic event-key order.
    ///
    /// This holds the whole log in memory at once; long-running code should
    /// prefer the streaming paths ([`Self::load_replica`],
    /// [`Self::operation_batch`]).
    ///
    /// # Errors
    ///
    /// Returns an error for malformed rows, unsupported encodings, or SQL.
    pub fn load_operations(&self) -> Result<Vec<StampedOperation>> {
        let mut operations = Vec::new();
        self.for_each_operation(|operation| {
            operations.push(operation);
            Ok(())
        })?;
        Ok(operations)
    }

    /// Decodes operations one at a time in event-key order, so callers can
    /// fold the log without ever holding all of it.
    fn for_each_operation(
        &self,
        mut visit: impl FnMut(StampedOperation) -> Result<()>,
    ) -> Result<()> {
        let mut statement = self.connection.prepare(
            "SELECT origin_node, counter, hlc_physical_millis, hlc_logical,
                    encoding_version, payload
             FROM operations
             ORDER BY hlc_physical_millis ASC, hlc_logical ASC,
                      origin_node ASC, counter ASC",
        )?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let origin_node: Vec<u8> = row.get(0)?;
            let counter: i64 = row.get(1)?;
            let physical: i64 = row.get(2)?;
            let logical: i64 = row.get(3)?;
            let encoding_version: i64 = row.get(4)?;
            check_encoding(encoding_version)?;
            let payload = Zeroizing::new(row.get::<_, Vec<u8>>(5)?);
            let operation = decode_stored_operation(payload.as_slice())?;
            validate_operation_row(&operation, &origin_node, counter, physical, logical)?;
            visit(operation)?;
        }
        Ok(())
    }

    /// Loads one operation by identity, or `None` when it was compacted.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed rows, unsupported encodings, or SQL.
    pub fn load_operation(&self, id: OpId) -> Result<Option<StampedOperation>> {
        let node = *id.node().as_uuid().as_bytes();
        let counter = sqlite_integer("operation counter", id.counter())?;
        let row = self
            .connection
            .query_row(
                "SELECT hlc_physical_millis, hlc_logical, encoding_version, payload
                 FROM operations WHERE origin_node = ?1 AND counter = ?2",
                (&node[..], counter),
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        Zeroizing::new(row.get::<_, Vec<u8>>(3)?),
                    ))
                },
            )
            .optional()?;
        let Some((physical, logical, encoding_version, payload)) = row else {
            return Ok(None);
        };
        check_encoding(encoding_version)?;
        let operation = decode_stored_operation(payload.as_slice())?;
        validate_operation_row(&operation, &node, counter, physical, logical)?;
        Ok(Some(operation))
    }

    /// Collects the operations `remote` has not seen, in `(node, counter)`
    /// order, reading only rows above the peer's contiguous frontier.
    ///
    /// The first missing operation is always included even when it alone
    /// exceeds the byte limit, so an oversized operation cannot stall a peer.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed rows, unsupported encodings, or SQL.
    pub fn operation_batch(
        &self,
        local: &SeenOps,
        remote: &SeenOps,
        limits: &BatchLimits,
    ) -> Result<OperationBatch> {
        let mut batch = OperationBatch::default();
        let mut total_bytes = 0_usize;
        let mut statement = self.connection.prepare_cached(
            "SELECT counter, encoding_version, payload FROM operations
             WHERE origin_node = ?1 AND counter > ?2
             ORDER BY counter ASC",
        )?;
        let mut nodes = local.known_nodes().collect::<Vec<NodeId>>();
        nodes.sort_unstable();
        for node in nodes {
            let remote_frontier = remote.frontier(node);
            if local.frontier(node) <= remote_frontier && local.gaps(node).next().is_none() {
                continue;
            }
            let node_bytes = *node.as_uuid().as_bytes();
            let after = sqlite_integer("operation counter", remote_frontier)?;
            let mut rows = statement.query((&node_bytes[..], after))?;
            while let Some(row) = rows.next()? {
                let counter = u64::try_from(row.get::<_, i64>(0)?).map_err(|_| {
                    StorageError::CorruptOperation("negative operation counter".to_owned())
                })?;
                let id = OpId::new(node, counter)
                    .map_err(|error| StorageError::CorruptOperation(error.to_string()))?;
                if remote.contains(id) {
                    continue;
                }
                if batch.operations.len() >= limits.max_ops {
                    batch.has_more = true;
                    return Ok(batch);
                }
                check_encoding(row.get::<_, i64>(1)?)?;
                let payload = Zeroizing::new(row.get::<_, Vec<u8>>(2)?);
                if !batch.operations.is_empty()
                    && total_bytes.saturating_add(payload.len()) > limits.max_bytes
                {
                    batch.has_more = true;
                    return Ok(batch);
                }
                total_bytes = total_bytes.saturating_add(payload.len());
                batch.operations.push(payload.to_vec());
            }
        }
        Ok(batch)
    }

    /// Deterministically rebuilds the materialized model from the operation log.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid persisted operations or model validation.
    pub fn rebuild_projection(&self) -> Result<Projection> {
        let mut projection = Projection::default();
        self.for_each_operation(|operation| {
            projection.apply(&operation)?;
            Ok(())
        })?;
        let compacted_seen = self.load_compacted_seen()?.unwrap_or_default();
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
        let mut projection = Projection::default();
        let mut max_timestamp = None;
        self.for_each_operation(|operation| {
            max_timestamp = max_timestamp.max(Some(operation.timestamp()));
            projection.apply(&operation)?;
            Ok(())
        })?;
        let compacted_seen = self.load_compacted_seen()?.unwrap_or_default();
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

        if let Some(max_timestamp) = max_timestamp
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

fn check_encoding(encoding_version: i64) -> Result<()> {
    if encoding_version == OPERATION_ENCODING_VERSION {
        Ok(())
    } else {
        Err(StorageError::CorruptOperation(format!(
            "unsupported encoding version {encoding_version}"
        )))
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
