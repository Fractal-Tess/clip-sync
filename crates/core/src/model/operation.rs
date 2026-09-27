use super::{ContentId, EventKey, HlcTimestamp, NodeId, OpId, Payload, Reference};

/// Immutable operation body. There is deliberately no operation that embeds
/// an active local clipboard action: remote replication only changes history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Operation {
    /// A copy small enough to replicate its bytes to every device.
    Add {
        content_id: ContentId,
        payload: Payload,
    },
    /// A large copy whose bytes stay on the authoring device.
    AddReference {
        content_id: ContentId,
        reference: Reference,
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
    ForgetDevice {
        node_id: NodeId,
    },
    /// An operation kind from an earlier version that no longer has any
    /// effect (replicated settings and manifest shares). It keeps its ID so
    /// every device still agrees on which operations exist.
    Retired,
}

impl Operation {
    #[must_use]
    pub const fn content_id(&self) -> Option<ContentId> {
        match self {
            Self::Add { content_id, .. }
            | Self::AddReference { content_id, .. }
            | Self::Touch { content_id }
            | Self::Delete { content_id }
            | Self::SetPin { content_id, .. } => Some(*content_id),
            Self::ForgetDevice { .. } | Self::Retired => None,
        }
    }
}

/// Operation metadata used for duplicate detection and deterministic ordering.
#[derive(Clone, Debug, PartialEq, Eq)]
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

    #[must_use]
    pub fn into_operation(self) -> Operation {
        self.operation
    }
}
