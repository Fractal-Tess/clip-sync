use thiserror::Error;

use crate::model::{
    ApplyOutcome, ContentId, HlcError, HlcTimestamp, HybridLogicalClock, NodeId, OpId, OpIdError,
    Operation, Payload, Projection, ProjectionError, StampedOperation,
};

/// In-memory authoring and projection state for one equal mesh member.
///
/// The caller must durably persist each returned local operation before it is
/// published to peers. Storage ownership remains outside this pure state machine.
#[derive(Clone, Debug)]
pub struct Replica {
    node_id: NodeId,
    last_counter: u64,
    clock: HybridLogicalClock,
    projection: Projection,
}

impl Replica {
    #[must_use]
    pub fn new(node_id: NodeId) -> Self {
        Self::restore(node_id, 0, HlcTimestamp::default(), Projection::default())
    }

    #[must_use]
    pub const fn restore(
        node_id: NodeId,
        last_counter: u64,
        last_timestamp: HlcTimestamp,
        projection: Projection,
    ) -> Self {
        Self {
            node_id,
            last_counter,
            clock: HybridLogicalClock::from_timestamp(last_timestamp),
            projection,
        }
    }

    #[must_use]
    pub const fn node_id(&self) -> NodeId {
        self.node_id
    }

    #[must_use]
    pub const fn last_counter(&self) -> u64 {
        self.last_counter
    }

    #[must_use]
    pub const fn last_timestamp(&self) -> HlcTimestamp {
        self.clock.last()
    }

    #[must_use]
    pub const fn projection(&self) -> &Projection {
        &self.projection
    }

    /// Authors an add or touch according to exact-content deduplication.
    ///
    /// # Errors
    ///
    /// Returns an error if the operation counter/clock is exhausted or the
    /// generated operation fails projection validation.
    pub fn copy(
        &mut self,
        payload: Payload,
        now_millis: u64,
    ) -> Result<StampedOperation, ReplicaError> {
        let content_id = payload.descriptor().content_id();
        let operation = if self.projection.is_visible(content_id) {
            Operation::Touch { content_id }
        } else {
            Operation::Add {
                content_id,
                payload,
            }
        };
        self.author(operation, now_millis)
    }

    /// Moves visible exact content to the top after user activation.
    ///
    /// # Errors
    ///
    /// Returns [`ReplicaError::ContentNotVisible`] if the content is absent or
    /// deleted, or a clock/counter error when authoring fails.
    pub fn activate(
        &mut self,
        content_id: ContentId,
        now_millis: u64,
    ) -> Result<StampedOperation, ReplicaError> {
        self.require_visible(content_id)?;
        self.author(Operation::Touch { content_id }, now_millis)
    }

    /// Authors a replicated deletion for visible content.
    ///
    /// # Errors
    ///
    /// Returns an error when content is not visible or authoring fails.
    pub fn delete(
        &mut self,
        content_id: ContentId,
        now_millis: u64,
    ) -> Result<StampedOperation, ReplicaError> {
        self.require_visible(content_id)?;
        self.author(Operation::Delete { content_id }, now_millis)
    }

    /// Authors a replicated pin register update for visible content.
    ///
    /// # Errors
    ///
    /// Returns an error when content is not visible or authoring fails.
    pub fn set_pinned(
        &mut self,
        content_id: ContentId,
        pinned: bool,
        now_millis: u64,
    ) -> Result<StampedOperation, ReplicaError> {
        self.require_visible(content_id)?;
        self.author(Operation::SetPin { content_id, pinned }, now_millis)
    }

    /// Deterministically applies one operation received from any peer.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid operation payloads or HLC exhaustion.
    pub fn ingest(
        &mut self,
        operation: &StampedOperation,
        now_millis: u64,
    ) -> Result<ApplyOutcome, ReplicaError> {
        let mut projection = self.projection.clone();
        let outcome = projection.apply(operation)?;
        if outcome == ApplyOutcome::Duplicate {
            return Ok(outcome);
        }

        let mut clock = self.clock;
        clock.merge(operation.timestamp(), now_millis)?;
        self.projection = projection;
        self.clock = clock;
        Ok(outcome)
    }

    fn author(
        &mut self,
        operation: Operation,
        now_millis: u64,
    ) -> Result<StampedOperation, ReplicaError> {
        let counter = self
            .last_counter
            .checked_add(1)
            .ok_or(ReplicaError::CounterExhausted)?;
        let id = OpId::new(self.node_id, counter)?;
        let mut clock = self.clock;
        let timestamp = clock.tick(now_millis)?;
        let stamped = StampedOperation::new(id, timestamp, operation);
        let mut projection = self.projection.clone();
        projection.apply(&stamped)?;

        self.last_counter = counter;
        self.clock = clock;
        self.projection = projection;
        Ok(stamped)
    }

    fn require_visible(&self, content_id: ContentId) -> Result<(), ReplicaError> {
        if self.projection.is_visible(content_id) {
            Ok(())
        } else {
            Err(ReplicaError::ContentNotVisible(content_id))
        }
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ReplicaError {
    #[error("local operation counter exhausted")]
    CounterExhausted,
    #[error("content {0} is not visible")]
    ContentNotVisible(ContentId),
    #[error(transparent)]
    Clock(#[from] HlcError),
    #[error(transparent)]
    OperationId(#[from] OpIdError),
    #[error(transparent)]
    Projection(#[from] ProjectionError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Representation;

    const CONTENT_KEY: [u8; 32] = [9; 32];

    fn payload(text: &[u8]) -> Payload {
        Payload::new(
            &CONTENT_KEY,
            vec![Representation::new("text/plain;charset=utf-8", text)],
        )
        .expect("valid payload")
    }

    #[test]
    fn exact_duplicate_becomes_touch() {
        let mut replica = Replica::new(NodeId::new());
        let add = replica.copy(payload(b"same"), 10).expect("add");
        let touch = replica.copy(payload(b"same"), 11).expect("touch");

        assert!(matches!(add.operation(), Operation::Add { .. }));
        assert!(matches!(touch.operation(), Operation::Touch { .. }));
        assert_eq!(replica.projection().visible_items().len(), 1);
        assert_eq!(replica.last_counter(), 2);
    }

    #[test]
    fn delete_then_copy_reintroduces_content() {
        let mut replica = Replica::new(NodeId::new());
        let add = replica.copy(payload(b"again"), 10).expect("add");
        let content_id = add.operation().content_id().expect("content operation");
        replica.delete(content_id, 11).expect("delete");
        assert!(!replica.projection().is_visible(content_id));

        let readd = replica.copy(payload(b"again"), 12).expect("re-add");
        assert!(matches!(readd.operation(), Operation::Add { .. }));
        assert!(replica.projection().is_visible(content_id));
    }

    #[test]
    fn duplicate_ingest_does_not_advance_clock() {
        let mut origin = Replica::new(NodeId::new());
        let operation = origin.copy(payload(b"remote"), 100).expect("origin copy");
        let mut peer = Replica::new(NodeId::new());

        assert_eq!(
            peer.ingest(&operation, 90).expect("first ingest"),
            ApplyOutcome::Applied
        );
        let timestamp = peer.last_timestamp();
        assert_eq!(
            peer.ingest(&operation, 200).expect("duplicate ingest"),
            ApplyOutcome::Duplicate
        );
        assert_eq!(peer.last_timestamp(), timestamp);
    }
}
