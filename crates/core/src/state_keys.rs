//! Keys for this host's local state, and the lock that makes the daemon its
//! only owner.
//!
//! The database key is random per host and lives in an owner-only file next
//! to the database, so changing the mesh secret never touches local storage.
//! The content-identity key derives from the mesh secret, because every host
//! must compute the same content IDs.

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
use zeroize::Zeroizing;

use crate::{
    crypto::{MeshSecret, SecretError},
    storage::StorageKey,
};

pub const KEY_FILENAME: &str = "history.key";
pub const STORE_LOCK_FILENAME: &str = "store.lock";
const KEY_BYTES: usize = 32;

/// Written by 0.3 and earlier: the database key wrapped by the mesh secret.
/// Read once to migrate, then removed.
const LEGACY_KEYSLOT_FILENAME: &str = "history.keyslot";
const LEGACY_PENDING_FILENAME: &str = "history.keyslot.next";

pub type Result<T> = std::result::Result<T, StateKeyError>;

#[derive(Debug, Error)]
pub enum StateKeyError {
    #[error("another clip-sync daemon holds the exclusive state lock")]
    StoreBusy,
    #[error("{0} is not a regular file owned by the current user with mode 0600")]
    UnsafeFile(PathBuf),
    #[error("{0} is not a valid 32-byte key file")]
    InvalidKeyFile(PathBuf),
    #[error("the legacy keyslot is corrupt or cannot be read with this mesh secret")]
    InvalidLegacyKeyslot,
    #[error(
        "the legacy keyslot is mid-migration or mid-rotation; finish it with clip-sync 0.3 first"
    )]
    UnfinishedLegacyKeyslot,
    #[error(
        "the legacy keyslot was rotated to another mesh secret, so content IDs would change; \
         migrate with the secret it was created under"
    )]
    RotatedLegacyKeyslot,
    #[error("secure randomness is unavailable")]
    Randomness,
    #[error(transparent)]
    Secret(#[from] SecretError),
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
    /// Returns [`StateKeyError::StoreBusy`] when another process owns the
    /// lock, or an I/O/security error for an unsafe state path.
    pub fn acquire(state_dir: impl AsRef<Path>) -> Result<Self> {
        let state_dir = state_dir.as_ref();
        create_private_directory(state_dir)?;
        let file = open_lock_file(&state_dir.join(STORE_LOCK_FILENAME))?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Self {
                file,
                state_dir: state_dir.to_path_buf(),
            }),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                Err(StateKeyError::StoreBusy)
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
        let _ = FileExt::unlock(&self.file);
    }
}

#[derive(Clone)]
pub struct StateKeys {
    storage: StorageKey,
    content_identity: Zeroizing<[u8; KEY_BYTES]>,
}

impl StateKeys {
    /// Loads this host's database key, creating it on first start or
    /// migrating it out of a legacy keyslot.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsafe or malformed key file, a legacy keyslot
    /// that cannot be migrated, or an I/O failure.
    pub fn open_or_create(lock: &StoreLock, secret: &MeshSecret) -> Result<Self> {
        let state_dir = lock.state_dir();
        let key_path = state_dir.join(KEY_FILENAME);
        let storage = if exists(&key_path)? {
            read_key_file(&key_path)?
        } else if exists(&state_dir.join(LEGACY_KEYSLOT_FILENAME))? {
            migrate_legacy_keyslot(state_dir, secret)?
        } else {
            let mut key = Zeroizing::new([0_u8; KEY_BYTES]);
            getrandom::fill(key.as_mut()).map_err(|_| StateKeyError::Randomness)?;
            write_key_file(&key_path, &key)?;
            key
        };
        Ok(Self {
            storage: StorageKey::from_bytes(*storage),
            content_identity: secret.content_key()?,
        })
    }

    #[must_use]
    pub const fn storage_key(&self) -> &StorageKey {
        &self.storage
    }

    #[must_use]
    pub fn content_identity_key(&self) -> &[u8; KEY_BYTES] {
        &self.content_identity
    }
}

impl std::fmt::Debug for StateKeys {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("StateKeys([REDACTED])")
    }
}

/// Moves the database key out of the legacy keyslot. The key file is durable
/// before the keyslot is removed, so an interruption at any point leaves one
/// of the two readable.
fn migrate_legacy_keyslot(
    state_dir: &Path,
    secret: &MeshSecret,
) -> Result<Zeroizing<[u8; KEY_BYTES]>> {
    if exists(&state_dir.join(LEGACY_PENDING_FILENAME))? {
        return Err(StateKeyError::UnfinishedLegacyKeyslot);
    }
    let keyslot_path = state_dir.join(LEGACY_KEYSLOT_FILENAME);
    let legacy = legacy::read(&keyslot_path, secret)?;
    // The keyslot kept the content key of the secret it was created under.
    // Deriving it from the current secret is only safe when they still
    // match, i.e. the secret was never rotated.
    if !bool::from(legacy.content_identity[..].ct_eq(&secret.content_key()?[..])) {
        return Err(StateKeyError::RotatedLegacyKeyslot);
    }
    write_key_file(&state_dir.join(KEY_FILENAME), &legacy.storage)?;
    fs::remove_file(&keyslot_path)?;
    sync_directory(state_dir)?;
    Ok(legacy.storage)
}

fn read_key_file(path: &Path) -> Result<Zeroizing<[u8; KEY_BYTES]>> {
    let file = open_private_file(path)?;
    let mut encoded = Zeroizing::new(Vec::with_capacity(KEY_BYTES));
    file.take(KEY_BYTES as u64 + 1).read_to_end(&mut encoded)?;
    let key: [u8; KEY_BYTES] = encoded
        .as_slice()
        .try_into()
        .map_err(|_| StateKeyError::InvalidKeyFile(path.to_path_buf()))?;
    Ok(Zeroizing::new(key))
}

fn write_key_file(path: &Path, key: &[u8; KEY_BYTES]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "key path has no parent"))?;
    let temporary = parent.join(format!(".{KEY_FILENAME}.{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(key)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        sync_directory(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(unix)]
fn open_private_file(path: &Path) -> Result<File> {
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
        return Err(StateKeyError::UnsafeFile(path.to_path_buf()));
    }
    Ok(File::from(fd))
}

#[cfg(not(unix))]
fn open_private_file(path: &Path) -> Result<File> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(StateKeyError::UnsafeFile(path.to_path_buf()));
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
        return Err(StateKeyError::UnsafeFile(path.to_path_buf()));
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
        .truncate(false)
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
        return Err(StateKeyError::UnsafeFile(path.to_path_buf()));
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

fn exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

mod legacy {
    //! Read-only decoder for the 0.3 keyslot format.

    use super::{
        Aead, KEY_BYTES, KeyInit, MeshSecret, Path, Payload, Read, Result, StateKeyError,
        XChaCha20Poly1305, XNonce, Zeroizing, open_private_file,
    };

    pub(super) const MAGIC: &[u8; 8] = b"CSKEYS01";
    pub(super) const VERSION: u8 = 1;
    pub(super) const NONCE_BYTES: usize = 24;
    pub(super) const FINAL_STATE: u8 = 0;
    const PLAINTEXT_BYTES: usize = 1 + KEY_BYTES * 3;
    const TAG_BYTES: usize = 16;
    const KEYSLOT_BYTES: usize = MAGIC.len() + 1 + NONCE_BYTES + PLAINTEXT_BYTES + TAG_BYTES;

    pub(super) struct LegacyKeys {
        pub(super) storage: Zeroizing<[u8; KEY_BYTES]>,
        pub(super) content_identity: Zeroizing<[u8; KEY_BYTES]>,
    }

    pub(super) fn read(path: &Path, secret: &MeshSecret) -> Result<LegacyKeys> {
        let file = open_private_file(path)?;
        let mut encoded = Zeroizing::new(Vec::new());
        file.take(KEYSLOT_BYTES as u64 + 1)
            .read_to_end(&mut encoded)?;
        let header = MAGIC.len() + 1;
        if encoded.len() != KEYSLOT_BYTES
            || &encoded[..MAGIC.len()] != MAGIC
            || encoded[MAGIC.len()] != VERSION
        {
            return Err(StateKeyError::InvalidLegacyKeyslot);
        }
        let key = secret.envelope_key()?;
        let cipher = XChaCha20Poly1305::new_from_slice(key.as_ref())
            .map_err(|_| StateKeyError::InvalidLegacyKeyslot)?;
        let nonce = XNonce::try_from(&encoded[header..header + NONCE_BYTES])
            .map_err(|_| StateKeyError::InvalidLegacyKeyslot)?;
        let plaintext = Zeroizing::new(
            cipher
                .decrypt(
                    &nonce,
                    Payload {
                        msg: &encoded[header + NONCE_BYTES..],
                        aad: &encoded[..header],
                    },
                )
                .map_err(|_| StateKeyError::InvalidLegacyKeyslot)?,
        );
        if plaintext[0] != FINAL_STATE {
            return Err(StateKeyError::UnfinishedLegacyKeyslot);
        }
        let key_at = |index: usize| {
            let start = 1 + index * KEY_BYTES;
            let mut key = Zeroizing::new([0_u8; KEY_BYTES]);
            key.copy_from_slice(&plaintext[start..start + KEY_BYTES]);
            key
        };
        // Index 1 held the retired chunk-store key.
        Ok(LegacyKeys {
            storage: key_at(0),
            content_identity: key_at(2),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn secret(byte: u8) -> MeshSecret {
        MeshSecret::parse(&[byte; 32]).unwrap()
    }

    /// Writes a keyslot the way 0.3 did, for migration tests.
    fn write_legacy_keyslot(state_dir: &Path, secret: &MeshSecret, storage: [u8; 32]) {
        let mut plaintext = vec![legacy::FINAL_STATE];
        plaintext.extend_from_slice(&storage);
        plaintext.extend_from_slice(&[0; 32]);
        plaintext.extend_from_slice(secret.content_key().unwrap().as_ref());
        let mut encoded = legacy::MAGIC.to_vec();
        encoded.push(legacy::VERSION);
        let header = encoded.clone();
        let nonce = [5_u8; legacy::NONCE_BYTES];
        encoded.extend_from_slice(&nonce);
        let key = secret.envelope_key().unwrap();
        let cipher = XChaCha20Poly1305::new_from_slice(key.as_ref()).unwrap();
        let sealed = cipher
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: &plaintext,
                    aad: &header,
                },
            )
            .unwrap();
        encoded.extend_from_slice(&sealed);
        let path = state_dir.join(LEGACY_KEYSLOT_FILENAME);
        fs::write(&path, encoded).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn first_start_creates_a_private_key_that_later_starts_reuse() {
        let directory = tempfile::tempdir().unwrap();
        let lock = StoreLock::acquire(directory.path()).unwrap();
        let first = StateKeys::open_or_create(&lock, &secret(1)).unwrap();
        let mode = fs::metadata(directory.path().join(KEY_FILENAME))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);

        let again = StateKeys::open_or_create(&lock, &secret(1)).unwrap();
        assert_eq!(first.storage.as_bytes(), again.storage.as_bytes());
    }

    #[test]
    fn database_key_does_not_depend_on_the_mesh_secret() {
        let directory = tempfile::tempdir().unwrap();
        let lock = StoreLock::acquire(directory.path()).unwrap();
        let before = StateKeys::open_or_create(&lock, &secret(1)).unwrap();
        let rotated = StateKeys::open_or_create(&lock, &secret(2)).unwrap();
        assert_eq!(before.storage.as_bytes(), rotated.storage.as_bytes());
        assert_ne!(
            before.content_identity_key(),
            rotated.content_identity_key()
        );
    }

    #[test]
    fn key_file_with_loose_permissions_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let lock = StoreLock::acquire(directory.path()).unwrap();
        StateKeys::open_or_create(&lock, &secret(1)).unwrap();
        let path = directory.path().join(KEY_FILENAME);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            StateKeys::open_or_create(&lock, &secret(1)),
            Err(StateKeyError::UnsafeFile(_))
        ));
    }

    #[test]
    fn legacy_keyslot_migrates_to_a_key_file_and_is_removed() {
        let directory = tempfile::tempdir().unwrap();
        let lock = StoreLock::acquire(directory.path()).unwrap();
        write_legacy_keyslot(directory.path(), &secret(3), [9; 32]);

        let keys = StateKeys::open_or_create(&lock, &secret(3)).unwrap();
        assert_eq!(keys.storage.as_bytes(), &[9; 32]);
        assert!(!directory.path().join(LEGACY_KEYSLOT_FILENAME).exists());
        let reopened = StateKeys::open_or_create(&lock, &secret(3)).unwrap();
        assert_eq!(reopened.storage.as_bytes(), &[9; 32]);
    }

    #[test]
    fn legacy_keyslot_is_kept_when_the_secret_does_not_open_it() {
        let directory = tempfile::tempdir().unwrap();
        let lock = StoreLock::acquire(directory.path()).unwrap();
        write_legacy_keyslot(directory.path(), &secret(3), [9; 32]);

        assert!(matches!(
            StateKeys::open_or_create(&lock, &secret(4)),
            Err(StateKeyError::InvalidLegacyKeyslot)
        ));
        assert!(directory.path().join(LEGACY_KEYSLOT_FILENAME).exists());
        assert!(!directory.path().join(KEY_FILENAME).exists());
    }

    #[test]
    fn second_lock_holder_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let _held = StoreLock::acquire(directory.path()).unwrap();
        assert!(matches!(
            StoreLock::acquire(directory.path()),
            Err(StateKeyError::StoreBusy)
        ));
    }
}
