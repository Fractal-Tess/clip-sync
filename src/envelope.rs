//! Authenticated wrapping, migration, and rotation of local state keys.

mod keyslot;
mod lock;
mod rekey;
mod state;

use std::io;

use thiserror::Error;

use crate::{crypto::SecretError, payload::ChunkStoreError, storage::StorageError};

pub use lock::StoreLock;
pub use rekey::{RekeyOutcome, rekey_state};
pub use state::StateKeys;

pub const KEYSLOT_FILENAME: &str = "history.keyslot";
pub const STORE_LOCK_FILENAME: &str = "store.lock";

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

#[cfg(test)]
use std::{
    fs,
    path::{Path, PathBuf},
};
#[cfg(test)]
use subtle::ConstantTimeEq;

#[cfg(test)]
use crate::{crypto::MeshSecret, payload::ChunkStore, storage::EncryptedStorage};
#[cfg(test)]
use keyslot::{PENDING_FILENAME, read_slot};
#[cfg(test)]
use rekey::{RekeyPhase, rekey_locked};
#[cfg(test)]
use state::{CHUNKS_DIRECTORY, DATABASE_FILENAME, verify_state};

#[cfg(test)]
mod tests;
