use std::{fmt, io::Read, path::Path};

use hkdf::Hkdf;
use sha2::Sha256;
use thiserror::Error;
use zeroize::Zeroizing;

use crate::{payload::ChunkStoreKey, storage::StorageKey, transport::Psk};

const SECRET_BYTES: usize = 32;
const CONTENT_KEY_INFO: &[u8] = b"clip-sync/content-id-key/v1";
const TRANSPORT_KEY_INFO: &[u8] = b"clip-sync/transport-auth-key/v1";
const DISCOVERY_KEY_INFO: &[u8] = b"clip-sync/interface-discovery-auth-key/v1";
const CHUNK_STORE_KEY_INFO: &[u8] = b"clip-sync/chunk-store/root-key/v1";
const ENVELOPE_KEY_INFO: &[u8] = b"clip-sync/envelope/key-encryption-key/v1";
const STORAGE_SALT: &[u8] = b"clip-sync/storage-salt/v1";

/// High-entropy shared mesh secret loaded from an owner-only file.
pub struct MeshSecret {
    bytes: Zeroizing<[u8; SECRET_BYTES]>,
}

impl MeshSecret {
    /// Loads a 32-byte raw or 64-character hexadecimal key file.
    ///
    /// A single trailing newline is accepted for SOPS-generated text files.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe permissions, excessive size, malformed
    /// encoding, or file I/O failure.
    pub fn load(path: &Path) -> Result<Self, SecretError> {
        let mut source = read_secret_file(path)?;
        if source.last() == Some(&b'\n') {
            source.pop();
            if source.last() == Some(&b'\r') {
                source.pop();
            }
        }
        Self::parse(&source)
    }

    /// Parses the supported secret encodings into zeroizing storage.
    ///
    /// # Errors
    ///
    /// Returns [`SecretError::InvalidEncoding`] unless the value is exactly
    /// 32 raw bytes or 64 hexadecimal characters.
    pub fn parse(source: &[u8]) -> Result<Self, SecretError> {
        let bytes = if source.len() == SECRET_BYTES {
            let mut bytes = [0; SECRET_BYTES];
            bytes.copy_from_slice(source);
            Zeroizing::new(bytes)
        } else if source.len() == SECRET_BYTES * 2 {
            let mut bytes = Zeroizing::new([0; SECRET_BYTES]);
            hex::decode_to_slice(source, bytes.as_mut())
                .map_err(|_| SecretError::InvalidEncoding)?;
            bytes
        } else {
            return Err(SecretError::InvalidEncoding);
        };
        Ok(Self { bytes })
    }

    /// Derives the transport authentication key.
    ///
    /// # Errors
    ///
    /// Returns an error if HKDF expansion or fixed-size PSK construction fails.
    pub fn transport_psk(&self) -> Result<Psk, SecretError> {
        let hkdf = Hkdf::<Sha256>::new(None, self.bytes.as_ref());
        let mut key = Zeroizing::new([0; SECRET_BYTES]);
        hkdf.expand(TRANSPORT_KEY_INFO, key.as_mut())
            .map_err(|_| SecretError::Derivation)?;
        Psk::new(key.as_ref()).map_err(|_| SecretError::Derivation)
    }

    /// Derives the key used to authenticate interface discovery beacons.
    ///
    /// # Errors
    ///
    /// Returns an error if HKDF expansion unexpectedly fails.
    pub fn discovery_key(&self) -> Result<Zeroizing<[u8; 32]>, SecretError> {
        let hkdf = Hkdf::<Sha256>::new(None, self.bytes.as_ref());
        let mut key = Zeroizing::new([0; 32]);
        hkdf.expand(DISCOVERY_KEY_INFO, key.as_mut())
            .map_err(|_| SecretError::Derivation)?;
        Ok(key)
    }

    /// Derives the `SQLCipher` spike key with domain separation.
    ///
    /// # Errors
    ///
    /// Returns an error if key derivation unexpectedly fails.
    pub fn storage_key(&self) -> Result<StorageKey, SecretError> {
        StorageKey::derive_from_secret(self.bytes.as_ref(), STORAGE_SALT)
            .map_err(|_| SecretError::Derivation)
    }

    /// Derives the keyed content-identity key.
    ///
    /// # Errors
    ///
    /// Returns an error if HKDF expansion unexpectedly fails.
    pub fn content_key(&self) -> Result<Zeroizing<[u8; 32]>, SecretError> {
        let hkdf = Hkdf::<Sha256>::new(None, self.bytes.as_ref());
        let mut key = Zeroizing::new([0; 32]);
        hkdf.expand(CONTENT_KEY_INFO, key.as_mut())
            .map_err(|_| SecretError::Derivation)?;
        Ok(key)
    }

    /// Derives the root key used by the encrypted chunk store.
    ///
    /// The store further separates this root into chunk encryption,
    /// identifier, and catalog keys.
    ///
    /// # Errors
    ///
    /// Returns an error if HKDF expansion unexpectedly fails.
    pub fn chunk_store_key(&self) -> Result<ChunkStoreKey, SecretError> {
        let hkdf = Hkdf::<Sha256>::new(None, self.bytes.as_ref());
        let mut key = Zeroizing::new([0; 32]);
        hkdf.expand(CHUNK_STORE_KEY_INFO, key.as_mut())
            .map_err(|_| SecretError::Derivation)?;
        Ok(ChunkStoreKey::from_bytes(*key))
    }

    pub(crate) fn envelope_key(&self) -> Result<Zeroizing<[u8; 32]>, SecretError> {
        let hkdf = Hkdf::<Sha256>::new(None, self.bytes.as_ref());
        let mut key = Zeroizing::new([0; 32]);
        hkdf.expand(ENVELOPE_KEY_INFO, key.as_mut())
            .map_err(|_| SecretError::Derivation)?;
        Ok(key)
    }
}

impl fmt::Debug for MeshSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("MeshSecret([REDACTED])")
    }
}

#[cfg(unix)]
fn read_secret_file(path: &Path) -> Result<Zeroizing<Vec<u8>>, SecretError> {
    use rustix::fs::{FileType, Mode, OFlags, fstat, open};

    // sops-nix exposes generation-owned secret files through a stable symlink.
    // Resolve that link first, then refuse a second final-component symlink and
    // validate the opened descriptor's owner, mode, type, and size.
    let resolved = std::fs::canonicalize(path)?;
    let fd = open(
        &resolved,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)?;
    let stat = fstat(&fd).map_err(std::io::Error::from)?;
    if !FileType::from_raw_mode(stat.st_mode).is_file() {
        return Err(SecretError::NotRegularFile);
    }
    let current_uid = rustix::process::getuid().as_raw();
    if stat.st_uid != current_uid {
        return Err(SecretError::UnsafeOwner {
            actual: stat.st_uid,
            expected: current_uid,
        });
    }
    let mode = stat.st_mode & 0o777;
    if mode & 0o077 != 0 {
        return Err(SecretError::UnsafePermissions(mode));
    }
    if stat.st_size > 256 {
        return Err(SecretError::TooLarge);
    }

    let mut source = Zeroizing::new(Vec::new());
    std::fs::File::from(fd).take(257).read_to_end(&mut source)?;
    if source.len() > 256 {
        return Err(SecretError::TooLarge);
    }
    Ok(source)
}

#[cfg(not(unix))]
fn read_secret_file(path: &Path) -> Result<Zeroizing<Vec<u8>>, SecretError> {
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(SecretError::NotRegularFile);
    }
    if metadata.len() > 256 {
        return Err(SecretError::TooLarge);
    }
    Ok(Zeroizing::new(std::fs::read(path)?))
}

#[derive(Debug, Error)]
pub enum SecretError {
    #[error("mesh secret path is not a regular file")]
    NotRegularFile,
    #[error("mesh secret file is unexpectedly large")]
    TooLarge,
    #[error("mesh secret owner {actual} does not match current user {expected}")]
    UnsafeOwner { actual: u32, expected: u32 },
    #[error("mesh secret must contain exactly 32 raw bytes or 64 hexadecimal characters")]
    InvalidEncoding,
    #[error("mesh secret permissions {0:o} expose it to group or other users")]
    UnsafePermissions(u32),
    #[error("mesh-secret key derivation failed")]
    Derivation,
    #[error("mesh-secret file I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    #[test]
    fn accepts_raw_and_hex_encodings() {
        let raw = [7; 32];
        let raw_secret = MeshSecret::parse(&raw).expect("raw secret");
        let hex_secret = MeshSecret::parse(hex::encode(raw).as_bytes()).expect("hex secret");

        assert_eq!(
            raw_secret.content_key().unwrap(),
            hex_secret.content_key().unwrap()
        );
        assert_eq!(format!("{raw_secret:?}"), "MeshSecret([REDACTED])");
    }

    #[test]
    fn load_accepts_one_trailing_newline() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("mesh.key");
        let mut file = fs::File::create(&path).expect("create key");
        file.write_all(b"0123456789abcdef0123456789abcdef\n")
            .expect("write key");
        drop(file);
        set_private_permissions(&path);

        MeshSecret::load(&path).expect("load secret");
    }

    #[cfg(unix)]
    fn set_private_permissions(path: &Path) {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("set permissions");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_group_readable_file() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("mesh.key");
        fs::write(&path, [3; 32]).expect("write key");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).expect("set permissions");

        assert!(matches!(
            MeshSecret::load(&path),
            Err(SecretError::UnsafePermissions(0o640))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn accepts_private_owned_secret_through_stable_symlink() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("temporary directory");
        let target = directory.path().join("target.key");
        let link = directory.path().join("mesh.key");
        fs::write(&target, [3; 32]).expect("write key");
        set_private_permissions(&target);
        symlink(&target, &link).expect("create symlink");

        MeshSecret::load(&link).expect("load sops-style secret symlink");
    }
}
