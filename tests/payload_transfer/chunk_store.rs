use super::support::*;

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
fn crash_reopen_reclaims_unbounded_staging_pressure_in_bounded_batches() {
    let directory = tempfile::tempdir().expect("tempdir");
    {
        let mut store = store(&directory);
        for index in 0_u32..1_100 {
            let bytes = index.to_le_bytes();
            store
                .stage_reader(
                    &mut Cursor::new(bytes),
                    bytes.len() as u64,
                    &CancellationToken::new(),
                )
                .expect("stage uncommitted chunk");
        }
        for index in 0..256 {
            fs::write(
                store
                    .root()
                    .join("staging")
                    .join(format!("{index}.staging")),
                b"injected partial write",
            )
            .expect("write injected staging");
        }
    }

    let reopened = store(&directory);
    assert_eq!(
        fs::read_dir(reopened.root().join("objects"))
            .expect("objects")
            .count(),
        0
    );
    assert_eq!(
        fs::read_dir(reopened.root().join("staging"))
            .expect("staging")
            .count(),
        0
    );
}

#[test]
fn crash_cleanup_reclaims_manifest_committed_before_history_transaction() {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut store = store(&directory);
    let blob = store
        .stage_reader(
            &mut Cursor::new(b"cross-database crash window"),
            1024,
            &CancellationToken::new(),
        )
        .expect("stage");
    let chunk = blob.chunks()[0].id();
    let manifest = store
        .commit_manifest(&StoredManifest::Blob(blob))
        .expect("commit before injected crash");
    assert!(store.has_chunk(chunk));

    assert_eq!(
        store
            .cleanup_untracked_manifests(&BTreeSet::new())
            .expect("startup reconciliation"),
        1
    );
    assert!(matches!(
        store.manifest(manifest),
        Err(ChunkStoreError::MissingManifest(_))
    ));
    assert!(!store.has_chunk(chunk));
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
