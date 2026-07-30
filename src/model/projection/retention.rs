use std::collections::BTreeSet;

use super::super::{Acknowledgements, ContentId, NodeId, OpId, SeenOps};
use super::{Projection, TombstoneView};

impl Projection {
    #[must_use]
    pub fn tombstones(&self) -> Vec<TombstoneView> {
        self.content
            .iter()
            .filter_map(|(content_id, state)| {
                state.deletion.map(|deletion| TombstoneView {
                    content_id: *content_id,
                    deletion,
                    currently_deleted: !state.is_visible(),
                })
            })
            .collect()
    }

    /// Returns tombstones acknowledged by every known, non-forgotten member.
    ///
    /// A forget operation only removes its target from the required set after
    /// all other non-forgotten members have acknowledged that forget.
    #[must_use]
    pub fn collectable_tombstones(
        &self,
        local_node: NodeId,
        acknowledgements: &Acknowledgements,
    ) -> Vec<TombstoneView> {
        let active = self.active_members(local_node, acknowledgements);
        self.tombstones()
            .into_iter()
            .filter(|tombstone| {
                tombstone.currently_deleted
                    && active.iter().all(|member| {
                        self.member_has_seen(
                            *member,
                            tombstone.deletion.operation_id(),
                            local_node,
                            acknowledgements,
                        ) && (*member == local_node
                            || acknowledgements
                                .peer(*member)
                                .is_some_and(|frontier| frontier.is_subset_of(&self.seen)))
                    })
            })
            .collect()
    }

    /// Removes fully acknowledged, currently-deleted content records from
    /// projection state. Callers must delete the corresponding immutable
    /// operations in the same durable transaction.
    pub(crate) fn remove_compacted_tombstones(&mut self, content_ids: &BTreeSet<ContentId>) {
        self.content
            .retain(|content_id, _| !content_ids.contains(content_id));
    }

    pub(crate) fn merge_compacted_seen(&mut self, compacted_seen: &SeenOps) {
        self.seen.merge(compacted_seen);
    }

    #[must_use]
    pub fn active_known_members(
        &self,
        local_node: NodeId,
        acknowledgements: &Acknowledgements,
    ) -> BTreeSet<NodeId> {
        self.active_members(local_node, acknowledgements)
    }

    #[must_use]
    pub fn stably_forgotten_devices(
        &self,
        local_node: NodeId,
        acknowledgements: &Acknowledgements,
    ) -> BTreeSet<NodeId> {
        let active = self.active_members(local_node, acknowledgements);
        self.forgotten_devices
            .keys()
            .copied()
            .filter(|target| !active.contains(target))
            .collect()
    }

    fn active_members(
        &self,
        local_node: NodeId,
        acknowledgements: &Acknowledgements,
    ) -> BTreeSet<NodeId> {
        let mut members = self.known_members.clone();
        members.insert(local_node);
        members.extend(acknowledgements.known_members());
        let forget_targets = self
            .forgotten_devices
            .keys()
            .copied()
            .collect::<BTreeSet<_>>();
        let remaining = members
            .difference(&forget_targets)
            .copied()
            .collect::<BTreeSet<_>>();

        for (target, forget) in &self.forgotten_devices {
            let stable = remaining.iter().all(|member| {
                self.member_has_seen(*member, forget.operation_id(), local_node, acknowledgements)
            });
            if stable {
                members.remove(target);
            }
        }
        members
    }

    fn member_has_seen(
        &self,
        member: NodeId,
        operation: OpId,
        local_node: NodeId,
        acknowledgements: &Acknowledgements,
    ) -> bool {
        if member == local_node {
            self.seen.contains(operation)
        } else {
            acknowledgements.has_seen(member, operation)
        }
    }
}
