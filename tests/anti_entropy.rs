//! Integration and property tests for the anti-entropy replication core.

#![allow(clippy::cast_possible_truncation, clippy::trivially_copy_pass_by_ref)]
//!
//! These tests exercise multi-node convergence, store-and-forward relaying,
//! idempotency under duplication/reordering, and resource-bound enforcement
//! without any networking.

use clip_sync::model::{
    HlcTimestamp, NodeId, OpId, Operation, Payload, Projection, Representation, SeenOps,
    StampedOperation,
};
use clip_sync::replication::{
    AntiEntropyState, BatchLimits, Codec, IngestOutcome, JsonV1Codec, OpBatch,
};
use proptest::prelude::*;
use uuid::Uuid;

// ── Helpers ────────────────────────────────────────────────────────────

const CONTENT_KEY: [u8; 32] = [9; 32];

fn node(id: u128) -> NodeId {
    NodeId::from_uuid(Uuid::from_u128(id))
}

fn make_add(node_id: NodeId, counter: u64, text: &[u8]) -> StampedOperation {
    let id = OpId::new(node_id, counter).unwrap();
    let ts = HlcTimestamp::new(counter * 1000, 0);
    let payload =
        Payload::new(&CONTENT_KEY, vec![Representation::new("text/plain", text)]).expect("valid");
    let content_id = payload.descriptor().content_id();
    StampedOperation::new(
        id,
        ts,
        Operation::Add {
            content_id,
            payload,
        },
    )
}

fn make_touch(node_id: NodeId, counter: u64, content_text: &[u8]) -> StampedOperation {
    let id = OpId::new(node_id, counter).unwrap();
    let ts = HlcTimestamp::new(counter * 1000, 0);
    let payload = Payload::new(
        &CONTENT_KEY,
        vec![Representation::new("text/plain", content_text)],
    )
    .expect("valid");
    let content_id = payload.descriptor().content_id();
    StampedOperation::new(id, ts, Operation::Touch { content_id })
}

fn make_delete(node_id: NodeId, counter: u64, content_text: &[u8]) -> StampedOperation {
    let id = OpId::new(node_id, counter).unwrap();
    let ts = HlcTimestamp::new(counter * 1000, 0);
    let payload = Payload::new(
        &CONTENT_KEY,
        vec![Representation::new("text/plain", content_text)],
    )
    .expect("valid");
    let content_id = payload.descriptor().content_id();
    StampedOperation::new(id, ts, Operation::Delete { content_id })
}

/// Encode an op, ingest it into a state, and apply to projection.
fn ingest_and_apply(
    state: &mut AntiEntropyState,
    projection: &mut Projection,
    raw: &[u8],
    codec: &JsonV1Codec,
) {
    match state.ingest_raw(raw, codec).unwrap() {
        IngestOutcome::Applied(op) => {
            projection.apply(&op).unwrap();
        }
        IngestOutcome::Duplicate => {}
    }
}

/// Transfer a full batch from sender to receiver, applying to projection.
fn sync_batch(
    sender: &AntiEntropyState,
    receiver: &mut AntiEntropyState,
    receiver_proj: &mut Projection,
    limits: &BatchLimits,
    codec: &JsonV1Codec,
) -> OpBatch {
    let batch = sender.compute_batch(receiver.seen(), limits);
    for entry in batch.entries() {
        ingest_and_apply(receiver, receiver_proj, entry, codec);
    }
    batch
}

/// Fully synchronize two nodes by exchanging batches until both are idle.
fn full_sync(
    a: &mut AntiEntropyState,
    a_proj: &mut Projection,
    b: &mut AntiEntropyState,
    b_proj: &mut Projection,
    codec: &JsonV1Codec,
) {
    let limits = BatchLimits::default();
    loop {
        let ab = sync_batch(a, b, b_proj, &limits, codec);
        let ba = sync_batch(b, a, a_proj, &limits, codec);
        if ab.is_empty() && ba.is_empty() {
            break;
        }
    }
}

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

// ── Property tests ─────────────────────────────────────────────────────

proptest! {
    #[test]
    fn ingest_order_does_not_affect_seen_state(
        counters in prop::collection::vec(1_u64..50, 1..30)
    ) {
        let codec = JsonV1Codec;
        let n = node(1);

        // Deduplicate counters (same counter = same OpId, must have same content)
        let unique: Vec<u64> = {
            let mut seen = std::collections::BTreeSet::new();
            counters.into_iter().filter(|c| seen.insert(*c)).collect()
        };

        // Create ops with unique counters (text derived from counter for consistency)
        let ops: Vec<StampedOperation> = unique.iter()
            .map(|&c| make_add(n, c, format!("op{c}").as_bytes()))
            .collect();

        // Encode them
        let encoded: Vec<Vec<u8>> = ops.iter()
            .map(|op| codec.encode_op(op).unwrap())
            .collect();

        // Forward-order ingest
        let mut forward = AntiEntropyState::new();
        let mut p_fwd = Projection::default();
        for raw in &encoded {
            ingest_and_apply(&mut forward, &mut p_fwd, raw, &codec);
        }

        // Reverse-order ingest
        let mut reverse = AntiEntropyState::new();
        let mut p_rev = Projection::default();
        for raw in encoded.iter().rev() {
            ingest_and_apply(&mut reverse, &mut p_rev, raw, &codec);
        }

        prop_assert_eq!(forward.seen(), reverse.seen());
        prop_assert_eq!(forward.log().len(), reverse.log().len());
    }

    #[test]
    fn batch_size_respects_limits(
        op_count in 1_u64..30,
        max_ops in 1_usize..20,
    ) {
        let codec = JsonV1Codec;
        let n = node(1);

        let mut state = AntiEntropyState::new();
        for i in 1..=op_count {
            let op = make_add(n, i, format!("data{i}").as_bytes());
            state.record_local(&op, &codec).unwrap();
        }

        let limits = BatchLimits {
            max_ops,
            max_bytes: usize::MAX,
        };
        let batch = state.compute_batch(&SeenOps::default(), &limits);
        prop_assert!(batch.len() <= max_ops);
        if (op_count as usize) > max_ops {
            prop_assert!(batch.has_more());
        }
    }

    #[test]
    fn full_sync_always_converges_two_nodes(
        ops_a in prop::collection::vec(1_u64..20, 0..15),
        ops_b in prop::collection::vec(1_u64..20, 0..15),
    ) {
        let codec = JsonV1Codec;
        let (na, nb) = (node(1), node(2));

        let mut sa = AntiEntropyState::new();
        let mut sb = AntiEntropyState::new();
        let mut pa = Projection::default();
        let mut pb = Projection::default();

        // Deduplicate counters for each node (each node's counters must be unique)
        let mut seen_a = std::collections::BTreeSet::new();
        for &c in &ops_a {
            if seen_a.insert(c) {
                let op = make_add(na, c, format!("a{c}").as_bytes());
                sa.record_local(&op, &codec).unwrap();
                pa.apply(&op).unwrap();
            }
        }
        let mut seen_b = std::collections::BTreeSet::new();
        for &c in &ops_b {
            if seen_b.insert(c) {
                let op = make_add(nb, c, format!("b{c}").as_bytes());
                sb.record_local(&op, &codec).unwrap();
                pb.apply(&op).unwrap();
            }
        }

        full_sync(&mut sa, &mut pa, &mut sb, &mut pb, &codec);

        prop_assert_eq!(sa.log().len(), sb.log().len());
        prop_assert_eq!(pa.seen_ops(), pb.seen_ops());
    }

    #[test]
    fn pagination_always_delivers_everything(
        op_count in 1_u64..40,
        batch_size in 1_usize..10,
    ) {
        let codec = JsonV1Codec;
        let n = node(1);

        let mut sender = AntiEntropyState::new();
        for i in 1..=op_count {
            let op = make_add(n, i, format!("p{i}").as_bytes());
            sender.record_local(&op, &codec).unwrap();
        }

        let mut receiver = AntiEntropyState::new();
        let mut p_recv = Projection::default();
        let limits = BatchLimits {
            max_ops: batch_size,
            max_bytes: usize::MAX,
        };

        let mut rounds = 0;
        loop {
            let batch = sender.compute_batch(receiver.seen(), &limits);
            if batch.is_empty() {
                break;
            }
            for entry in batch.entries() {
                ingest_and_apply(&mut receiver, &mut p_recv, entry, &codec);
            }
            rounds += 1;
            prop_assert!(rounds <= 100, "should converge");
        }

        prop_assert_eq!(receiver.log().len(), op_count as usize);
    }
}
