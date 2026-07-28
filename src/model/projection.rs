use std::{collections::BTreeMap, fmt};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{
    ContentId, EventKey, NodeId, Operation, Payload, SeenOps, SettingValue, StampedOperation,
};

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
    payload: Option<Register<Payload>>,
}

impl ContentState {
    fn new() -> Self {
        Self {
            activity: None,
            deletion: None,
            pin: None,
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
}

/// Read-only visible history entry. Payload bytes are accessible explicitly,
/// while its Debug representation remains redacted by `Payload`.
#[derive(Clone, Copy, Debug)]
pub struct ContentView<'a> {
    content_id: ContentId,
    last_activity: EventKey,
    pinned: bool,
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
    pub const fn payload(self) -> Option<&'a Payload> {
        self.payload
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
}

impl Projection {
    /// Applies one immutable operation or identifies an exact replay.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectionError::PayloadContentIdMismatch`] when an `Add`
    /// operation names a different ID than its payload descriptor.
    pub fn apply(&mut self, stamped: &StampedOperation) -> Result<ApplyOutcome, ProjectionError> {
        if let Operation::Add {
            content_id,
            payload,
        } = stamped.operation()
            && *content_id != payload.descriptor().content_id()
        {
            return Err(ProjectionError::PayloadContentIdMismatch {
                operation: *content_id,
                payload: payload.descriptor().content_id(),
            });
        }

        if !self.seen.record(stamped.id()) {
            return Ok(ApplyOutcome::Duplicate);
        }

        let event = stamped.event_key();
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
    pub fn is_device_forgotten(&self, node_id: NodeId) -> bool {
        self.forgotten_devices.contains_key(&node_id)
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
}

impl fmt::Debug for Projection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Projection")
            .field("seen", &self.seen)
            .field("content_records", &self.content.len())
            .field("settings", &self.settings)
            .field("forgotten_devices", &self.forgotten_devices)
            .finish()
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ProjectionError {
    #[error("Add operation content ID {operation} does not match payload ID {payload}")]
    PayloadContentIdMismatch {
        operation: ContentId,
        payload: ContentId,
    },
}
