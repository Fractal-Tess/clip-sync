use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    payload::{ManifestId, StoredManifest},
    transfer::{TransferId, TransferPhase},
};

use super::{ContentId, EventKey, NodeId, Payload, SeenOps, SettingValue, SharedSetting};

mod apply;
mod queries;
mod retention;

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

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TransferMetadata {
    content_id: ContentId,
    manifest_id: ManifestId,
    manifest: StoredManifest,
    quota_exempt: bool,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TransferTerminal {
    content_id: ContentId,
    manifest_id: ManifestId,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TransferProjectionState {
    begin: Option<Register<TransferMetadata>>,
    complete: Option<Register<TransferTerminal>>,
    cancelled: Option<Register<TransferTerminal>>,
}

impl TransferProjectionState {
    fn new() -> Self {
        Self {
            begin: None,
            complete: None,
            cancelled: None,
        }
    }

    fn phase(&self) -> TransferPhase {
        if self.terminal_matches(self.cancelled.as_ref()) {
            TransferPhase::Cancelled
        } else if self.terminal_matches(self.complete.as_ref()) {
            TransferPhase::Complete
        } else {
            TransferPhase::Pending
        }
    }

    fn terminal_matches(&self, terminal: Option<&Register<TransferTerminal>>) -> bool {
        self.begin
            .as_ref()
            .zip(terminal)
            .is_some_and(|(begin, terminal)| {
                begin.value.content_id == terminal.value.content_id
                    && begin.value.manifest_id == terminal.value.manifest_id
            })
    }

    fn activity(&self) -> Option<EventKey> {
        let begin = self.begin.as_ref()?;
        let completion = self
            .complete
            .as_ref()
            .filter(|complete| self.terminal_matches(Some(complete)))
            .map_or(begin.event, |complete| complete.event);
        Some(begin.event.max(completion))
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

/// Read-only replicated transfer transaction state.
#[derive(Clone, Copy, Debug)]
pub struct TransferView<'a> {
    transfer_id: TransferId,
    source_node: Option<NodeId>,
    content_id: Option<ContentId>,
    manifest_id: Option<ManifestId>,
    manifest: Option<&'a StoredManifest>,
    phase: TransferPhase,
    quota_exempt: bool,
}

impl<'a> TransferView<'a> {
    #[must_use]
    pub const fn transfer_id(self) -> TransferId {
        self.transfer_id
    }

    #[must_use]
    pub const fn source_node(self) -> Option<NodeId> {
        self.source_node
    }

    #[must_use]
    pub const fn content_id(self) -> Option<ContentId> {
        self.content_id
    }

    #[must_use]
    pub const fn manifest_id(self) -> Option<ManifestId> {
        self.manifest_id
    }

    #[must_use]
    pub const fn manifest(self) -> Option<&'a StoredManifest> {
        self.manifest
    }

    #[must_use]
    pub const fn phase(self) -> TransferPhase {
        self.phase
    }

    #[must_use]
    pub const fn quota_exempt(self) -> bool {
        self.quota_exempt
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
    transfers: BTreeMap<TransferId, TransferProjectionState>,
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
            .field("transfers", &self.transfers.len())
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

fn transfer_view(transfer_id: TransferId, state: &TransferProjectionState) -> TransferView<'_> {
    let metadata = state.begin.as_ref().map(|begin| &begin.value);
    TransferView {
        transfer_id,
        source_node: state
            .begin
            .as_ref()
            .map(|begin| begin.event.operation_id().node()),
        content_id: metadata.map(|metadata| metadata.content_id),
        manifest_id: metadata.map(|metadata| metadata.manifest_id),
        manifest: metadata.map(|metadata| &metadata.manifest),
        phase: state.phase(),
        quota_exempt: metadata.is_some_and(|metadata| metadata.quota_exempt),
    }
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
