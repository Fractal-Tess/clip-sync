use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::OpId;

/// A transport-independent hybrid logical clock timestamp.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct HlcTimestamp {
    physical_millis: u64,
    logical: u32,
}

impl HlcTimestamp {
    #[must_use]
    pub const fn new(physical_millis: u64, logical: u32) -> Self {
        Self {
            physical_millis,
            logical,
        }
    }

    #[must_use]
    pub const fn physical_millis(self) -> u64 {
        self.physical_millis
    }

    #[must_use]
    pub const fn logical(self) -> u32 {
        self.logical
    }
}

/// Stateful HLC generator. Callers supply wall-clock milliseconds so clock
/// behavior remains deterministic and straightforward to test.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HybridLogicalClock {
    last: HlcTimestamp,
}

impl HybridLogicalClock {
    #[must_use]
    pub const fn from_timestamp(last: HlcTimestamp) -> Self {
        Self { last }
    }

    #[must_use]
    pub const fn last(self) -> HlcTimestamp {
        self.last
    }

    /// Creates a local timestamp, remaining monotonic if wall time regresses.
    ///
    /// # Errors
    ///
    /// Returns [`HlcError::LogicalOverflow`] if no later timestamp can be
    /// represented at the current physical millisecond.
    pub fn tick(&mut self, now_millis: u64) -> Result<HlcTimestamp, HlcError> {
        let next = if now_millis > self.last.physical_millis {
            HlcTimestamp::new(now_millis, 0)
        } else {
            HlcTimestamp::new(self.last.physical_millis, increment(self.last.logical)?)
        };
        self.last = next;
        Ok(next)
    }

    /// Observes a remote timestamp and returns the next causally-later local
    /// timestamp according to the standard HLC merge rule.
    ///
    /// # Errors
    ///
    /// Returns [`HlcError::LogicalOverflow`] if no causally later timestamp can
    /// be represented at the selected physical millisecond.
    pub fn merge(
        &mut self,
        remote: HlcTimestamp,
        now_millis: u64,
    ) -> Result<HlcTimestamp, HlcError> {
        let physical = now_millis
            .max(self.last.physical_millis)
            .max(remote.physical_millis);

        let logical = match (
            physical == self.last.physical_millis,
            physical == remote.physical_millis,
        ) {
            (true, true) => increment(self.last.logical.max(remote.logical))?,
            (true, false) => increment(self.last.logical)?,
            (false, true) => increment(remote.logical)?,
            (false, false) => 0,
        };

        let next = HlcTimestamp::new(physical, logical);
        self.last = next;
        Ok(next)
    }
}

fn increment(value: u32) -> Result<u32, HlcError> {
    value.checked_add(1).ok_or(HlcError::LogicalOverflow)
}

/// Total ordering key for replicated events. HLC orders user-visible time;
/// operation identity resolves equal timestamps without arrival-order input.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EventKey {
    timestamp: HlcTimestamp,
    operation_id: OpId,
}

impl EventKey {
    #[must_use]
    pub const fn new(timestamp: HlcTimestamp, operation_id: OpId) -> Self {
        Self {
            timestamp,
            operation_id,
        }
    }

    #[must_use]
    pub const fn timestamp(self) -> HlcTimestamp {
        self.timestamp
    }

    #[must_use]
    pub const fn operation_id(self) -> OpId {
        self.operation_id
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum HlcError {
    #[error("hybrid logical clock counter exhausted at one physical instant")]
    LogicalOverflow,
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use uuid::Uuid;

    use super::*;
    use crate::model::NodeId;

    #[test]
    fn merge_advances_past_local_and_remote() {
        let local = HlcTimestamp::new(100, 4);
        let remote = HlcTimestamp::new(100, 9);
        let mut clock = HybridLogicalClock::from_timestamp(local);

        let merged = clock.merge(remote, 50).unwrap();

        assert_eq!(merged, HlcTimestamp::new(100, 10));
        assert!(merged > local);
        assert!(merged > remote);
    }

    #[test]
    fn wall_clock_ahead_resets_logical_component() {
        let mut clock = HybridLogicalClock::from_timestamp(HlcTimestamp::new(100, 4));
        assert_eq!(
            clock.merge(HlcTimestamp::new(90, 2), 101).unwrap(),
            HlcTimestamp::new(101, 0)
        );
    }

    #[test]
    fn equal_hlc_uses_operation_identity_as_tie_breaker() {
        let timestamp = HlcTimestamp::new(100, 0);
        let low = OpId::new(NodeId::from_uuid(Uuid::from_u128(1)), 1).unwrap();
        let high = OpId::new(NodeId::from_uuid(Uuid::from_u128(2)), 1).unwrap();
        assert!(EventKey::new(timestamp, low) < EventKey::new(timestamp, high));
    }

    proptest! {
        #[test]
        fn ticks_stay_monotonic_across_clock_regressions(times in prop::collection::vec(any::<u64>(), 1..256)) {
            let mut clock = HybridLogicalClock::default();
            let mut previous = clock.last();

            for now in times {
                let next = clock.tick(now).unwrap();
                prop_assert!(next > previous);
                previous = next;
            }
        }

        #[test]
        fn merge_is_later_than_both_inputs(
            local_physical in 0_u64..1_000_000,
            local_logical in 0_u32..10_000,
            remote_physical in 0_u64..1_000_000,
            remote_logical in 0_u32..10_000,
            now in 0_u64..1_000_000,
        ) {
            let local = HlcTimestamp::new(local_physical, local_logical);
            let remote = HlcTimestamp::new(remote_physical, remote_logical);
            let mut clock = HybridLogicalClock::from_timestamp(local);
            let merged = clock.merge(remote, now).unwrap();
            prop_assert!(merged > local);
            prop_assert!(merged > remote);
        }
    }
}
