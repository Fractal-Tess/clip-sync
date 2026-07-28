use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{
    Acknowledgements, ContentId, EffectiveSharedSettings, EventKey, NodeId, Operation, Payload,
    SeenOps, SettingValue, SharedSetting, StampedOperation,
};

const MAX_SETTING_KEY_BYTES: usize = 128;
const MAX_SETTING_TEXT_BYTES: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Register<T> {
    event: EventKey,
    value: T,
}

impl<T> Register<T> {
    fn replace_if_newer(&mut self, event: EventKey, value: T) {
        if event > self.event {
            *self = Self { event, value };
        }
    }
}

fn write_register<T>(register: &mut Option<Register<T>>, event: EventKey, value: T) {
    match register {
        Some(current) => current.replace_if_newer(event, value),
        None => *register = Some(Register { event, value }),
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ContentState {
    activity: Option<EventKey>,
    deletion: Option<EventKey>,
    pin: Option<Register<bool>>,
    quota_exempt: Option<Register<bool>>,
    payload: Option<Register<Payload>>,
}

impl ContentState {
    fn new() -> Self {
        Self {
            activity: None,
            deletion: None,
            pin: None,
            quota_exempt: None,
            payload: None,
        }
    }

    fn is_visible(&self) -> bool {
        self.activity
            .is_some_and(|activity| self.deletion.is_none_or(|deletion| activity > deletion))
    }

    fn is_pinned(&self) -> bool {
        if !self.is_visible() {
            return false;
        }

        self.pin.as_ref().is_some_and(|pin| {
            pin.value && self.deletion.is_none_or(|deletion| pin.event > deletion)
        })
    }

    fn is_quota_exempt(&self) -> bool {
        if !self.is_visible() {
            return false;
        }

        self.quota_exempt.as_ref().is_some_and(|exempt| {
            exempt.value && self.deletion.is_none_or(|deletion| exempt.event > deletion)
        })
    }
}

/// Read-only visible history entry. Payload bytes are accessible explicitly,
/// while its Debug representation remains redacted by `Payload`.
#[derive(Clone, Copy, Debug)]
pub struct ContentView<'a> {
    content_id: ContentId,
    last_activity: EventKey,
    pinned: bool,
    quota_exempt: bool,
    payload: Option<&'a Payload>,
}

impl<'a> ContentView<'a> {
    #[must_use]
    pub const fn content_id(self) -> ContentId {
        self.content_id
    }

    #[must_use]
    pub const fn last_activity(self) -> EventKey {
        self.last_activity
    }

    #[must_use]
    pub const fn pinned(self) -> bool {
        self.pinned
    }

    #[must_use]
    pub const fn quota_exempt(self) -> bool {
        self.quota_exempt
    }

    #[must_use]
    pub const fn payload(self) -> Option<&'a Payload> {
        self.payload
    }
}

/// Deterministic quota evaluation over the currently visible projection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuotaPlan {
    quota_bytes: u64,
    chargeable_bytes: u128,
    excluded_bytes: u128,
    missing_payloads: Vec<ContentId>,
    evictions: Vec<ContentId>,
}

impl QuotaPlan {
    #[must_use]
    pub const fn quota_bytes(&self) -> u64 {
        self.quota_bytes
    }

    #[must_use]
    pub const fn chargeable_bytes(&self) -> u128 {
        self.chargeable_bytes
    }

    #[must_use]
    pub const fn excluded_bytes(&self) -> u128 {
        self.excluded_bytes
    }

    #[must_use]
    pub fn missing_payloads(&self) -> &[ContentId] {
        &self.missing_payloads
    }

    #[must_use]
    pub fn evictions(&self) -> &[ContentId] {
        &self.evictions
    }

    #[must_use]
    pub fn is_satisfied(&self) -> bool {
        self.evictions.is_empty() && self.chargeable_bytes <= u128::from(self.quota_bytes)
    }
}

/// A retained deletion marker and the exact operation peers must acknowledge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TombstoneView {
    content_id: ContentId,
    deletion: EventKey,
    currently_deleted: bool,
}

impl TombstoneView {
    #[must_use]
    pub const fn content_id(self) -> ContentId {
        self.content_id
    }

    #[must_use]
    pub const fn deletion(self) -> EventKey {
        self.deletion
    }

    #[must_use]
    pub const fn currently_deleted(self) -> bool {
        self.currently_deleted
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied,
    Duplicate,
}

/// Materialized deterministic state derived from immutable operations.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Projection {
    seen: SeenOps,
    content: BTreeMap<ContentId, ContentState>,
    settings: BTreeMap<String, Register<SettingValue>>,
    forgotten_devices: BTreeMap<NodeId, EventKey>,
    known_members: BTreeSet<NodeId>,
}

impl Projection {
    pub(crate) fn validate_operation(stamped: &StampedOperation) -> Result<(), ProjectionError> {
        match stamped.operation() {
            Operation::Add {
                content_id,
                payload,
            }
            | Operation::AddQuotaExempt {
                content_id,
                payload,
            } => {
                payload.validate_structure()?;
                if *content_id != payload.descriptor().content_id() {
                    return Err(ProjectionError::PayloadContentIdMismatch {
                        operation: *content_id,
                        payload: payload.descriptor().content_id(),
                    });
                }
            }
            Operation::SetSetting { key, value } => validate_setting(key, value)?,
            _ => {}
        }
        Ok(())
    }

    /// Applies one immutable operation or identifies an exact replay.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectionError::PayloadContentIdMismatch`] when an `Add`
    /// operation names a different ID than its payload descriptor.
    pub fn apply(&mut self, stamped: &StampedOperation) -> Result<ApplyOutcome, ProjectionError> {
        Self::validate_operation(stamped)?;

        if !self.seen.record(stamped.id()) {
            return Ok(ApplyOutcome::Duplicate);
        }

        let event = stamped.event_key();
        self.known_members.insert(stamped.id().node());
        match stamped.operation() {
            Operation::Add {
                content_id,
                payload,
            } => {
                let state = self
                    .content
                    .entry(*content_id)
                    .or_insert_with(ContentState::new);
                state.activity = Some(state.activity.map_or(event, |current| current.max(event)));
                write_register(&mut state.payload, event, payload.clone());
                write_register(&mut state.quota_exempt, event, false);
            }
            Operation::AddQuotaExempt {
                content_id,
                payload,
            } => {
                let state = self
                    .content
                    .entry(*content_id)
                    .or_insert_with(ContentState::new);
                state.activity = Some(state.activity.map_or(event, |current| current.max(event)));
                write_register(&mut state.payload, event, payload.clone());
                write_register(&mut state.quota_exempt, event, true);
            }
            Operation::Touch { content_id } => {
                let state = self
                    .content
                    .entry(*content_id)
                    .or_insert_with(ContentState::new);
                state.activity = Some(state.activity.map_or(event, |current| current.max(event)));
            }
            Operation::Delete { content_id } => {
                let state = self
                    .content
                    .entry(*content_id)
                    .or_insert_with(ContentState::new);
                state.deletion = Some(state.deletion.map_or(event, |current| current.max(event)));
            }
            Operation::SetPin { content_id, pinned } => {
                let state = self
                    .content
                    .entry(*content_id)
                    .or_insert_with(ContentState::new);
                write_register(&mut state.pin, event, *pinned);
            }
            Operation::SetSetting { key, value } => {
                self.settings
                    .entry(key.clone())
                    .and_modify(|current| current.replace_if_newer(event, value.clone()))
                    .or_insert_with(|| Register {
                        event,
                        value: value.clone(),
                    });
            }
            Operation::ForgetDevice { node_id } => {
                self.forgotten_devices
                    .entry(*node_id)
                    .and_modify(|current| *current = (*current).max(event))
                    .or_insert(event);
            }
        }

        Ok(ApplyOutcome::Applied)
    }

    /// Applies a sequence using the same validation as [`Self::apply`].
    ///
    /// # Errors
    ///
    /// Returns the first projection validation error.
    pub fn apply_all<'a>(
        &mut self,
        operations: impl IntoIterator<Item = &'a StampedOperation>,
    ) -> Result<(), ProjectionError> {
        for operation in operations {
            self.apply(operation)?;
        }
        Ok(())
    }

    #[must_use]
    pub const fn seen_ops(&self) -> &SeenOps {
        &self.seen
    }

    pub fn is_visible(&self, content_id: ContentId) -> bool {
        self.content
            .get(&content_id)
            .is_some_and(ContentState::is_visible)
    }

    pub fn is_pinned(&self, content_id: ContentId) -> bool {
        self.content
            .get(&content_id)
            .is_some_and(ContentState::is_pinned)
    }

    pub fn is_quota_exempt(&self, content_id: ContentId) -> bool {
        self.content
            .get(&content_id)
            .is_some_and(ContentState::is_quota_exempt)
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
    pub fn setting_event(&self, key: &str) -> Option<EventKey> {
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
            if !state.is_visible() {
                continue;
            }
            let Some(payload) = state.payload.as_ref().map(|payload| &payload.value) else {
                missing_payloads.push(*content_id);
                continue;
            };
            let size = u128::from(payload.descriptor().logical_size());
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
                active.iter().all(|member| {
                    self.member_has_seen(
                        *member,
                        tombstone.deletion.operation_id(),
                        local_node,
                        acknowledgements,
                    )
                })
            })
            .collect()
    }

    /// Visible entries in deterministic newest-first timeline order.
    #[must_use]
    pub fn visible_items(&self) -> Vec<ContentView<'_>> {
        let mut visible = self
            .content
            .iter()
            .filter_map(|(content_id, state)| {
                if !state.is_visible() {
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

    fn active_members(
        &self,
        local_node: NodeId,
        acknowledgements: &Acknowledgements,
    ) -> BTreeSet<NodeId> {
        let mut members = self.known_members.clone();
        members.insert(local_node);
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
        operation: super::OpId,
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

impl fmt::Debug for Projection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Projection")
            .field("seen", &self.seen)
            .field("content_records", &self.content.len())
            .field("settings", &self.settings)
            .field("forgotten_devices", &self.forgotten_devices)
            .field("known_members", &self.known_members)
            .finish()
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ProjectionError {
    #[error("payload structure is invalid: {0}")]
    InvalidPayload(#[from] super::ContentError),
    #[error("Add operation content ID {operation} does not match payload ID {payload}")]
    PayloadContentIdMismatch {
        operation: ContentId,
        payload: ContentId,
    },
    #[error("shared setting key must not be empty")]
    EmptySettingKey,
    #[error("shared setting key is malformed or exceeds 128 bytes")]
    InvalidSettingKey,
    #[error("shared setting text exceeds 4096 bytes")]
    SettingTextTooLong,
    #[error("shared setting {key:?} requires a positive unsigned integer")]
    InvalidKnownSetting { key: String },
}

fn validate_setting(key: &str, value: &SettingValue) -> Result<(), ProjectionError> {
    if key.is_empty() {
        return Err(ProjectionError::EmptySettingKey);
    }
    if key.len() > MAX_SETTING_KEY_BYTES
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(ProjectionError::InvalidSettingKey);
    }
    if matches!(value, SettingValue::Text(text) if text.len() > MAX_SETTING_TEXT_BYTES) {
        return Err(ProjectionError::SettingTextTooLong);
    }
    if SharedSetting::from_key(key).is_some()
        && !matches!(value, SettingValue::Unsigned(value) if *value > 0)
    {
        return Err(ProjectionError::InvalidKnownSetting {
            key: key.to_owned(),
        });
    }
    Ok(())
}
