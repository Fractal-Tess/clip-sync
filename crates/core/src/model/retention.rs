use std::collections::{BTreeMap, BTreeSet};

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
    known_members: BTreeSet<NodeId>,
}

impl Acknowledgements {
    pub fn record(&mut self, peer: NodeId, seen: &SeenOps) {
        self.known_members.insert(peer);
        self.peers.entry(peer).or_default().merge(seen);
    }

    pub fn record_known(&mut self, member: NodeId) {
        self.known_members.insert(member);
    }

    pub fn remove_peer(&mut self, peer: NodeId) {
        self.peers.remove(&peer);
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

    pub fn known_members(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.known_members.iter().copied()
    }
}
