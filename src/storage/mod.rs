use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::path::Path;

use hkdf::Hkdf;
use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior};
use sha2::Sha256;
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{
    model::{
        Acknowledgements, ApplyOutcome, ContentId, HlcTimestamp, NodeId, OpId, Payload, Projection,
        ProjectionError, SeenOps, SharedSetting, StampedOperation,
    },
    replica::{Replica, ReplicaError},
};

const SCHEMA_VERSION: u32 = 3;
const OPERATION_ENCODING_VERSION: i64 = 1;
const SQLCIPHER_KEY_BYTES: usize = 32;
const SQLCIPHER_KEY_HEX_CHARS: usize = SQLCIPHER_KEY_BYTES * 2;
const MAX_SQLITE_INTEGER: u64 = i64::MAX as u64;

const MIGRATION_1: &str = "
    BEGIN IMMEDIATE;
    CREATE TABLE storage_meta (
        key TEXT PRIMARY KEY NOT NULL CHECK (length(key) > 0),
        value TEXT NOT NULL
    ) STRICT;
    INSERT INTO storage_meta (key, value) VALUES ('schema_version', '1');
    PRAGMA user_version = 1;
    COMMIT;
";

const MIGRATION_2: &str = "
    BEGIN IMMEDIATE;
    CREATE TABLE operations (
        origin_node BLOB NOT NULL CHECK (length(origin_node) = 16),
        counter INTEGER NOT NULL CHECK (counter BETWEEN 1 AND 9223372036854775807),
        hlc_physical_millis INTEGER NOT NULL
            CHECK (hlc_physical_millis BETWEEN 0 AND 9223372036854775807),
        hlc_logical INTEGER NOT NULL CHECK (hlc_logical BETWEEN 0 AND 4294967295),
        encoding_version INTEGER NOT NULL CHECK (encoding_version = 1),
        payload BLOB NOT NULL,
        PRIMARY KEY (origin_node, counter)
    ) STRICT, WITHOUT ROWID;
    CREATE INDEX operations_event_order
        ON operations (hlc_physical_millis, hlc_logical, origin_node, counter);
    CREATE TABLE local_replica (
        singleton INTEGER PRIMARY KEY NOT NULL CHECK (singleton = 1),
        node_id BLOB NOT NULL UNIQUE CHECK (length(node_id) = 16),
        next_operation_counter INTEGER NOT NULL
            CHECK (next_operation_counter BETWEEN 1 AND 9223372036854775807),
        last_hlc_physical_millis INTEGER NOT NULL
            CHECK (last_hlc_physical_millis BETWEEN 0 AND 9223372036854775807),
        last_hlc_logical INTEGER NOT NULL CHECK (last_hlc_logical BETWEEN 0 AND 4294967295)
    ) STRICT;
    UPDATE storage_meta SET value = '2' WHERE key = 'schema_version';
    PRAGMA user_version = 2;
    COMMIT;
";

const MIGRATION_3: &str = "
    BEGIN IMMEDIATE;
    CREATE TABLE peer_acknowledgements (
        peer_node BLOB PRIMARY KEY NOT NULL CHECK (length(peer_node) = 16),
        frontier BLOB NOT NULL
    ) STRICT, WITHOUT ROWID;
    UPDATE storage_meta SET value = '3' WHERE key = 'schema_version';
    PRAGMA user_version = 3;
    COMMIT;
";

pub type Result<T> = std::result::Result<T, StorageError>;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("storage key must be 32 bytes")]
    InvalidKeyLength,

    #[error("storage key derivation failed")]
    KeyDerivation,

    #[error("SQLCipher is not available")]
    CipherUnavailable,

    #[error("SQLite FTS5 is not available")]
    Fts5Unavailable,

    #[error("storage database could not be opened with the supplied key")]
    InvalidKey,

    #[error("storage database is not a regular, non-symlink file owned by the current user")]
    UnsafeDatabaseFile,

    #[error("storage schema is incompatible: {0}")]
    IncompatibleSchema(String),

    #[error("operation {0} already exists with different serialized bytes")]
    OperationConflict(OpId),

    #[error("{field} value {value} exceeds SQLite's signed integer range")]
    IntegerOutOfRange { field: &'static str, value: u64 },

    #[error("operation counter is exhausted")]
    CounterExhausted,

    #[error("local operation belongs to node {operation}, not local node {local}")]
    ReplicaNodeMismatch { operation: NodeId, local: NodeId },

    #[error("new remote operation claims the local node identity {0}")]
    RemoteOperationClaimsLocalIdentity(NodeId),

    #[error("local operation counter must be {expected}, got {actual}")]
    UnexpectedOperationCounter { expected: u64, actual: u64 },

    #[error("local operation timestamp {operation:?} does not advance past persisted HLC {last:?}")]
    HlcRegression {
        operation: HlcTimestamp,
        last: HlcTimestamp,
    },

    #[error(
        "observed HLC {observed:?} must advance past operation {operation:?} and persisted HLC {last:?}"
    )]
    InvalidObservedHlc {
        observed: HlcTimestamp,
        operation: HlcTimestamp,
        last: HlcTimestamp,
    },

    #[error("received a new peer operation claiming the local node identity {0}")]
    LocalOriginIngest(NodeId),

    #[error("persisted local operation log does not match replica metadata: {0}")]
    LocalOperationLogMismatch(String),

    #[error("serialized operation is invalid: {0}")]
    OperationDeserialization(#[source] serde_json::Error),

    #[error("stored operation is inconsistent: {0}")]
    CorruptOperation(String),

    #[error("stored local replica metadata is invalid: {0}")]
    CorruptReplicaMetadata(String),

    #[error("operation serialization failed: {0}")]
    OperationSerialization(#[source] serde_json::Error),

    #[error("acknowledgement serialization failed: {0}")]
    AcknowledgementSerialization(#[source] serde_json::Error),

    #[error("stored acknowledgement is invalid: {0}")]
    AcknowledgementDeserialization(#[source] serde_json::Error),

    #[error(transparent)]
    Projection(#[from] ProjectionError),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
}

#[derive(Clone)]
pub struct StorageKey {
    bytes: Zeroizing<[u8; SQLCIPHER_KEY_BYTES]>,
}

impl StorageKey {
    #[must_use]
    pub fn from_bytes(bytes: [u8; SQLCIPHER_KEY_BYTES]) -> Self {
        Self {
            bytes: Zeroizing::new(bytes),
        }
    }

    /// Copies exactly 32 key bytes into zeroizing storage.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::InvalidKeyLength`] for any other length.
    pub fn try_from_slice(bytes: &[u8]) -> Result<Self> {
        let bytes: [u8; SQLCIPHER_KEY_BYTES] = bytes
            .try_into()
            .map_err(|_| StorageError::InvalidKeyLength)?;
        Ok(Self::from_bytes(bytes))
    }

    /// Derives a domain-separated `SQLCipher` key from a high-entropy secret.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::KeyDerivation`] if HKDF expansion fails.
    pub fn derive_from_secret(secret: &[u8], salt: &[u8]) -> Result<Self> {
        let hkdf = Hkdf::<Sha256>::new(Some(salt), secret);
        let mut bytes = Zeroizing::new([0_u8; SQLCIPHER_KEY_BYTES]);
        hkdf.expand(b"clip-sync/storage/sqlcipher-key/v1", bytes.as_mut())
            .map_err(|_| StorageError::KeyDerivation)?;
        Ok(Self { bytes })
    }

    fn as_bytes(&self) -> &[u8; SQLCIPHER_KEY_BYTES] {
        &self.bytes
    }
}

impl TryFrom<&[u8]> for StorageKey {
    type Error = StorageError;

    fn try_from(value: &[u8]) -> Result<Self> {
        Self::try_from_slice(value)
    }
}

/// Durable state used to stamp the next operation created by this replica.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplicaMetadata {
    node_id: NodeId,
    next_operation_counter: u64,
    last_hlc: HlcTimestamp,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppendOutcome {
    Inserted,
    AlreadyPresent,
}

pub struct EncryptedStorage {
    connection: Connection,
}

impl EncryptedStorage {
    /// Opens or initializes an encrypted database, applies migrations, and
    /// creates the persistent local replica identity on first open.
    ///
    /// # Errors
    ///
    /// Returns an error for I/O failures, wrong keys, unavailable SQLCipher/FTS5,
    /// incompatible schemas, or invalid persisted metadata.
    pub fn open(path: impl AsRef<Path>, key: &StorageKey) -> Result<Self> {
        let path = path.as_ref();
        let should_initialize = should_initialize(path)?;
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;

        apply_key(&connection, key)?;
        verify_sqlcipher(&connection)?;
        configure_connection(&connection)?;

        if should_initialize {
            verify_fts5(&connection)?;
            apply_migrations(&connection, 0)?;
        } else {
            let schema_version = existing_schema_version(&connection)?;
            verify_fts5(&connection)?;
            apply_migrations(&connection, schema_version)?;
        }
        verify_current_schema(&connection)?;
        ensure_local_replica(&connection)?;

        Ok(Self { connection })
    }

    /// Returns the active `SQLCipher` version.
    ///
    /// # Errors
    ///
    /// Returns an error when `SQLCipher` does not answer the version pragma.
    pub fn cipher_version(&self) -> Result<String> {
        cipher_version(&self.connection)
    }

    /// Returns the durable identity, next one-based counter, and last local HLC.
    ///
    /// # Errors
    ///
    /// Returns an error if the metadata row is missing or malformed.
    pub fn replica_metadata(&self) -> Result<ReplicaMetadata> {
        read_replica_metadata(&self.connection)
    }

    /// Alias emphasizing that the metadata belongs to this local replica.
    ///
    /// # Errors
    ///
    /// Returns an error if the metadata row is missing or malformed.
    pub fn local_replica_metadata(&self) -> Result<ReplicaMetadata> {
        self.replica_metadata()
    }

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
            if insert_serialized_operation(&transaction, operation, bytes)?
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
        let mut projection = Projection::default();
        projection.apply_all(&operations)?;
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
        let mut projection = Projection::default();
        projection.apply_all(&operations)?;
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

    /// Monotonically records the anti-entropy frontier acknowledged by a peer.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed existing data, serialization, or SQL.
    pub fn record_peer_acknowledgement(&mut self, peer: NodeId, seen: &SeenOps) -> Result<()> {
        let mut acknowledgements = self.acknowledgements()?;
        acknowledgements.record(peer, seen);
        let merged = acknowledgements.peer(peer).ok_or_else(|| {
            StorageError::CorruptReplicaMetadata(
                "peer acknowledgement disappeared during merge".to_owned(),
            )
        })?;
        let encoded =
            serde_json::to_vec(merged).map_err(StorageError::AcknowledgementSerialization)?;
        let peer_bytes = *peer.as_uuid().as_bytes();
        self.connection.execute(
            "INSERT INTO peer_acknowledgements (peer_node, frontier)
             VALUES (?1, ?2)
             ON CONFLICT(peer_node) DO UPDATE SET frontier = excluded.frontier",
            (&peer_bytes[..], encoded),
        )?;
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
        Ok(acknowledgements)
    }

    /// Sets a metadata value. This compatibility API is retained for the
    /// `SQLCipher` at-rest validation probe.
    ///
    /// # Errors
    ///
    /// Returns an error when the encrypted database write fails.
    pub fn set_meta_value(&mut self, key: &str, value: &str) -> Result<()> {
        self.connection.execute(
            "INSERT INTO storage_meta (key, value)
             VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            (key, value),
        )?;
        Ok(())
    }

    /// Reads a metadata value.
    ///
    /// # Errors
    ///
    /// Returns an error when the encrypted query fails or the key is invalid.
    pub fn meta_value(&self, key: &str) -> Result<Option<String>> {
        self.connection
            .query_row(
                "SELECT value FROM storage_meta WHERE key = ?1",
                [key],
                |row| row.get(0),
            )
            .optional()
            .map_err(normalize_key_error)
    }

    /// Checkpoints and truncates the encrypted WAL.
    ///
    /// # Errors
    ///
    /// Returns an error when `SQLite` cannot complete the checkpoint.
    pub fn checkpoint(&self) -> Result<()> {
        self.connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
    }

    /// Closes the database and reports deferred `SQLite` failures.
    ///
    /// # Errors
    ///
    /// Returns an error when `SQLite` cannot close cleanly.
    pub fn close(self) -> Result<()> {
        self.connection.close().map_err(|(_, error)| error.into())
    }
}

/// Crash-consistent owner of encrypted storage and its in-memory replica.
///
/// Local mutations are authored against a clone, persisted transactionally,
/// and only then published to the live in-memory state. Peer ingest similarly
/// persists the HLC merge alongside the operation.
pub struct HistoryStore {
    storage: EncryptedStorage,
    replica: Replica,
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

    /// Explicitly shares payload, applying replicated oversized exemption.
    ///
    /// # Errors
    ///
    /// Returns authoring or durable-storage errors without changing live state.
    pub fn share_explicit(
        &mut self,
        payload: Payload,
        now_millis: u64,
    ) -> std::result::Result<StampedOperation, HistoryError> {
        self.commit_one(|replica| replica.share_explicit(payload, now_millis))
    }

    /// Captures payload and atomically persists its deterministic quota
    /// evictions in the same local operation transaction.
    ///
    /// # Errors
    ///
    /// Returns authoring, incomplete quota-state, or storage errors without
    /// changing live state.
    pub fn copy_and_enforce(
        &mut self,
        payload: Payload,
        now_millis: u64,
    ) -> std::result::Result<Vec<StampedOperation>, HistoryError> {
        self.commit_many(|replica| replica.copy_and_enforce(payload, now_millis))
    }

    /// Explicitly shares payload and atomically persists quota evictions for
    /// all other chargeable entries.
    ///
    /// # Errors
    ///
    /// Returns authoring, incomplete quota-state, or storage errors without
    /// changing live state.
    pub fn share_explicit_and_enforce(
        &mut self,
        payload: Payload,
        now_millis: u64,
    ) -> std::result::Result<Vec<StampedOperation>, HistoryError> {
        self.commit_many(|replica| replica.share_explicit_and_enforce(payload, now_millis))
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

    /// Updates a replicated shared setting.
    ///
    /// # Errors
    ///
    /// Returns setting validation, authoring, or durable-storage errors.
    pub fn set_shared_setting(
        &mut self,
        setting: SharedSetting,
        value: u64,
        now_millis: u64,
    ) -> std::result::Result<StampedOperation, HistoryError> {
        self.commit_one(|replica| replica.set_shared_setting(setting, value, now_millis))
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
    /// Returns incomplete-state, authoring, or durable-storage errors.
    pub fn enforce_quota(
        &mut self,
        now_millis: u64,
    ) -> std::result::Result<Vec<StampedOperation>, HistoryError> {
        self.commit_many(|replica| replica.enforce_quota(now_millis))
    }

    /// Changes the mesh quota and atomically persists its resulting evictions.
    ///
    /// # Errors
    ///
    /// Returns setting, incomplete-state, authoring, or storage errors.
    pub fn set_mesh_quota_and_enforce(
        &mut self,
        quota_bytes: u64,
        now_millis: u64,
    ) -> std::result::Result<Vec<StampedOperation>, HistoryError> {
        self.commit_many(|replica| replica.set_mesh_quota_and_enforce(quota_bytes, now_millis))
    }

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
        let mut next = self.replica.clone();
        let outcome = next.ingest(operation, now_millis)?;
        if outcome == ApplyOutcome::Duplicate {
            return Ok(outcome);
        }
        self.storage
            .append_ingested_operation(operation, next.last_timestamp())?;
        self.replica = next;
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
        let mut next = self.replica.clone();
        let outcomes = operations
            .iter()
            .map(|operation| next.ingest(operation, now_millis))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        self.storage
            .append_remote_operations(operations, next.last_timestamp())?;
        self.replica = next;
        Ok(outcomes)
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

    fn commit_one(
        &mut self,
        author: impl FnOnce(&mut Replica) -> std::result::Result<StampedOperation, ReplicaError>,
    ) -> std::result::Result<StampedOperation, HistoryError> {
        let mut next = self.replica.clone();
        let operation = author(&mut next)?;
        self.storage.append_local_operation(&operation)?;
        self.replica = next;
        Ok(operation)
    }

    fn commit_many(
        &mut self,
        author: impl FnOnce(&mut Replica) -> std::result::Result<Vec<StampedOperation>, ReplicaError>,
    ) -> std::result::Result<Vec<StampedOperation>, HistoryError> {
        let mut next = self.replica.clone();
        let operations = author(&mut next)?;
        self.storage.append_local_operations(&operations)?;
        self.replica = next;
        Ok(operations)
    }
}

#[derive(Debug, Error)]
pub enum HistoryError {
    #[error("content ID is invalid: {0}")]
    InvalidContentId(String),
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

fn sqlite_integer(field: &'static str, value: u64) -> Result<i64> {
    value
        .try_into()
        .map_err(|_| StorageError::IntegerOutOfRange { field, value })
}

fn ensure_local_replica(connection: &Connection) -> Result<()> {
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
    read_replica_metadata(connection).map(|_| ())
}

fn read_replica_metadata(connection: &Connection) -> Result<ReplicaMetadata> {
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

fn update_replica_metadata(connection: &Connection, metadata: ReplicaMetadata) -> Result<()> {
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

fn should_initialize(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                return Err(StorageError::UnsafeDatabaseFile);
            }
            restrict_database_permissions(path)?;
            Ok(metadata.len() == 0)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            create_private_database(path)?;
            Ok(true)
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn create_private_database(path: &Path) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn create_private_database(path: &Path) -> Result<()> {
    let file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn restrict_database_permissions(path: &Path) -> Result<()> {
    use rustix::fs::{FileType, Mode, OFlags, fchmod, fstat, open};

    let fd = open(
        path,
        OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)?;
    let stat = fstat(&fd).map_err(std::io::Error::from)?;
    if !FileType::from_raw_mode(stat.st_mode).is_file()
        || stat.st_uid != rustix::process::getuid().as_raw()
    {
        return Err(StorageError::UnsafeDatabaseFile);
    }
    fchmod(&fd, Mode::RUSR | Mode::WUSR).map_err(std::io::Error::from)?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_database_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

fn apply_key(connection: &Connection, key: &StorageKey) -> Result<()> {
    let mut pragma = Zeroizing::new(String::with_capacity(
        "PRAGMA cipher_log_level = NONE; PRAGMA key = \"x''\";".len() + SQLCIPHER_KEY_HEX_CHARS,
    ));
    pragma.push_str("PRAGMA cipher_log_level = NONE; PRAGMA key = \"x'");
    for byte in key.as_bytes() {
        write!(pragma, "{byte:02x}").map_err(|_| StorageError::KeyDerivation)?;
    }
    pragma.push_str("'\";");

    connection.execute_batch(pragma.as_str())?;
    Ok(())
}

fn configure_connection(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "
        PRAGMA temp_store = MEMORY;
        PRAGMA foreign_keys = ON;
        ",
    )?;

    let temp_store: i64 = connection.query_row("PRAGMA temp_store;", [], |row| row.get(0))?;
    if temp_store != 2 {
        return Err(StorageError::IncompatibleSchema(
            "memory temp_store could not be enabled".to_owned(),
        ));
    }

    let foreign_keys_enabled: i64 =
        connection.query_row("PRAGMA foreign_keys;", [], |row| row.get(0))?;
    if foreign_keys_enabled != 1 {
        return Err(StorageError::IncompatibleSchema(
            "foreign key enforcement could not be enabled".to_owned(),
        ));
    }

    Ok(())
}

fn verify_sqlcipher(connection: &Connection) -> Result<()> {
    let version = cipher_version(connection)?;
    if version.trim().is_empty() {
        return Err(StorageError::CipherUnavailable);
    }
    Ok(())
}

fn cipher_version(connection: &Connection) -> Result<String> {
    connection
        .query_row("PRAGMA cipher_version;", [], |row| row.get(0))
        .map_err(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => StorageError::CipherUnavailable,
            error => error.into(),
        })
}

fn verify_fts5(connection: &Connection) -> Result<()> {
    connection
        .execute_batch(
            "
            CREATE VIRTUAL TABLE temp.storage_fts5_probe USING fts5(value);
            DROP TABLE temp.storage_fts5_probe;
            ",
        )
        .map_err(|_| StorageError::Fts5Unavailable)
}

fn existing_schema_version(connection: &Connection) -> Result<u32> {
    force_schema_read(connection)?;
    let schema_version = connection
        .query_row(
            "SELECT value FROM storage_meta WHERE key = 'schema_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(normalize_key_error)?
        .ok_or_else(|| StorageError::IncompatibleSchema("missing schema_version".to_owned()))?;
    let schema_version = schema_version.parse::<u32>().map_err(|_| {
        StorageError::IncompatibleSchema(format!(
            "schema_version {schema_version:?} is not an integer"
        ))
    })?;
    let user_version: u32 = connection.query_row("PRAGMA user_version;", [], |row| row.get(0))?;
    if schema_version != user_version {
        return Err(StorageError::IncompatibleSchema(format!(
            "schema_version {schema_version} disagrees with user_version {user_version}"
        )));
    }
    Ok(schema_version)
}

fn apply_migrations(connection: &Connection, current_version: u32) -> Result<()> {
    if current_version > SCHEMA_VERSION {
        return Err(StorageError::IncompatibleSchema(format!(
            "unsupported schema_version {current_version}"
        )));
    }

    if current_version < 1 {
        connection.execute_batch(MIGRATION_1)?;
    }
    if current_version < 2 {
        connection.execute_batch(MIGRATION_2)?;
    }
    if current_version < 3 {
        connection.execute_batch(MIGRATION_3)?;
    }
    Ok(())
}

fn verify_current_schema(connection: &Connection) -> Result<()> {
    let version = existing_schema_version(connection)?;
    if version != SCHEMA_VERSION {
        return Err(StorageError::IncompatibleSchema(format!(
            "unsupported schema_version {version}"
        )));
    }

    for table in ["operations", "local_replica", "peer_acknowledgements"] {
        let exists = connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1
             )",
            [table],
            |row| row.get::<_, bool>(0),
        )?;
        if !exists {
            return Err(StorageError::IncompatibleSchema(format!(
                "missing {table} table"
            )));
        }
    }
    Ok(())
}

fn force_schema_read(connection: &Connection) -> Result<()> {
    connection
        .query_row("SELECT count(*) FROM sqlite_master;", [], |row| {
            row.get::<_, i64>(0)
        })
        .map(|_| ())
        .map_err(normalize_key_error)
}

fn normalize_key_error(error: rusqlite::Error) -> StorageError {
    match &error {
        rusqlite::Error::SqliteFailure(sqlite_error, message)
            if sqlite_error.code == rusqlite::ErrorCode::NotADatabase
                || sqlite_error.code == rusqlite::ErrorCode::DatabaseCorrupt
                || message
                    .as_deref()
                    .is_some_and(|message| message.contains("file is not a database")) =>
        {
            StorageError::InvalidKey
        }
        _ => error.into(),
    }
}
