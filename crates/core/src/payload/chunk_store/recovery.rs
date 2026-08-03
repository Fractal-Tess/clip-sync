use std::{collections::BTreeSet, fs, io};

use rusqlite::{TransactionBehavior, params};

use super::{ChunkId, ChunkStore, ChunkStoreError, ManifestId};

impl ChunkStore {
    /// Reclaims committed manifests that have no durable history reference.
    ///
    /// This closes the cross-database crash window between committing chunks
    /// and appending the corresponding history operation. IDs are scanned in
    /// fixed batches so recovery memory remains bounded.
    ///
    /// # Errors
    ///
    /// Returns malformed catalog, SQL, or filesystem cleanup errors.
    pub fn cleanup_untracked_manifests(
        &mut self,
        retained: &BTreeSet<ManifestId>,
    ) -> Result<usize, ChunkStoreError> {
        const BATCH: usize = 1024;
        let mut cursor: Option<[u8; 32]> = None;
        let mut removed = 0_usize;
        loop {
            let mut statement = self.connection.prepare(
                "SELECT id FROM manifests
                 WHERE (?1 IS NULL OR id > ?1)
                 ORDER BY id
                 LIMIT ?2",
            )?;
            let rows = statement.query_map(
                params![cursor.as_ref().map(<[u8; 32]>::as_slice), 1024_i64],
                |row| row.get::<_, Vec<u8>>(0),
            )?;
            let mut ids = Vec::with_capacity(BATCH);
            for row in rows {
                let bytes: [u8; 32] = row?
                    .try_into()
                    .map_err(|_| ChunkStoreError::CorruptCatalog)?;
                ids.push(ManifestId(bytes));
            }
            drop(statement);
            let Some(last) = ids.last() else {
                break;
            };
            cursor = Some(*last.as_bytes());
            for id in ids {
                if !retained.contains(&id) && self.remove_manifest(id)? {
                    removed = removed
                        .checked_add(1)
                        .ok_or(ChunkStoreError::SizeOverflow)?;
                }
            }
        }
        Ok(removed)
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
}
