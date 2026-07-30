use rusqlite::{OptionalExtension, TransactionBehavior};
use zeroize::Zeroizing;

use super::{ChunkStore, ChunkStoreError, ManifestId, StoredManifest};

impl ChunkStore {
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
}
