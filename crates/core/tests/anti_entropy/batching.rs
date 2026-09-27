use super::support::*;

// ── Batch bounds ───────────────────────────────────────────────────────

#[test]
fn batch_never_exceeds_max_ops() {
    let n = node(1);
    let mut peer = Peer::new();
    let operations = (1..=50)
        .map(|i| make_add(n, i, format!("op{i}").as_bytes()))
        .collect::<Vec<_>>();
    peer.receive(&operations);

    for max_ops in [1, 5, 10, 25, 50] {
        let limits = BatchLimits {
            max_ops,
            max_bytes: usize::MAX,
        };
        let batch = peer.batch_for(&SeenOps::default(), &limits);
        assert!(batch.operations.len() <= max_ops);
        assert_eq!(batch.has_more, max_ops < 50);
    }
}

#[test]
fn batch_respects_byte_budget() {
    let n = node(1);
    let mut peer = Peer::new();
    let operations = (1..=20)
        .map(|i| make_add(n, i, format!("{i:0>200}").as_bytes()))
        .collect::<Vec<_>>();
    peer.receive(&operations);

    let one = peer.batch_for(
        &SeenOps::default(),
        &BatchLimits {
            max_ops: usize::MAX,
            max_bytes: 1,
        },
    );
    assert_eq!(
        one.operations.len(),
        1,
        "the first operation is always sent, even over budget"
    );
    assert!(one.has_more);

    let all = peer.batch_for(&SeenOps::default(), &BatchLimits::default());
    assert_eq!(all.operations.len(), 20);
    assert!(!all.has_more);
}

#[test]
fn pagination_delivers_all_ops_across_batches() {
    let n = node(1);
    let total_ops = 25;
    let mut sender = Peer::new();
    let operations = (1..=total_ops)
        .map(|i| make_add(n, i, format!("op{i}").as_bytes()))
        .collect::<Vec<_>>();
    sender.receive(&operations);

    let mut receiver = Peer::new();
    let limits = BatchLimits {
        max_ops: 7,
        max_bytes: usize::MAX,
    };
    let mut rounds = 0;
    while sync_batch(&sender, &mut receiver, &limits) > 0 {
        rounds += 1;
        assert!(rounds <= 10, "should converge within bounded rounds");
    }

    assert_eq!(receiver.operations().len(), total_ops as usize);
    assert_eq!(rounds, 4); // ceil(25/7)
    assert_eq!(receiver.projection(), sender.projection());
}

#[test]
fn batch_skips_operations_the_peer_holds_above_a_gap() {
    let n = node(1);
    let mut sender = Peer::new();
    let operations = (1..=5)
        .map(|i| make_add(n, i, format!("op{i}").as_bytes()))
        .collect::<Vec<_>>();
    sender.receive(&operations);

    let mut receiver = Peer::new();
    receiver.receive(&[operations[0].clone(), operations[3].clone()]);

    let batch = sender.batch_for(receiver.seen(), &BatchLimits::default());
    let counters = batch
        .operations
        .iter()
        .map(|operation| operation.id().counter())
        .collect::<Vec<_>>();
    assert_eq!(counters, vec![2, 3, 5]);
}

// ── No false gap acknowledgment ────────────────────────────────────────

#[test]
fn does_not_falsely_acknowledge_gaps() {
    let n = node(1);
    let mut sender = Peer::new();
    // Sender has ops 1, 2, 4, 5 (missing 3)
    let operations = [1, 2, 4, 5]
        .into_iter()
        .map(|i| make_add(n, i, format!("op{i}").as_bytes()))
        .collect::<Vec<_>>();
    sender.receive(&operations);

    let batch = sender.batch_for(&SeenOps::default(), &BatchLimits::default());
    assert_eq!(batch.operations.len(), 4);

    let mut receiver = Peer::new();
    receiver.receive(&batch.operations);

    // The frontier stops at 2 because op 3 is missing; 4 and 5 are sparse.
    assert_eq!(receiver.seen().frontier(n), 2);
    assert!(receiver.seen().contains(OpId::new(n, 4).unwrap()));
    assert!(receiver.seen().contains(OpId::new(n, 5).unwrap()));
    assert!(!receiver.seen().contains(OpId::new(n, 3).unwrap()));
}
