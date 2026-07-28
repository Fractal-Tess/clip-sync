use serde::{Deserialize, Serialize};

use crate::{
    payload::{ManifestId, StoredManifest},
    transfer::TransferId,
};

use super::{ContentId, EventKey, HlcTimestamp, NodeId, OpId, Payload};

pub const DEFAULT_MESH_QUOTA_BYTES: u64 = 1024 * 1024 * 1024;
pub const DEFAULT_CAPTURE_THRESHOLD_BYTES: u64 = 20 * 1024 * 1024;

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

/// Type-safe names for shared settings understood by this protocol version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SharedSetting {
    MeshQuotaBytes,
    CaptureThresholdBytes,
}

impl SharedSetting {
    pub const MESH_QUOTA_KEY: &'static str = "mesh_quota_bytes";
    pub const CAPTURE_THRESHOLD_KEY: &'static str = "capture_threshold_bytes";

    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::MeshQuotaBytes => Self::MESH_QUOTA_KEY,
            Self::CaptureThresholdBytes => Self::CAPTURE_THRESHOLD_KEY,
        }
    }

    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            Self::MESH_QUOTA_KEY => Some(Self::MeshQuotaBytes),
            Self::CAPTURE_THRESHOLD_KEY => Some(Self::CaptureThresholdBytes),
            _ => None,
        }
    }
}

/// Effective shared settings after applying replicated LWW registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EffectiveSharedSettings {
    pub mesh_quota_bytes: u64,
    pub capture_threshold_bytes: u64,
}

impl Default for EffectiveSharedSettings {
    fn default() -> Self {
        Self {
            mesh_quota_bytes: DEFAULT_MESH_QUOTA_BYTES,
            capture_threshold_bytes: DEFAULT_CAPTURE_THRESHOLD_BYTES,
        }
    }
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
    /// Explicit share whose payload was larger than the effective mesh quota
    /// when authored. It remains quota-exempt until explicitly deleted.
    AddQuotaExempt {
        content_id: ContentId,
        payload: Payload,
    },
    /// Announces a manifest-backed explicit share before peer chunk fetching.
    BeginShare {
        transfer_id: TransferId,
        content_id: ContentId,
        manifest_id: ManifestId,
        manifest: StoredManifest,
        quota_exempt: bool,
    },
    /// Declares that the origin durably retained every manifest chunk.
    CompleteShare {
        transfer_id: TransferId,
        content_id: ContentId,
        manifest_id: ManifestId,
    },
    /// Dominating cancellation tombstone for an incomplete share.
    CancelShare {
        transfer_id: TransferId,
        content_id: ContentId,
        manifest_id: ManifestId,
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
            | Self::AddQuotaExempt { content_id, .. }
            | Self::BeginShare { content_id, .. }
            | Self::CompleteShare { content_id, .. }
            | Self::CancelShare { content_id, .. }
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
