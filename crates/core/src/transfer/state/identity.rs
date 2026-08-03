use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use uuid::Uuid;

const DEFAULT_MAX_CHUNKS: usize = 1_048_576;
const DEFAULT_MAX_PEERS: usize = 256;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TransferId(Uuid);

impl TransferId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    #[must_use]
    pub const fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    #[must_use]
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Default for TransferId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for TransferId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("TransferId")
            .field(&self.to_string())
            .finish()
    }
}

impl fmt::Display for TransferId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for TransferId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(value).map(Self)
    }
}

impl Serialize for TransferId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for TransferId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferPhase {
    Pending,
    Staging,
    Replicating,
    Paused,
    Complete,
    Cancelled,
    Failed,
}

impl TransferPhase {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Complete | Self::Cancelled | Self::Failed)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransferStateLimits {
    pub max_chunks: usize,
    pub max_peers: usize,
}

impl Default for TransferStateLimits {
    fn default() -> Self {
        Self {
            max_chunks: DEFAULT_MAX_CHUNKS,
            max_peers: DEFAULT_MAX_PEERS,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerProgress {
    pub(super) verified_bytes: u64,
    pub(super) complete: bool,
}

impl PeerProgress {
    #[must_use]
    pub const fn verified_bytes(self) -> u64 {
        self.verified_bytes
    }

    #[must_use]
    pub const fn complete(self) -> bool {
        self.complete
    }
}
