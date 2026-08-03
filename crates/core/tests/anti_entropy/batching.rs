use super::support::*;

// ── Batch bounds ───────────────────────────────────────────────────────

#[test]
fn batch_never_exceeds_max_ops() {
    let codec = JsonV1Codec;
    let n = node(1);

    let mut state = AntiEntropyState::new();
    for i in 1..=50 {
        let op = make_add(n, i, format!("op{i}").as_bytes());
        state.record_local(&op, &codec).unwrap();
    }

    for max_ops in [1, 5, 10, 25, 50] {
        let limits = BatchLimits {
            max_ops,
            max_bytes: usize::MAX,
        };
        let batch = state.compute_batch(&SeenOps::default(), &limits);
        assert!(batch.len() <= max_ops);
        if max_ops < 50 {
            assert!(batch.has_more());
        }
    }
}

#[test]
fn batch_respects_byte_budget() {
    let codec = JsonV1Codec;
    let n = node(1);

    let mut state = AntiEntropyState::new();
    for i in 1..=20 {
        let op = make_add(n, i, &[b'a'; 200]);
        state.record_local(&op, &codec).unwrap();
    }

    let per_op_size = state.log().get(OpId::new(n, 1).unwrap()).unwrap().len();

    let limits = BatchLimits {
        max_ops: usize::MAX,
        max_bytes: per_op_size * 5 + 1,
    };
    let batch = state.compute_batch(&SeenOps::default(), &limits);
    assert_eq!(batch.len(), 5);
    assert!(batch.has_more());
    assert!(batch.total_bytes() <= limits.max_bytes);
}

#[test]
fn pagination_delivers_all_ops_across_batches() {
    let codec = JsonV1Codec;
    let n = node(1);
    let total_ops = 25;

    let mut sender = AntiEntropyState::new();
    for i in 1..=total_ops {
        let op = make_add(n, i, format!("op{i}").as_bytes());
        sender.record_local(&op, &codec).unwrap();
    }

    let mut receiver = AntiEntropyState::new();
    let mut p_receiver = Projection::default();
    let limits = BatchLimits {
        max_ops: 7,
        max_bytes: usize::MAX,
    };

    let mut rounds = 0;
    loop {
        let batch = sender.compute_batch(receiver.seen(), &limits);
        if batch.is_empty() {
            break;
        }
        for entry in batch.entries() {
            ingest_and_apply(&mut receiver, &mut p_receiver, entry, &codec);
        }
        rounds += 1;
        assert!(rounds <= 10, "should converge within bounded rounds");
    }

    assert_eq!(receiver.log().len(), total_ops as usize);
    assert!(rounds >= 4); // ceil(25/7) = 4
}

// ── No false gap acknowledgment ────────────────────────────────────────

#[test]
fn does_not_falsely_acknowledge_gaps() {
    let codec = JsonV1Codec;
    let n = node(1);

    let mut sender = AntiEntropyState::new();
    // Sender has ops 1, 2, 4, 5 (missing 3)
    for i in [1, 2, 4, 5] {
        let op = make_add(n, i, format!("op{i}").as_bytes());
        sender.record_local(&op, &codec).unwrap();
    }

    let batch = sender.compute_batch(&SeenOps::default(), &BatchLimits::default());
    assert_eq!(batch.len(), 4);

    // Receiver ingests the batch
    let mut receiver = AntiEntropyState::new();
    let mut p_receiver = Projection::default();
    for entry in batch.entries() {
        ingest_and_apply(&mut receiver, &mut p_receiver, entry, &codec);
    }

    // Receiver's frontier for this node should be 2 (not 5), because op 3 is missing
    assert_eq!(receiver.seen().frontier(n), 2);
    // Ops 4 and 5 are in the sparse gap set
    assert!(receiver.seen().contains(OpId::new(n, 4).unwrap()));
    assert!(receiver.seen().contains(OpId::new(n, 5).unwrap()));
    assert!(!receiver.seen().contains(OpId::new(n, 3).unwrap()));
}

// ── Missing-locally detection ──────────────────────────────────────────

#[test]
fn missing_locally_identifies_needed_ops() {
    let codec = JsonV1Codec;
    let n = node(1);

    let mut remote = AntiEntropyState::new();
    for i in 1..=10 {
        let op = make_add(n, i, format!("op{i}").as_bytes());
        remote.record_local(&op, &codec).unwrap();
    }

    let mut local = AntiEntropyState::new();
    // Local only has 1-5
    for i in 1..=5 {
        let op = make_add(n, i, format!("op{i}").as_bytes());
        local.record_local(&op, &codec).unwrap();
    }

    let missing = local.missing_locally(remote.seen());
    let counters: Vec<u64> = missing.iter().map(|id| id.counter()).collect();
    assert_eq!(counters, vec![6, 7, 8, 9, 10]);
}
