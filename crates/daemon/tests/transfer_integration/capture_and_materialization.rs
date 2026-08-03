use super::support::*;

#[tokio::test]
async fn automatic_file_and_directory_capture_is_private_durable_and_remotely_materialized() {
    let mut origin = Node::new();
    let mut remote = Node::new();
    let source_file = origin.directory.path().join("single.txt");
    let source_directory = origin.directory.path().join("folder");
    let source_file_path = source_file.to_string_lossy().into_owned();
    let source_directory_path = source_directory.to_string_lossy().into_owned();
    fs::create_dir(&source_directory).expect("source directory");
    fs::create_dir(source_directory.join("nested")).expect("nested directory");
    fs::write(&source_file, b"automatic file bytes").expect("source file");
    fs::write(
        source_directory.join("nested/data.bin"),
        b"automatic directory bytes",
    )
    .expect("nested source file");
    let clipboard = uri_clipboard(&[&source_file, &source_directory]);
    let origin_path = origin.directory.path().to_string_lossy().into_owned();
    let (runtime, mesh, shutdown) = spawn_mesh(origin.history.replica().node_id());

    let result = capture_automatic_clipboard(
        &clipboard,
        &CONTENT_KEY,
        &mut origin.transfers,
        &mut origin.history,
        &mesh,
        100,
        &CancellationToken::new(),
    )
    .await
    .expect("automatic file capture");
    let (transfer_id, content_id) = match result {
        AutomaticClipboardCaptureResult::Files {
            transfer_id,
            content_id,
        } => (transfer_id, content_id),
        other => panic!("expected file capture, got {other:?}"),
    };
    assert_eq!(
        origin
            .history
            .projection()
            .transfer(transfer_id)
            .expect("transfer")
            .phase(),
        TransferPhase::Complete
    );
    assert!(
        origin.history.projection().payload(content_id).is_none(),
        "origin URI bytes must not be retained as inline history"
    );
    let operations = origin
        .history
        .storage()
        .load_operations()
        .expect("captured operations");
    for operation in &operations {
        let encoded = serde_json::to_string(operation).expect("encode operation");
        assert!(
            !encoded.contains(&origin_path),
            "replicated operation leaked the origin path: {encoded}"
        );
    }

    fs::remove_file(&source_file).expect("delete source file after capture");
    fs::remove_dir_all(&source_directory).expect("delete source directory after capture");
    let (local_manifest, local_root, local_uri) = activate_file_snapshot(&origin, content_id);
    assert!(!local_uri.contains(&source_file_path));
    assert!(!local_uri.contains(&source_directory_path));
    assert_automatic_snapshot_bytes(&local_root);
    cleanup_file_snapshot(&origin, local_manifest, &local_root);

    remote.ingest(&operations);
    assert!(move_chunks(&origin, &mut remote, 64) > 0);
    let (remote_manifest, remote_root, remote_uri) = activate_file_snapshot(&remote, content_id);
    assert_automatic_snapshot_bytes(&remote_root);
    assert!(remote_uri.contains(remote_root.to_str().expect("UTF-8 runtime root")));
    assert!(!remote_uri.contains(&origin_path));
    cleanup_file_snapshot(&remote, remote_manifest, &remote_root);

    shutdown.cancel();
    runtime.wait().await;
}

#[cfg(unix)]
#[tokio::test]
async fn automatic_file_capture_rejects_unsafe_and_oversize_offers_without_history() {
    use std::os::unix::{fs::symlink, net::UnixListener};

    let directory = tempfile::tempdir().expect("node directory");
    let mut history = HistoryStore::open(
        directory.path().join("history.db"),
        &StorageKey::from_bytes(STORAGE_KEY),
    )
    .expect("history");
    let mut transfers = open_transfers_with_threshold(&directory, 32);
    let (runtime, mesh, shutdown) = spawn_mesh(history.replica().node_id());
    let outside = directory.path().join("outside.txt");
    fs::write(&outside, b"outside").expect("outside file");
    let symlink_root = directory.path().join("symlink.txt");
    symlink(&outside, &symlink_root).expect("root symlink");
    let escaping_directory = directory.path().join("escaping");
    fs::create_dir(&escaping_directory).expect("escaping directory");
    symlink(&outside, escaping_directory.join("escape")).expect("child symlink");
    let socket_path = directory.path().join("special.sock");
    let _listener = UnixListener::bind(&socket_path).expect("Unix socket");
    let oversized = directory.path().join("oversized.bin");
    fs::write(&oversized, vec![0x5a; 33]).expect("oversized source");

    for (index, path) in [
        symlink_root.as_path(),
        escaping_directory.as_path(),
        socket_path.as_path(),
        oversized.as_path(),
    ]
    .into_iter()
    .enumerate()
    {
        let outcome = capture_automatic_clipboard(
            &uri_clipboard(&[path]),
            &CONTENT_KEY,
            &mut transfers,
            &mut history,
            &mesh,
            u64::try_from(index + 1).expect("timestamp"),
            &CancellationToken::new(),
        )
        .await
        .expect("safe rejection");
        assert_eq!(
            outcome,
            AutomaticClipboardCaptureResult::RejectedFiles,
            "unsafe path was accepted: {}",
            path.display()
        );
        assert!(history.projection().visible_items().is_empty());
        assert!(transfers.progress().is_empty());
    }
    assert!(
        history
            .storage()
            .load_operations()
            .expect("operation log")
            .is_empty()
    );

    shutdown.cancel();
    runtime.wait().await;
}

#[tokio::test]
async fn automatic_non_file_capture_preserves_all_mime_representations() {
    let mut node = Node::new();
    let content = ClipboardContent::new_with_limit(
        vec![
            ClipboardRepresentation::new(
                MimeType::new("text/plain;charset=utf-8").expect("plain MIME"),
                b"plain".to_vec(),
            ),
            ClipboardRepresentation::new(
                MimeType::new("text/html").expect("HTML MIME"),
                b"<b>plain</b>".to_vec(),
            ),
            ClipboardRepresentation::new(
                MimeType::new("application/x-clip-sync-test").expect("custom MIME"),
                vec![0, 1, 2, 3],
            ),
        ],
        1024,
    )
    .expect("multi-MIME clipboard");
    let (runtime, mesh, shutdown) = spawn_mesh(node.history.replica().node_id());

    let outcome = capture_automatic_clipboard(
        &content,
        &CONTENT_KEY,
        &mut node.transfers,
        &mut node.history,
        &mesh,
        300,
        &CancellationToken::new(),
    )
    .await
    .expect("automatic non-file capture");
    let content_id = match outcome {
        AutomaticClipboardCaptureResult::Payload { content_id } => content_id,
        other => panic!("expected payload capture, got {other:?}"),
    };
    let retained = node
        .history
        .projection()
        .payload(content_id)
        .expect("retained payload");
    assert_eq!(retained.representations().len(), 3);
    for representation in content.representations() {
        assert_eq!(
            retained
                .representations()
                .iter()
                .find(|retained| retained.mime() == representation.mime_type().as_str())
                .expect("retained MIME")
                .bytes(),
            representation.bytes()
        );
    }
    assert!(node.transfers.progress().is_empty());

    shutdown.cancel();
    runtime.wait().await;
}

#[test]
fn remote_file_snapshot_materializes_beneath_private_runtime_root() {
    let mut origin = Node::new();
    let mut remote = Node::new();
    let source_root = origin.directory.path().join("source");
    fs::create_dir(&source_root).expect("source root");
    fs::write(source_root.join("data.bin"), b"remote exact file bytes").expect("source file");
    let snapshot = snapshot_file_uris(
        std::slice::from_ref(&source_root),
        origin.transfers.store_mut(),
        FileSnapshotLimits::default(),
        &CancellationToken::new(),
    )
    .expect("snapshot");
    let manifest = StoredManifest::Files(snapshot);
    let manifest_id = origin
        .transfers
        .store_mut()
        .commit_manifest(&manifest)
        .expect("manifest");
    let original_uri = Url::from_file_path(&source_root)
        .expect("file URL")
        .to_string();
    let content_id = Payload::new(
        &CONTENT_KEY,
        vec![Representation::new("text/uri-list", original_uri)],
    )
    .expect("URI payload")
    .descriptor()
    .content_id();
    let (transfer_id, begin) = origin
        .transfers
        .begin_committed_manifest_share(
            content_id,
            manifest_id,
            &manifest,
            false,
            &mut origin.history,
            200,
        )
        .expect("begin files");
    let complete = origin
        .transfers
        .complete_payload_share(transfer_id, &mut origin.history, 201)
        .expect("complete files");
    remote.ingest(&[begin, complete]);
    move_chunks(&origin, &mut remote, 64);

    let activated = remote
        .transfers
        .activate(
            content_id,
            remote.history.projection(),
            &CONTENT_KEY,
            16 * 1024 * 1024,
            &CancellationToken::new(),
        )
        .expect("remote file activation");
    let materialized = activated
        .materialized_manifest()
        .expect("materialized manifest");
    let runtime_root = remote.directory.path().join("runtime/materialized");
    let restored = runtime_root
        .join(materialized.to_string())
        .join("source/data.bin");
    assert_eq!(
        fs::read(restored).expect("restored bytes"),
        b"remote exact file bytes"
    );
    let uri_bytes = activated
        .content()
        .bytes_for_mime("text/uri-list")
        .expect("URI representation");
    assert!(
        String::from_utf8_lossy(&uri_bytes)
            .contains(runtime_root.to_str().expect("runtime path UTF-8"))
    );
    remote
        .transfers
        .cleanup_materialization(materialized)
        .expect("cleanup");
    assert!(!runtime_root.join(materialized.to_string()).exists());
}
