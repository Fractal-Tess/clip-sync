use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use fs2::FileExt;
use subtle::ConstantTimeEq;
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{
    crypto::{MeshSecret, SecretError},
    payload::{ChunkStore, ChunkStoreError, ChunkStoreKey},
    storage::{EncryptedStorage, StorageError, StorageKey},
};

pub const KEYSLOT_FILENAME: &str = "history.keyslot";
pub const STORE_LOCK_FILENAME: &str = "store.lock";
const PENDING_FILENAME: &str = "history.keyslot.next";
const DATABASE_FILENAME: &str = "history.db";
const CHUNKS_DIRECTORY: &str = "chunks";
const KEYSLOT_MAGIC: &[u8; 8] = b"CSKEYS01";
const KEYSLOT_VERSION: u8 = 1;
const NONCE_BYTES: usize = 24;
const DATA_KEY_BYTES: usize = 32;
const PLAINTEXT_BYTES: usize = 1 + DATA_KEY_BYTES * 3;
const TAG_BYTES: usize = 16;
const KEYSLOT_BYTES: usize = KEYSLOT_MAGIC.len() + 1 + NONCE_BYTES + PLAINTEXT_BYTES + TAG_BYTES;

pub type Result<T> = std::result::Result<T, EnvelopeError>;

#[derive(Debug, Error)]
pub enum EnvelopeError {
    #[error("another clip-sync daemon or store operation holds the exclusive state lock")]
    StoreBusy,
    #[error("the state lock is not a regular owner-only file")]
    UnsafeLock,
    #[error("the keyslot is not a regular file owned by the current user with mode 0600")]
    UnsafeKeyslot,
    #[error("the keyslot has an unsupported or corrupt encoding")]
    InvalidKeyslot,
    #[error("the supplied mesh secret cannot authenticate the keyslot")]
    WrongSecret,
    #[error("secure randomness is unavailable")]
    Randomness,
    #[error("a pending rekey candidate is invalid or does not match the committed data keys")]
    InvalidPendingRekey,
    #[error("the legacy directly-derived database cannot be opened with the supplied old secret")]
    LegacyKeyMismatch,
    #[error("injected rekey interruption after {0}")]
    InjectedInterruption(&'static str),
    #[error(transparent)]
    Secret(#[from] SecretError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    ChunkStore(#[from] ChunkStoreError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Process-lifetime exclusive owner of the local daemon/store state.
pub struct StoreLock {
    file: File,
    state_dir: PathBuf,
}

impl StoreLock {
    /// Creates and exclusively locks the owner-only state lock without waiting.
    ///
    /// # Errors
    ///
    /// Returns [`EnvelopeError::StoreBusy`] when another process owns the lock,
    /// or an I/O/security error for an unsafe state path.
    pub fn acquire(state_dir: impl AsRef<Path>) -> Result<Self> {
        let state_dir = state_dir.as_ref();
        create_private_directory(state_dir)?;
        let path = state_dir.join(STORE_LOCK_FILENAME);
        let file = open_lock_file(&path)?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Self {
                file,
                state_dir: state_dir.to_path_buf(),
            }),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                Err(EnvelopeError::StoreBusy)
            }
            Err(error) => Err(error.into()),
        }
    }

    #[must_use]
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// Stable store and content-identity keys authenticated and wrapped by the mesh secret.
#[derive(Clone)]
pub struct StateKeys {
    storage: StorageKey,
    chunks: ChunkStoreKey,
    content_identity: Zeroizing<[u8; DATA_KEY_BYTES]>,
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
                    return Ok(committed.keys);
                }
                Err(error) => return Err(error),
            }
        }

        if path_exists(&pending_path)? {
            return Err(EnvelopeError::InvalidPendingRekey);
        }

        initialize_keyslot(state_dir, secret)
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

    fn same_data_keys(&self, other: &Self) -> bool {
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

/// Result of an idempotent offline mesh-secret rotation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RekeyOutcome {
    Rotated,
    AlreadyCurrent,
}

/// Rewraps stable local data keys under a new mesh secret.
///
/// This function acquires the same exclusive lock held by the daemon. It
/// durably writes and verifies a pending sidecar before atomically replacing
/// the committed keyslot and then verifies a fresh reopen.
///
/// # Errors
///
/// Returns an error when the daemon is active, either secret is wrong, local
/// state fails verification, or the atomic update cannot be made durable.
pub fn rekey_state(
    state_dir: impl AsRef<Path>,
    old_secret: &MeshSecret,
    new_secret: &MeshSecret,
) -> Result<RekeyOutcome> {
    let lock = StoreLock::acquire(state_dir)?;
    rekey_locked(&lock, old_secret, new_secret, |_| Ok(()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlotState {
    Final = 0,
    DatabaseMigration = 1,
}

#[derive(Clone)]
struct Slot {
    state: SlotState,
    keys: StateKeys,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RekeyPhase {
    PendingDurable,
    SidecarCommitted,
}

fn rekey_locked(
    lock: &StoreLock,
    old_secret: &MeshSecret,
    new_secret: &MeshSecret,
    mut phase_hook: impl FnMut(RekeyPhase) -> Result<()>,
) -> Result<RekeyOutcome> {
    let state_dir = lock.state_dir();
    let keyslot_path = state_dir.join(KEYSLOT_FILENAME);
    let pending_path = state_dir.join(PENDING_FILENAME);
    if bool::from(
        old_secret
            .envelope_key()?
            .as_ref()
            .ct_eq(new_secret.envelope_key()?.as_ref()),
    ) {
        StateKeys::open_or_create(lock, old_secret)?;
        return Ok(RekeyOutcome::AlreadyCurrent);
    }

    if !path_exists(&keyslot_path)? {
        StateKeys::open_or_create(lock, old_secret)?;
    }

    let slot = match read_slot(&keyslot_path, old_secret) {
        Ok(slot) => slot,
        Err(EnvelopeError::WrongSecret) => match read_slot(&keyslot_path, new_secret) {
            Ok(slot) => {
                let keys = recover_database_migration(state_dir, new_secret, slot)?;
                verify_state(state_dir, &keys)?;
                return Ok(RekeyOutcome::AlreadyCurrent);
            }
            Err(EnvelopeError::WrongSecret) => return Err(EnvelopeError::WrongSecret),
            Err(error) => return Err(error),
        },
        Err(error) => return Err(error),
    };
    let keys = recover_database_migration(state_dir, old_secret, slot)?;
    verify_state(state_dir, &keys)?;

    if path_exists(&pending_path)? {
        let pending =
            read_slot(&pending_path, new_secret).map_err(|_| EnvelopeError::InvalidPendingRekey)?;
        if pending.state != SlotState::Final || !pending.keys.same_data_keys(&keys) {
            return Err(EnvelopeError::InvalidPendingRekey);
        }
    } else {
        let pending = Slot {
            state: SlotState::Final,
            keys: keys.clone(),
        };
        write_slot_new(&pending_path, new_secret, &pending)?;
        sync_directory(state_dir)?;
    }

    let pending =
        read_slot(&pending_path, new_secret).map_err(|_| EnvelopeError::InvalidPendingRekey)?;
    if pending.state != SlotState::Final || !pending.keys.same_data_keys(&keys) {
        return Err(EnvelopeError::InvalidPendingRekey);
    }
    verify_state(state_dir, &pending.keys)?;
    phase_hook(RekeyPhase::PendingDurable)?;

    commit_pending(&pending_path, &keyslot_path)?;
    phase_hook(RekeyPhase::SidecarCommitted)?;

    let committed = read_slot(&keyslot_path, new_secret)?;
    if committed.state != SlotState::Final || !committed.keys.same_data_keys(&keys) {
        return Err(EnvelopeError::InvalidKeyslot);
    }
    verify_state(state_dir, &committed.keys)?;
    Ok(RekeyOutcome::Rotated)
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

fn recover_database_migration(
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

fn verify_state(state_dir: &Path, keys: &StateKeys) -> Result<()> {
    let database_path = state_dir.join(DATABASE_FILENAME);
    EncryptedStorage::open(&database_path, keys.storage_key())?.close()?;
    ChunkStore::verify_key(state_dir.join(CHUNKS_DIRECTORY), keys.chunk_store_key())?;
    Ok(())
}

fn read_slot(path: &Path, secret: &MeshSecret) -> Result<Slot> {
    let file = open_private_keyslot(path)?;
    let mut encoded = Zeroizing::new(Vec::new());
    file.take((KEYSLOT_BYTES + 1) as u64)
        .read_to_end(&mut encoded)?;
    if encoded.len() != KEYSLOT_BYTES
        || &encoded[..KEYSLOT_MAGIC.len()] != KEYSLOT_MAGIC
        || encoded[KEYSLOT_MAGIC.len()] != KEYSLOT_VERSION
    {
        return Err(EnvelopeError::InvalidKeyslot);
    }

    let header_bytes = KEYSLOT_MAGIC.len() + 1;
    let nonce_end = header_bytes + NONCE_BYTES;
    let envelope_key = secret.envelope_key()?;
    let cipher = XChaCha20Poly1305::new_from_slice(envelope_key.as_ref())
        .map_err(|_| EnvelopeError::InvalidKeyslot)?;
    let nonce = XNonce::try_from(&encoded[header_bytes..nonce_end])
        .map_err(|_| EnvelopeError::InvalidKeyslot)?;
    let plaintext = cipher
        .decrypt(
            &nonce,
            Payload {
                msg: &encoded[nonce_end..],
                aad: &encoded[..header_bytes],
            },
        )
        .map_err(|_| EnvelopeError::WrongSecret)?;
    decode_plaintext(&plaintext)
}

fn encode_slot(secret: &MeshSecret, slot: &Slot) -> Result<Zeroizing<Vec<u8>>> {
    let mut nonce = [0_u8; NONCE_BYTES];
    getrandom::fill(&mut nonce).map_err(|_| EnvelopeError::Randomness)?;
    let mut plaintext = Zeroizing::new(Vec::with_capacity(PLAINTEXT_BYTES));
    plaintext.push(slot.state as u8);
    plaintext.extend_from_slice(slot.keys.storage.as_bytes());
    plaintext.extend_from_slice(slot.keys.chunks.as_bytes());
    plaintext.extend_from_slice(slot.keys.content_identity.as_ref());

    let mut encoded = Zeroizing::new(Vec::with_capacity(KEYSLOT_BYTES));
    encoded.extend_from_slice(KEYSLOT_MAGIC);
    encoded.push(KEYSLOT_VERSION);
    encoded.extend_from_slice(&nonce);
    let envelope_key = secret.envelope_key()?;
    let cipher = XChaCha20Poly1305::new_from_slice(envelope_key.as_ref())
        .map_err(|_| EnvelopeError::InvalidKeyslot)?;
    let nonce = XNonce::from(nonce);
    let aad_end = KEYSLOT_MAGIC.len() + 1;
    let encrypted = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: &plaintext,
                aad: &encoded[..aad_end],
            },
        )
        .map_err(|_| EnvelopeError::InvalidKeyslot)?;
    encoded.extend_from_slice(&encrypted);
    Ok(encoded)
}

fn decode_plaintext(plaintext: &[u8]) -> Result<Slot> {
    if plaintext.len() != PLAINTEXT_BYTES {
        return Err(EnvelopeError::InvalidKeyslot);
    }
    let state = match plaintext[0] {
        0 => SlotState::Final,
        1 => SlotState::DatabaseMigration,
        _ => return Err(EnvelopeError::InvalidKeyslot),
    };
    let storage_end = 1 + DATA_KEY_BYTES;
    let chunks_end = storage_end + DATA_KEY_BYTES;
    let storage = plaintext[1..storage_end]
        .try_into()
        .map_err(|_| EnvelopeError::InvalidKeyslot)?;
    let chunks = plaintext[storage_end..chunks_end]
        .try_into()
        .map_err(|_| EnvelopeError::InvalidKeyslot)?;
    let content_identity = plaintext[chunks_end..]
        .try_into()
        .map_err(|_| EnvelopeError::InvalidKeyslot)?;
    Ok(Slot {
        state,
        keys: StateKeys {
            storage: StorageKey::from_bytes(storage),
            chunks: ChunkStoreKey::from_bytes(chunks),
            content_identity: Zeroizing::new(content_identity),
        },
    })
}

fn atomic_replace_slot(path: &Path, secret: &MeshSecret, slot: &Slot) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "keyslot path has no parent"))?;
    let temporary = parent.join(format!(".history.keyslot.{}.tmp", Uuid::new_v4()));
    let result = (|| {
        write_slot_new(&temporary, secret, slot)?;
        fs::rename(&temporary, path)?;
        sync_directory(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn write_slot_new(path: &Path, secret: &MeshSecret, slot: &Slot) -> Result<()> {
    let encoded = encode_slot(secret, slot)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(&encoded)?;
    file.sync_all()?;
    Ok(())
}

fn commit_pending(pending: &Path, committed: &Path) -> Result<()> {
    let parent = committed
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "keyslot path has no parent"))?;
    fs::rename(pending, committed)?;
    sync_directory(parent)?;
    Ok(())
}

fn random_key() -> Result<[u8; DATA_KEY_BYTES]> {
    let mut key = [0_u8; DATA_KEY_BYTES];
    getrandom::fill(&mut key).map_err(|_| EnvelopeError::Randomness)?;
    Ok(key)
}

#[cfg(unix)]
fn open_private_keyslot(path: &Path) -> Result<File> {
    use rustix::fs::{FileType, Mode, OFlags, fstat, open};

    let fd = open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;
    let stat = fstat(&fd).map_err(io::Error::from)?;
    if !FileType::from_raw_mode(stat.st_mode).is_file()
        || stat.st_uid != rustix::process::getuid().as_raw()
        || stat.st_mode & 0o777 != 0o600
    {
        return Err(EnvelopeError::UnsafeKeyslot);
    }
    Ok(File::from(fd))
}

#[cfg(not(unix))]
fn open_private_keyslot(path: &Path) -> Result<File> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(EnvelopeError::UnsafeKeyslot);
    }
    Ok(File::open(path)?)
}

#[cfg(unix)]
fn open_lock_file(path: &Path) -> Result<File> {
    use rustix::fs::{FileType, Mode, OFlags, fchmod, fstat, open};

    let fd = open(
        path,
        OFlags::RDWR | OFlags::CREATE | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(io::Error::from)?;
    let stat = fstat(&fd).map_err(io::Error::from)?;
    if !FileType::from_raw_mode(stat.st_mode).is_file()
        || stat.st_uid != rustix::process::getuid().as_raw()
    {
        return Err(EnvelopeError::UnsafeLock);
    }
    fchmod(&fd, Mode::RUSR | Mode::WUSR).map_err(io::Error::from)?;
    Ok(File::from(fd))
}

#[cfg(not(unix))]
fn open_lock_file(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(path)?)
}

#[cfg(unix)]
fn create_private_directory(path: &Path) -> Result<()> {
    use rustix::fs::{FileType, Mode, OFlags, fchmod, fstat, open};

    fs::create_dir_all(path)?;
    let fd = open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;
    let stat = fstat(&fd).map_err(io::Error::from)?;
    if !FileType::from_raw_mode(stat.st_mode).is_dir()
        || stat.st_uid != rustix::process::getuid().as_raw()
    {
        return Err(EnvelopeError::UnsafeLock);
    }
    fchmod(&fd, Mode::RUSR | Mode::WUSR | Mode::XUSR).map_err(io::Error::from)?;
    Ok(())
}

#[cfg(not(unix))]
fn create_private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    Ok(())
}

fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

fn path_exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::{ChunkStoreConfig, StoredManifest};
    use std::io::Cursor;
    use tokio_util::sync::CancellationToken;

    fn secret(byte: u8) -> MeshSecret {
        MeshSecret::parse(&[byte; 32]).expect("mesh secret")
    }

    #[test]
    fn interruption_before_commit_resumes_without_changing_data_keys() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let old = secret(1);
        let new = secret(2);
        let lock = StoreLock::acquire(directory.path()).expect("store lock");
        let original = StateKeys::open_or_create(&lock, &old).expect("initialize");

        let error = rekey_locked(&lock, &old, &new, |phase| {
            if phase == RekeyPhase::PendingDurable {
                Err(EnvelopeError::InjectedInterruption("pending keyslot sync"))
            } else {
                Ok(())
            }
        })
        .expect_err("interrupt rekey");
        assert!(matches!(error, EnvelopeError::InjectedInterruption(_)));
        assert!(read_slot(&directory.path().join(KEYSLOT_FILENAME), &old).is_ok());
        assert!(directory.path().join(PENDING_FILENAME).exists());

        let outcome =
            rekey_locked(&lock, &old, &new, |_| Ok(())).expect("resume interrupted rekey");
        assert_eq!(outcome, RekeyOutcome::Rotated);
        let reopened = StateKeys::open_or_create(&lock, &new).expect("open with new secret");
        assert!(original.same_data_keys(&reopened));
        assert!(matches!(
            StateKeys::open_or_create(&lock, &old),
            Err(EnvelopeError::WrongSecret)
        ));
    }

    #[test]
    fn interruption_after_commit_is_idempotently_recovered() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let old = secret(3);
        let new = secret(4);
        let lock = StoreLock::acquire(directory.path()).expect("store lock");
        StateKeys::open_or_create(&lock, &old).expect("initialize");

        let error = rekey_locked(&lock, &old, &new, |phase| {
            if phase == RekeyPhase::SidecarCommitted {
                Err(EnvelopeError::InjectedInterruption("sidecar commit"))
            } else {
                Ok(())
            }
        })
        .expect_err("interrupt after commit");
        assert!(matches!(error, EnvelopeError::InjectedInterruption(_)));
        assert_eq!(
            rekey_locked(&lock, &old, &new, |_| Ok(())).expect("recognize committed rotation"),
            RekeyOutcome::AlreadyCurrent
        );
    }

    #[test]
    fn new_secret_recovers_a_verified_precommit_candidate() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let old = secret(5);
        let new = secret(6);
        let lock = StoreLock::acquire(directory.path()).expect("store lock");
        let original = StateKeys::open_or_create(&lock, &old).expect("initialize");
        rekey_locked(&lock, &old, &new, |phase| {
            if phase == RekeyPhase::PendingDurable {
                Err(EnvelopeError::InjectedInterruption("pending keyslot sync"))
            } else {
                Ok(())
            }
        })
        .expect_err("interrupt before commit");

        let recovered = StateKeys::open_or_create(&lock, &new).expect("recover pending keyslot");
        assert!(original.same_data_keys(&recovered));
        assert!(!directory.path().join(PENDING_FILENAME).exists());
        assert!(read_slot(&directory.path().join(KEYSLOT_FILENAME), &new).is_ok());
    }

    #[test]
    fn wrong_secret_is_read_only_and_cannot_rotate() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let old = secret(7);
        let wrong = secret(8);
        let new = secret(9);
        let lock = StoreLock::acquire(directory.path()).expect("store lock");
        StateKeys::open_or_create(&lock, &old).expect("initialize");
        verify_state(
            directory.path(),
            &read_slot(&directory.path().join(KEYSLOT_FILENAME), &old)
                .expect("read keyslot")
                .keys,
        )
        .expect("create database");
        let before = snapshot_files(directory.path());

        assert!(matches!(
            StateKeys::open_or_create(&lock, &wrong),
            Err(EnvelopeError::WrongSecret)
        ));
        assert!(matches!(
            rekey_locked(&lock, &wrong, &new, |_| Ok(())),
            Err(EnvelopeError::WrongSecret)
        ));
        assert_eq!(snapshot_files(directory.path()), before);
    }

    #[test]
    fn direct_derived_database_and_chunks_migrate_without_payload_rewrite() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let old = secret(10);
        let new = secret(11);
        let database_path = directory.path().join(DATABASE_FILENAME);
        let legacy_storage = old.storage_key().expect("legacy storage key");
        let legacy_chunks = old.chunk_store_key().expect("legacy chunk key");
        let legacy_content = old.content_key().expect("legacy content key");
        let marker = "direct-derived-migration-marker";
        let mut storage =
            EncryptedStorage::open(&database_path, &legacy_storage).expect("legacy database");
        storage
            .set_meta_value("migration_probe", marker)
            .expect("write probe");
        storage.checkpoint().expect("checkpoint");
        storage.close().expect("close database");

        let chunk_root = directory.path().join(CHUNKS_DIRECTORY);
        let mut chunk_store =
            ChunkStore::open(&chunk_root, &legacy_chunks, ChunkStoreConfig::default())
                .expect("legacy chunk store");
        let payload = b"payload-ciphertext-must-not-change";
        let blob = chunk_store
            .stage_reader(
                &mut Cursor::new(payload),
                payload.len() as u64,
                &CancellationToken::new(),
            )
            .expect("stage payload");
        let manifest_id = chunk_store
            .commit_manifest(&StoredManifest::Blob(blob.clone()))
            .expect("commit manifest");
        let object_path = chunk_root
            .join("objects")
            .join(blob.chunks()[0].id().to_string());
        let object_before = fs::read(&object_path).expect("read encrypted chunk");
        drop(chunk_store);

        let lock = StoreLock::acquire(directory.path()).expect("store lock");
        let keys = StateKeys::open_or_create(&lock, &old).expect("migrate direct storage");
        assert!(!bool::from(
            keys.storage_key()
                .as_bytes()
                .ct_eq(legacy_storage.as_bytes())
        ));
        assert!(bool::from(
            keys.chunk_store_key()
                .as_bytes()
                .ct_eq(legacy_chunks.as_bytes())
        ));
        assert!(bool::from(
            keys.content_identity_key().ct_eq(legacy_content.as_ref())
        ));
        assert!(matches!(
            EncryptedStorage::open(&database_path, &legacy_storage),
            Err(StorageError::InvalidKey)
        ));
        assert_eq!(
            EncryptedStorage::open(&database_path, keys.storage_key())
                .expect("open migrated database")
                .meta_value("migration_probe")
                .expect("read probe"),
            Some(marker.to_owned())
        );

        let reopened = ChunkStore::open(
            &chunk_root,
            keys.chunk_store_key(),
            ChunkStoreConfig::default(),
        )
        .expect("open migrated chunk store");
        let stored_blob = match reopened.manifest(manifest_id).expect("load manifest") {
            StoredManifest::Blob(blob) => blob,
            StoredManifest::Files(_) => panic!("expected blob manifest"),
        };
        let mut restored = Vec::new();
        reopened
            .read_blob(&stored_blob, &mut restored, &CancellationToken::new())
            .expect("decrypt migrated payload");
        assert_eq!(restored, payload);
        drop(reopened);

        assert_eq!(
            rekey_locked(&lock, &old, &new, |_| Ok(())).expect("rotate wrapper"),
            RekeyOutcome::Rotated
        );
        assert_eq!(
            fs::read(&object_path).expect("read chunk after rekey"),
            object_before
        );
        assert!(StateKeys::open_or_create(&lock, &new).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn keyslot_is_mode_0600_and_rejects_permission_drift() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("temporary directory");
        let old = secret(12);
        let lock = StoreLock::acquire(directory.path()).expect("store lock");
        StateKeys::open_or_create(&lock, &old).expect("initialize");
        let path = directory.path().join(KEYSLOT_FILENAME);
        assert_eq!(
            fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
            0o600
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).expect("make keyslot unsafe");
        assert!(matches!(
            StateKeys::open_or_create(&lock, &old),
            Err(EnvelopeError::UnsafeKeyslot)
        ));
    }

    #[test]
    fn keyslot_contains_no_mesh_secret_or_unwrapped_data_keys() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let old_bytes = [13; 32];
        let new_bytes = [14; 32];
        let old = MeshSecret::parse(&old_bytes).expect("old secret");
        let new = MeshSecret::parse(&new_bytes).expect("new secret");
        let lock = StoreLock::acquire(directory.path()).expect("store lock");
        let keys = StateKeys::open_or_create(&lock, &old).expect("initialize");
        let storage_bytes = *keys.storage_key().as_bytes();
        let chunk_bytes = *keys.chunk_store_key().as_bytes();
        let content_bytes = *keys.content_identity_key();

        rekey_locked(&lock, &old, &new, |_| Ok(())).expect("rotate");
        let encoded = fs::read(directory.path().join(KEYSLOT_FILENAME)).expect("read keyslot");
        for plaintext in [
            old_bytes.as_slice(),
            new_bytes.as_slice(),
            storage_bytes.as_slice(),
            chunk_bytes.as_slice(),
            content_bytes.as_slice(),
        ] {
            assert!(
                !contains_subslice(&encoded, plaintext),
                "keyslot exposed secret bytes"
            );
        }
    }

    #[test]
    fn exclusive_lock_rejects_a_second_owner() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let _first = StoreLock::acquire(directory.path()).expect("first lock");
        assert!(matches!(
            StoreLock::acquire(directory.path()),
            Err(EnvelopeError::StoreBusy)
        ));
    }

    fn snapshot_files(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut paths = fs::read_dir(root)
            .expect("read state directory")
            .map(|entry| entry.expect("directory entry").path())
            .filter(|path| path.is_file())
            .collect::<Vec<_>>();
        paths.sort();
        paths
            .into_iter()
            .map(|path| {
                let bytes = fs::read(&path).expect("read state file");
                (path, bytes)
            })
            .collect()
    }

    fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }
}
