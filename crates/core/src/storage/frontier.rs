use std::collections::BTreeSet;

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior};
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::model::{
    Acknowledgements, ContentId, NodeId, OpId, Projection, SeenOps, StampedOperation,
};

use super::{EncryptedStorage, Result, StorageError, error::sqlite_integer};

const COMPACTED_SEEN_ENCODING_VERSION: i64 = 1;

impl EncryptedStorage {
    pub(super) fn load_compacted_seen(&self) -> Result<Option<SeenOps>> {
        let row = self
            .connection
            .query_row(
                "SELECT encoding_version, payload
                 FROM compacted_seen WHERE singleton = 1",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?;
        let Some((encoding_version, payload)) = row else {
            return Ok(None);
        };
        if encoding_version != COMPACTED_SEEN_ENCODING_VERSION {
            return Err(StorageError::IncompatibleSchema(format!(
                "unsupported compacted seen encoding {encoding_version}"
            )));
        }
        let payload = Zeroizing::new(payload);
        serde_json::from_slice(payload.as_slice())
            .map(Some)
            .map_err(StorageError::CompactedSeenDeserialization)
    }

    /// Monotonically records the anti-entropy frontier acknowledged by a peer.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed existing data, serialization, or SQL.
    pub fn record_peer_acknowledgement(&mut self, peer: NodeId, seen: &SeenOps) -> Result<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        record_acknowledgement(&transaction, peer, seen)?;
        record_known_member(&transaction, peer)?;
        transaction.commit()?;
        Ok(())
    }

    /// Loads all persisted peer acknowledgement frontiers.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed node IDs/frontiers or SQL.
    pub fn acknowledgements(&self) -> Result<Acknowledgements> {
        let mut statement = self
            .connection
            .prepare("SELECT peer_node, frontier FROM peer_acknowledgements ORDER BY peer_node")?;
        let mut rows = statement.query([])?;
        let mut acknowledgements = Acknowledgements::default();
        while let Some(row) = rows.next()? {
            let peer_bytes: Vec<u8> = row.get(0)?;
            let peer = Uuid::from_slice(&peer_bytes)
                .map(NodeId::from_uuid)
                .map_err(|_| {
                    StorageError::CorruptReplicaMetadata(
                        "acknowledgement peer ID is not a UUID".to_owned(),
                    )
                })?;
            let encoded = Zeroizing::new(row.get::<_, Vec<u8>>(1)?);
            let seen: SeenOps = serde_json::from_slice(&encoded)
                .map_err(StorageError::AcknowledgementDeserialization)?;
            acknowledgements.record(peer, &seen);
        }
        drop(rows);
        drop(statement);

        let mut statement = self
            .connection
            .prepare("SELECT node_id FROM known_members ORDER BY node_id")?;
        let members = statement.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
        for member in members {
            let member = member?;
            let member = Uuid::from_slice(&member)
                .map(NodeId::from_uuid)
                .map_err(|_| {
                    StorageError::CorruptReplicaMetadata("known member ID is not a UUID".to_owned())
                })?;
            acknowledgements.record_known(member);
        }
        Ok(acknowledgements)
    }

    /// Persists the compacted-operation seen summary, deletes every operation
    /// whose content record was compacted, and drops acknowledgements
    /// belonging to stably forgotten members in one transaction.
    ///
    /// # Errors
    ///
    /// Returns an error without changing storage if snapshot encoding,
    /// operation decoding, or any database mutation fails.
    pub fn compact_tombstones(
        &mut self,
        projection: &Projection,
        content_ids: &BTreeSet<ContentId>,
        forgotten_members: &BTreeSet<NodeId>,
    ) -> Result<Vec<OpId>> {
        if content_ids.is_empty() && forgotten_members.is_empty() {
            return Ok(Vec::new());
        }
        let snapshot = serde_json::to_vec(projection.seen_ops())
            .map_err(StorageError::CompactedSeenSerialization)?;
        let operations = self.load_operations()?;
        let compacted = operations
            .iter()
            .filter(|operation| {
                operation
                    .operation()
                    .content_id()
                    .is_some_and(|content_id| content_ids.contains(&content_id))
            })
            .map(StampedOperation::id)
            .collect::<Vec<_>>();

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO compacted_seen (singleton, encoding_version, payload)
             VALUES (1, ?1, ?2)
             ON CONFLICT(singleton) DO UPDATE SET
                 encoding_version = excluded.encoding_version,
                 payload = excluded.payload",
            (COMPACTED_SEEN_ENCODING_VERSION, snapshot),
        )?;
        for operation in &compacted {
            let node = *operation.node().as_uuid().as_bytes();
            let counter = sqlite_integer("operation counter", operation.counter())?;
            transaction.execute(
                "DELETE FROM operations WHERE origin_node = ?1 AND counter = ?2",
                (&node[..], counter),
            )?;
        }
        for member in forgotten_members {
            let node = *member.as_uuid().as_bytes();
            transaction.execute(
                "DELETE FROM peer_acknowledgements WHERE peer_node = ?1",
                [&node[..]],
            )?;
        }
        transaction.commit()?;
        Ok(compacted)
    }
}

pub(super) fn record_acknowledgement(
    transaction: &Transaction<'_>,
    peer: NodeId,
    seen: &SeenOps,
) -> Result<()> {
    let peer_bytes = *peer.as_uuid().as_bytes();
    let existing = transaction
        .query_row(
            "SELECT frontier FROM peer_acknowledgements WHERE peer_node = ?1",
            [&peer_bytes[..]],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?;
    let mut merged = match existing {
        Some(encoded) => serde_json::from_slice(&encoded)
            .map_err(StorageError::AcknowledgementDeserialization)?,
        None => SeenOps::default(),
    };
    merged.merge(seen);
    let encoded =
        serde_json::to_vec(&merged).map_err(StorageError::AcknowledgementSerialization)?;
    transaction.execute(
        "INSERT INTO peer_acknowledgements (peer_node, frontier)
         VALUES (?1, ?2)
         ON CONFLICT(peer_node) DO UPDATE SET frontier = excluded.frontier",
        (&peer_bytes[..], encoded),
    )?;
    Ok(())
}

pub(super) fn record_known_member(transaction: &Transaction<'_>, member: NodeId) -> Result<()> {
    let node = *member.as_uuid().as_bytes();
    transaction.execute(
        "INSERT INTO known_members (node_id) VALUES (?1)
         ON CONFLICT(node_id) DO NOTHING",
        [&node[..]],
    )?;
    Ok(())
}
