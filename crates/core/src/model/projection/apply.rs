use super::super::{Operation, StampedOperation};
use super::{
    ApplyOutcome, ContentState, Projection, ProjectionError, Register, TransferMetadata,
    TransferProjectionState, TransferTerminal, validate_setting, write_register,
};

impl Projection {
    pub(crate) fn validate_operation(stamped: &StampedOperation) -> Result<(), ProjectionError> {
        match stamped.operation() {
            Operation::Add {
                content_id,
                payload,
            }
            | Operation::AddQuotaExempt {
                content_id,
                payload,
            } => {
                payload.validate_structure()?;
                if *content_id != payload.descriptor().content_id() {
                    return Err(ProjectionError::PayloadContentIdMismatch {
                        operation: *content_id,
                        payload: payload.descriptor().content_id(),
                    });
                }
            }
            Operation::SetSetting { key, value } => validate_setting(key, value)?,
            _ => {}
        }
        Ok(())
    }

    /// Applies one immutable operation or identifies an exact replay.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectionError::PayloadContentIdMismatch`] when an `Add`
    /// operation names a different ID than its payload descriptor.
    #[allow(clippy::too_many_lines)]
    pub fn apply(&mut self, stamped: &StampedOperation) -> Result<ApplyOutcome, ProjectionError> {
        Self::validate_operation(stamped)?;

        if !self.seen.record(stamped.id()) {
            return Ok(ApplyOutcome::Duplicate);
        }

        let event = stamped.event_key();
        self.known_members.insert(stamped.id().node());
        match stamped.operation() {
            Operation::Add {
                content_id,
                payload,
            } => {
                let state = self
                    .content
                    .entry(*content_id)
                    .or_insert_with(ContentState::new);
                state.activity = Some(state.activity.map_or(event, |current| current.max(event)));
                write_register(&mut state.payload, event, payload.clone());
                write_register(&mut state.quota_exempt, event, false);
            }
            Operation::AddQuotaExempt {
                content_id,
                payload,
            } => {
                let state = self
                    .content
                    .entry(*content_id)
                    .or_insert_with(ContentState::new);
                state.activity = Some(state.activity.map_or(event, |current| current.max(event)));
                write_register(&mut state.payload, event, payload.clone());
                write_register(&mut state.quota_exempt, event, true);
            }
            Operation::BeginShare {
                transfer_id,
                content_id,
                manifest_id,
                manifest,
                quota_exempt,
            } => {
                let transfer = self
                    .transfers
                    .entry(*transfer_id)
                    .or_insert_with(TransferProjectionState::new);
                write_register(
                    &mut transfer.begin,
                    event,
                    TransferMetadata {
                        content_id: *content_id,
                        manifest_id: *manifest_id,
                        manifest: manifest.clone(),
                        quota_exempt: *quota_exempt,
                    },
                );
                let state = self
                    .content
                    .entry(*content_id)
                    .or_insert_with(ContentState::new);
                state.activity = Some(state.activity.map_or(event, |current| current.max(event)));
                write_register(&mut state.quota_exempt, event, *quota_exempt);
                if transfer.terminal_matches(transfer.complete.as_ref())
                    && let Some(complete) = transfer.complete.as_ref()
                {
                    let completion = complete.event;
                    state.activity = Some(
                        state
                            .activity
                            .map_or(completion, |current| current.max(completion)),
                    );
                }
            }
            Operation::CompleteShare {
                transfer_id,
                content_id,
                manifest_id,
            } => {
                let transfer = self
                    .transfers
                    .entry(*transfer_id)
                    .or_insert_with(TransferProjectionState::new);
                write_register(
                    &mut transfer.complete,
                    event,
                    TransferTerminal {
                        content_id: *content_id,
                        manifest_id: *manifest_id,
                    },
                );
                if transfer.terminal_matches(transfer.complete.as_ref()) {
                    let state = self
                        .content
                        .entry(*content_id)
                        .or_insert_with(ContentState::new);
                    state.activity =
                        Some(state.activity.map_or(event, |current| current.max(event)));
                }
            }
            Operation::CancelShare {
                transfer_id,
                content_id,
                manifest_id,
            } => {
                let transfer = self
                    .transfers
                    .entry(*transfer_id)
                    .or_insert_with(TransferProjectionState::new);
                write_register(
                    &mut transfer.cancelled,
                    event,
                    TransferTerminal {
                        content_id: *content_id,
                        manifest_id: *manifest_id,
                    },
                );
            }
            Operation::Touch { content_id } => {
                let state = self
                    .content
                    .entry(*content_id)
                    .or_insert_with(ContentState::new);
                state.activity = Some(state.activity.map_or(event, |current| current.max(event)));
            }
            Operation::Delete { content_id } => {
                let state = self
                    .content
                    .entry(*content_id)
                    .or_insert_with(ContentState::new);
                state.deletion = Some(state.deletion.map_or(event, |current| current.max(event)));
            }
            Operation::SetPin { content_id, pinned } => {
                let state = self
                    .content
                    .entry(*content_id)
                    .or_insert_with(ContentState::new);
                write_register(&mut state.pin, event, *pinned);
            }
            Operation::SetSetting { key, value } => {
                self.settings
                    .entry(key.clone())
                    .and_modify(|current| current.replace_if_newer(event, value.clone()))
                    .or_insert_with(|| Register {
                        event,
                        value: value.clone(),
                    });
            }
            Operation::ForgetDevice { node_id } => {
                self.forgotten_devices
                    .entry(*node_id)
                    .and_modify(|current| *current = (*current).max(event))
                    .or_insert(event);
            }
        }

        Ok(ApplyOutcome::Applied)
    }

    /// Applies a sequence using the same validation as [`Self::apply`].
    ///
    /// # Errors
    ///
    /// Returns the first projection validation error.
    pub fn apply_all<'a>(
        &mut self,
        operations: impl IntoIterator<Item = &'a StampedOperation>,
    ) -> Result<(), ProjectionError> {
        for operation in operations {
            self.apply(operation)?;
        }
        Ok(())
    }
}
