use super::support::*;
use proptest::prelude::*;

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
