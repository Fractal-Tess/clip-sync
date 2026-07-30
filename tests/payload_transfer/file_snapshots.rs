use super::support::*;

#[test]
fn file_snapshot_rejects_symlinks_and_materializes_safe_metadata() {
    let directory = tempfile::tempdir().expect("tempdir");
    let source = directory.path().join("source");
    fs::create_dir(&source).expect("source dir");
    fs::create_dir(source.join("nested")).expect("nested dir");
    fs::write(source.join("nested/data.txt"), b"exact file bytes").expect("write data");
    fs::write(source.join("run.sh"), b"#!/bin/sh\nexit 0\n").expect("write script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(source.join("run.sh"), fs::Permissions::from_mode(0o755))
            .expect("make executable");
    }

    let mut store = store(&directory);
    let cancellation = CancellationToken::new();
    let snapshot = snapshot_file_uris(
        std::slice::from_ref(&source),
        &mut store,
        FileSnapshotLimits {
            max_logical_bytes: 1024 * 1024,
            ..FileSnapshotLimits::default()
        },
        &cancellation,
    )
    .expect("snapshot");
    let manifest_id = store
        .commit_manifest(&StoredManifest::Files(snapshot))
        .expect("commit snapshot");

    let runtime = directory.path().join("runtime/materialized");
    let materializer =
        Materializer::new(&runtime, MaterializerConfig::default()).expect("materializer");
    let activation = materializer
        .materialize(&store, manifest_id, &cancellation)
        .expect("materialize");
    assert_eq!(
        fs::read(activation.directory().join("source/nested/data.txt")).expect("read restored"),
        b"exact file bytes"
    );
    assert!(
        std::str::from_utf8(activation.uri_list())
            .expect("URI UTF-8")
            .contains("source")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let regular = fs::metadata(activation.directory().join("source/nested/data.txt"))
            .expect("regular metadata")
            .permissions()
            .mode()
            & 0o777;
        let executable = fs::metadata(activation.directory().join("source/run.sh"))
            .expect("script metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(regular, 0o600);
        assert_eq!(executable, 0o700);
    }
    assert!(materializer.cleanup(manifest_id).expect("cleanup"));
    assert!(!activation.directory().exists());

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("/etc/passwd", source.join("escape")).expect("create symlink");
        assert!(matches!(
            snapshot_file_uris(
                &[source],
                &mut store,
                FileSnapshotLimits::default(),
                &cancellation
            ),
            Err(FileSnapshotError::Symlink(_))
        ));
    }
}

#[test]
fn uri_parsing_is_bounded_and_local_only() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("name with spaces.txt");
    fs::write(&path, b"x").expect("write");
    let uri = Url::from_file_path(&path).expect("file URL");
    let body = format!("# comment\r\ncopy\r\n{uri}\r\n");
    let parsed =
        parse_file_uri_list(body.as_bytes(), FileSnapshotLimits::default()).expect("parse");
    assert_eq!(parsed, vec![path]);

    assert!(matches!(
        parse_file_uri_list(b"file://other-host/tmp/a\n", FileSnapshotLimits::default()),
        Err(FileSnapshotError::RemoteFileUri)
    ));
    assert!(
        parse_file_uri_list(
            b"https://example.test/file\n",
            FileSnapshotLimits::default()
        )
        .is_err()
    );
}

#[test]
fn materialization_reports_free_space_failure_before_writing() {
    let directory = tempfile::tempdir().expect("tempdir");
    let source = directory.path().join("large.bin");
    fs::write(&source, vec![1_u8; 4096]).expect("write");
    let mut store = store(&directory);
    let cancellation = CancellationToken::new();
    let snapshot = snapshot_file_uris(
        &[source],
        &mut store,
        FileSnapshotLimits::default(),
        &cancellation,
    )
    .expect("snapshot");
    let manifest_id = store
        .commit_manifest(&StoredManifest::Files(snapshot))
        .expect("manifest");
    let runtime = directory.path().join("runtime");
    fs::create_dir(&runtime).expect("runtime");
    let available = fs2::available_space(&runtime).expect("free space");
    let materializer = Materializer::new(
        &runtime,
        MaterializerConfig {
            free_space_reserve_bytes: available.saturating_add(1024 * 1024 * 1024),
        },
    )
    .expect("materializer");
    assert!(matches!(
        materializer.materialize(&store, manifest_id, &cancellation),
        Err(MaterializationError::InsufficientSpace { .. })
    ));
}

#[cfg(unix)]
#[test]
fn startup_cleanup_reclaims_crashed_materializations_without_following_symlinks() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().expect("tempdir");
    let runtime = directory.path().join("runtime");
    let materializer =
        Materializer::new(&runtime, MaterializerConfig::default()).expect("materializer");
    let abandoned = runtime.join("abandoned-manifest");
    fs::create_dir(&abandoned).expect("abandoned directory");
    fs::write(abandoned.join("payload"), b"runtime plaintext").expect("abandoned payload");
    let outside = directory.path().join("must-survive");
    fs::write(&outside, b"outside").expect("outside target");
    symlink(&outside, runtime.join("abandoned-link")).expect("abandoned symlink");
    fs::write(runtime.join(".partial.staging"), b"partial").expect("abandoned staging");

    assert_eq!(
        materializer.cleanup_abandoned().expect("startup cleanup"),
        3
    );
    assert_eq!(
        fs::read(&outside).expect("outside target survives"),
        b"outside"
    );
    assert_eq!(fs::read_dir(&runtime).expect("runtime root").count(), 0);
}

#[tokio::test]
async fn grace_cleanup_can_be_cancelled_by_reactivation() {
    let directory = tempfile::tempdir().expect("tempdir");
    let materializer = Materializer::new(
        directory.path().join("runtime"),
        MaterializerConfig::default(),
    )
    .expect("materializer");
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    // An arbitrary absent ID is enough to verify timer cancellation semantics.
    let mut store = store(&directory);
    let blob = store
        .stage_reader(&mut Cursor::new(b"x"), 1, &CancellationToken::new())
        .expect("blob");
    let id = store
        .commit_manifest(&StoredManifest::Blob(blob))
        .expect("id");
    assert!(
        !materializer
            .cleanup_after_grace(id, Duration::from_secs(30), cancellation)
            .await
            .expect("cancel cleanup")
    );
}
