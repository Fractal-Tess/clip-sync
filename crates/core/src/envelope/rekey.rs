use std::path::Path;

use subtle::ConstantTimeEq;

use crate::crypto::MeshSecret;

use super::{
    EnvelopeError, KEYSLOT_FILENAME, Result, StoreLock,
    keyslot::{PENDING_FILENAME, Slot, SlotState, commit_pending, read_slot, write_slot_new},
    lock::{path_exists, sync_directory},
    state::{StateKeys, recover_database_migration, verify_state},
};

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
pub(super) enum RekeyPhase {
    PendingDurable,
    SidecarCommitted,
}

pub(super) fn rekey_locked(
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
