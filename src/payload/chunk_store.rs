use std::{
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    str::FromStr,
};

use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{AeadInOut, KeyInit},
};
use hkdf::Hkdf;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use sha2::Sha256;
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zeroize::Zeroizing;

use super::FileSnapshot;

const CHUNK_DOMAIN: &[u8] = b"clip-sync/chunk-id/v1\0";
const MANIFEST_DOMAIN: &[u8] = b"clip-sync/manifest-id/v1\0";
const CHUNK_MAGIC: &[u8; 8] = b"CSCHUNK1";
const CHUNK_HEADER_BYTES: usize = 8 + 24;
const AEAD_TAG_BYTES: usize = 16;
const MIN_CHUNK_BYTES: usize = 64 * 1024;
const MAX_CHUNK_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_CHUNK_BYTES: usize = 1024 * 1024;
const DEFAULT_MAX_PAYLOAD_BYTES: u64 = 1024 * 1024 * 1024 * 1024;
const DEFAULT_MAX_CHUNKS: usize = 1_048_576;

/// Root secret for the chunk store. Debug output is always redacted.
#[derive(Clone)]
pub struct ChunkStoreKey(Zeroizing<[u8; 32]>);

impl ChunkStoreKey {
    #[must_use]
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    pub(crate) fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for ChunkStoreKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ChunkStoreKey([REDACTED])")
    }
}

macro_rules! digest_id {
    ($name:ident) => {
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name([u8; 32]);

        impl $name {
            #[must_use]
            pub const fn as_bytes(&self) -> &[u8; 32] {
                &self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter
                    .debug_tuple(stringify!($name))
                    .field(&self.to_string())
                    .finish()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&hex::encode(self.0))
            }
        }

        impl FromStr for $name {
            type Err = ChunkStoreError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                if value.len() != 64 {
                    return Err(ChunkStoreError::InvalidIdentifier);
                }
                let mut bytes = [0; 32];
                hex::decode_to_slice(value, &mut bytes)
                    .map_err(|_| ChunkStoreError::InvalidIdentifier)?;
                Ok(Self(bytes))
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.to_string())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                String::deserialize(deserializer)?
                    .parse()
                    .map_err(de::Error::custom)
            }
        }
    };
}

digest_id!(ChunkId);
digest_id!(ManifestId);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkRef {
    id: ChunkId,
    logical_size: u32,
}

impl ChunkRef {
    #[must_use]
    pub const fn from_parts(id: ChunkId, logical_size: u32) -> Self {
        Self { id, logical_size }
    }

    #[must_use]
    pub const fn id(&self) -> ChunkId {
        self.id
    }

    #[must_use]
    pub const fn logical_size(&self) -> u32 {
        self.logical_size
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobManifest {
    logical_size: u64,
    chunks: Vec<ChunkRef>,
}

impl BlobManifest {
    #[must_use]
    pub const fn logical_size(&self) -> u64 {
        self.logical_size
    }

    #[must_use]
    pub fn chunks(&self) -> &[ChunkRef] {
        &self.chunks
    }
}

/// One MIME representation stored as an ordered encrypted blob.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MimeBlob {
    mime: String,
    blob: BlobManifest,
}

impl MimeBlob {
    #[must_use]
    pub fn mime(&self) -> &str {
        &self.mime
    }

    #[must_use]
    pub const fn blob(&self) -> &BlobManifest {
        &self.blob
    }
}

/// Canonical multi-MIME clipboard payload whose bytes live in encrypted chunks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MimeBundleManifest {
    logical_size: u64,
    representations: Vec<MimeBlob>,
}

impl MimeBundleManifest {
    #[must_use]
    pub const fn logical_size(&self) -> u64 {
        self.logical_size
    }

    #[must_use]
    pub fn representations(&self) -> &[MimeBlob] {
        &self.representations
    }
}

/// Encrypted manifest body retained in the `SQLCipher` catalog.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum StoredManifest {
    Blob(BlobManifest),
    MimeBundle(MimeBundleManifest),
    Files(FileSnapshot),
}

impl StoredManifest {
    #[must_use]
    pub fn logical_size(&self) -> u64 {
        match self {
            Self::Blob(blob) => blob.logical_size(),
            Self::MimeBundle(bundle) => bundle.logical_size(),
            Self::Files(files) => files.logical_size(),
        }
    }

    pub(crate) fn visit_chunks(&self, mut visitor: impl FnMut(&ChunkRef)) {
        match self {
            Self::Blob(blob) => {
                for chunk in blob.chunks() {
                    visitor(chunk);
                }
            }
            Self::MimeBundle(bundle) => {
                for representation in bundle.representations() {
                    for chunk in representation.blob().chunks() {
                        visitor(chunk);
                    }
                }
            }
            Self::Files(files) => {
                for entry in files.entries() {
                    if let Some(blob) = entry.blob() {
                        for chunk in blob.chunks() {
                            visitor(chunk);
                        }
                    }
                }
            }
        }
    }

    /// Returns the canonical chunk references used by this manifest.
    #[must_use]
    pub fn chunks(&self) -> Vec<ChunkRef> {
        let mut chunks = Vec::new();
        self.visit_chunks(|chunk| chunks.push(chunk.clone()));
        chunks
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkStoreConfig {
    pub chunk_bytes: usize,
    pub max_payload_bytes: u64,
    pub max_chunks_per_manifest: usize,
}

impl Default for ChunkStoreConfig {
    fn default() -> Self {
        Self {
            chunk_bytes: DEFAULT_CHUNK_BYTES,
            max_payload_bytes: DEFAULT_MAX_PAYLOAD_BYTES,
            max_chunks_per_manifest: DEFAULT_MAX_CHUNKS,
        }
    }
}

/// Single-owner encrypted fixed-size chunk store.
pub struct ChunkStore {
    root: PathBuf,
    chunks_dir: PathBuf,
    staging_dir: PathBuf,
    connection: Connection,
    chunk_key: Zeroizing<[u8; 32]>,
    id_key: Zeroizing<[u8; 32]>,
    config: ChunkStoreConfig,
}

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

    /// Streams a bounded reader into encrypted staged chunks.
    ///
    /// Chunks remain unreferenced until a containing manifest is committed.
    ///
    /// # Errors
    ///
    /// Returns an error on cancellation, limit overflow, I/O, or encryption.
    pub fn stage_reader(
        &mut self,
        reader: &mut impl Read,
        maximum_bytes: u64,
        cancellation: &CancellationToken,
    ) -> Result<BlobManifest, ChunkStoreError> {
        let maximum_bytes = maximum_bytes.min(self.config.max_payload_bytes);
        let mut chunks = Vec::new();
        let mut total = 0_u64;
        let mut buffer = Zeroizing::new(vec![0_u8; self.config.chunk_bytes]);

        loop {
            ensure_not_cancelled(cancellation)?;
            let read = read_chunk(reader, &mut buffer, cancellation)?;
            if read == 0 {
                break;
            }
            total = total
                .checked_add(u64::try_from(read).map_err(|_| ChunkStoreError::SizeOverflow)?)
                .ok_or(ChunkStoreError::SizeOverflow)?;
            if total > maximum_bytes {
                return Err(ChunkStoreError::PayloadTooLarge {
                    observed: total,
                    maximum: maximum_bytes,
                });
            }
            if chunks.len() == self.config.max_chunks_per_manifest {
                return Err(ChunkStoreError::TooManyChunks {
                    maximum: self.config.max_chunks_per_manifest,
                });
            }
            let chunk = self.stage_plain_chunk(&buffer[..read])?;
            chunks.push(chunk);
        }

        Ok(BlobManifest {
            logical_size: total,
            chunks,
        })
    }

    /// Streams canonical MIME representations into a single encrypted bundle.
    ///
    /// # Errors
    ///
    /// Returns an error for empty/duplicate MIME names, size overflow, limits,
    /// cancellation, I/O, or encryption failures.
    pub fn stage_mime_bundle(
        &mut self,
        representations: &mut [(String, &[u8])],
        maximum_bytes: u64,
        cancellation: &CancellationToken,
    ) -> Result<MimeBundleManifest, ChunkStoreError> {
        if representations.is_empty() {
            return Err(ChunkStoreError::MalformedManifest);
        }
        representations.sort_unstable_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
        let mut prior: Option<&str> = None;
        let mut logical_size = 0_u64;
        let mut bundled = Vec::with_capacity(representations.len());
        for (mime, bytes) in representations {
            if mime.is_empty()
                || mime.len() > 256
                || mime.as_bytes().contains(&0)
                || prior == Some(mime.as_str())
            {
                return Err(ChunkStoreError::MalformedManifest);
            }
            prior = Some(mime);
            let remaining = maximum_bytes.checked_sub(logical_size).ok_or(
                ChunkStoreError::PayloadTooLarge {
                    observed: logical_size,
                    maximum: maximum_bytes,
                },
            )?;
            let blob = self.stage_reader(&mut io::Cursor::new(*bytes), remaining, cancellation)?;
            logical_size = logical_size
                .checked_add(blob.logical_size())
                .ok_or(ChunkStoreError::SizeOverflow)?;
            bundled.push(MimeBlob {
                mime: mime.clone(),
                blob,
            });
        }
        let manifest = MimeBundleManifest {
            logical_size,
            representations: bundled,
        };
        validate_mime_bundle(&manifest, self.config)?;
        Ok(manifest)
    }

    /// Commits a validated encrypted manifest and increments chunk refcounts.
    /// Re-committing the same keyed manifest is idempotent.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed/missing chunks, serialization, or SQL.
    pub fn commit_manifest(
        &mut self,
        manifest: &StoredManifest,
    ) -> Result<ManifestId, ChunkStoreError> {
        self.validate_manifest(manifest)?;
        let body = Zeroizing::new(serde_json::to_vec(manifest)?);
        let id = self.manifest_id(&body);
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = transaction
            .query_row(
                "SELECT body FROM manifests WHERE id = ?1",
                [id.as_bytes().as_slice()],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?;
        if let Some(existing) = existing {
            if existing != *body {
                return Err(ChunkStoreError::IdentifierCollision);
            }
            transaction.commit()?;
            return Ok(id);
        }

        let mut chunk_refs = Vec::new();
        manifest.visit_chunks(|chunk| chunk_refs.push(chunk.clone()));
        for chunk in &chunk_refs {
            let changed = transaction.execute(
                "UPDATE chunk_catalog SET ref_count = ref_count + 1
                 WHERE id = ?1 AND logical_size = ?2",
                (
                    chunk.id.as_bytes().as_slice(),
                    i64::from(chunk.logical_size),
                ),
            )?;
            if changed != 1 {
                return Err(ChunkStoreError::MissingChunk(chunk.id));
            }
        }
        transaction.execute(
            "INSERT INTO manifests(id, encoding_version, body) VALUES(?1, 1, ?2)",
            (id.as_bytes().as_slice(), body.as_slice()),
        )?;
        transaction.commit()?;
        Ok(id)
    }

    /// Loads and revalidates an encrypted manifest from `SQLCipher`.
    ///
    /// # Errors
    ///
    /// Returns an error when absent, malformed, or inconsistent.
    pub fn manifest(&self, id: ManifestId) -> Result<StoredManifest, ChunkStoreError> {
        let body = self
            .connection
            .query_row(
                "SELECT body FROM manifests WHERE id = ?1 AND encoding_version = 1",
                [id.as_bytes().as_slice()],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?
            .ok_or(ChunkStoreError::MissingManifest(id))?;
        let body = Zeroizing::new(body);
        if self.manifest_id(&body) != id {
            return Err(ChunkStoreError::CorruptManifest(id));
        }
        let manifest =
            serde_json::from_slice(&body).map_err(|_| ChunkStoreError::CorruptManifest(id))?;
        self.validate_manifest(&manifest)
            .map_err(|_| ChunkStoreError::CorruptManifest(id))?;
        Ok(manifest)
    }

    /// Validates a replicated manifest and its keyed identifier without
    /// changing catalog state.
    ///
    /// # Errors
    ///
    /// Returns malformed/oversized manifest or identifier mismatch errors.
    pub fn validate_manifest_id(
        &self,
        expected_id: ManifestId,
        manifest: &StoredManifest,
    ) -> Result<(), ChunkStoreError> {
        self.validate_manifest(manifest)?;
        let body = Zeroizing::new(serde_json::to_vec(manifest)?);
        if self.manifest_id(&body) != expected_id {
            return Err(ChunkStoreError::IdentifierMismatch);
        }
        Ok(())
    }

    /// Computes the keyed identifier of a valid manifest without committing it.
    ///
    /// # Errors
    ///
    /// Returns malformed/oversized manifest or serialization errors.
    pub fn manifest_id_for(
        &self,
        manifest: &StoredManifest,
    ) -> Result<ManifestId, ChunkStoreError> {
        self.validate_manifest(manifest)?;
        let body = Zeroizing::new(serde_json::to_vec(manifest)?);
        Ok(self.manifest_id(&body))
    }

    /// Durably records an incoming manifest before all of its chunks exist.
    ///
    /// Staged manifests pin any imported zero-reference chunks across restart.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed manifests, identifier mismatches, or
    /// catalog failures.
    pub fn stage_incoming_manifest(
        &mut self,
        expected_id: ManifestId,
        manifest: &StoredManifest,
    ) -> Result<(), ChunkStoreError> {
        self.validate_manifest_id(expected_id, manifest)?;
        let body = Zeroizing::new(serde_json::to_vec(manifest)?);
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = transaction
            .query_row(
                "SELECT body FROM staged_manifests WHERE id = ?1",
                [expected_id.as_bytes().as_slice()],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?;
        if existing
            .as_deref()
            .is_some_and(|existing| existing != body.as_slice())
        {
            return Err(ChunkStoreError::IdentifierCollision);
        }
        transaction.execute(
            "INSERT INTO staged_manifests(id, encoding_version, body) VALUES(?1, 1, ?2)
             ON CONFLICT(id) DO NOTHING",
            (expected_id.as_bytes().as_slice(), body.as_slice()),
        )?;
        let mut chunks = Vec::new();
        manifest.visit_chunks(|chunk| chunks.push(chunk.clone()));
        for chunk in chunks {
            transaction.execute(
                "INSERT INTO staged_manifest_chunks(manifest_id, chunk_id, logical_size)
                 VALUES(?1, ?2, ?3) ON CONFLICT(manifest_id, chunk_id) DO NOTHING",
                (
                    expected_id.as_bytes().as_slice(),
                    chunk.id.as_bytes().as_slice(),
                    i64::from(chunk.logical_size),
                ),
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Promotes a fully received staged manifest into retained history.
    ///
    /// # Errors
    ///
    /// Returns an error until every authenticated chunk is present.
    pub fn promote_staged_manifest(
        &mut self,
        id: ManifestId,
    ) -> Result<StoredManifest, ChunkStoreError> {
        let body = self
            .connection
            .query_row(
                "SELECT body FROM staged_manifests WHERE id = ?1 AND encoding_version = 1",
                [id.as_bytes().as_slice()],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?
            .ok_or(ChunkStoreError::MissingManifest(id))?;
        let manifest: StoredManifest =
            serde_json::from_slice(&body).map_err(|_| ChunkStoreError::CorruptManifest(id))?;
        let committed = self.commit_manifest(&manifest)?;
        if committed != id {
            return Err(ChunkStoreError::IdentifierMismatch);
        }
        self.connection.execute(
            "DELETE FROM staged_manifests WHERE id = ?1",
            [id.as_bytes().as_slice()],
        )?;
        Ok(manifest)
    }

    /// Cancels incoming staging and reclaims chunks not retained elsewhere.
    ///
    /// # Errors
    ///
    /// Returns catalog or cleanup errors.
    pub fn abandon_staged_manifest(&mut self, id: ManifestId) -> Result<bool, ChunkStoreError> {
        let changed = self.connection.execute(
            "DELETE FROM staged_manifests WHERE id = ?1",
            [id.as_bytes().as_slice()],
        )?;
        self.cleanup_unreferenced()?;
        Ok(changed != 0)
    }

    /// Drops a corrupt, unretained incoming chunk so another peer can retry it.
    ///
    /// # Errors
    ///
    /// Refuses to remove a chunk referenced by a committed manifest.
    pub fn discard_unretained_chunk(&mut self, id: ChunkId) -> Result<bool, ChunkStoreError> {
        let ref_count = self
            .connection
            .query_row(
                "SELECT ref_count FROM chunk_catalog WHERE id = ?1",
                [id.as_bytes().as_slice()],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        let Some(ref_count) = ref_count else {
            return Ok(false);
        };
        if ref_count != 0 {
            return Err(ChunkStoreError::ChunkRetained(id));
        }
        self.connection.execute(
            "DELETE FROM chunk_catalog WHERE id = ?1 AND ref_count = 0",
            [id.as_bytes().as_slice()],
        )?;
        match fs::remove_file(self.chunk_path(id)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(true)
    }

    #[must_use]
    pub fn has_chunk(&self, id: ChunkId) -> bool {
        self.connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM chunk_catalog WHERE id = ?1)",
                [id.as_bytes().as_slice()],
                |row| row.get::<_, bool>(0),
            )
            .unwrap_or(false)
            && self.chunk_path(id).is_file()
    }

    /// Authenticates and decrypts one chunk into a caller-provided writer.
    ///
    /// # Errors
    ///
    /// Returns an error for cancellation, corruption, authentication, or I/O.
    pub fn read_chunk(
        &self,
        chunk: &ChunkRef,
        writer: &mut impl Write,
        cancellation: &CancellationToken,
    ) -> Result<(), ChunkStoreError> {
        ensure_not_cancelled(cancellation)?;
        let plaintext = self.decrypt_chunk_file(chunk.id, chunk.logical_size)?;
        ensure_not_cancelled(cancellation)?;
        writer.write_all(&plaintext)?;
        Ok(())
    }

    /// Streams a complete blob with per-chunk authentication.
    ///
    /// # Errors
    ///
    /// Returns an error for cancellation, malformed manifests, corruption, or I/O.
    pub fn read_blob(
        &self,
        blob: &BlobManifest,
        writer: &mut impl Write,
        cancellation: &CancellationToken,
    ) -> Result<(), ChunkStoreError> {
        validate_blob(blob, self.config)?;
        for chunk in blob.chunks() {
            self.read_chunk(chunk, writer, cancellation)?;
        }
        Ok(())
    }

    /// Authenticates and reconstructs every MIME representation in order.
    ///
    /// # Errors
    ///
    /// Returns an error for cancellation, corruption, authentication, or I/O.
    pub fn read_mime_bundle(
        &self,
        bundle: &MimeBundleManifest,
        cancellation: &CancellationToken,
    ) -> Result<Vec<(String, Vec<u8>)>, ChunkStoreError> {
        validate_mime_bundle(bundle, self.config)?;
        let mut representations = Vec::with_capacity(bundle.representations.len());
        for representation in &bundle.representations {
            let capacity = usize::try_from(representation.blob.logical_size)
                .map_err(|_| ChunkStoreError::SizeOverflow)?;
            let mut bytes = Vec::with_capacity(capacity);
            self.read_blob(&representation.blob, &mut bytes, cancellation)?;
            representations.push((representation.mime.clone(), bytes));
        }
        Ok(representations)
    }

    /// Fully authenticates one locally stored encrypted chunk.
    ///
    /// # Errors
    ///
    /// Returns corruption, authentication, identifier, or I/O errors.
    pub fn verify_chunk(&self, chunk: &ChunkRef) -> Result<(), ChunkStoreError> {
        self.decrypt_chunk_file(chunk.id, chunk.logical_size)?;
        Ok(())
    }

    /// Copies one bounded encrypted chunk object for a dedicated mesh stream.
    ///
    /// # Errors
    ///
    /// Returns an error if the chunk is missing, oversized, cancelled, or unreadable.
    pub fn export_encrypted_chunk(
        &self,
        id: ChunkId,
        writer: &mut impl Write,
        cancellation: &CancellationToken,
    ) -> Result<u64, ChunkStoreError> {
        ensure_not_cancelled(cancellation)?;
        let mut file = File::open(self.chunk_path(id))?;
        let expected = self.encrypted_object_bytes();
        if file.metadata()?.len() != expected {
            return Err(ChunkStoreError::CorruptChunk(id));
        }
        let copied = copy_bounded(&mut file, writer, expected, cancellation)?;
        if copied != expected {
            return Err(ChunkStoreError::CorruptChunk(id));
        }
        Ok(copied)
    }

    /// Imports, authenticates, and atomically installs a bounded encrypted object.
    ///
    /// # Errors
    ///
    /// Returns an error for cancellation, truncation, excess bytes, identifier
    /// mismatch, failed authentication, I/O, or SQL.
    pub fn import_encrypted_chunk(
        &mut self,
        id: ChunkId,
        logical_size: u32,
        reader: &mut impl Read,
        cancellation: &CancellationToken,
    ) -> Result<(), ChunkStoreError> {
        validate_chunk_size(logical_size, self.config)?;
        let expected = self.encrypted_object_bytes();
        let temporary = self.temporary_path();
        let mut file = create_private_file(&temporary)?;
        let copied = copy_bounded(reader, &mut file, expected, cancellation)?;
        if copied != expected {
            drop(file);
            let _ = fs::remove_file(&temporary);
            return Err(ChunkStoreError::TruncatedChunk);
        }
        let mut extra = [0_u8; 1];
        if reader.read(&mut extra)? != 0 {
            drop(file);
            let _ = fs::remove_file(&temporary);
            return Err(ChunkStoreError::OversizedChunk);
        }
        file.sync_all()?;
        drop(file);
        let plaintext = match self.decrypt_path(&temporary, id, logical_size) {
            Ok(plaintext) => plaintext,
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                return Err(error);
            }
        };
        if self.chunk_id(&plaintext) != id {
            let _ = fs::remove_file(&temporary);
            return Err(ChunkStoreError::IdentifierMismatch);
        }
        drop(plaintext);

        let destination = self.chunk_path(id);
        if destination.exists() {
            self.decrypt_chunk_file(id, logical_size)?;
            fs::remove_file(&temporary)?;
        } else {
            fs::rename(&temporary, &destination)?;
        }
        self.connection.execute(
            "INSERT INTO chunk_catalog(id, logical_size, ref_count) VALUES(?1, ?2, 0)
             ON CONFLICT(id) DO NOTHING",
            (id.as_bytes().as_slice(), i64::from(logical_size)),
        )?;
        Ok(())
    }

    /// Deletes a manifest and reclaims chunks with no remaining references.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed catalog state, SQL, or filesystem I/O.
    pub fn remove_manifest(&mut self, id: ManifestId) -> Result<bool, ChunkStoreError> {
        let body = self
            .connection
            .query_row(
                "SELECT body FROM manifests WHERE id = ?1",
                [id.as_bytes().as_slice()],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?;
        let Some(body) = body else {
            return Ok(false);
        };
        let manifest: StoredManifest =
            serde_json::from_slice(&body).map_err(|_| ChunkStoreError::CorruptManifest(id))?;
        self.validate_manifest(&manifest)
            .map_err(|_| ChunkStoreError::CorruptManifest(id))?;
        let mut chunks = Vec::new();
        manifest.visit_chunks(|chunk| chunks.push(chunk.id));

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "DELETE FROM manifests WHERE id = ?1",
            [id.as_bytes().as_slice()],
        )?;
        for chunk in &chunks {
            let changed = transaction.execute(
                "UPDATE chunk_catalog SET ref_count = ref_count - 1
                 WHERE id = ?1 AND ref_count > 0",
                [chunk.as_bytes().as_slice()],
            )?;
            if changed != 1 {
                return Err(ChunkStoreError::RefcountUnderflow(*chunk));
            }
        }
        transaction.commit()?;
        self.cleanup_unreferenced()?;
        Ok(true)
    }

    /// Removes all cataloged zero-ref chunks and abandoned staging files.
    ///
    /// # Errors
    ///
    /// Returns an error for SQL or filesystem failures.
    pub fn cleanup_unreferenced(&mut self) -> Result<usize, ChunkStoreError> {
        let mut removed = 0_usize;
        loop {
            let mut statement = self.connection.prepare(
                "SELECT id FROM chunk_catalog
                     WHERE ref_count = 0
                       AND NOT EXISTS (
                         SELECT 1 FROM staged_manifest_chunks
                         WHERE staged_manifest_chunks.chunk_id = chunk_catalog.id
                       )
                     LIMIT 1024",
            )?;
            let rows = statement.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
            let mut ids = Vec::with_capacity(1024);
            for row in rows {
                let bytes = row?;
                let bytes: [u8; 32] = bytes
                    .try_into()
                    .map_err(|_| ChunkStoreError::CorruptCatalog)?;
                ids.push(ChunkId(bytes));
            }
            drop(statement);
            if ids.is_empty() {
                break;
            }

            let transaction = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            for id in &ids {
                transaction.execute(
                    "DELETE FROM chunk_catalog
                     WHERE id = ?1 AND ref_count = 0
                       AND NOT EXISTS (
                         SELECT 1 FROM staged_manifest_chunks
                         WHERE staged_manifest_chunks.chunk_id = chunk_catalog.id
                       )",
                    [id.as_bytes().as_slice()],
                )?;
            }
            transaction.commit()?;
            for id in &ids {
                match fs::remove_file(self.chunk_path(*id)) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            removed = removed
                .checked_add(ids.len())
                .ok_or(ChunkStoreError::SizeOverflow)?;
        }
        self.cleanup_staging()?;
        removed
            .checked_add(self.cleanup_orphan_objects()?)
            .ok_or(ChunkStoreError::SizeOverflow)
    }

    /// Removes abandoned atomic-write temporary files.
    ///
    /// # Errors
    ///
    /// Returns an error for directory enumeration or file removal failures.
    pub fn cleanup_staging(&self) -> Result<usize, ChunkStoreError> {
        let mut removed = 0;
        for entry in fs::read_dir(&self.staging_dir)? {
            let entry = entry?;
            let metadata = entry.file_type()?;
            if metadata.is_file() || metadata.is_symlink() {
                fs::remove_file(entry.path())?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    fn cleanup_orphan_objects(&self) -> Result<usize, ChunkStoreError> {
        let mut removed = 0;
        for entry in fs::read_dir(&self.chunks_dir)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let cataloged = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<ChunkId>().ok())
                .is_some_and(|id| {
                    self.connection
                        .query_row(
                            "SELECT EXISTS(SELECT 1 FROM chunk_catalog WHERE id = ?1)",
                            [id.as_bytes().as_slice()],
                            |row| row.get::<_, bool>(0),
                        )
                        .unwrap_or(false)
                });
            if cataloged {
                if !file_type.is_file() || file_type.is_symlink() {
                    return Err(ChunkStoreError::CorruptCatalog);
                }
            } else if file_type.is_file() || file_type.is_symlink() {
                fs::remove_file(entry.path())?;
                removed += 1;
            } else {
                return Err(ChunkStoreError::CorruptCatalog);
            }
        }
        Ok(removed)
    }

    fn stage_plain_chunk(&mut self, plaintext: &[u8]) -> Result<ChunkRef, ChunkStoreError> {
        let logical_size =
            u32::try_from(plaintext.len()).map_err(|_| ChunkStoreError::SizeOverflow)?;
        validate_chunk_size(logical_size, self.config)?;
        let id = self.chunk_id(plaintext);
        let destination = self.chunk_path(id);
        if destination.exists() {
            let metadata = fs::symlink_metadata(&destination)?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(ChunkStoreError::CorruptChunk(id));
            }
            self.decrypt_chunk_file(id, logical_size)?;
            self.connection.execute(
                "INSERT INTO chunk_catalog(id, logical_size, ref_count) VALUES(?1, ?2, 0)
                 ON CONFLICT(id) DO NOTHING",
                (id.as_bytes().as_slice(), i64::from(logical_size)),
            )?;
            return Ok(ChunkRef { id, logical_size });
        }

        let mut nonce = [0_u8; 24];
        getrandom::fill(&mut nonce).map_err(|_| ChunkStoreError::Randomness)?;
        let mut encrypted =
            Zeroizing::new(Vec::with_capacity(self.config.chunk_bytes + AEAD_TAG_BYTES));
        encrypted.extend_from_slice(plaintext);
        encrypted.resize(self.config.chunk_bytes, 0);
        let cipher = XChaCha20Poly1305::new_from_slice(self.chunk_key.as_ref())
            .map_err(|_| ChunkStoreError::Encryption)?;
        let nonce = XNonce::from(nonce);
        cipher
            .encrypt_in_place(&nonce, &chunk_aad(id, logical_size), &mut *encrypted)
            .map_err(|_| ChunkStoreError::Encryption)?;

        let temporary = self.temporary_path();
        let mut file = create_private_file(&temporary)?;
        file.write_all(CHUNK_MAGIC)?;
        file.write_all(&nonce)?;
        file.write_all(&encrypted)?;
        file.sync_all()?;
        drop(file);
        match fs::rename(&temporary, &destination) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                fs::remove_file(&temporary)?;
            }
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                return Err(error.into());
            }
        }
        self.connection.execute(
            "INSERT INTO chunk_catalog(id, logical_size, ref_count) VALUES(?1, ?2, 0)
             ON CONFLICT(id) DO NOTHING",
            (id.as_bytes().as_slice(), i64::from(logical_size)),
        )?;
        Ok(ChunkRef { id, logical_size })
    }

    fn decrypt_chunk_file(
        &self,
        id: ChunkId,
        logical_size: u32,
    ) -> Result<Zeroizing<Vec<u8>>, ChunkStoreError> {
        self.decrypt_path(&self.chunk_path(id), id, logical_size)
    }

    fn decrypt_path(
        &self,
        path: &Path,
        id: ChunkId,
        logical_size: u32,
    ) -> Result<Zeroizing<Vec<u8>>, ChunkStoreError> {
        let mut file = File::open(path)?;
        if file.metadata()?.len() != self.encrypted_object_bytes() {
            return Err(ChunkStoreError::CorruptChunk(id));
        }
        let mut magic = [0_u8; 8];
        let mut nonce = [0_u8; 24];
        file.read_exact(&mut magic)?;
        file.read_exact(&mut nonce)?;
        if &magic != CHUNK_MAGIC {
            return Err(ChunkStoreError::CorruptChunk(id));
        }
        let mut encrypted = Zeroizing::new(vec![0_u8; self.config.chunk_bytes + AEAD_TAG_BYTES]);
        file.read_exact(&mut encrypted)?;
        let cipher = XChaCha20Poly1305::new_from_slice(self.chunk_key.as_ref())
            .map_err(|_| ChunkStoreError::Encryption)?;
        let nonce = XNonce::from(nonce);
        cipher
            .decrypt_in_place(&nonce, &chunk_aad(id, logical_size), &mut *encrypted)
            .map_err(|_| ChunkStoreError::Authentication(id))?;
        encrypted
            .truncate(usize::try_from(logical_size).map_err(|_| ChunkStoreError::SizeOverflow)?);
        if self.chunk_id(&encrypted) != id {
            return Err(ChunkStoreError::IdentifierMismatch);
        }
        Ok(encrypted)
    }

    fn validate_manifest(&self, manifest: &StoredManifest) -> Result<(), ChunkStoreError> {
        if manifest.logical_size() > self.config.max_payload_bytes {
            return Err(ChunkStoreError::PayloadTooLarge {
                observed: manifest.logical_size(),
                maximum: self.config.max_payload_bytes,
            });
        }
        let mut count = 0_usize;
        let mut invalid = None;
        manifest.visit_chunks(|chunk| {
            count = count.saturating_add(1);
            if validate_chunk_size(chunk.logical_size, self.config).is_err() {
                invalid = Some(chunk.id);
            }
        });
        if count > self.config.max_chunks_per_manifest {
            return Err(ChunkStoreError::TooManyChunks {
                maximum: self.config.max_chunks_per_manifest,
            });
        }
        if let Some(id) = invalid {
            return Err(ChunkStoreError::CorruptChunk(id));
        }
        match manifest {
            StoredManifest::Blob(blob) => validate_blob(blob, self.config),
            StoredManifest::MimeBundle(bundle) => validate_mime_bundle(bundle, self.config),
            StoredManifest::Files(files) => files
                .validate(self.config.max_payload_bytes)
                .map_err(|_| ChunkStoreError::MalformedManifest),
        }
    }

    fn chunk_id(&self, plaintext: &[u8]) -> ChunkId {
        let mut hasher = blake3::Hasher::new_keyed(&self.id_key);
        hasher.update(CHUNK_DOMAIN);
        hasher.update(&(plaintext.len() as u64).to_be_bytes());
        hasher.update(plaintext);
        ChunkId(*hasher.finalize().as_bytes())
    }

    fn manifest_id(&self, body: &[u8]) -> ManifestId {
        let mut hasher = blake3::Hasher::new_keyed(&self.id_key);
        hasher.update(MANIFEST_DOMAIN);
        hasher.update(&(body.len() as u64).to_be_bytes());
        hasher.update(body);
        ManifestId(*hasher.finalize().as_bytes())
    }

    fn chunk_path(&self, id: ChunkId) -> PathBuf {
        self.chunks_dir.join(id.to_string())
    }

    fn temporary_path(&self) -> PathBuf {
        self.staging_dir.join(format!("{}.staging", Uuid::new_v4()))
    }

    fn encrypted_object_bytes(&self) -> u64 {
        u64::try_from(CHUNK_HEADER_BYTES + self.config.chunk_bytes + AEAD_TAG_BYTES)
            .expect("bounded chunk object size")
    }
}

fn validate_mime_bundle(
    bundle: &MimeBundleManifest,
    config: ChunkStoreConfig,
) -> Result<(), ChunkStoreError> {
    if bundle.representations.is_empty() {
        return Err(ChunkStoreError::MalformedManifest);
    }
    let mut prior: Option<&str> = None;
    let mut total = 0_u64;
    for representation in &bundle.representations {
        if representation.mime.is_empty()
            || representation.mime.len() > 256
            || representation.mime.as_bytes().contains(&0)
            || prior.is_some_and(|prior| prior >= representation.mime.as_str())
        {
            return Err(ChunkStoreError::MalformedManifest);
        }
        prior = Some(&representation.mime);
        validate_blob(&representation.blob, config)?;
        total = total
            .checked_add(representation.blob.logical_size())
            .ok_or(ChunkStoreError::SizeOverflow)?;
    }
    if total != bundle.logical_size || total > config.max_payload_bytes {
        return Err(ChunkStoreError::MalformedManifest);
    }
    Ok(())
}

fn validate_config(config: ChunkStoreConfig) -> Result<(), ChunkStoreError> {
    if !(MIN_CHUNK_BYTES..=MAX_CHUNK_BYTES).contains(&config.chunk_bytes)
        || !config.chunk_bytes.is_power_of_two()
    {
        return Err(ChunkStoreError::InvalidChunkSize);
    }
    if config.max_payload_bytes == 0 || config.max_chunks_per_manifest == 0 {
        return Err(ChunkStoreError::InvalidLimits);
    }
    Ok(())
}

fn validate_blob(blob: &BlobManifest, config: ChunkStoreConfig) -> Result<(), ChunkStoreError> {
    if blob.chunks.len() > config.max_chunks_per_manifest {
        return Err(ChunkStoreError::TooManyChunks {
            maximum: config.max_chunks_per_manifest,
        });
    }
    let mut total = 0_u64;
    for (index, chunk) in blob.chunks.iter().enumerate() {
        validate_chunk_size(chunk.logical_size, config)?;
        if index + 1 != blob.chunks.len()
            && usize::try_from(chunk.logical_size).ok() != Some(config.chunk_bytes)
        {
            return Err(ChunkStoreError::MalformedManifest);
        }
        total = total
            .checked_add(u64::from(chunk.logical_size))
            .ok_or(ChunkStoreError::SizeOverflow)?;
    }
    if total != blob.logical_size || total > config.max_payload_bytes {
        return Err(ChunkStoreError::MalformedManifest);
    }
    if blob.logical_size == 0 && !blob.chunks.is_empty()
        || blob.logical_size > 0 && blob.chunks.is_empty()
    {
        return Err(ChunkStoreError::MalformedManifest);
    }
    Ok(())
}

fn validate_chunk_size(logical_size: u32, config: ChunkStoreConfig) -> Result<(), ChunkStoreError> {
    if logical_size == 0
        || usize::try_from(logical_size).map_or(true, |size| size > config.chunk_bytes)
    {
        return Err(ChunkStoreError::InvalidChunkSize);
    }
    Ok(())
}

fn read_chunk(
    reader: &mut impl Read,
    buffer: &mut [u8],
    cancellation: &CancellationToken,
) -> Result<usize, ChunkStoreError> {
    let mut filled = 0;
    while filled < buffer.len() {
        ensure_not_cancelled(cancellation)?;
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(filled)
}

fn copy_bounded(
    reader: &mut impl Read,
    writer: &mut impl Write,
    maximum: u64,
    cancellation: &CancellationToken,
) -> Result<u64, ChunkStoreError> {
    let mut limited = reader.take(maximum);
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        ensure_not_cancelled(cancellation)?;
        let read = limited.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        writer.write_all(&buffer[..read])?;
        total += u64::try_from(read).map_err(|_| ChunkStoreError::SizeOverflow)?;
    }
    Ok(total)
}

fn ensure_not_cancelled(cancellation: &CancellationToken) -> Result<(), ChunkStoreError> {
    if cancellation.is_cancelled() {
        Err(ChunkStoreError::Cancelled)
    } else {
        Ok(())
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

fn chunk_aad(id: ChunkId, logical_size: u32) -> Vec<u8> {
    let mut aad = Vec::with_capacity(CHUNK_DOMAIN.len() + 32 + 4);
    aad.extend_from_slice(CHUNK_DOMAIN);
    aad.extend_from_slice(id.as_bytes());
    aad.extend_from_slice(&logical_size.to_be_bytes());
    aad
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn create_private_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn set_private_file_permissions(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum ChunkStoreError {
    #[error("chunk size must be a power of two between 64 KiB and 4 MiB")]
    InvalidChunkSize,
    #[error("chunk-store limits must be nonzero")]
    InvalidLimits,
    #[error("chunk-store key derivation failed")]
    KeyDerivation,
    #[error("secure randomness is unavailable")]
    Randomness,
    #[error("SQLCipher is unavailable for the chunk catalog")]
    CipherUnavailable,
    #[error("chunk catalog could not be opened with this key")]
    InvalidKey,
    #[error("payload reached {observed} bytes, exceeding its {maximum}-byte limit")]
    PayloadTooLarge { observed: u64, maximum: u64 },
    #[error("manifest exceeds the {maximum}-chunk limit")]
    TooManyChunks { maximum: usize },
    #[error("payload size overflow")]
    SizeOverflow,
    #[error("operation cancelled")]
    Cancelled,
    #[error("invalid keyed identifier")]
    InvalidIdentifier,
    #[error("keyed identifier collision")]
    IdentifierCollision,
    #[error("chunk identifier did not match authenticated plaintext")]
    IdentifierMismatch,
    #[error("chunk encryption failed")]
    Encryption,
    #[error("chunk {0} failed authentication")]
    Authentication(ChunkId),
    #[error("chunk {0} is missing")]
    MissingChunk(ChunkId),
    #[error("chunk {0} is corrupt")]
    CorruptChunk(ChunkId),
    #[error("chunk {0} is retained and cannot be discarded")]
    ChunkRetained(ChunkId),
    #[error("manifest {0} is missing")]
    MissingManifest(ManifestId),
    #[error("manifest {0} is corrupt")]
    CorruptManifest(ManifestId),
    #[error("manifest is internally inconsistent")]
    MalformedManifest,
    #[error("chunk {0} refcount would underflow")]
    RefcountUnderflow(ChunkId),
    #[error("chunk catalog is corrupt")]
    CorruptCatalog,
    #[error("incoming chunk was truncated")]
    TruncatedChunk,
    #[error("incoming chunk exceeded the fixed object size")]
    OversizedChunk,
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
