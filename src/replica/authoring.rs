use crate::model::{ContentId, Operation, Payload, SettingValue, SharedSetting, StampedOperation};
use crate::{
    payload::{ManifestId, StoredManifest},
    transfer::{TransferId, TransferPhase},
};

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
        node_id: crate::model::NodeId,
        now_millis: u64,
    ) -> Result<StampedOperation, ReplicaError> {
        if node_id == self.node_id {
            return Err(ReplicaError::CannotForgetLocalDevice);
        }
        self.author(Operation::ForgetDevice { node_id }, now_millis)
    }
}
