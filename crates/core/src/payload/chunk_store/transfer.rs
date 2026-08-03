use std::{
    fs::{self, File},
    io::{self, Read, Write},
};

use rusqlite::OptionalExtension;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use super::{
    BlobManifest, ChunkId, ChunkRef, ChunkStore, ChunkStoreError, MimeBlob, MimeBundleManifest,
    support::{
        copy_bounded, create_private_file, ensure_not_cancelled, read_chunk as read_into_chunk,
    },
    validation::{validate_blob, validate_chunk_size, validate_mime_bundle},
};

impl ChunkStore {
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
            let read = read_into_chunk(reader, &mut buffer, cancellation)?;
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
}
