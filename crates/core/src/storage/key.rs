use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::Zeroizing;

use super::{Result, StorageError};

pub(super) const SQLCIPHER_KEY_BYTES: usize = 32;
pub(super) const SQLCIPHER_KEY_HEX_CHARS: usize = SQLCIPHER_KEY_BYTES * 2;

#[derive(Clone)]
pub struct StorageKey {
    bytes: Zeroizing<[u8; SQLCIPHER_KEY_BYTES]>,
}

impl StorageKey {
    #[must_use]
    pub fn from_bytes(bytes: [u8; SQLCIPHER_KEY_BYTES]) -> Self {
        Self {
            bytes: Zeroizing::new(bytes),
        }
    }

    /// Copies exactly 32 key bytes into zeroizing storage.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::InvalidKeyLength`] for any other length.
    pub fn try_from_slice(bytes: &[u8]) -> Result<Self> {
        let bytes: [u8; SQLCIPHER_KEY_BYTES] = bytes
            .try_into()
            .map_err(|_| StorageError::InvalidKeyLength)?;
        Ok(Self::from_bytes(bytes))
    }

    /// Derives a domain-separated `SQLCipher` key from a high-entropy secret.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::KeyDerivation`] if HKDF expansion fails.
    pub fn derive_from_secret(secret: &[u8], salt: &[u8]) -> Result<Self> {
        let hkdf = Hkdf::<Sha256>::new(Some(salt), secret);
        let mut bytes = Zeroizing::new([0_u8; SQLCIPHER_KEY_BYTES]);
        hkdf.expand(b"clip-sync/storage/sqlcipher-key/v1", bytes.as_mut())
            .map_err(|_| StorageError::KeyDerivation)?;
        Ok(Self { bytes })
    }

    pub(crate) fn as_bytes(&self) -> &[u8; SQLCIPHER_KEY_BYTES] {
        &self.bytes
    }
}

impl TryFrom<&[u8]> for StorageKey {
    type Error = StorageError;

    fn try_from(value: &[u8]) -> Result<Self> {
        Self::try_from_slice(value)
    }
}
