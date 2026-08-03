use std::{fs, io, path::Path};

use hkdf::Hkdf;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use sha2::Sha256;
use zeroize::Zeroizing;

use super::{
    ChunkStore, ChunkStoreConfig, ChunkStoreError, ChunkStoreKey,
    support::{create_private_dir, set_private_file_permissions},
    validation::validate_config,
};

impl ChunkStore {
    /// Authenticates an existing chunk catalog without modifying store state.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsafe path, wrong key, unavailable `SQLCipher`,
    /// corrupt schema, or database I/O failure.
    pub fn verify_key(root: impl AsRef<Path>, key: &ChunkStoreKey) -> Result<(), ChunkStoreError> {
        let catalog_path = root.as_ref().join("catalog.db");
        match fs::symlink_metadata(&catalog_path) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        }
        let catalog_key = derive_key(&key.0, b"clip-sync/chunk-store/catalog/v1")?;
        let connection = Connection::open_with_flags(
            &catalog_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        apply_catalog_key(&connection, &catalog_key)?;
        for table in ["chunk_catalog", "manifests"] {
            let exists = connection
                .query_row(
                    "SELECT EXISTS(
                         SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1
                     )",
                    [table],
                    |row| row.get::<_, bool>(0),
                )
                .map_err(normalize_catalog_error)?;
            if !exists {
                return Err(ChunkStoreError::CorruptCatalog);
            }
        }
        connection
            .close()
            .map_err(|(_, error)| ChunkStoreError::Sql(error))
    }

    /// Opens a chunk directory and its `SQLCipher` manifest/refcount catalog.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid limits, key derivation, filesystem, cipher,
    /// or schema failures.
    pub fn open(
        root: impl AsRef<Path>,
        key: &ChunkStoreKey,
        config: ChunkStoreConfig,
    ) -> Result<Self, ChunkStoreError> {
        validate_config(config)?;
        let root = root.as_ref().to_path_buf();
        let chunks_dir = root.join("objects");
        let staging_dir = root.join("staging");
        create_private_dir(&root)?;
        create_private_dir(&chunks_dir)?;
        create_private_dir(&staging_dir)?;

        let chunk_key = derive_key(&key.0, b"clip-sync/chunk-store/encryption/v1")?;
        let id_key = derive_key(&key.0, b"clip-sync/chunk-store/identifiers/v1")?;
        let catalog_key = derive_key(&key.0, b"clip-sync/chunk-store/catalog/v1")?;
        let catalog_path = root.join("catalog.db");
        let connection = Connection::open_with_flags(
            &catalog_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        apply_catalog_key(&connection, &catalog_key)?;
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
             PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             PRAGMA journal_size_limit = 16777216;
             PRAGMA wal_autocheckpoint = 1000;
             CREATE TABLE IF NOT EXISTS chunk_catalog (
                 id BLOB PRIMARY KEY NOT NULL CHECK(length(id) = 32),
                 logical_size INTEGER NOT NULL CHECK(logical_size BETWEEN 1 AND 4194304),
                 ref_count INTEGER NOT NULL CHECK(ref_count >= 0)
             ) STRICT, WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS manifests (
                 id BLOB PRIMARY KEY NOT NULL CHECK(length(id) = 32),
                 encoding_version INTEGER NOT NULL CHECK(encoding_version = 1),
                 body BLOB NOT NULL
             ) STRICT, WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS staged_manifests (
                 id BLOB PRIMARY KEY NOT NULL CHECK(length(id) = 32),
                 encoding_version INTEGER NOT NULL CHECK(encoding_version = 1),
                 body BLOB NOT NULL
             ) STRICT, WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS staged_manifest_chunks (
                 manifest_id BLOB NOT NULL CHECK(length(manifest_id) = 32),
                 chunk_id BLOB NOT NULL CHECK(length(chunk_id) = 32),
                 logical_size INTEGER NOT NULL CHECK(logical_size BETWEEN 1 AND 4194304),
                 PRIMARY KEY(manifest_id, chunk_id),
                 FOREIGN KEY(manifest_id) REFERENCES staged_manifests(id) ON DELETE CASCADE
             ) STRICT, WITHOUT ROWID;",
            )
            .map_err(normalize_catalog_error)?;
        set_private_file_permissions(&catalog_path)?;

        let mut store = Self {
            root,
            chunks_dir,
            staging_dir,
            connection,
            chunk_key,
            id_key,
            config,
        };
        store.cleanup_unreferenced()?;
        Ok(store)
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub const fn chunk_bytes(&self) -> usize {
        self.config.chunk_bytes
    }

    #[must_use]
    pub fn encrypted_chunk_bytes(&self) -> u64 {
        self.encrypted_object_bytes()
    }
}

fn derive_key(root: &[u8; 32], info: &[u8]) -> Result<Zeroizing<[u8; 32]>, ChunkStoreError> {
    let hkdf = Hkdf::<Sha256>::new(None, root);
    let mut output = Zeroizing::new([0_u8; 32]);
    hkdf.expand(info, output.as_mut())
        .map_err(|_| ChunkStoreError::KeyDerivation)?;
    Ok(output)
}

fn apply_catalog_key(connection: &Connection, key: &[u8; 32]) -> Result<(), ChunkStoreError> {
    connection.pragma_update(None, "key", format!("x'{}'", hex::encode(key)))?;
    let version: Option<String> = connection
        .query_row("PRAGMA cipher_version;", [], |row| row.get(0))
        .optional()?;
    if version.as_deref().is_none_or(str::is_empty) {
        return Err(ChunkStoreError::CipherUnavailable);
    }
    connection
        .query_row("SELECT count(*) FROM sqlite_master", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(normalize_catalog_error)?;
    Ok(())
}

fn normalize_catalog_error(error: rusqlite::Error) -> ChunkStoreError {
    match &error {
        rusqlite::Error::SqliteFailure(code, _)
            if matches!(
                code.code,
                rusqlite::ErrorCode::NotADatabase | rusqlite::ErrorCode::DatabaseCorrupt
            ) =>
        {
            ChunkStoreError::InvalidKey
        }
        _ => error.into(),
    }
}
