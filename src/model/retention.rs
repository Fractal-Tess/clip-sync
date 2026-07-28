use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{NodeId, OpId, SeenOps};

/// Persistable acknowledgement frontiers advertised by known peers.
///
/// Acknowledgements are local anti-entropy metadata rather than replicated
/// history operations. Recording a peer repeatedly performs a monotonic union,
/// so delayed frontier messages cannot move retention safety backwards.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Acknowledgements {
    peers: BTreeMap<NodeId, SeenOps>,
}

impl Acknowledgements {
    pub fn record(&mut self, peer: NodeId, seen: &SeenOps) {
        self.peers.entry(peer).or_default().merge(seen);
    }

    #[must_use]
    pub fn peer(&self, peer: NodeId) -> Option<&SeenOps> {
        self.peers.get(&peer)
    }

    #[must_use]
    pub fn has_seen(&self, peer: NodeId, operation: OpId) -> bool {
        self.peer(peer)
            .is_some_and(|frontier| frontier.contains(operation))
    }

    pub fn peers(&self) -> impl Iterator<Item = (NodeId, &SeenOps)> {
        self.peers.iter().map(|(node, seen)| (*node, seen))
    }
}
