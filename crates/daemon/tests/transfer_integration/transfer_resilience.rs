use super::support::*;

#[test]
fn interrupted_transfer_resumes_after_coordinator_restart() {
    let mut origin = Node::new();
    let mut remote = Node::new();
    let source_payload = payload(vec![0xa5; CHUNK_BYTES * 3 + 17]);
    let (content_id, operations) = begin_complete(&mut origin, &source_payload);
    remote.ingest(&operations);

    assert_eq!(move_chunks(&origin, &mut remote, 1), 1);
    let before = remote.transfers.progress()[0];
    assert_eq!(before.verified_chunks, 1);
    remote.restart_transfers();
    assert_eq!(remote.transfers.progress()[0].verified_chunks, 1);

    assert!(move_chunks(&origin, &mut remote, 64) >= 2);
    let progress = remote.transfers.progress()[0];
    assert_eq!(progress.phase, TransferPhase::Complete);
    let activated = remote
        .transfers
        .activate(
            content_id,
            remote.history.projection(),
            &CONTENT_KEY,
            16 * 1024 * 1024,
            &CancellationToken::new(),
        )
        .expect("activate");
    assert_eq!(activated.content(), &clipboard(&source_payload));
}

#[test]
fn cancellation_dominates_pending_partial_and_locally_complete_phases() {
    for chunks_before_cancel in [0_usize, 1, usize::MAX] {
        let mut origin = Node::new();
        let mut remote = Node::new();
        let source_payload = payload(vec![0x44; CHUNK_BYTES * 2 + 9]);
        let (_, operations) = begin_complete(&mut origin, &source_payload);
        remote.ingest(&operations);

        if chunks_before_cancel != 0 {
            move_chunks(&origin, &mut remote, chunks_before_cancel);
        }
        let transfer_id = origin.transfers.progress()[0].transfer_id;
        let cancel = origin
            .transfers
            .cancel(transfer_id, &mut origin.history, 102)
            .expect("cancel");
        remote.ingest(&[cancel]);

        assert_eq!(
            remote.transfers.progress()[0].phase,
            TransferPhase::Cancelled
        );
        assert!(
            remote
                .transfers
                .missing_chunks(16)
                .expect("missing")
                .is_empty()
        );
        let objects = fs::read_dir(remote.transfers.store().root().join("objects"))
            .expect("objects")
            .count();
        assert_eq!(objects, 0, "cancel must reclaim remote partial chunks");
    }
}

#[test]
fn cancelling_one_duplicate_manifest_share_preserves_the_other_transfer() {
    let mut origin = Node::new();
    let mut remote = Node::new();
    let source_payload = payload(vec![0x61; CHUNK_BYTES * 2 + 11]);
    let (content_id, first_operations) = begin_complete(&mut origin, &source_payload);
    let (_, second_operations) = begin_complete(&mut origin, &source_payload);
    let first_transfer = operation_transfer_id(&first_operations[0]).expect("first transfer");
    let second_transfer = operation_transfer_id(&second_operations[0]).expect("second transfer");
    remote.ingest(&first_operations);
    remote.ingest(&second_operations);

    assert_eq!(move_chunks(&origin, &mut remote, 1), 1);
    let cancellation = origin
        .transfers
        .cancel(first_transfer, &mut origin.history, 102)
        .expect("cancel first transfer");
    remote.ingest(&[cancellation]);
    origin.restart_transfers();
    remote.restart_transfers();

    move_chunks(&origin, &mut remote, 64);
    assert!(remote.history.projection().is_visible(content_id));
    let second = remote
        .transfers
        .progress()
        .into_iter()
        .find(|progress| progress.transfer_id == second_transfer)
        .expect("second transfer progress");
    assert_eq!(second.phase, TransferPhase::Complete);
    let activated = remote
        .transfers
        .activate(
            content_id,
            remote.history.projection(),
            &CONTENT_KEY,
            16 * 1024 * 1024,
            &CancellationToken::new(),
        )
        .expect("activate surviving duplicate share");
    assert_eq!(activated.content(), &clipboard(&source_payload));
}

#[test]
fn corrupted_encrypted_chunk_is_rejected_without_progress() {
    let mut origin = Node::new();
    let mut remote = Node::new();
    let source_payload = payload(vec![0x99; CHUNK_BYTES + 1]);
    let (_, operations) = begin_complete(&mut origin, &source_payload);
    remote.ingest(&operations);
    let request = remote.transfers.missing_chunks(1).expect("missing")[0];
    let mut encrypted = origin
        .transfers
        .export_chunk(request, &CancellationToken::new())
        .expect("export");
    *encrypted.last_mut().expect("encrypted byte") ^= 1;

    assert!(matches!(
        remote
            .transfers
            .import_chunk(request, &encrypted, &CancellationToken::new()),
        Err(TransferCoordinatorError::ChunkStore(_))
    ));
    assert_eq!(remote.transfers.progress()[0].verified_chunks, 0);
    assert_eq!(
        fs::read_dir(remote.transfers.store().root().join("staging"))
            .expect("staging")
            .count(),
        0
    );
}

#[test]
fn offline_peer_catches_up_from_non_origin_replica() {
    let mut origin = Node::new();
    let mut relay = Node::new();
    let mut offline = Node::new();
    let source_payload = payload(vec![0x2a; CHUNK_BYTES * 2 + 77]);
    let (content_id, operations) = begin_complete(&mut origin, &source_payload);
    relay.ingest(&operations);
    move_chunks(&origin, &mut relay, 64);
    assert_eq!(relay.transfers.progress()[0].phase, TransferPhase::Complete);

    // The origin is not consulted after this point: the offline peer consumes
    // the forwarded operation log and encrypted chunks from the relay.
    let forwarded = relay
        .history
        .storage()
        .load_operations()
        .expect("forwarded operations");
    offline.ingest(&forwarded);
    move_chunks(&relay, &mut offline, 64);
    let activated = offline
        .transfers
        .activate(
            content_id,
            offline.history.projection(),
            &CONTENT_KEY,
            16 * 1024 * 1024,
            &CancellationToken::new(),
        )
        .expect("offline activation");
    assert_eq!(activated.content(), &clipboard(&source_payload));
}

#[test]
fn replicated_cancellation_converges_when_terminal_operations_are_reordered() {
    let mut origin = Node::new();
    let source_payload = payload(vec![0x18; CHUNK_BYTES + 3]);
    let (content_id, mut operations) = begin_complete(&mut origin, &source_payload);
    let transfer_id = origin.transfers.progress()[0].transfer_id;
    operations.push(
        origin
            .transfers
            .cancel(transfer_id, &mut origin.history, 102)
            .expect("cancel"),
    );

    let mut forward = Projection::default();
    forward.apply_all(&operations).expect("forward projection");
    for order in [
        [0_usize, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ] {
        let mut reordered = Projection::default();
        reordered
            .apply_all(order.map(|index| &operations[index]))
            .expect("reordered projection");
        assert_eq!(forward, reordered);
    }
    assert!(!forward.is_visible(content_id));
    assert_eq!(
        forward.transfer(transfer_id).expect("transfer").phase(),
        TransferPhase::Cancelled
    );
}
