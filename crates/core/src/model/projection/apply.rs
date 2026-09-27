use super::super::{Operation, StampedOperation};
use super::{ApplyOutcome, ContentState, ItemKind, Projection, ProjectionError, write_register};

impl Projection {
    pub(crate) fn validate_operation(stamped: &StampedOperation) -> Result<(), ProjectionError> {
        match stamped.operation() {
            Operation::Add {
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
            Operation::AddReference { reference, .. } => reference.validate()?,
            _ => {}
        }
        Ok(())
    }

    /// Applies one immutable operation or identifies an exact replay.
    ///
    /// Validation runs before anything changes, so an error leaves the
    /// projection untouched.
    ///
    /// # Errors
    ///
    /// Returns a validation error for a malformed payload or reference.
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
                let state = self.content_mut(*content_id);
                state.activity = Some(state.activity.map_or(event, |current| current.max(event)));
                write_register(&mut state.item, event, ItemKind::inline(payload));
            }
            Operation::AddReference {
                content_id,
                reference,
            } => {
                let state = self.content_mut(*content_id);
                state.activity = Some(state.activity.map_or(event, |current| current.max(event)));
                write_register(
                    &mut state.item,
                    event,
                    ItemKind::Reference(reference.clone()),
                );
            }
            Operation::Touch { content_id } => {
                let state = self.content_mut(*content_id);
                state.activity = Some(state.activity.map_or(event, |current| current.max(event)));
            }
            Operation::Delete { content_id } => {
                let state = self.content_mut(*content_id);
                state.deletion = Some(state.deletion.map_or(event, |current| current.max(event)));
            }
            Operation::SetPin { content_id, pinned } => {
                write_register(&mut self.content_mut(*content_id).pin, event, *pinned);
            }
            Operation::ForgetDevice { node_id } => {
                self.forgotten_devices
                    .entry(*node_id)
                    .and_modify(|current| *current = (*current).max(event))
                    .or_insert(event);
            }
            Operation::Retired => {}
        }

        Ok(ApplyOutcome::Applied)
    }

    fn content_mut(&mut self, content_id: super::ContentId) -> &mut ContentState {
        self.content
            .entry(content_id)
            .or_insert_with(ContentState::new)
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
