use crate::model::{ContentId, Operation, Payload, Reference, StampedOperation};

use super::{Replica, ReplicaError, parse_content_id};

impl Replica {
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

    /// Authors a large copy whose bytes stay on this device. A repeated copy
    /// of the same item re-publishes the description, which may have changed
    /// (a file edited in place keeps its path, and so its content ID).
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid reference or authoring failure.
    pub fn add_reference(
        &mut self,
        content_id: ContentId,
        reference: Reference,
        now_millis: u64,
    ) -> Result<StampedOperation, ReplicaError> {
        self.author(
            Operation::AddReference {
                content_id,
                reference,
            },
            now_millis,
        )
    }

    /// Captures payload and immediately applies the history quota.
    ///
    /// The returned batch starts with the add/touch and is followed by any
    /// deterministic delete operations.
    ///
    /// # Errors
    ///
    /// Returns an authoring error. Operations authored before the failure
    /// stay applied; `HistoryStore` restores durable state.
    pub fn copy_and_enforce(
        &mut self,
        payload: Payload,
        quota_bytes: u64,
        now_millis: u64,
    ) -> Result<Vec<StampedOperation>, ReplicaError> {
        let mut operations = vec![self.copy(payload, now_millis)?];
        operations.extend(self.enforce_quota(quota_bytes, now_millis)?);
        Ok(operations)
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

    /// Parses an external content ID and authors a replicated deletion.
    ///
    /// # Errors
    ///
    /// Returns a typed invalid-ID, visibility, clock, or counter error.
    pub fn delete_by_id(
        &mut self,
        content_id: &str,
        now_millis: u64,
    ) -> Result<StampedOperation, ReplicaError> {
        self.delete(parse_content_id(content_id)?, now_millis)
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

    /// Pins visible content mesh-wide.
    ///
    /// # Errors
    ///
    /// Returns an error when content is not visible or authoring fails.
    pub fn pin(
        &mut self,
        content_id: ContentId,
        now_millis: u64,
    ) -> Result<StampedOperation, ReplicaError> {
        self.set_pinned(content_id, true, now_millis)
    }

    /// Unpins visible content mesh-wide.
    ///
    /// # Errors
    ///
    /// Returns an error when content is not visible or authoring fails.
    pub fn unpin(
        &mut self,
        content_id: ContentId,
        now_millis: u64,
    ) -> Result<StampedOperation, ReplicaError> {
        self.set_pinned(content_id, false, now_millis)
    }

    /// Parses an external content ID and pins it mesh-wide.
    ///
    /// # Errors
    ///
    /// Returns a typed invalid-ID, visibility, clock, or counter error.
    pub fn pin_by_id(
        &mut self,
        content_id: &str,
        now_millis: u64,
    ) -> Result<StampedOperation, ReplicaError> {
        self.pin(parse_content_id(content_id)?, now_millis)
    }

    /// Parses an external content ID and unpins it mesh-wide.
    ///
    /// # Errors
    ///
    /// Returns a typed invalid-ID, visibility, clock, or counter error.
    pub fn unpin_by_id(
        &mut self,
        content_id: &str,
        now_millis: u64,
    ) -> Result<StampedOperation, ReplicaError> {
        self.unpin(parse_content_id(content_id)?, now_millis)
    }

    /// Authors deterministic oldest-first quota deletions.
    ///
    /// Items whose payload has not arrived yet (a touch or pin that overtook
    /// its add during reconciliation) have no known size, so they are left out
    /// of this round. Failing instead would make a local copy fail whenever a
    /// sync happened to be half-way through, losing that copy.
    ///
    /// # Errors
    ///
    /// Propagates clock/counter/projection failures. Operations authored
    /// before the failure stay applied; `HistoryStore` restores durable state.
    pub fn enforce_quota(
        &mut self,
        quota_bytes: u64,
        now_millis: u64,
    ) -> Result<Vec<StampedOperation>, ReplicaError> {
        let evictions = self.projection.quota_plan(quota_bytes).evictions().to_vec();
        evictions
            .into_iter()
            .map(|content_id| self.author(Operation::Delete { content_id }, now_millis))
            .collect()
    }

    /// Replicates a device-forget decision.
    ///
    /// # Errors
    ///
    /// The local device cannot forget itself. Other authoring failures are
    /// returned without changing state.
    pub fn forget_device(
        &mut self,
        node_id: crate::model::NodeId,
        now_millis: u64,
    ) -> Result<StampedOperation, ReplicaError> {
        if node_id == self.node_id {
            return Err(ReplicaError::CannotForgetLocalDevice);
        }
        self.author(Operation::ForgetDevice { node_id }, now_millis)
    }
}
