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

    let outcome = rekey_locked(&lock, &old, &new, |_| Ok(())).expect("resume interrupted rekey");
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
        StoredManifest::Files(_) | StoredManifest::MimeBundle(_) => {
            panic!("expected blob manifest")
        }
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

#[test]
fn wrong_secret_is_read_only_and_authenticated_open_reclaims_crash_temp() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let correct = secret(21);
    let wrong = secret(22);
    let lock = StoreLock::acquire(directory.path()).expect("store lock");
    StateKeys::open_or_create(&lock, &correct).expect("initialize");
    let orphan = directory.path().join(".history.keyslot.crashed.tmp");
    fs::write(&orphan, b"abandoned atomic replacement").expect("write orphan");
    let before = snapshot_files(directory.path());

    assert!(matches!(
        StateKeys::open_or_create(&lock, &wrong),
        Err(EnvelopeError::WrongSecret)
    ));
    assert_eq!(snapshot_files(directory.path()), before);
    assert!(orphan.exists());

    StateKeys::open_or_create(&lock, &correct).expect("authenticated reopen");
    assert!(!orphan.exists());
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
