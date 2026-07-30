use super::support::*;

// ── Three-node convergence ─────────────────────────────────────────────

#[test]
fn three_node_convergence_under_normal_conditions() {
    let codec = JsonV1Codec;
    let (n1, n2, n3) = (node(1), node(2), node(3));

    let mut s1 = AntiEntropyState::new();
    let mut s2 = AntiEntropyState::new();
    let mut s3 = AntiEntropyState::new();
    let mut p1 = Projection::default();
    let mut p2 = Projection::default();
    let mut p3 = Projection::default();

    // Each node authors operations locally
    let ops1: Vec<_> = (1..=3)
        .map(|i| make_add(n1, i, format!("n1-op{i}").as_bytes()))
        .collect();
    let ops2: Vec<_> = (1..=2)
        .map(|i| make_add(n2, i, format!("n2-op{i}").as_bytes()))
        .collect();
    let ops3: Vec<_> = (1..=4)
        .map(|i| make_add(n3, i, format!("n3-op{i}").as_bytes()))
        .collect();

    for op in &ops1 {
        s1.record_local(op, &codec).unwrap();
        p1.apply(op).unwrap();
    }
    for op in &ops2 {
        s2.record_local(op, &codec).unwrap();
        p2.apply(op).unwrap();
    }
    for op in &ops3 {
        s3.record_local(op, &codec).unwrap();
        p3.apply(op).unwrap();
    }

    // Full mesh sync: 1↔2, 2↔3, 1↔3
    full_sync(&mut s1, &mut p1, &mut s2, &mut p2, &codec);
    full_sync(&mut s2, &mut p2, &mut s3, &mut p3, &codec);
    full_sync(&mut s1, &mut p1, &mut s3, &mut p3, &codec);

    // All three should see the same 9 operations
    assert_eq!(s1.log().len(), 9);
    assert_eq!(s2.log().len(), 9);
    assert_eq!(s3.log().len(), 9);

    // Projections converge
    assert_eq!(p1.visible_items().len(), p2.visible_items().len());
    assert_eq!(p2.visible_items().len(), p3.visible_items().len());
    assert_eq!(*p1.seen_ops(), *p2.seen_ops());
    assert_eq!(*p2.seen_ops(), *p3.seen_ops());
}

#[test]
fn three_node_convergence_under_partition() {
    let codec = JsonV1Codec;
    let (n1, n2, n3) = (node(1), node(2), node(3));

    let mut s1 = AntiEntropyState::new();
    let mut s2 = AntiEntropyState::new();
    let mut s3 = AntiEntropyState::new();
    let mut p1 = Projection::default();
    let mut p2 = Projection::default();
    let mut p3 = Projection::default();

    // Phase 1: n1 and n2 are connected, n3 is partitioned
    for i in 1..=3 {
        let op = make_add(n1, i, format!("n1-{i}").as_bytes());
        s1.record_local(&op, &codec).unwrap();
        p1.apply(&op).unwrap();
    }
    for i in 1..=2 {
        let op = make_add(n2, i, format!("n2-{i}").as_bytes());
        s2.record_local(&op, &codec).unwrap();
        p2.apply(&op).unwrap();
    }
    // n3 authors independently during partition
    for i in 1..=2 {
        let op = make_add(n3, i, format!("n3-{i}").as_bytes());
        s3.record_local(&op, &codec).unwrap();
        p3.apply(&op).unwrap();
    }

    // Sync n1 ↔ n2 only
    full_sync(&mut s1, &mut p1, &mut s2, &mut p2, &codec);
    assert_eq!(s1.log().len(), 5);
    assert_eq!(s2.log().len(), 5);
    assert_eq!(s3.log().len(), 2); // Still partitioned

    // Phase 2: Partition heals, n3 connects to n2
    full_sync(&mut s2, &mut p2, &mut s3, &mut p3, &codec);
    // n3 now has everything from n1 and n2 via store-and-forward through n2
    assert_eq!(s3.log().len(), 7);

    // Final sync to propagate n3's ops back
    full_sync(&mut s1, &mut p1, &mut s3, &mut p3, &codec);

    assert_eq!(s1.log().len(), 7);
    assert_eq!(s2.log().len(), 7);
    assert_eq!(s3.log().len(), 7);
    assert_eq!(*p1.seen_ops(), *p2.seen_ops());
    // n2 hasn't gotten n3's ops yet via this path, do final sync
    full_sync(&mut s1, &mut p1, &mut s2, &mut p2, &codec);
    assert_eq!(*p1.seen_ops(), *p2.seen_ops());
    assert_eq!(*p2.seen_ops(), *p3.seen_ops());
}

// ── Store-and-forward with origin offline ──────────────────────────────

#[test]
fn store_and_forward_origin_offline() {
    let codec = JsonV1Codec;
    let n1 = node(1);

    // Node 1 authors operations
    let mut s1 = AntiEntropyState::new();
    let mut p1 = Projection::default();
    for i in 1..=5 {
        let op = make_add(n1, i, format!("origin-{i}").as_bytes());
        s1.record_local(&op, &codec).unwrap();
        p1.apply(&op).unwrap();
    }

    // Node 1 syncs to Node 2
    let mut s2 = AntiEntropyState::new();
    let mut p2 = Projection::default();
    full_sync(&mut s1, &mut p1, &mut s2, &mut p2, &codec);
    assert_eq!(s2.log().len(), 5);

    // Node 1 goes offline. Node 3 connects only to Node 2.
    let mut s3 = AntiEntropyState::new();
    let mut p3 = Projection::default();
    full_sync(&mut s2, &mut p2, &mut s3, &mut p3, &codec);

    // Node 3 has all of Node 1's operations, forwarded through Node 2
    assert_eq!(s3.log().len(), 5);
    for i in 1..=5 {
        assert!(s3.seen().contains(OpId::new(n1, i).unwrap()));
    }
    assert_eq!(*p2.seen_ops(), *p3.seen_ops());

    // Raw bytes are preserved (forwarded identically)
    for i in 1..=5 {
        let id = OpId::new(n1, i).unwrap();
        assert_eq!(s1.log().get(id), s3.log().get(id));
    }
}

// ── Idempotent ingest under duplication ────────────────────────────────

#[test]
fn duplicate_batch_entries_are_idempotent() {
    let codec = JsonV1Codec;
    let n = node(1);

    let mut sender = AntiEntropyState::new();
    let mut p_sender = Projection::default();
    for i in 1..=3 {
        let op = make_add(n, i, format!("op{i}").as_bytes());
        sender.record_local(&op, &codec).unwrap();
        p_sender.apply(&op).unwrap();
    }

    let mut receiver = AntiEntropyState::new();
    let mut p_receiver = Projection::default();

    // Send the same batch three times (simulating network retries)
    let batch = sender.compute_batch(receiver.seen(), &BatchLimits::default());
    for _ in 0..3 {
        for entry in batch.entries() {
            ingest_and_apply(&mut receiver, &mut p_receiver, entry, &codec);
        }
    }

    assert_eq!(receiver.log().len(), 3);
    assert_eq!(*p_sender.seen_ops(), *p_receiver.seen_ops());
}

// ── Reordered batches still converge ───────────────────────────────────

#[test]
fn reordered_batches_converge() {
    let codec = JsonV1Codec;
    let n = node(1);

    let mut sender = AntiEntropyState::new();
    let mut p_sender = Projection::default();
    for i in 1..=6 {
        let op = make_add(n, i, format!("op{i}").as_bytes());
        sender.record_local(&op, &codec).unwrap();
        p_sender.apply(&op).unwrap();
    }

    // Collect all raw bytes
    let all_raw: Vec<Vec<u8>> = sender.log().iter().map(|(_, raw)| raw.to_vec()).collect();

    // Deliver in reverse order
    let mut r1 = AntiEntropyState::new();
    let mut p1 = Projection::default();
    for raw in all_raw.iter().rev() {
        ingest_and_apply(&mut r1, &mut p1, raw, &codec);
    }

    // Deliver in forward order
    let mut r2 = AntiEntropyState::new();
    let mut p2 = Projection::default();
    for raw in &all_raw {
        ingest_and_apply(&mut r2, &mut p2, raw, &codec);
    }

    // Both receivers converge to same state
    assert_eq!(*p1.seen_ops(), *p2.seen_ops());
    assert_eq!(r1.log().len(), r2.log().len());
}

// ── Mixed operation types ──────────────────────────────────────────────

#[test]
fn convergence_with_mixed_operation_types() {
    let codec = JsonV1Codec;
    let (n1, n2) = (node(1), node(2));

    let mut s1 = AntiEntropyState::new();
    let mut s2 = AntiEntropyState::new();
    let mut p1 = Projection::default();
    let mut p2 = Projection::default();

    // Node 1: Add, Touch, Delete sequence
    let add = make_add(n1, 1, b"content");
    s1.record_local(&add, &codec).unwrap();
    p1.apply(&add).unwrap();

    let touch = make_touch(n1, 2, b"content");
    s1.record_local(&touch, &codec).unwrap();
    p1.apply(&touch).unwrap();

    let delete = make_delete(n1, 3, b"content");
    s1.record_local(&delete, &codec).unwrap();
    p1.apply(&delete).unwrap();

    // Node 2: Independent add
    let add2 = make_add(n2, 1, b"other");
    s2.record_local(&add2, &codec).unwrap();
    p2.apply(&add2).unwrap();

    // Sync
    full_sync(&mut s1, &mut p1, &mut s2, &mut p2, &codec);

    // Both see the delete result and the independent add
    assert_eq!(p1.visible_items().len(), p2.visible_items().len());
    assert_eq!(*p1.seen_ops(), *p2.seen_ops());
}

// ── Raw byte preservation for forwarding ───────────────────────────────

#[test]
fn raw_bytes_preserved_across_three_hops() {
    let codec = JsonV1Codec;
    let n = node(1);

    // Origin creates ops
    let mut origin = AntiEntropyState::new();
    let mut p_origin = Projection::default();
    for i in 1..=3 {
        let op = make_add(n, i, format!("op{i}").as_bytes());
        origin.record_local(&op, &codec).unwrap();
        p_origin.apply(&op).unwrap();
    }

    // Hop 1: origin → relay
    let mut relay = AntiEntropyState::new();
    let mut p_relay = Projection::default();
    full_sync(&mut origin, &mut p_origin, &mut relay, &mut p_relay, &codec);

    // Hop 2: relay → destination (origin offline)
    let mut dest = AntiEntropyState::new();
    let mut p_dest = Projection::default();
    full_sync(&mut relay, &mut p_relay, &mut dest, &mut p_dest, &codec);

    // Verify exact byte preservation at every hop
    for i in 1..=3 {
        let id = OpId::new(n, i).unwrap();
        let origin_bytes = origin.log().get(id).unwrap();
        let relay_bytes = relay.log().get(id).unwrap();
        let dest_bytes = dest.log().get(id).unwrap();
        assert_eq!(origin_bytes, relay_bytes);
        assert_eq!(relay_bytes, dest_bytes);
    }
}
