use serde::{Deserialize, Serialize};

use super::{ContentId, EventKey, HlcTimestamp, NodeId, OpId, Payload};

/// Replicated shared-setting value. Text is intended for non-secret policy
/// values only; local paths, keys, and other bootstrap settings are not shared.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum SettingValue {
    Bool(bool),
    Integer(i64),
    Unsigned(u64),
    Text(String),
}

/// Immutable operation body. There is deliberately no operation that embeds
/// an active local clipboard action: remote replication only changes history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Operation {
    Add {
        content_id: ContentId,
        payload: Payload,
    },
    Touch {
        content_id: ContentId,
    },
    Delete {
        content_id: ContentId,
    },
    SetPin {
        content_id: ContentId,
        pinned: bool,
    },
    SetSetting {
        key: String,
        value: SettingValue,
    },
    ForgetDevice {
        node_id: NodeId,
    },
}

impl Operation {
    #[must_use]
    pub const fn content_id(&self) -> Option<ContentId> {
        match self {
            Self::Add { content_id, .. }
            | Self::Touch { content_id }
            | Self::Delete { content_id }
            | Self::SetPin { content_id, .. } => Some(*content_id),
            Self::SetSetting { .. } | Self::ForgetDevice { .. } => None,
        }
    }
}

/// Operation metadata used for duplicate detection and deterministic ordering.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StampedOperation {
    id: OpId,
    timestamp: HlcTimestamp,
    operation: Operation,
}

impl StampedOperation {
    #[must_use]
    pub const fn new(id: OpId, timestamp: HlcTimestamp, operation: Operation) -> Self {
        Self {
            id,
            timestamp,
            operation,
        }
    }

    #[must_use]
    pub const fn id(&self) -> OpId {
        self.id
    }

    #[must_use]
    pub const fn timestamp(&self) -> HlcTimestamp {
        self.timestamp
    }

    #[must_use]
    pub const fn event_key(&self) -> EventKey {
        EventKey::new(self.timestamp, self.id)
    }

    #[must_use]
    pub const fn operation(&self) -> &Operation {
        &self.operation
    }
}
