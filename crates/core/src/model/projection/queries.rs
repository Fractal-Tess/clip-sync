use super::super::{ContentId, EventKey, NodeId, OpId, SeenOps};
use super::{ContentView, ItemKind, Projection, QuotaPlan};

impl Projection {
    #[must_use]
    pub const fn seen_ops(&self) -> &SeenOps {
        &self.seen
    }

    #[must_use]
    pub fn is_visible(&self, content_id: ContentId) -> bool {
        self.content
            .get(&content_id)
            .is_some_and(super::ContentState::is_visible)
    }

    #[must_use]
    pub fn is_pinned(&self, content_id: ContentId) -> bool {
        self.content
            .get(&content_id)
            .is_some_and(super::ContentState::is_pinned)
    }

    /// The item's description, or `None` while its add has not arrived.
    #[must_use]
    pub fn item(&self, content_id: ContentId) -> Option<&ItemKind> {
        self.content
            .get(&content_id)
            .and_then(|state| state.item.as_ref())
            .map(|item| &item.value)
    }

    /// The operation whose item won the content's register. Storage reads
    /// inline bytes from it, and its author is a reference's origin.
    #[must_use]
    pub fn item_operation(&self, content_id: ContentId) -> Option<OpId> {
        self.item_event(content_id).map(EventKey::operation_id)
    }

    /// Originating event for a retained content item.
    #[must_use]
    pub fn item_event(&self, content_id: ContentId) -> Option<EventKey> {
        self.content
            .get(&content_id)
            .and_then(|state| state.item.as_ref())
            .map(|item| item.event)
    }

    #[must_use]
    pub fn is_device_forgotten(&self, node_id: NodeId) -> bool {
        self.forgotten_devices.contains_key(&node_id)
    }

    pub fn known_members(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.known_members.iter().copied()
    }

    pub fn forgotten_devices(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.forgotten_devices.keys().copied()
    }

    /// Computes the oldest-first eviction set. Only inline items are charged:
    /// they are the ones every device stores. Pins are excluded, and so are
    /// references, whose bytes never leave their origin.
    ///
    /// An item whose add has not arrived yet (a touch or pin overtook it) has
    /// no known size and is left out until it does.
    #[must_use]
    pub fn quota_plan(&self, quota_bytes: u64) -> QuotaPlan {
        let mut chargeable_bytes = 0_u128;
        let mut excluded_bytes = 0_u128;
        let mut candidates = Vec::new();

        for (content_id, state) in &self.content {
            if !state.is_visible() {
                continue;
            }
            let Some(item) = state.item.as_ref() else {
                continue;
            };
            let size = u128::from(item.value.logical_size());
            if state.is_pinned() || matches!(item.value, ItemKind::Reference(_)) {
                excluded_bytes += size;
            } else {
                chargeable_bytes += size;
                if let Some(activity) = state.activity {
                    candidates.push((activity, *content_id, size));
                }
            }
        }

        candidates.sort_unstable_by(|left, right| {
            left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1))
        });
        let mut retained = chargeable_bytes;
        let mut evictions = Vec::new();
        for (_, content_id, size) in candidates {
            if retained <= u128::from(quota_bytes) {
                break;
            }
            retained -= size;
            evictions.push(content_id);
        }

        QuotaPlan {
            quota_bytes,
            chargeable_bytes,
            excluded_bytes,
            evictions,
        }
    }

    /// Visible entries in deterministic newest-first timeline order.
    #[must_use]
    pub fn visible_items(&self) -> Vec<ContentView<'_>> {
        let mut visible = self
            .content
            .iter()
            .filter(|(_, state)| state.is_visible())
            .filter_map(|(content_id, state)| {
                Some(ContentView {
                    content_id: *content_id,
                    last_activity: state.activity?,
                    pinned: state.is_pinned(),
                    item: state.item.as_ref(),
                })
            })
            .collect::<Vec<_>>();
        visible.sort_unstable_by(|left, right| {
            right
                .last_activity
                .cmp(&left.last_activity)
                .then_with(|| left.content_id.cmp(&right.content_id))
        });
        visible
    }
}
