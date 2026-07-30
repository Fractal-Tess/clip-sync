use crate::{
    payload::{ManifestId, StoredManifest},
    transfer::{TransferId, TransferPhase},
};

use super::super::{
    ContentId, EffectiveSharedSettings, NodeId, Payload, SeenOps, SettingValue, SharedSetting,
};
use super::{ContentState, ContentView, Projection, QuotaPlan, TransferView, transfer_view};

impl Projection {
    #[must_use]
    pub const fn seen_ops(&self) -> &SeenOps {
        &self.seen
    }

    #[must_use]
    pub fn is_visible(&self, content_id: ContentId) -> bool {
        self.content
            .get(&content_id)
            .is_some_and(|state| self.content_is_visible(content_id, state))
    }

    #[must_use]
    pub fn is_pinned(&self, content_id: ContentId) -> bool {
        self.content
            .get(&content_id)
            .is_some_and(|state| self.content_is_visible(content_id, state) && state.is_pinned())
    }

    #[must_use]
    pub fn is_quota_exempt(&self, content_id: ContentId) -> bool {
        self.content.get(&content_id).is_some_and(|state| {
            self.content_is_visible(content_id, state) && state.is_quota_exempt()
        })
    }

    #[must_use]
    pub fn payload(&self, content_id: ContentId) -> Option<&Payload> {
        self.content
            .get(&content_id)
            .and_then(|state| state.payload.as_ref())
            .map(|payload| &payload.value)
    }

    #[must_use]
    pub fn setting(&self, key: &str) -> Option<&SettingValue> {
        self.settings.get(key).map(|setting| &setting.value)
    }

    #[must_use]
    pub fn setting_event(&self, key: &str) -> Option<super::super::EventKey> {
        self.settings.get(key).map(|setting| setting.event)
    }

    #[must_use]
    pub fn effective_shared_settings(&self) -> EffectiveSharedSettings {
        let mut effective = EffectiveSharedSettings::default();
        if let Some(SettingValue::Unsigned(value)) =
            self.setting(SharedSetting::MeshQuotaBytes.key())
        {
            effective.mesh_quota_bytes = *value;
        }
        if let Some(SettingValue::Unsigned(value)) =
            self.setting(SharedSetting::CaptureThresholdBytes.key())
        {
            effective.capture_threshold_bytes = *value;
        }
        effective
    }

    #[must_use]
    pub fn is_device_forgotten(&self, node_id: NodeId) -> bool {
        self.forgotten_devices.contains_key(&node_id)
    }

    pub fn known_members(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.known_members.iter().copied()
    }

    #[must_use]
    pub fn transfer(&self, transfer_id: TransferId) -> Option<TransferView<'_>> {
        self.transfers
            .get(&transfer_id)
            .map(|state| transfer_view(transfer_id, state))
    }

    #[must_use]
    pub fn transfers(&self) -> Vec<TransferView<'_>> {
        self.transfers
            .iter()
            .map(|(id, state)| transfer_view(*id, state))
            .collect()
    }

    #[must_use]
    pub fn completed_manifest_for_content(
        &self,
        content_id: ContentId,
    ) -> Option<(TransferId, ManifestId, &StoredManifest)> {
        self.transfers
            .iter()
            .filter_map(|(id, state)| {
                if state.phase() != TransferPhase::Complete {
                    return None;
                }
                let metadata = &state.begin.as_ref()?.value;
                let activity = state.activity()?;
                (metadata.content_id == content_id).then_some((
                    activity,
                    *id,
                    metadata.manifest_id,
                    &metadata.manifest,
                ))
            })
            .max_by_key(|(activity, id, _, _)| (*activity, *id))
            .map(|(_, id, manifest_id, manifest)| (id, manifest_id, manifest))
    }

    #[must_use]
    pub fn manifest_for_content(
        &self,
        content_id: ContentId,
    ) -> Option<(TransferId, ManifestId, &StoredManifest)> {
        self.transfers
            .iter()
            .filter_map(|(id, state)| {
                if state.phase() == TransferPhase::Cancelled {
                    return None;
                }
                let metadata = &state.begin.as_ref()?.value;
                let activity = state.activity()?;
                (metadata.content_id == content_id).then_some((
                    activity,
                    *id,
                    metadata.manifest_id,
                    &metadata.manifest,
                ))
            })
            .max_by_key(|(activity, id, _, _)| (*activity, *id))
            .map(|(_, id, manifest_id, manifest)| (id, manifest_id, manifest))
    }

    pub fn forgotten_devices(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.forgotten_devices.keys().copied()
    }

    /// Stable, non-secret revision derived from the winning shared-setting
    /// registers. It is written to TOML so a daemon can recognize its own
    /// atomic replacement after a restart or config-watch notification.
    #[must_use]
    pub fn shared_settings_revision(&self) -> String {
        let mut hash = blake3::Hasher::new();
        hash.update(b"clip-sync/shared-settings-revision/v1");
        for setting in [
            SharedSetting::MeshQuotaBytes,
            SharedSetting::CaptureThresholdBytes,
        ] {
            hash.update(setting.key().as_bytes());
            if let Some(event) = self.setting_event(setting.key()) {
                hash.update(&event.timestamp().physical_millis().to_be_bytes());
                hash.update(&event.timestamp().logical().to_be_bytes());
                hash.update(event.operation_id().node().as_uuid().as_bytes());
                hash.update(&event.operation_id().counter().to_be_bytes());
            } else {
                hash.update(&[0; 36]);
            }
        }
        hash.finalize().to_hex().to_string()
    }

    /// Computes the oldest-first eviction set from only quota-chargeable
    /// entries. Pins and explicit oversized shares are excluded from both the
    /// usage total and the candidate set.
    #[must_use]
    pub fn quota_plan(&self, quota_bytes: u64) -> QuotaPlan {
        let mut chargeable_bytes = 0_u128;
        let mut excluded_bytes = 0_u128;
        let mut missing_payloads = Vec::new();
        let mut candidates = Vec::new();

        for (content_id, state) in &self.content {
            if !self.content_is_visible(*content_id, state) {
                continue;
            }
            let size = if let Some(payload) = state.payload.as_ref().map(|payload| &payload.value) {
                u128::from(payload.descriptor().logical_size())
            } else if let Some((_, _, manifest)) = self.manifest_for_content(*content_id) {
                u128::from(manifest.logical_size())
            } else {
                missing_payloads.push(*content_id);
                continue;
            };
            if state.is_pinned() || state.is_quota_exempt() {
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
            missing_payloads,
            evictions,
        }
    }

    #[must_use]
    pub fn effective_quota_plan(&self) -> QuotaPlan {
        self.quota_plan(self.effective_shared_settings().mesh_quota_bytes)
    }

    /// Visible entries in deterministic newest-first timeline order.
    #[must_use]
    pub fn visible_items(&self) -> Vec<ContentView<'_>> {
        let mut visible = self
            .content
            .iter()
            .filter_map(|(content_id, state)| {
                if !self.content_is_visible(*content_id, state) {
                    return None;
                }

                Some(ContentView {
                    content_id: *content_id,
                    last_activity: state.activity?,
                    pinned: state.is_pinned(),
                    quota_exempt: state.is_quota_exempt(),
                    payload: state.payload.as_ref().map(|payload| &payload.value),
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

    fn content_is_visible(&self, content_id: ContentId, state: &ContentState) -> bool {
        if !state.is_visible() {
            return false;
        }
        if state.payload.is_some() {
            return true;
        }
        let mut has_transfer = false;
        let has_live_transfer = self.transfers.values().any(|transfer| {
            let matches_content = transfer
                .begin
                .as_ref()
                .is_some_and(|begin| begin.value.content_id == content_id);
            has_transfer |= matches_content;
            matches_content && transfer.phase() != TransferPhase::Cancelled
        });
        !has_transfer || has_live_transfer
    }
}
