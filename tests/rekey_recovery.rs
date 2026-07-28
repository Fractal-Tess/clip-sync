#![cfg(unix)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use clip_sync::{
    crypto::MeshSecret,
    envelope::{KEYSLOT_FILENAME, StateKeys, StoreLock},
    storage::EncryptedStorage,
};

#[test]
#[allow(clippy::too_many_lines)]
fn cli_migrates_rotates_reopens_and_fails_closed() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().expect("temporary directory");
    let home = directory.path().join("home");
    let state_home = directory.path().join("state");
    let runtime = directory.path().join("runtime");
    let state_dir = state_home.join("clip-sync");
    fs::create_dir_all(&home).expect("create home");
    fs::create_dir_all(&runtime).expect("create runtime");
    fs::create_dir_all(&state_dir).expect("create state");

    let old_bytes = [0x31; 32];
    let new_bytes = [0x52; 32];
    let wrong_old_bytes = [0x73; 32];
    let wrong_new_bytes = [0x94; 32];
    let old_path = private_key_file(directory.path(), "old.key", &old_bytes);
    let new_path = private_key_file(directory.path(), "new.key", &new_bytes);
    let wrong_old_path = private_key_file(directory.path(), "wrong-old.key", &wrong_old_bytes);
    let wrong_new_path = private_key_file(directory.path(), "wrong-new.key", &wrong_new_bytes);
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

    let output = rekey_command(
        directory.path(),
        &home,
        &state_home,
        &runtime,
        &old_path,
        &new_path,
    );
    assert!(
        output.status.success(),
        "rekey failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("rotated and verified"),
        "unexpected output: {}",
        String::from_utf8_lossy(&output.stdout)
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

    let already_current = rekey_command(
        directory.path(),
        &home,
        &state_home,
        &runtime,
        &old_path,
        &new_path,
    );
    assert!(already_current.status.success());
    assert!(
        String::from_utf8_lossy(&already_current.stdout).contains("already uses the new secret")
    );

    let before_wrong_key = snapshot_tree(&state_dir);
    let wrong = rekey_command(
        directory.path(),
        &home,
        &state_home,
        &runtime,
        &wrong_old_path,
        &wrong_new_path,
    );
    assert!(!wrong.status.success());
    assert!(
        String::from_utf8_lossy(&wrong.stderr)
            .contains("supplied mesh secret cannot authenticate the keyslot")
    );
    assert_eq!(snapshot_tree(&state_dir), before_wrong_key);

    let daemon_lock = StoreLock::acquire(&state_dir).expect("simulate daemon ownership");
    let busy = rekey_command(
        directory.path(),
        &home,
        &state_home,
        &runtime,
        &old_path,
        &new_path,
    );
    assert!(!busy.status.success());
    assert!(String::from_utf8_lossy(&busy.stderr).contains("exclusive state lock"));
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

fn rekey_command(
    working_directory: &Path,
    home: &Path,
    state_home: &Path,
    runtime: &Path,
    old_key_file: &Path,
    new_key_file: &Path,
) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_clip-sync"))
        .current_dir(working_directory)
        .env("HOME", home)
        .env("XDG_STATE_HOME", state_home)
        .env("XDG_RUNTIME_DIR", runtime)
        .args([
            "rekey",
            "--old-key-file",
            old_key_file.to_str().expect("UTF-8 old key path"),
            "--new-key-file",
            new_key_file.to_str().expect("UTF-8 new key path"),
        ])
        .output()
        .expect("run clip-sync rekey")
}

fn private_key_file(directory: &Path, name: &str, bytes: &[u8; 32]) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let path = directory.join(name);
    fs::write(&path, bytes).expect("write mesh key");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .expect("set mesh key permissions");
    path
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
