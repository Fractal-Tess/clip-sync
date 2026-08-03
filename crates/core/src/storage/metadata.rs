use rusqlite::{Connection, OptionalExtension};
use uuid::Uuid;

use crate::model::{HlcTimestamp, NodeId};

use super::{Result, StorageError, error::sqlite_integer};

/// Durable state used to stamp the next operation created by this replica.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplicaMetadata {
    pub(super) node_id: NodeId,
    pub(super) next_operation_counter: u64,
    pub(super) last_hlc: HlcTimestamp,
}

impl ReplicaMetadata {
    #[must_use]
    pub const fn node_id(self) -> NodeId {
        self.node_id
    }

    #[must_use]
    pub const fn next_operation_counter(self) -> u64 {
        self.next_operation_counter
    }

    #[must_use]
    pub const fn last_hlc(self) -> HlcTimestamp {
        self.last_hlc
    }
}

pub(super) fn ensure_local_replica(connection: &Connection) -> Result<()> {
    let node_id = NodeId::new();
    let node_bytes = *node_id.as_uuid().as_bytes();
    connection.execute(
        "INSERT INTO local_replica (
             singleton, node_id, next_operation_counter,
             last_hlc_physical_millis, last_hlc_logical
         ) VALUES (1, ?1, 1, 0, 0)
         ON CONFLICT(singleton) DO NOTHING",
        [&node_bytes[..]],
    )?;
    let metadata = read_replica_metadata(connection)?;
    let node = *metadata.node_id.as_uuid().as_bytes();
    connection.execute(
        "INSERT INTO known_members (node_id) VALUES (?1)
         ON CONFLICT(node_id) DO NOTHING",
        [&node[..]],
    )?;
    Ok(())
}

pub(super) fn read_replica_metadata(connection: &Connection) -> Result<ReplicaMetadata> {
    let row = connection
        .query_row(
            "SELECT node_id, next_operation_counter,
                    last_hlc_physical_millis, last_hlc_logical
             FROM local_replica WHERE singleton = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((node_bytes, next_counter, physical, logical)) = row else {
        return Err(StorageError::CorruptReplicaMetadata(
            "missing singleton row".to_owned(),
        ));
    };

    let uuid = Uuid::from_slice(&node_bytes).map_err(|_| {
        StorageError::CorruptReplicaMetadata("node ID is not a 16-byte UUID".to_owned())
    })?;
    let next_operation_counter = u64::try_from(next_counter)
        .map_err(|_| StorageError::CorruptReplicaMetadata("next counter is negative".to_owned()))?;
    if next_operation_counter == 0 {
        return Err(StorageError::CorruptReplicaMetadata(
            "next counter is zero".to_owned(),
        ));
    }
    let physical_millis = u64::try_from(physical).map_err(|_| {
        StorageError::CorruptReplicaMetadata("last HLC physical time is negative".to_owned())
    })?;
    let logical = u32::try_from(logical).map_err(|_| {
        StorageError::CorruptReplicaMetadata("last HLC logical value is out of range".to_owned())
    })?;

    Ok(ReplicaMetadata {
        node_id: NodeId::from_uuid(uuid),
        next_operation_counter,
        last_hlc: HlcTimestamp::new(physical_millis, logical),
    })
}

pub(super) fn update_replica_metadata(
    connection: &Connection,
    metadata: ReplicaMetadata,
) -> Result<()> {
    let next_counter = sqlite_integer("next operation counter", metadata.next_operation_counter)?;
    let physical = sqlite_integer(
        "last_hlc.physical_millis",
        metadata.last_hlc.physical_millis(),
    )?;
    let logical = i64::from(metadata.last_hlc.logical());
    let changed = connection.execute(
        "UPDATE local_replica
         SET next_operation_counter = ?1,
             last_hlc_physical_millis = ?2,
             last_hlc_logical = ?3
         WHERE singleton = 1",
        (next_counter, physical, logical),
    )?;
    if changed != 1 {
        return Err(StorageError::CorruptReplicaMetadata(
            "singleton row disappeared during transaction".to_owned(),
        ));
    }
    Ok(())
}
