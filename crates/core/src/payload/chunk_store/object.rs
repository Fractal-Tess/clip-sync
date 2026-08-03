use std::{
    fs,
    fs::File,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{AeadInOut, KeyInit},
};
use uuid::Uuid;
use zeroize::Zeroizing;

use super::{
    AEAD_TAG_BYTES, CHUNK_DOMAIN, CHUNK_HEADER_BYTES, CHUNK_MAGIC, ChunkId, ChunkRef, ChunkStore,
    ChunkStoreError, MANIFEST_DOMAIN, ManifestId, support::create_private_file,
    validation::validate_chunk_size,
};

impl ChunkStore {
    pub(super) fn stage_plain_chunk(
        &mut self,
        plaintext: &[u8],
    ) -> Result<ChunkRef, ChunkStoreError> {
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

    pub(super) fn decrypt_chunk_file(
        &self,
        id: ChunkId,
        logical_size: u32,
    ) -> Result<Zeroizing<Vec<u8>>, ChunkStoreError> {
        self.decrypt_path(&self.chunk_path(id), id, logical_size)
    }

    pub(super) fn decrypt_path(
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
    pub(super) fn chunk_id(&self, plaintext: &[u8]) -> ChunkId {
        let mut hasher = blake3::Hasher::new_keyed(&self.id_key);
        hasher.update(CHUNK_DOMAIN);
        hasher.update(&(plaintext.len() as u64).to_be_bytes());
        hasher.update(plaintext);
        ChunkId(*hasher.finalize().as_bytes())
    }

    pub(super) fn manifest_id(&self, body: &[u8]) -> ManifestId {
        let mut hasher = blake3::Hasher::new_keyed(&self.id_key);
        hasher.update(MANIFEST_DOMAIN);
        hasher.update(&(body.len() as u64).to_be_bytes());
        hasher.update(body);
        ManifestId(*hasher.finalize().as_bytes())
    }

    pub(super) fn chunk_path(&self, id: ChunkId) -> PathBuf {
        self.chunks_dir.join(id.to_string())
    }

    pub(super) fn temporary_path(&self) -> PathBuf {
        self.staging_dir.join(format!("{}.staging", Uuid::new_v4()))
    }

    pub(super) fn encrypted_object_bytes(&self) -> u64 {
        u64::try_from(CHUNK_HEADER_BYTES + self.config.chunk_bytes + AEAD_TAG_BYTES)
            .expect("bounded chunk object size")
    }
}

fn chunk_aad(id: ChunkId, logical_size: u32) -> Vec<u8> {
    let mut aad = Vec::with_capacity(CHUNK_DOMAIN.len() + 32 + 4);
    aad.extend_from_slice(CHUNK_DOMAIN);
    aad.extend_from_slice(id.as_bytes());
    aad.extend_from_slice(&logical_size.to_be_bytes());
    aad
}
