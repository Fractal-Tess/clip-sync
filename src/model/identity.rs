use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// Stable, internal identity for one mesh member.
///
/// Hostnames are deliberately not part of replication identity.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(Uuid);

impl NodeId {
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

impl Default for NodeId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("NodeId").field(&self.0).finish()
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for NodeId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value.parse().map(Self)
    }
}

/// Globally unique identity of an operation. Counters are one-based and must
/// be persisted by their originating node before an operation is published.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct OpId {
    node: NodeId,
    counter: u64,
}

impl OpId {
    /// Creates a one-based operation identity.
    ///
    /// # Errors
    ///
    /// Returns [`OpIdError::ZeroCounter`] when `counter` is zero.
    pub fn new(node: NodeId, counter: u64) -> Result<Self, OpIdError> {
        if counter == 0 {
            return Err(OpIdError::ZeroCounter);
        }

        Ok(Self { node, counter })
    }

    #[must_use]
    pub const fn node(self) -> NodeId {
        self.node
    }

    #[must_use]
    pub const fn counter(self) -> u64 {
        self.counter
    }
}

impl fmt::Display for OpId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.node, self.counter)
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum OpIdError {
    #[error("operation counters are one-based")]
    ZeroCounter,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_id_round_trips_as_text() {
        let id = NodeId::from_uuid(Uuid::from_u128(42));
        assert_eq!(id.to_string().parse(), Ok(id));
    }

    #[test]
    fn operation_counters_are_one_based() {
        let node = NodeId::from_uuid(Uuid::nil());
        assert_eq!(OpId::new(node, 0), Err(OpIdError::ZeroCounter));
        assert_eq!(OpId::new(node, 1).unwrap().counter(), 1);
    }
}
