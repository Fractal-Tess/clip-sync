use std::{fs, io::Cursor, time::Duration};

use clip_sync::{
    model::NodeId,
    payload::{
        ChunkStore, ChunkStoreConfig, ChunkStoreError, ChunkStoreKey, ExplicitShareError,
        ExplicitSharePolicy, FileSnapshotError, FileSnapshotLimits, MaterializationError,
        Materializer, MaterializerConfig, StoredManifest, parse_file_uri_list, snapshot_file_uris,
    },
    transfer::{TransferControl, TransferId, TransferPhase, TransferRecord, TransferStateLimits},
};
use prost::Message;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use url::Url;

const CHUNK_BYTES: usize = 64 * 1024;

fn store(directory: &TempDir) -> ChunkStore {
    ChunkStore::open(
        directory.path().join("chunks"),
        &ChunkStoreKey::from_bytes([0x42; 32]),
        ChunkStoreConfig {
            chunk_bytes: CHUNK_BYTES,
            max_payload_bytes: 16 * 1024 * 1024,
            max_chunks_per_manifest: 256,
        },
    )
    .expect("open chunk store")
}

#[test]
fn encrypted_fixed_chunks_deduplicate_and_refcounts_reclaim() {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut store = store(&directory);
    let cancellation = CancellationToken::new();
    let shared = vec![b's'; CHUNK_BYTES];
    let secret = b"known plaintext fixture that must not survive on disk";

    let mut first_bytes = shared.clone();
    first_bytes.extend_from_slice(secret);
    let first = store
        .stage_reader(
            &mut Cursor::new(first_bytes.clone()),
            first_bytes.len() as u64,
            &cancellation,
        )
        .expect("stage first");
    let first_id = store
        .commit_manifest(&StoredManifest::Blob(first.clone()))
        .expect("commit first");

    let mut second_bytes = shared;
    second_bytes.extend_from_slice(b"different suffix");
    let second = store
        .stage_reader(
            &mut Cursor::new(second_bytes.clone()),
            second_bytes.len() as u64,
            &cancellation,
        )
        .expect("stage second");
    assert_eq!(first.chunks()[0].id(), second.chunks()[0].id());
    let shared_chunk = first.chunks()[0].id();
    let second_id = store
        .commit_manifest(&StoredManifest::Blob(second.clone()))
        .expect("commit second");

    let object_lengths: Vec<_> = fs::read_dir(store.root().join("objects"))
        .expect("objects")
        .map(|entry| entry.expect("entry").metadata().expect("metadata").len())
        .collect();
    assert!(object_lengths.len() >= 3);
    assert!(object_lengths.windows(2).all(|pair| pair[0] == pair[1]));

    for entry in walk_files(store.root()) {
        let bytes = fs::read(entry).expect("read persistent file");
        assert!(!bytes.windows(secret.len()).any(|window| window == secret));
    }

    assert!(store.remove_manifest(first_id).expect("remove first"));
    assert!(store.has_chunk(shared_chunk));
    let mut restored = Vec::new();
    store
        .read_blob(&second, &mut restored, &cancellation)
        .expect("read retained second");
    assert_eq!(restored, second_bytes);

    assert!(store.remove_manifest(second_id).expect("remove second"));
    assert!(!store.has_chunk(shared_chunk));
}

#[test]
fn corrupt_encrypted_chunk_fails_authentication() {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut store = store(&directory);
    let cancellation = CancellationToken::new();
    let blob = store
        .stage_reader(&mut Cursor::new(vec![7_u8; 1024]), 1024, &cancellation)
        .expect("stage");
    store
        .commit_manifest(&StoredManifest::Blob(blob.clone()))
        .expect("commit");
    let chunk = &blob.chunks()[0];
    let path = store.root().join("objects").join(chunk.id().to_string());
    let mut encrypted = fs::read(&path).expect("read object");
    *encrypted.last_mut().expect("nonempty") ^= 1;
    fs::write(path, encrypted).expect("corrupt object");

    assert!(matches!(
        store.read_blob(&blob, &mut Vec::new(), &cancellation),
        Err(ChunkStoreError::Authentication(_))
    ));
}

#[test]
fn encrypted_chunk_stream_import_authenticates_and_resumes() {
    let source_directory = tempfile::tempdir().expect("source tempdir");
    let destination_directory = tempfile::tempdir().expect("destination tempdir");
    let mut source = store(&source_directory);
    let mut destination = store(&destination_directory);
    let cancellation = CancellationToken::new();
    let blob = source
        .stage_reader(
            &mut Cursor::new(vec![0x5a_u8; CHUNK_BYTES]),
            CHUNK_BYTES as u64,
            &cancellation,
        )
        .expect("stage");
    let chunk = &blob.chunks()[0];
    let mut encrypted = Vec::new();
    source
        .export_encrypted_chunk(chunk.id(), &mut encrypted, &cancellation)
        .expect("export");
    destination
        .import_encrypted_chunk(
            chunk.id(),
            chunk.logical_size(),
            &mut Cursor::new(&encrypted),
            &cancellation,
        )
        .expect("import");
    let mut plaintext = Vec::new();
    destination
        .read_chunk(chunk, &mut plaintext, &cancellation)
        .expect("authenticated read");
    assert_eq!(plaintext, vec![0x5a_u8; CHUNK_BYTES]);

    *encrypted.last_mut().expect("encrypted bytes") ^= 1;
    let third_directory = tempfile::tempdir().expect("third tempdir");
    let mut third = store(&third_directory);
    assert!(matches!(
        third.import_encrypted_chunk(
            chunk.id(),
            chunk.logical_size(),
            &mut Cursor::new(encrypted),
            &cancellation
        ),
        Err(ChunkStoreError::Authentication(_))
    ));
}

#[test]
fn reopen_reclaims_uncommitted_chunks_and_staging() {
    let directory = tempfile::tempdir().expect("tempdir");
    let chunk_id = {
        let mut store = store(&directory);
        let blob = store
            .stage_reader(
                &mut Cursor::new(b"never committed"),
                15,
                &CancellationToken::new(),
            )
            .expect("stage");
        let chunk_id = blob.chunks()[0].id();
        assert!(store.has_chunk(chunk_id));
        fs::write(store.root().join("staging/abandoned.staging"), b"partial")
            .expect("abandoned staging");
        chunk_id
    };

    let reopened = store(&directory);
    assert!(!reopened.has_chunk(chunk_id));
    assert_eq!(
        fs::read_dir(reopened.root().join("staging"))
            .expect("staging")
            .count(),
        0
    );
}

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

#[test]
fn explicit_share_requires_confirmation_and_marks_quota_exemption() {
    let policy = ExplicitSharePolicy {
        automatic_capture_threshold_bytes: 20 * 1024 * 1024,
        mesh_quota_bytes: 1024 * 1024 * 1024,
        maximum_explicit_share_bytes: 4 * 1024 * 1024 * 1024,
        free_space_reserve_bytes: 1024,
    };
    let inspection = policy
        .inspect(2 * 1024 * 1024 * 1024, 3 * 1024 * 1024 * 1024)
        .expect("inspection");
    assert!(inspection.confirmation_required());
    assert!(inspection.quota_exempt());
    assert!(inspection.human_size().contains("GiB"));
    assert_eq!(
        policy.authorize(inspection, false),
        Err(ExplicitShareError::ConfirmationRequired)
    );
    assert!(
        policy
            .authorize(inspection, true)
            .expect("authorize")
            .quota_exempt()
    );
}

#[test]
fn explicit_share_capture_starts_only_from_authorized_decision() {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut store = store(&directory);
    let policy = ExplicitSharePolicy {
        automatic_capture_threshold_bytes: 4,
        mesh_quota_bytes: 8,
        maximum_explicit_share_bytes: 1024,
        free_space_reserve_bytes: 0,
    };
    let inspection = policy.inspect(10, 1024).expect("inspection");
    assert_eq!(
        policy.authorize(inspection, false),
        Err(ExplicitShareError::ConfirmationRequired)
    );
    let decision = policy.authorize(inspection, true).expect("authorize");
    let captured = decision
        .capture_blob(
            &mut store,
            &mut Cursor::new(b"0123456789"),
            &CancellationToken::new(),
        )
        .expect("capture");
    assert_eq!(captured.logical_size(), 10);
    assert!(captured.quota_exempt());
    assert_eq!(
        store
            .manifest(captured.manifest_id())
            .expect("stored manifest"),
        *captured.manifest()
    );

    let mismatch = policy
        .authorize(policy.inspect(11, 1024).expect("inspection"), true)
        .expect("authorize");
    assert!(
        mismatch
            .capture_blob(
                &mut store,
                &mut Cursor::new(b"short"),
                &CancellationToken::new()
            )
            .is_err()
    );
}

#[test]
fn transfer_state_resumes_idempotently_and_cancel_dominates() {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut store = store(&directory);
    let cancellation = CancellationToken::new();
    let blob = store
        .stage_reader(
            &mut Cursor::new(vec![9_u8; CHUNK_BYTES + 10]),
            (CHUNK_BYTES + 10) as u64,
            &cancellation,
        )
        .expect("stage");
    let manifest_id = store
        .commit_manifest(&StoredManifest::Blob(blob.clone()))
        .expect("manifest");
    let transfer_id = TransferId::new();
    let mut transfer = TransferRecord::new(
        transfer_id,
        manifest_id,
        blob.logical_size(),
        blob.chunks(),
        false,
        TransferStateLimits::default(),
    )
    .expect("transfer");
    transfer.begin_staging().expect("staging");
    transfer.begin_replication().expect("replication");
    let first = &blob.chunks()[0];
    assert!(
        transfer
            .mark_chunk_verified(first.id(), first.logical_size())
            .expect("verify")
    );
    assert!(
        !transfer
            .mark_chunk_verified(first.id(), first.logical_size())
            .expect("duplicate verify")
    );
    transfer.pause().expect("pause");
    assert_eq!(transfer.missing_chunks(32).expect("missing").len(), 1);
    transfer.begin_replication().expect("resume");
    let node = NodeId::new();
    transfer
        .update_peer(
            node,
            transfer.verified_bytes(),
            false,
            TransferStateLimits::default(),
        )
        .expect("peer progress");
    let token = transfer.cancellation_token();
    assert!(transfer.cancel().expect("cancel"));
    assert_eq!(transfer.phase(), TransferPhase::Cancelled);
    assert!(token.is_cancelled());
    assert!(!transfer.cancel().expect("repeat cancellation"));
    let encoded = transfer
        .encode_bounded_json(TransferStateLimits::default(), 64 * 1024)
        .expect("encode persisted state");
    let restored =
        TransferRecord::decode_bounded_json(&encoded, TransferStateLimits::default(), 64 * 1024)
            .expect("restore persisted state");
    assert_eq!(restored.phase(), TransferPhase::Cancelled);
    assert!(restored.cancellation_token().is_cancelled());
}

#[test]
fn transfer_control_is_bounded_and_contains_no_chunk_payload() {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut store = store(&directory);
    let blob = store
        .stage_reader(&mut Cursor::new(b"payload"), 7, &CancellationToken::new())
        .expect("blob");
    let manifest = store
        .commit_manifest(&StoredManifest::Blob(blob.clone()))
        .expect("manifest");
    let transfer = TransferId::new();
    let begin = TransferControl::begin(transfer, manifest, 7, 1, false);
    let encoded = begin.encode_bounded().expect("encode");
    assert_eq!(
        TransferControl::decode_bounded(&encoded)
            .expect("decode")
            .encoded_len(),
        encoded.len()
    );

    let too_many = vec![blob.chunks()[0].id(); 257];
    assert!(
        TransferControl::request(transfer, &too_many)
            .encode_bounded()
            .is_err()
    );
}

fn walk_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(path) = pending.pop() {
        for entry in fs::read_dir(path).expect("read dir") {
            let entry = entry.expect("entry");
            let kind = entry.file_type().expect("file type");
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() {
                files.push(entry.path());
            }
        }
    }
    files
}
