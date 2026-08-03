use std::{fs, io, path::Path};

use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use crate::{
    crypto::MeshSecret,
    payload::{ChunkStore, ChunkStoreKey},
    storage::{EncryptedStorage, StorageError, StorageKey},
};

use super::{
    EnvelopeError, KEYSLOT_FILENAME, Result, StoreLock,
    keyslot::{
        DATA_KEY_BYTES, PENDING_FILENAME, Slot, SlotState, atomic_replace_slot, commit_pending,
        read_slot,
    },
    lock::{cleanup_orphan_keyslot_temps, path_exists},
};

pub(super) const DATABASE_FILENAME: &str = "history.db";
pub(super) const CHUNKS_DIRECTORY: &str = "chunks";

/// Stable store and content-identity keys authenticated and wrapped by the mesh secret.
#[derive(Clone)]
pub struct StateKeys {
    pub(super) storage: StorageKey,
    pub(super) chunks: ChunkStoreKey,
    pub(super) content_identity: Zeroizing<[u8; DATA_KEY_BYTES]>,
}

impl StateKeys {
    /// Opens, initializes, or crash-recovers the keyslot while the store lock is held.
    ///
    /// Existing directly-derived `SQLCipher` databases are migrated to a random
    /// database key. Existing chunk stores and keyed content identities retain
    /// their former derived roots so identifiers and ciphertext do not change.
    ///
    /// # Errors
    ///
    /// Returns an error for a wrong secret, unsafe keyslot, failed migration,
    /// corrupt storage, or I/O failure.
    pub fn open_or_create(lock: &StoreLock, secret: &MeshSecret) -> Result<Self> {
        let state_dir = lock.state_dir();
        let keyslot_path = state_dir.join(KEYSLOT_FILENAME);
        let pending_path = state_dir.join(PENDING_FILENAME);

        if path_exists(&keyslot_path)? {
            match read_slot(&keyslot_path, secret) {
                Ok(slot) => {
                    let keys = recover_database_migration(state_dir, secret, slot)?;
                    if path_exists(&pending_path)?
                        && let Ok(pending) = read_slot(&pending_path, secret)
                        && (pending.state != SlotState::Final
                            || !pending.keys.same_data_keys(&keys))
                    {
                        return Err(EnvelopeError::InvalidPendingRekey);
                    }
                    verify_state(state_dir, &keys)?;
                    cleanup_orphan_keyslot_temps(state_dir)?;
                    return Ok(keys);
                }
                Err(EnvelopeError::WrongSecret) if path_exists(&pending_path)? => {
                    let pending = read_slot(&pending_path, secret)?;
                    if pending.state != SlotState::Final {
                        return Err(EnvelopeError::InvalidPendingRekey);
                    }
                    verify_state(state_dir, &pending.keys)?;
                    commit_pending(&pending_path, &keyslot_path)?;
                    let committed = read_slot(&keyslot_path, secret)?;
                    verify_state(state_dir, &committed.keys)?;
                    cleanup_orphan_keyslot_temps(state_dir)?;
                    return Ok(committed.keys);
                }
                Err(error) => return Err(error),
            }
        }

        if path_exists(&pending_path)? {
            return Err(EnvelopeError::InvalidPendingRekey);
        }

        let keys = initialize_keyslot(state_dir, secret)?;
        cleanup_orphan_keyslot_temps(state_dir)?;
        Ok(keys)
    }

    #[must_use]
    pub const fn storage_key(&self) -> &StorageKey {
        &self.storage
    }

    #[must_use]
    pub const fn chunk_store_key(&self) -> &ChunkStoreKey {
        &self.chunks
    }

    #[must_use]
    pub fn content_identity_key(&self) -> &[u8; DATA_KEY_BYTES] {
        &self.content_identity
    }

    pub(super) fn same_data_keys(&self, other: &Self) -> bool {
        bool::from(self.storage.as_bytes().ct_eq(other.storage.as_bytes()))
            && bool::from(self.chunks.as_bytes().ct_eq(other.chunks.as_bytes()))
            && bool::from(
                self.content_identity
                    .as_ref()
                    .ct_eq(other.content_identity.as_ref()),
            )
    }
}

impl std::fmt::Debug for StateKeys {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("StateKeys([REDACTED])")
    }
}

fn initialize_keyslot(state_dir: &Path, secret: &MeshSecret) -> Result<StateKeys> {
    let database_path = state_dir.join(DATABASE_FILENAME);
    let chunks_path = state_dir.join(CHUNKS_DIRECTORY);
    let database_exists = match fs::symlink_metadata(&database_path) {
        Ok(metadata) => metadata.len() > 0,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    let chunk_catalog_exists = path_exists(&chunks_path.join("catalog.db"))?;

    let chunks = if chunk_catalog_exists {
        let legacy = secret.chunk_store_key()?;
        ChunkStore::verify_key(&chunks_path, &legacy)?;
        legacy
    } else {
        ChunkStoreKey::from_bytes(random_key()?)
    };
    let keys = StateKeys {
        storage: StorageKey::from_bytes(random_key()?),
        chunks,
        content_identity: secret.content_key()?,
    };
    let state = if database_exists {
        let legacy = secret.storage_key()?;
        EncryptedStorage::open(&database_path, &legacy)
            .map_err(|error| match error {
                StorageError::InvalidKey => EnvelopeError::LegacyKeyMismatch,
                error => error.into(),
            })?
            .close()?;
        SlotState::DatabaseMigration
    } else {
        SlotState::Final
    };
    let slot = Slot {
        state,
        keys: keys.clone(),
    };
    atomic_replace_slot(&state_dir.join(KEYSLOT_FILENAME), secret, &slot)?;
    recover_database_migration(state_dir, secret, slot)
}

pub(super) fn recover_database_migration(
    state_dir: &Path,
    secret: &MeshSecret,
    slot: Slot,
) -> Result<StateKeys> {
    if slot.state == SlotState::Final {
        return Ok(slot.keys);
    }

    let database_path = state_dir.join(DATABASE_FILENAME);
    match EncryptedStorage::open(&database_path, slot.keys.storage_key()) {
        Ok(storage) => storage.close()?,
        Err(StorageError::InvalidKey) => {
            let legacy = secret.storage_key()?;
            let storage =
                EncryptedStorage::open(&database_path, &legacy).map_err(|error| match error {
                    StorageError::InvalidKey => EnvelopeError::LegacyKeyMismatch,
                    error => error.into(),
                })?;
            storage.rekey(slot.keys.storage_key())?;
            EncryptedStorage::open(&database_path, slot.keys.storage_key())?.close()?;
        }
        Err(error) => return Err(error.into()),
    }

    let final_slot = Slot {
        state: SlotState::Final,
        keys: slot.keys.clone(),
    };
    atomic_replace_slot(&state_dir.join(KEYSLOT_FILENAME), secret, &final_slot)?;
    Ok(slot.keys)
}

pub(super) fn verify_state(state_dir: &Path, keys: &StateKeys) -> Result<()> {
    let database_path = state_dir.join(DATABASE_FILENAME);
    EncryptedStorage::open(&database_path, keys.storage_key())?.close()?;
    ChunkStore::verify_key(state_dir.join(CHUNKS_DIRECTORY), keys.chunk_store_key())?;
    Ok(())
}

fn random_key() -> Result<[u8; DATA_KEY_BYTES]> {
    let mut key = [0_u8; DATA_KEY_BYTES];
    getrandom::fill(&mut key).map_err(|_| EnvelopeError::Randomness)?;
    Ok(key)
}
