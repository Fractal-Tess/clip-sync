use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use clip_sync::storage::{EncryptedStorage, StorageError, StorageKey};

#[test]
fn sqlcipher_storage_encrypts_plaintext_and_reopens_with_key() {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("storage.db");
    let key = StorageKey::derive_from_secret(
        b"storage encryption test correct secret",
        b"storage encryption test salt",
    )
    .unwrap();
    let wrong_key = StorageKey::derive_from_secret(
        b"storage encryption test wrong secret",
        b"storage encryption test salt",
    )
    .unwrap();
    let plaintext = unique_plaintext();

    {
        let mut storage = EncryptedStorage::open(&db_path, &key).unwrap();
        assert!(!storage.cipher_version().unwrap().trim().is_empty());
        storage
            .set_meta_value("encryption_plaintext_probe", &plaintext)
            .unwrap();
        assert_eq!(
            storage.meta_value("encryption_plaintext_probe").unwrap(),
            Some(plaintext.clone())
        );
        storage.checkpoint().unwrap();
        storage.close().unwrap();
    }

    assert_plaintext_absent(&db_path, plaintext.as_bytes());
    assert_private_permissions(&db_path);

    {
        let storage = EncryptedStorage::open(&db_path, &key).unwrap();
        assert_eq!(
            storage.meta_value("encryption_plaintext_probe").unwrap(),
            Some(plaintext)
        );
        storage.close().unwrap();
    }

    let before_wrong_key_attempt = read_existing_storage_files(&db_path);
    let Err(error) = EncryptedStorage::open(&db_path, &wrong_key) else {
        panic!("wrong key unexpectedly opened encrypted storage");
    };
    assert!(matches!(error, StorageError::InvalidKey));
    assert_eq!(
        read_existing_storage_files(&db_path),
        before_wrong_key_attempt,
        "wrong-key open attempt must not modify database files"
    );
}

#[cfg(unix)]
#[test]
fn encrypted_storage_rejects_database_symlinks() {
    use std::os::unix::fs::symlink;

    let temp_dir = tempfile::tempdir().unwrap();
    let target = temp_dir.path().join("target.db");
    let link = temp_dir.path().join("history.db");
    fs::write(&target, b"not a database").unwrap();
    symlink(&target, &link).unwrap();
    let key = StorageKey::from_bytes([0x5a; 32]);

    assert!(matches!(
        EncryptedStorage::open(&link, &key),
        Err(StorageError::UnsafeDatabaseFile)
    ));
    assert_eq!(fs::read(&target).unwrap(), b"not a database");
}

fn unique_plaintext() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("clip-sync-encryption-probe-{}-{nanos}", std::process::id())
}

fn assert_plaintext_absent(db_path: &Path, plaintext: &[u8]) {
    for path in storage_paths(db_path) {
        if !path.exists() {
            continue;
        }
        let bytes = fs::read(&path).unwrap();
        assert!(
            !contains_subslice(&bytes, plaintext),
            "plaintext was found in {}",
            path.display()
        );
    }
}

fn read_existing_storage_files(db_path: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    storage_paths(db_path)
        .into_iter()
        .filter(|path| path.exists())
        .map(|path| {
            let bytes = fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect()
}

fn storage_paths(db_path: &Path) -> Vec<PathBuf> {
    let db_path = db_path.as_os_str().to_string_lossy();
    ["", "-wal", "-shm"]
        .into_iter()
        .map(|suffix| PathBuf::from(format!("{db_path}{suffix}")))
        .collect()
}

#[cfg(unix)]
fn assert_private_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}
