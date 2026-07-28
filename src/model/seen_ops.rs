use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Deserializer, Serialize, de};

const MAX_FRONTIER_NODES: usize = 4096;
const MAX_SPARSE_COUNTERS: usize = 65_536;

use super::{NodeId, OpId};

/// Operations observed for one node: every counter at or below `frontier` is
/// present, plus the sparse out-of-order counters in `gaps`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
struct NodeSeen {
    frontier: u64,
    gaps: BTreeSet<u64>,
}

impl NodeSeen {
    fn record(&mut self, counter: u64) -> bool {
        if counter <= self.frontier {
            return false;
        }
        if !self.gaps.insert(counter) {
            return false;
        }
        self.advance_frontier();
        true
    }

    fn advance_frontier(&mut self) {
        while let Some(next) = self.frontier.checked_add(1) {
            if !self.gaps.remove(&next) {
                break;
            }
            self.frontier = next;
        }
    }

    fn merge(&mut self, other: &Self) {
        if other.frontier > self.frontier {
            self.frontier = other.frontier;
            self.gaps.retain(|counter| *counter > self.frontier);
        }
        self.gaps.extend(
            other
                .gaps
                .iter()
                .copied()
                .filter(|counter| *counter > self.frontier),
        );
        self.advance_frontier();
    }
}

/// Compact duplicate detector and anti-entropy frontier. `gaps` names the
/// sparse set above the contiguous frontier (operations received across gaps),
/// not the missing counters themselves.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SeenOps {
    nodes: BTreeMap<NodeId, NodeSeen>,
}

impl<'de> Deserialize<'de> for SeenOps {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireNodeSeen {
            frontier: u64,
            gaps: Vec<u64>,
        }

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireSeenOps {
            nodes: BTreeMap<NodeId, WireNodeSeen>,
        }

        let wire = WireSeenOps::deserialize(deserializer)?;
        if wire.nodes.len() > MAX_FRONTIER_NODES {
            return Err(de::Error::custom("frontier contains too many nodes"));
        }

        let mut sparse_count = 0_usize;
        let mut nodes = BTreeMap::new();
        for (node, seen) in wire.nodes {
            let wire_gap_count = seen.gaps.len();
            sparse_count = sparse_count
                .checked_add(wire_gap_count)
                .ok_or_else(|| de::Error::custom("frontier sparse counter count overflowed"))?;
            if sparse_count > MAX_SPARSE_COUNTERS {
                return Err(de::Error::custom(
                    "frontier contains too many sparse counters",
                ));
            }

            let gaps = seen.gaps.into_iter().collect::<BTreeSet<_>>();
            if gaps.len() != wire_gap_count {
                return Err(de::Error::custom(
                    "frontier contains duplicate sparse counters",
                ));
            }
            if gaps.iter().any(|counter| *counter <= seen.frontier) {
                return Err(de::Error::custom(
                    "frontier sparse counters must be above its contiguous frontier",
                ));
            }
            nodes.insert(
                node,
                NodeSeen {
                    frontier: seen.frontier,
                    gaps,
                },
            );
        }
        Ok(Self { nodes })
    }
}

impl SeenOps {
    /// Returns true only when this operation was not already represented.
    pub fn record(&mut self, operation_id: OpId) -> bool {
        self.nodes
            .entry(operation_id.node())
            .or_default()
            .record(operation_id.counter())
    }

    #[must_use]
    pub fn contains(&self, operation_id: OpId) -> bool {
        self.nodes.get(&operation_id.node()).is_some_and(|seen| {
            operation_id.counter() <= seen.frontier || seen.gaps.contains(&operation_id.counter())
        })
    }

    #[must_use]
    pub fn frontier(&self, node: NodeId) -> u64 {
        self.nodes.get(&node).map_or(0, |seen| seen.frontier)
    }

    pub fn gaps(&self, node: NodeId) -> impl Iterator<Item = u64> + '_ {
        self.nodes
            .get(&node)
            .into_iter()
            .flat_map(|seen| seen.gaps.iter().copied())
    }

    pub fn known_nodes(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.nodes.keys().copied()
    }

    /// Set-union merge used when persisted summaries are combined.
    pub fn merge(&mut self, other: &Self) {
        for (node, other_seen) in &other.nodes {
            self.nodes.entry(*node).or_default().merge(other_seen);
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use uuid::Uuid;

    use super::*;

    fn node() -> NodeId {
        NodeId::from_uuid(Uuid::from_u128(1))
    }

    fn operation(counter: u64) -> OpId {
        OpId::new(node(), counter).unwrap()
    }

    #[test]
    fn out_of_order_delivery_closes_frontier_gaps() {
        let mut seen = SeenOps::default();
        assert!(seen.record(operation(3)));
        assert_eq!(seen.frontier(node()), 0);
        assert_eq!(seen.gaps(node()).collect::<Vec<_>>(), vec![3]);

        assert!(seen.record(operation(1)));
        assert_eq!(seen.frontier(node()), 1);
        assert!(seen.record(operation(2)));
        assert_eq!(seen.frontier(node()), 3);
        assert!(seen.gaps(node()).next().is_none());
    }

    #[test]
    fn duplicate_detection_covers_frontier_and_sparse_entries() {
        let mut seen = SeenOps::default();
        assert!(seen.record(operation(2)));
        assert!(!seen.record(operation(2)));
        assert!(seen.record(operation(1)));
        assert!(!seen.record(operation(1)));
        assert!(!seen.record(operation(2)));
    }

    #[test]
    fn deserialization_rejects_noncanonical_sparse_counters() {
        let id = node();
        let below_frontier = format!(r#"{{"nodes":{{"{id}":{{"frontier":2,"gaps":[2]}}}}}}"#);
        assert!(serde_json::from_str::<SeenOps>(&below_frontier).is_err());

        let duplicate = format!(r#"{{"nodes":{{"{id}":{{"frontier":0,"gaps":[2,2]}}}}}}"#);
        assert!(serde_json::from_str::<SeenOps>(&duplicate).is_err());
    }

    proptest! {
        #[test]
        fn delivery_order_does_not_change_summary(
            counters in prop::collection::vec(1_u64..200, 0..500)
        ) {
            let mut forward = SeenOps::default();
            for counter in &counters {
                forward.record(operation(*counter));
            }

            let mut reverse = SeenOps::default();
            for counter in counters.iter().rev() {
                reverse.record(operation(*counter));
            }

            prop_assert_eq!(forward, reverse);
        }

        #[test]
        fn merge_is_commutative(
            left in prop::collection::vec(1_u64..200, 0..300),
            right in prop::collection::vec(1_u64..200, 0..300),
        ) {
            let mut left_summary = SeenOps::default();
            for counter in left {
                left_summary.record(operation(counter));
            }
            let mut right_summary = SeenOps::default();
            for counter in right {
                right_summary.record(operation(counter));
            }

            let mut left_then_right = left_summary.clone();
            left_then_right.merge(&right_summary);
            let mut right_then_left = right_summary;
            right_then_left.merge(&left_summary);
            prop_assert_eq!(left_then_right, right_then_left);
        }
    }
}
