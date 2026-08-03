#![cfg(unix)]

use std::{
    fs,
    path::{Path, PathBuf},
};

use clip_sync_core::{
    crypto::MeshSecret,
    envelope::{KEYSLOT_FILENAME, RekeyOutcome, StateKeys, StoreLock, rekey_state},
    storage::EncryptedStorage,
};

#[test]
#[allow(clippy::too_many_lines)]
fn rekey_migrates_rotates_reopens_and_fails_closed() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().expect("temporary directory");
    let state_dir = directory.path().join("state").join("clip-sync");
    fs::create_dir_all(&state_dir).expect("create state");

    let old_bytes = [0x31; 32];
    let new_bytes = [0x52; 32];
    let wrong_old_bytes = [0x73; 32];
    let wrong_new_bytes = [0x94; 32];
    let old = MeshSecret::parse(&old_bytes).expect("old mesh secret");
    let new = MeshSecret::parse(&new_bytes).expect("new mesh secret");
    let probe = "cli-direct-database-rekey-plaintext-probe";

    let database_path = state_dir.join("history.db");
    let mut direct = EncryptedStorage::open(
        &database_path,
        &old.storage_key().expect("direct-derived key"),
    )
    .expect("create direct-derived database");
    direct
        .set_meta_value("cli_rekey_probe", probe)
        .expect("write probe");
    direct.checkpoint().expect("checkpoint");
    direct.close().expect("close direct database");

    assert_eq!(
        rekey_state(&state_dir, &old, &new).expect("rotate and verify state"),
        RekeyOutcome::Rotated
    );

    let keyslot = state_dir.join(KEYSLOT_FILENAME);
    assert_eq!(
        fs::metadata(&keyslot)
            .expect("keyslot metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let lock = StoreLock::acquire(&state_dir).expect("acquire store after CLI");
    let keys = StateKeys::open_or_create(&lock, &new).expect("open rotated keyslot");
    assert_eq!(
        keys.content_identity_key(),
        old.content_key()
            .expect("old content identity key")
            .as_ref(),
        "rotation must preserve keyed content identities"
    );
    assert_eq!(
        EncryptedStorage::open(&database_path, keys.storage_key())
            .expect("reopen rotated database")
            .meta_value("cli_rekey_probe")
            .expect("read probe"),
        Some(probe.to_owned())
    );
    drop(lock);

    assert_eq!(
        rekey_state(&state_dir, &old, &new).expect("idempotent rekey"),
        RekeyOutcome::AlreadyCurrent
    );

    let before_wrong_key = snapshot_tree(&state_dir);
    let wrong_old = MeshSecret::parse(&wrong_old_bytes).expect("wrong old mesh secret");
    let wrong_new = MeshSecret::parse(&wrong_new_bytes).expect("wrong new mesh secret");
    let wrong = rekey_state(&state_dir, &wrong_old, &wrong_new)
        .expect_err("wrong secrets must fail closed");
    assert!(
        wrong
            .to_string()
            .contains("supplied mesh secret cannot authenticate the keyslot")
    );
    assert_eq!(snapshot_tree(&state_dir), before_wrong_key);

    let daemon_lock = StoreLock::acquire(&state_dir).expect("simulate daemon ownership");
    let busy = rekey_state(&state_dir, &old, &new).expect_err("active daemon lock must win");
    assert!(busy.to_string().contains("exclusive state lock"));
    drop(daemon_lock);

    for (path, bytes) in snapshot_tree(&state_dir) {
        for plaintext in [
            old_bytes.as_slice(),
            new_bytes.as_slice(),
            wrong_old_bytes.as_slice(),
            wrong_new_bytes.as_slice(),
            probe.as_bytes(),
        ] {
            assert!(
                !contains_subslice(&bytes, plaintext),
                "plaintext found in {}",
                path.display()
            );
        }
    }
}

fn snapshot_tree(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files = Vec::new();
    collect_files(root, &mut files);
    files.sort_by(|left, right| left.0.cmp(&right.0));
    files
}

fn collect_files(root: &Path, files: &mut Vec<(PathBuf, Vec<u8>)>) {
    for entry in fs::read_dir(root).expect("read state directory") {
        let path = entry.expect("state entry").path();
        if path.is_dir() {
            collect_files(&path, files);
        } else if path.is_file() {
            files.push((path.clone(), fs::read(path).expect("read state file")));
        }
    }
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}
