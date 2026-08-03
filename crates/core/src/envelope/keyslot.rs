use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::Path,
};

use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{crypto::MeshSecret, payload::ChunkStoreKey, storage::StorageKey};

use super::{
    EnvelopeError, Result,
    lock::{open_private_keyslot, sync_directory},
    state::StateKeys,
};

pub(super) const PENDING_FILENAME: &str = "history.keyslot.next";
pub(super) const DATA_KEY_BYTES: usize = 32;
const KEYSLOT_MAGIC: &[u8; 8] = b"CSKEYS01";
const KEYSLOT_VERSION: u8 = 1;
const NONCE_BYTES: usize = 24;
const PLAINTEXT_BYTES: usize = 1 + DATA_KEY_BYTES * 3;
const TAG_BYTES: usize = 16;
const KEYSLOT_BYTES: usize = KEYSLOT_MAGIC.len() + 1 + NONCE_BYTES + PLAINTEXT_BYTES + TAG_BYTES;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SlotState {
    Final = 0,
    DatabaseMigration = 1,
}

#[derive(Clone)]
pub(super) struct Slot {
    pub(super) state: SlotState,
    pub(super) keys: StateKeys,
}

pub(super) fn read_slot(path: &Path, secret: &MeshSecret) -> Result<Slot> {
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

pub(super) fn atomic_replace_slot(path: &Path, secret: &MeshSecret, slot: &Slot) -> Result<()> {
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

pub(super) fn write_slot_new(path: &Path, secret: &MeshSecret, slot: &Slot) -> Result<()> {
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

pub(super) fn commit_pending(pending: &Path, committed: &Path) -> Result<()> {
    let parent = committed
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "keyslot path has no parent"))?;
    fs::rename(pending, committed)?;
    sync_directory(parent)?;
    Ok(())
}
