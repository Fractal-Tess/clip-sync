use super::{
    BlobManifest, ChunkStore, ChunkStoreConfig, ChunkStoreError, MAX_CHUNK_BYTES, MIN_CHUNK_BYTES,
    MimeBundleManifest, StoredManifest,
};

impl ChunkStore {
    pub(super) fn validate_manifest(
        &self,
        manifest: &StoredManifest,
    ) -> Result<(), ChunkStoreError> {
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
}

pub(super) fn validate_mime_bundle(
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

pub(super) fn validate_config(config: ChunkStoreConfig) -> Result<(), ChunkStoreError> {
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

pub(super) fn validate_blob(
    blob: &BlobManifest,
    config: ChunkStoreConfig,
) -> Result<(), ChunkStoreError> {
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

pub(super) fn validate_chunk_size(
    logical_size: u32,
    config: ChunkStoreConfig,
) -> Result<(), ChunkStoreError> {
    if logical_size == 0
        || usize::try_from(logical_size).map_or(true, |size| size > config.chunk_bytes)
    {
        return Err(ChunkStoreError::InvalidChunkSize);
    }
    Ok(())
}
