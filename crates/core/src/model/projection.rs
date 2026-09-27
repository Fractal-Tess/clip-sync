use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use thiserror::Error;

use super::{
    ContentId, EventKey, NodeId, Payload, PayloadDescriptor, Reference, ReferenceError, SeenOps,
};

mod apply;
mod queries;
mod retention;

#[derive(Clone, Debug, PartialEq, Eq)]
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

#[derive(Clone, PartialEq, Eq)]
struct ContentState {
    activity: Option<EventKey>,
    deletion: Option<EventKey>,
    pin: Option<Register<bool>>,
    item: Option<Register<ItemKind>>,
}

impl ContentState {
    const fn new() -> Self {
        Self {
            activity: None,
            deletion: None,
            pin: None,
            item: None,
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
}

/// What the projection retains of a history item. Inline bytes stay in the
/// operation log that carried them and are loaded only when the item is
/// activated or previewed, so memory does not grow with retained history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ItemKind {
    Inline {
        descriptor: PayloadDescriptor,
        /// Clipboard text excerpt; it is content, so it must never be logged.
        text_preview: Option<String>,
    },
    /// Bytes stay on the origin device, the author of the winning operation.
    Reference(Reference),
}

impl ItemKind {
    fn inline(payload: &Payload) -> Self {
        Self::Inline {
            descriptor: payload.descriptor().clone(),
            text_preview: payload.text_preview(),
        }
    }

    #[must_use]
    pub fn logical_size(&self) -> u64 {
        match self {
            Self::Inline { descriptor, .. } => descriptor.logical_size(),
            Self::Reference(reference) => reference.logical_size(),
        }
    }

    #[must_use]
    pub fn mime_types(&self) -> Vec<String> {
        match self {
            Self::Inline { descriptor, .. } => descriptor
                .representations()
                .iter()
                .map(|representation| representation.mime().to_owned())
                .collect(),
            Self::Reference(reference) => reference.mime_types(),
        }
    }
}

/// Read-only visible history entry.
#[derive(Clone, Copy)]
pub struct ContentView<'a> {
    content_id: ContentId,
    last_activity: EventKey,
    pinned: bool,
    item: Option<&'a Register<ItemKind>>,
}

impl fmt::Debug for ContentView<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ContentView")
            .field("content_id", &self.content_id)
            .field("last_activity", &self.last_activity)
            .field("pinned", &self.pinned)
            .finish_non_exhaustive()
    }
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

    /// `None` while a touch or pin has arrived ahead of the item's add.
    #[must_use]
    pub fn item(self) -> Option<&'a ItemKind> {
        self.item.map(|item| &item.value)
    }

    /// The operation that introduced the item: when and by which device.
    #[must_use]
    pub fn origin(self) -> Option<EventKey> {
        self.item.map(|item| item.event)
    }
}

/// Deterministic quota evaluation over the currently visible projection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuotaPlan {
    quota_bytes: u64,
    chargeable_bytes: u128,
    excluded_bytes: u128,
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
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Projection {
    seen: SeenOps,
    content: BTreeMap<ContentId, ContentState>,
    forgotten_devices: BTreeMap<NodeId, EventKey>,
    known_members: BTreeSet<NodeId>,
}

impl fmt::Debug for Projection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Projection")
            .field("seen", &self.seen)
            .field("content_records", &self.content.len())
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
    #[error("reference is invalid: {0}")]
    InvalidReference(#[from] ReferenceError),
}
