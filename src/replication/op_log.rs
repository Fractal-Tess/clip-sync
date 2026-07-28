//! Immutable append-only operation log with content-integrity checking.
//!
//! Each operation is stored as its exact serialized bytes, keyed by [`OpId`].
//! Inserting an [`OpId`] that already exists with **identical** bytes is
//! idempotent (returns [`Ok(false)`]). Inserting the same [`OpId`] with
//! **different** bytes is rejected, preserving the immutability invariant.
//!
//! Storing raw bytes enables future unknown-field forwarding: a node running
//! an older schema can relay operations that contain fields it does not
//! understand, as long as the transport delivers the exact byte sequence.

use std::collections::BTreeMap;

use thiserror::Error;

use crate::model::OpId;

/// Append-only log of serialized operations keyed by [`OpId`].
///
/// Iteration order is deterministic: sorted by `(NodeId, counter)`.
#[derive(Clone, Debug, Default)]
pub struct OpLog {
    entries: BTreeMap<OpId, Vec<u8>>,
}

impl OpLog {
    /// Inserts an operation or verifies an exact duplicate.
    ///
    /// Returns `Ok(true)` when the operation is newly inserted, `Ok(false)`
    /// when an exact byte-identical duplicate already exists.
    ///
    /// # Errors
    ///
    /// Returns [`OpLogError::ConflictingContent`] when the same [`OpId`]
    /// exists with different serialized bytes.
    pub fn insert(&mut self, id: OpId, raw: Vec<u8>) -> Result<bool, OpLogError> {
        match self.entries.get(&id) {
            Some(existing) if *existing == raw => Ok(false),
            Some(_) => Err(OpLogError::ConflictingContent(id)),
            None => {
                self.entries.insert(id, raw);
                Ok(true)
            }
        }
    }

    /// Returns the raw bytes for an operation, if present.
    #[must_use]
    pub fn get(&self, id: OpId) -> Option<&[u8]> {
        self.entries.get(&id).map(Vec::as_slice)
    }

    /// Returns `true` if the log contains this operation.
    #[must_use]
    pub fn contains(&self, id: OpId) -> bool {
        self.entries.contains_key(&id)
    }

    /// Number of stored operations.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the log is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Deterministic iterator over all entries, sorted by `(NodeId, counter)`.
    pub fn iter(&self) -> impl Iterator<Item = (OpId, &[u8])> {
        self.entries.iter().map(|(id, raw)| (*id, raw.as_slice()))
    }

    /// Removes compacted durable entries while leaving the caller's
    /// acknowledgement/seen summary intact.
    pub fn remove_all(&mut self, operations: &[OpId]) {
        for operation in operations {
            self.entries.remove(operation);
        }
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum OpLogError {
    #[error("operation {0} already exists with different serialized content")]
    ConflictingContent(OpId),
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::model::NodeId;

    fn op(counter: u64) -> OpId {
        let node = NodeId::from_uuid(Uuid::from_u128(1));
        OpId::new(node, counter).unwrap()
    }

    #[test]
    fn insert_new_returns_true() {
        let mut log = OpLog::default();
        assert!(log.insert(op(1), b"data".to_vec()).unwrap());
        assert_eq!(log.len(), 1);
    }

    #[test]
    fn exact_duplicate_returns_false() {
        let mut log = OpLog::default();
        log.insert(op(1), b"data".to_vec()).unwrap();
        assert!(!log.insert(op(1), b"data".to_vec()).unwrap());
        assert_eq!(log.len(), 1);
    }

    #[test]
    fn conflicting_content_is_rejected() {
        let mut log = OpLog::default();
        log.insert(op(1), b"original".to_vec()).unwrap();
        let err = log.insert(op(1), b"tampered".to_vec()).unwrap_err();
        assert_eq!(err, OpLogError::ConflictingContent(op(1)));
        // Original preserved
        assert_eq!(log.get(op(1)).unwrap(), b"original");
    }

    #[test]
    fn iteration_is_sorted_by_op_id() {
        let mut log = OpLog::default();
        log.insert(op(3), b"c".to_vec()).unwrap();
        log.insert(op(1), b"a".to_vec()).unwrap();
        log.insert(op(2), b"b".to_vec()).unwrap();

        let counters: Vec<u64> = log.iter().map(|(id, _)| id.counter()).collect();
        assert_eq!(counters, vec![1, 2, 3]);
    }
}
