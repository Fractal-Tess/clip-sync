use super::support::*;

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
