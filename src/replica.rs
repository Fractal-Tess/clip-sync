use thiserror::Error;

use crate::model::{
    ApplyOutcome, ContentId, HlcError, HlcTimestamp, HybridLogicalClock, NodeId, OpId, OpIdError,
    Operation, Payload, Projection, ProjectionError, SettingValue, SharedSetting, StampedOperation,
};
use crate::{
    payload::{ManifestId, StoredManifest},
    transfer::{TransferId, TransferPhase},
};

/// Generous tolerance for wall-clock skew without allowing a malformed peer
/// event to pin the local HLC arbitrarily far into the future.
pub const MAX_REMOTE_CLOCK_SKEW_MILLIS: u64 = 24 * 60 * 60 * 1000;

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

    /// Authors an explicit share. Payloads larger than the current mesh quota
    /// carry replicated quota-exemption metadata; smaller shares behave like a
    /// normal copy.
    ///
    /// # Errors
    ///
    /// Returns an error if the operation counter/clock is exhausted or the
    /// generated operation fails projection validation.
    pub fn share_explicit(
        &mut self,
        payload: Payload,
        now_millis: u64,
    ) -> Result<StampedOperation, ReplicaError> {
        let content_id = payload.descriptor().content_id();
        let quota = self.projection.effective_shared_settings().mesh_quota_bytes;
        let oversized = payload.descriptor().logical_size() > quota;
        let operation = if oversized {
            Operation::AddQuotaExempt {
                content_id,
                payload,
            }
        } else if self.projection.is_visible(content_id) {
            Operation::Touch { content_id }
        } else {
            Operation::Add {
                content_id,
                payload,
            }
        };
        self.author(operation, now_millis)
    }

    /// Authors the pending replicated half of a manifest-backed share.
    ///
    /// # Errors
    ///
    /// Returns an authoring or projection validation error.
    pub fn begin_manifest_share(
        &mut self,
        transfer_id: TransferId,
        content_id: ContentId,
        manifest_id: ManifestId,
        manifest: StoredManifest,
        quota_exempt: bool,
        now_millis: u64,
    ) -> Result<StampedOperation, ReplicaError> {
        self.author(
            Operation::BeginShare {
                transfer_id,
                content_id,
                manifest_id,
                manifest,
                quota_exempt,
            },
            now_millis,
        )
    }

    /// Authors completion after local encrypted chunks are durable.
    ///
    /// # Errors
    ///
    /// Returns an error for an absent/cancelled transfer or authoring failure.
    pub fn complete_manifest_share(
        &mut self,
        transfer_id: TransferId,
        now_millis: u64,
    ) -> Result<StampedOperation, ReplicaError> {
        let transfer = self
            .projection
            .transfer(transfer_id)
            .ok_or(ReplicaError::TransferNotFound(transfer_id))?;
        if transfer.phase() == TransferPhase::Cancelled {
            return Err(ReplicaError::TransferCancelled(transfer_id));
        }
        let content_id = transfer
            .content_id()
            .ok_or(ReplicaError::TransferNotFound(transfer_id))?;
        let manifest_id = transfer
            .manifest_id()
            .ok_or(ReplicaError::TransferNotFound(transfer_id))?;
        self.author(
            Operation::CompleteShare {
                transfer_id,
                content_id,
                manifest_id,
            },
            now_millis,
        )
    }

    /// Authors a cancellation tombstone which dominates completion ordering.
    ///
    /// # Errors
    ///
    /// Returns an error for an absent transfer or authoring failure.
    pub fn cancel_manifest_share(
        &mut self,
        transfer_id: TransferId,
        now_millis: u64,
    ) -> Result<StampedOperation, ReplicaError> {
        let transfer = self
            .projection
            .transfer(transfer_id)
            .ok_or(ReplicaError::TransferNotFound(transfer_id))?;
        let content_id = transfer
            .content_id()
            .ok_or(ReplicaError::TransferNotFound(transfer_id))?;
        let manifest_id = transfer
            .manifest_id()
            .ok_or(ReplicaError::TransferNotFound(transfer_id))?;
        self.author(
            Operation::CancelShare {
                transfer_id,
                content_id,
                manifest_id,
            },
            now_millis,
        )
    }

    /// Captures payload and immediately applies the effective replicated quota.
    ///
    /// The returned batch starts with the add/touch and is followed by any
    /// deterministic delete operations. The replica changes atomically.
    ///
    /// # Errors
    ///
    /// Returns an authoring or quota-state error without changing the replica.
    pub fn copy_and_enforce(
        &mut self,
        payload: Payload,
        now_millis: u64,
    ) -> Result<Vec<StampedOperation>, ReplicaError> {
        let mut next = self.clone();
        let copied = next.copy(payload, now_millis)?;
        let mut operations = vec![copied];
        operations.extend(next.enforce_quota(now_millis)?);
        *self = next;
        Ok(operations)
    }

    /// Explicitly shares payload and immediately applies quota to all other
    /// chargeable history.
    ///
    /// # Errors
    ///
    /// Returns an authoring or quota-state error without changing the replica.
    pub fn share_explicit_and_enforce(
        &mut self,
        payload: Payload,
        now_millis: u64,
    ) -> Result<Vec<StampedOperation>, ReplicaError> {
        let mut next = self.clone();
        let shared = next.share_explicit(payload, now_millis)?;
        let mut operations = vec![shared];
        operations.extend(next.enforce_quota(now_millis)?);
        *self = next;
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

    /// Updates one known shared setting through its replicated LWW register.
    ///
    /// # Errors
    ///
    /// Zero is rejected for byte limits. Clock/counter and projection errors
    /// are propagated without mutating the replica.
    pub fn set_shared_setting(
        &mut self,
        setting: SharedSetting,
        value: u64,
        now_millis: u64,
    ) -> Result<StampedOperation, ReplicaError> {
        if value == 0 {
            return Err(ReplicaError::InvalidSharedSetting { setting, value });
        }
        self.author(
            Operation::SetSetting {
                key: setting.key().to_owned(),
                value: SettingValue::Unsigned(value),
            },
            now_millis,
        )
    }

    /// Authors deterministic oldest-first quota deletions using the effective
    /// replicated quota.
    ///
    /// # Errors
    ///
    /// Refuses to make a partial decision if a visible payload is unavailable,
    /// and propagates clock/counter/projection failures atomically.
    pub fn enforce_quota(
        &mut self,
        now_millis: u64,
    ) -> Result<Vec<StampedOperation>, ReplicaError> {
        let plan = self.projection.effective_quota_plan();
        if !plan.missing_payloads().is_empty() {
            return Err(ReplicaError::QuotaStateIncomplete(
                plan.missing_payloads().to_vec(),
            ));
        }

        let mut next = self.clone();
        let mut operations = Vec::with_capacity(plan.evictions().len());
        for content_id in plan.evictions() {
            operations.push(next.author(
                Operation::Delete {
                    content_id: *content_id,
                },
                now_millis,
            )?);
        }
        *self = next;
        Ok(operations)
    }

    /// Changes the replicated quota and immediately authors the deterministic
    /// evictions implied by the new value.
    ///
    /// # Errors
    ///
    /// Returns a validation, incomplete-state, clock, counter, or projection
    /// error without changing the replica.
    pub fn set_mesh_quota_and_enforce(
        &mut self,
        quota_bytes: u64,
        now_millis: u64,
    ) -> Result<Vec<StampedOperation>, ReplicaError> {
        let mut next = self.clone();
        let setting =
            next.set_shared_setting(SharedSetting::MeshQuotaBytes, quota_bytes, now_millis)?;
        let mut operations = vec![setting];
        operations.extend(next.enforce_quota(now_millis)?);
        *self = next;
        Ok(operations)
    }

    /// Replicates a device-forget decision.
    ///
    /// # Errors
    ///
    /// The local device cannot forget itself. Other authoring failures are
    /// returned without changing state.
    pub fn forget_device(
        &mut self,
        node_id: NodeId,
        now_millis: u64,
    ) -> Result<StampedOperation, ReplicaError> {
        if node_id == self.node_id {
            return Err(ReplicaError::CannotForgetLocalDevice);
        }
        self.author(Operation::ForgetDevice { node_id }, now_millis)
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
        let max_remote_millis = now_millis.saturating_add(MAX_REMOTE_CLOCK_SKEW_MILLIS);
        if operation.timestamp().physical_millis() > max_remote_millis {
            return Err(ReplicaError::RemoteClockTooFarAhead {
                remote: operation.timestamp().physical_millis(),
                local: now_millis,
            });
        }
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
    #[error("transfer {0} is not available")]
    TransferNotFound(TransferId),
    #[error("transfer {0} was cancelled")]
    TransferCancelled(TransferId),
    #[error("content ID is invalid: {0}")]
    InvalidContentId(String),
    #[error("shared setting {setting:?} cannot be set to {value}")]
    InvalidSharedSetting { setting: SharedSetting, value: u64 },
    #[error("quota cannot be evaluated while payloads are unavailable: {0:?}")]
    QuotaStateIncomplete(Vec<ContentId>),
    #[error("the local device cannot forget itself")]
    CannotForgetLocalDevice,
    #[error("remote clock {remote}ms is too far ahead of local clock {local}ms")]
    RemoteClockTooFarAhead { remote: u64, local: u64 },
    #[error(transparent)]
    Clock(#[from] HlcError),
    #[error(transparent)]
    OperationId(#[from] OpIdError),
    #[error(transparent)]
    Projection(#[from] ProjectionError),
}

fn parse_content_id(content_id: &str) -> Result<ContentId, ReplicaError> {
    content_id
        .parse()
        .map_err(|error: crate::model::ContentIdParseError| {
            ReplicaError::InvalidContentId(error.to_string())
        })
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
