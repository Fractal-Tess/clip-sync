use super::support::*;

fn adds(
    node_id: NodeId,
    prefix: &str,
    counters: std::ops::RangeInclusive<u64>,
) -> Vec<StampedOperation> {
    counters
        .map(|i| make_add(node_id, i, format!("{prefix}-{i}").as_bytes()))
        .collect()
}

// ── Three-node convergence ─────────────────────────────────────────────

#[test]
fn three_node_convergence_under_normal_conditions() {
    let (mut p1, mut p2, mut p3) = (Peer::new(), Peer::new(), Peer::new());
    p1.receive(&adds(node(1), "n1", 1..=3));
    p2.receive(&adds(node(2), "n2", 1..=2));
    p3.receive(&adds(node(3), "n3", 1..=4));

    full_sync(&mut p1, &mut p2);
    full_sync(&mut p2, &mut p3);
    full_sync(&mut p1, &mut p3);

    for peer in [&p1, &p2, &p3] {
        assert_eq!(peer.operations().len(), 9);
    }
    assert_eq!(p1.projection(), p2.projection());
    assert_eq!(p2.projection(), p3.projection());
}

#[test]
fn three_node_convergence_under_partition() {
    let (mut p1, mut p2, mut p3) = (Peer::new(), Peer::new(), Peer::new());

    // Phase 1: n1 and n2 are connected, n3 authors while partitioned.
    p1.receive(&adds(node(1), "n1", 1..=3));
    p2.receive(&adds(node(2), "n2", 1..=2));
    p3.receive(&adds(node(3), "n3", 1..=2));

    full_sync(&mut p1, &mut p2);
    assert_eq!(p1.operations().len(), 5);
    assert_eq!(p2.operations().len(), 5);
    assert_eq!(p3.operations().len(), 2);

    // Phase 2: n3 reaches only n2 and receives n1's work through it.
    full_sync(&mut p2, &mut p3);
    assert_eq!(p3.operations().len(), 7);

    full_sync(&mut p1, &mut p3);
    for peer in [&p1, &p2, &p3] {
        assert_eq!(peer.operations().len(), 7);
    }
    assert_eq!(p1.projection(), p2.projection());
    assert_eq!(p2.projection(), p3.projection());
}

// ── Store-and-forward with origin offline ──────────────────────────────

#[test]
fn store_and_forward_origin_offline() {
    let n1 = node(1);
    let mut origin = Peer::new();
    origin.receive(&adds(n1, "origin", 1..=5));

    let mut relay = Peer::new();
    full_sync(&mut origin, &mut relay);
    assert_eq!(relay.operations().len(), 5);

    // The origin goes offline; a third peer reaches only the relay.
    let mut late = Peer::new();
    full_sync(&mut relay, &mut late);

    for i in 1..=5 {
        let id = OpId::new(n1, i).unwrap();
        assert!(late.seen().contains(id));
        assert_eq!(late.operation(id), origin.operation(id));
    }
    assert_eq!(late.projection(), origin.projection());
}

// ── Idempotent ingest under duplication ────────────────────────────────

#[test]
fn duplicate_batch_entries_are_idempotent() {
    let mut sender = Peer::new();
    sender.receive(&adds(node(1), "op", 1..=3));

    let mut receiver = Peer::new();
    let batch = sender.batch_for(receiver.seen(), &BatchLimits::default());
    // The same batch arrives three times, as after network retries.
    for _ in 0..3 {
        receiver.receive(&batch.operations);
    }

    assert_eq!(receiver.operations().len(), 3);
    assert_eq!(receiver.projection(), sender.projection());
}

// ── Reordered batches still converge ───────────────────────────────────

#[test]
fn reordered_batches_converge() {
    let operations = adds(node(1), "op", 1..=6);

    let mut reversed = Peer::new();
    for operation in operations.iter().rev() {
        reversed.receive(std::slice::from_ref(operation));
    }
    let mut forward = Peer::new();
    for operation in &operations {
        forward.receive(std::slice::from_ref(operation));
    }

    assert_eq!(reversed.projection(), forward.projection());
    assert_eq!(reversed.operations(), forward.operations());
}

// ── Mixed operation types ──────────────────────────────────────────────

#[test]
fn convergence_with_mixed_operation_types() {
    let (n1, n2) = (node(1), node(2));
    let mut p1 = Peer::new();
    p1.receive(&[
        make_add(n1, 1, b"content"),
        make_touch(n1, 2, b"content"),
        make_delete(n1, 3, b"content"),
    ]);
    let mut p2 = Peer::new();
    p2.receive(&[make_add(n2, 1, b"other")]);

    full_sync(&mut p1, &mut p2);

    assert_eq!(p1.projection(), p2.projection());
    assert_eq!(p1.projection().visible_items().len(), 1);
}

#[test]
fn touch_that_overtakes_its_add_converges_once_the_add_arrives() {
    let (n1, n2) = (node(1), node(2));
    let add = make_add(n1, 1, b"content");
    let touch = make_touch(n2, 1, b"content");

    let mut early = Peer::new();
    early.receive(std::slice::from_ref(&touch));
    early.receive(std::slice::from_ref(&add));
    let mut ordered = Peer::new();
    ordered.receive(&[add, touch]);

    assert_eq!(early.projection(), ordered.projection());
    assert_eq!(early.projection().visible_items().len(), 1);
}
