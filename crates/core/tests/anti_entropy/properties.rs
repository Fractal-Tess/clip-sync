use std::collections::BTreeSet;

use super::support::*;
use proptest::prelude::*;

fn unique(counters: Vec<u64>) -> Vec<u64> {
    let mut seen = BTreeSet::new();
    counters.into_iter().filter(|c| seen.insert(*c)).collect()
}

// Each case opens encrypted databases, so fewer cases keep the suite quick
// while still covering orderings well beyond the hand-written tests.
proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn ingest_order_does_not_affect_state(
        counters in prop::collection::vec(1_u64..50, 1..30)
    ) {
        let n = node(1);
        let operations = unique(counters)
            .into_iter()
            .map(|c| make_add(n, c, format!("op{c}").as_bytes()))
            .collect::<Vec<_>>();

        let mut forward = Peer::new();
        for operation in &operations {
            forward.receive(std::slice::from_ref(operation));
        }
        let mut reverse = Peer::new();
        for operation in operations.iter().rev() {
            reverse.receive(std::slice::from_ref(operation));
        }

        prop_assert_eq!(forward.projection(), reverse.projection());
        prop_assert_eq!(forward.operations(), reverse.operations());
    }

    #[test]
    fn full_sync_always_converges_two_nodes(
        ops_a in prop::collection::vec(1_u64..20, 0..15),
        ops_b in prop::collection::vec(1_u64..20, 0..15),
    ) {
        let (na, nb) = (node(1), node(2));
        let mut a = Peer::new();
        let mut b = Peer::new();
        a.receive(
            &unique(ops_a)
                .into_iter()
                .map(|c| make_add(na, c, format!("a{c}").as_bytes()))
                .collect::<Vec<_>>(),
        );
        b.receive(
            &unique(ops_b)
                .into_iter()
                .map(|c| make_add(nb, c, format!("b{c}").as_bytes()))
                .collect::<Vec<_>>(),
        );

        full_sync(&mut a, &mut b);

        prop_assert_eq!(a.operations(), b.operations());
        prop_assert_eq!(a.projection(), b.projection());
    }

    #[test]
    fn pagination_always_delivers_everything(
        op_count in 1_u64..40,
        batch_size in 1_usize..10,
    ) {
        let n = node(1);
        let mut sender = Peer::new();
        sender.receive(
            &(1..=op_count)
                .map(|i| make_add(n, i, format!("p{i}").as_bytes()))
                .collect::<Vec<_>>(),
        );

        let mut receiver = Peer::new();
        let limits = BatchLimits {
            max_ops: batch_size,
            max_bytes: usize::MAX,
        };
        let mut rounds = 0;
        while sync_batch(&sender, &mut receiver, &limits) > 0 {
            rounds += 1;
            prop_assert!(rounds <= 100, "should converge");
        }

        prop_assert_eq!(receiver.operations().len(), op_count as usize);
        prop_assert_eq!(receiver.projection(), sender.projection());
    }
}
