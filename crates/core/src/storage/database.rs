use std::fmt::Write as _;
use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use zeroize::Zeroizing;

use crate::model::NodeId;

use super::{
    ReplicaMetadata, Result, StorageError, StorageKey,
    error::sqlite_integer,
    frontier::record_known_member,
    key::SQLCIPHER_KEY_HEX_CHARS,
    metadata::{ensure_local_replica, read_replica_metadata},
    schema::{
        apply_key, apply_migrations, cipher_version, configure_connection, existing_schema_version,
        normalize_key_error, should_initialize, verify_current_schema, verify_fts5,
        verify_sqlcipher,
    },
};

pub struct EncryptedStorage {
    pub(super) connection: Connection,
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

    /// Resets this database to a new, empty replica identity.
    ///
    /// This is recovery after a device was forgotten, not mesh-key
    /// revocation. The encrypted operation history and acknowledgement state
    /// are cleared so the new identity can join and reconcile as a fresh
    /// member without replaying operations attributed to the forgotten ID.
    ///
    /// # Errors
    ///
    /// Returns an error without changing storage if the reset transaction
    /// cannot commit.
    pub fn reset_replica_identity(&mut self) -> Result<NodeId> {
        let previous = read_replica_metadata(&self.connection)?;
        let replacement = NodeId::new();
        let replacement_bytes = *replacement.as_uuid().as_bytes();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute("DELETE FROM operations", [])?;
        transaction.execute("DELETE FROM peer_acknowledgements", [])?;
        transaction.execute("DELETE FROM known_members", [])?;
        transaction.execute("DELETE FROM compacted_seen", [])?;
        let physical = sqlite_integer(
            "last_hlc.physical_millis",
            previous.last_hlc.physical_millis(),
        )?;
        let logical = i64::from(previous.last_hlc.logical());
        let changed = transaction.execute(
            "UPDATE local_replica
             SET node_id = ?1,
                 next_operation_counter = 1,
                 last_hlc_physical_millis = ?2,
                 last_hlc_logical = ?3
             WHERE singleton = 1",
            (&replacement_bytes[..], physical, logical),
        )?;
        if changed != 1 {
            return Err(StorageError::CorruptReplicaMetadata(
                "singleton row disappeared during identity reset".to_owned(),
            ));
        }
        record_known_member(&transaction, replacement)?;
        transaction.commit()?;
        Ok(replacement)
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

    /// Transactionally changes the `SQLCipher` key after checkpointing the WAL.
    ///
    /// The connection is consumed so callers must reopen with the new key and
    /// verify the database before committing any external key metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when checkpointing, `SQLCipher` rekeying, integrity
    /// verification, or closing fails.
    pub fn rekey(self, new_key: &StorageKey) -> Result<()> {
        self.checkpoint()?;
        let mut pragma = Zeroizing::new(String::with_capacity(
            "PRAGMA rekey = \"x''\";".len() + SQLCIPHER_KEY_HEX_CHARS,
        ));
        pragma.push_str("PRAGMA rekey = \"x'");
        for byte in new_key.as_bytes() {
            write!(pragma, "{byte:02x}").map_err(|_| StorageError::KeyDerivation)?;
        }
        pragma.push_str("'\";");
        self.connection.execute_batch(pragma.as_str())?;
        let integrity: String = self
            .connection
            .query_row("PRAGMA quick_check;", [], |row| row.get(0))?;
        if integrity != "ok" {
            return Err(StorageError::IncompatibleSchema(format!(
                "database integrity check failed after rekey: {integrity}"
            )));
        }
        self.close()
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
