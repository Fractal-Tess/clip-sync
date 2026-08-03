//! Anti-entropy state machine.
//!
//! [`AntiEntropyState`] is the coordinator that ties together an [`OpLog`]
//! (immutable raw-byte store) and [`SeenOps`] (compact duplicate/frontier
//! tracker). It exposes a transport-independent API for ingesting operations
//! and computing deterministic bounded batches.

use thiserror::Error;

use crate::model::{OpId, SeenOps, StampedOperation};

use super::codec::{Codec, CodecError};
use super::op_log::{OpLog, OpLogError};

const MAX_MISSING_LOCALLY: usize = 65_536;

/// Resource limits for a single anti-entropy batch.
#[derive(Clone, Copy, Debug)]
pub struct BatchLimits {
    /// Maximum number of operations in one batch.
    pub max_ops: usize,
    /// Maximum total raw bytes across all operations in one batch.
    pub max_bytes: usize,
}

impl Default for BatchLimits {
    fn default() -> Self {
        Self {
            max_ops: 256,
            max_bytes: 1024 * 1024, // 1 MiB
        }
    }
}

/// A bounded, deterministically ordered batch of serialized operations.
#[derive(Clone, Debug)]
pub struct OpBatch {
    /// Raw serialized operations, in deterministic `(NodeId, counter)` order.
    entries: Vec<Vec<u8>>,
    /// `true` when the sender has additional operations beyond this batch.
    has_more: bool,
}

impl OpBatch {
    /// The serialized operations in this batch.
    #[must_use]
    pub fn entries(&self) -> &[Vec<u8>] {
        &self.entries
    }

    /// Whether the sender has more operations available after this batch.
    #[must_use]
    pub const fn has_more(&self) -> bool {
        self.has_more
    }

    /// Number of operations in this batch.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether this batch is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Total serialized bytes across all entries.
    #[must_use]
    pub fn total_bytes(&self) -> usize {
        self.entries.iter().map(Vec::len).sum()
    }
}

/// Outcome of ingesting a single operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IngestOutcome {
    /// The operation was new and has been stored. The caller should apply
    /// the contained [`StampedOperation`] to their [`Projection`].
    Applied(StampedOperation),
    /// An exact byte-identical operation was already present. No action needed.
    Duplicate,
}

/// Anti-entropy state machine. Combines an immutable operation log with
/// a compact frontier/gap tracker to support idempotent ingest and
/// deterministic bounded batch computation.
///
/// This type owns no networking and makes no persistence guarantees. The
/// caller is responsible for durability (snapshotting the log) and for
/// applying returned [`StampedOperation`]s to their [`Projection`].
#[derive(Clone, Debug, Default)]
pub struct AntiEntropyState {
    seen: SeenOps,
    log: OpLog,
}

impl AntiEntropyState {
    /// Creates an empty state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Restores state from a previously persisted log and seen-set.
    #[must_use]
    pub fn restore(seen: SeenOps, log: OpLog) -> Self {
        Self { seen, log }
    }

    /// The current frontier/gap summary. Send this to a peer so they can
    /// compute what operations you need.
    #[must_use]
    pub fn seen(&self) -> &SeenOps {
        &self.seen
    }

    /// The raw operation log.
    #[must_use]
    pub fn log(&self) -> &OpLog {
        &self.log
    }

    /// Ingest a remote operation from its raw serialized bytes.
    ///
    /// Decodes the bytes, checks the immutable log for conflicts, stores
    /// the raw bytes for future forwarding, and updates the seen-set.
    ///
    /// Returns [`IngestOutcome::Applied`] with the decoded operation when
    /// new, or [`IngestOutcome::Duplicate`] for exact replays.
    ///
    /// # Errors
    ///
    /// - [`AntiEntropyError::Codec`] if decoding fails.
    /// - [`AntiEntropyError::OpLog`] if the same [`OpId`] exists with
    ///   different serialized content (integrity violation).
    pub fn ingest_raw(
        &mut self,
        raw: &[u8],
        codec: &dyn Codec,
    ) -> Result<IngestOutcome, AntiEntropyError> {
        let op = codec.decode_op(raw)?;
        let id = op.id();

        // A compacted operation remains in the durable seen summary but no
        // longer has a forwarding-log entry. Never recreate that entry from a
        // stale or adversarial replay.
        if self.seen.contains(id) && !self.log.contains(id) {
            return Ok(IngestOutcome::Duplicate);
        }

        let is_new = self.log.insert(id, raw.to_vec())?;
        if !is_new {
            return Ok(IngestOutcome::Duplicate);
        }

        self.seen.record(id);
        Ok(IngestOutcome::Applied(op))
    }

    /// Record a locally authored operation.
    ///
    /// The caller has already applied the operation to its [`Projection`]
    /// via [`Replica`]. This method encodes it, stores the raw bytes in
    /// the log, and updates the seen-set.
    ///
    /// # Errors
    ///
    /// - [`AntiEntropyError::Codec`] if encoding fails.
    /// - [`AntiEntropyError::OpLog`] on conflict (should not happen for
    ///   locally authored operations with monotonic counters).
    pub fn record_local(
        &mut self,
        op: &StampedOperation,
        codec: &dyn Codec,
    ) -> Result<(), AntiEntropyError> {
        let raw = codec.encode_op(op)?;
        self.log.insert(op.id(), raw)?;
        self.seen.record(op.id());
        Ok(())
    }

    /// Compute a deterministic, bounded batch of operations that `remote`
    /// does not have.
    ///
    /// Operations are yielded in `(NodeId, counter)` order. The batch
    /// respects both `limits.max_ops` and `limits.max_bytes`. If either
    /// limit is reached before all missing operations are included,
    /// [`OpBatch::has_more`] returns `true`.
    ///
    /// This method never includes an operation the remote already has
    /// (per their frontier and gap set), and never skips a gap in a way
    /// that would falsely advance the remote's frontier.
    #[must_use]
    pub fn compute_batch(&self, remote: &SeenOps, limits: &BatchLimits) -> OpBatch {
        let mut entries = Vec::new();
        let mut total_bytes: usize = 0;
        let mut has_more = false;

        for (id, raw) in self.log.iter() {
            if remote.contains(id) {
                continue;
            }

            // Check resource bounds *before* including.
            if entries.len() >= limits.max_ops {
                has_more = true;
                break;
            }
            if !entries.is_empty() && total_bytes.saturating_add(raw.len()) > limits.max_bytes {
                has_more = true;
                break;
            }

            entries.push(raw.to_vec());
            total_bytes = total_bytes.saturating_add(raw.len());
        }

        OpBatch { entries, has_more }
    }

    /// Returns a bounded prefix of [`OpId`]s that `remote` advertises but we do
    /// not have locally. Useful for requesting specific operations without
    /// allowing a hostile frontier to trigger an unbounded range expansion.
    #[must_use]
    pub fn missing_locally(&self, remote: &SeenOps) -> Vec<OpId> {
        let mut missing = Vec::new();
        for node in remote.known_nodes() {
            let remote_frontier = remote.frontier(node);
            let local_frontier = self.seen.frontier(node);

            // Contiguous range the remote has but we don't.
            if let Some(first_missing) = local_frontier.checked_add(1) {
                for counter in first_missing..=remote_frontier {
                    let Ok(id) = OpId::new(node, counter) else {
                        continue;
                    };
                    if !self.seen.contains(id) {
                        missing.push(id);
                        if missing.len() == MAX_MISSING_LOCALLY {
                            return missing;
                        }
                    }
                }
            }

            // Sparse entries above the remote frontier that we lack.
            for counter in remote.gaps(node) {
                let Ok(id) = OpId::new(node, counter) else {
                    continue;
                };
                if !self.seen.contains(id) {
                    missing.push(id);
                    if missing.len() == MAX_MISSING_LOCALLY {
                        return missing;
                    }
                }
            }
        }
        missing
    }

    /// Drops operations that were compacted from durable storage. The seen
    /// frontier deliberately remains unchanged so peers cannot replay or
    /// request already-compacted history.
    pub fn compact_log(&mut self, operations: &[OpId]) {
        self.log.remove_all(operations);
    }
}

#[derive(Debug, Error)]
pub enum AntiEntropyError {
    #[error(transparent)]
    Codec(#[from] CodecError),
    #[error(transparent)]
    OpLog(#[from] OpLogError),
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::model::{HlcTimestamp, NodeId, Operation, Payload, Representation};
    use crate::replication::JsonV1Codec;

    const KEY: [u8; 32] = [7; 32];

    fn node(id: u128) -> NodeId {
        NodeId::from_uuid(Uuid::from_u128(id))
    }

    fn make_op(node_id: NodeId, counter: u64, text: &[u8]) -> StampedOperation {
        let id = OpId::new(node_id, counter).unwrap();
        let ts = HlcTimestamp::new(counter * 1000, 0);
        let payload =
            Payload::new(&KEY, vec![Representation::new("text/plain", text)]).expect("valid");
        let content_id = payload.descriptor().content_id();
        StampedOperation::new(
            id,
            ts,
            Operation::Add {
                content_id,
                payload,
            },
        )
    }

    fn touch_op(node_id: NodeId, counter: u64) -> StampedOperation {
        let id = OpId::new(node_id, counter).unwrap();
        let ts = HlcTimestamp::new(counter * 1000, 0);
        let payload =
            Payload::new(&KEY, vec![Representation::new("text/plain", b"x")]).expect("valid");
        let content_id = payload.descriptor().content_id();
        StampedOperation::new(id, ts, Operation::Touch { content_id })
    }

    #[test]
    fn ingest_new_operation() {
        let codec = JsonV1Codec;
        let mut state = AntiEntropyState::new();
        let op = make_op(node(1), 1, b"hello");
        let raw = codec.encode_op(&op).unwrap();

        let outcome = state.ingest_raw(&raw, &codec).unwrap();
        assert!(matches!(outcome, IngestOutcome::Applied(_)));
        assert!(state.seen().contains(op.id()));
        assert!(state.log().contains(op.id()));
    }

    #[test]
    fn compacted_operation_replay_does_not_restore_forwarding_entry() {
        let codec = JsonV1Codec;
        let op = make_op(node(1), 1, b"compacted");
        let raw = codec.encode_op(&op).unwrap();
        let mut seen = SeenOps::default();
        seen.record(op.id());
        let mut state = AntiEntropyState::restore(seen, OpLog::default());

        assert_eq!(
            state.ingest_raw(&raw, &codec).unwrap(),
            IngestOutcome::Duplicate
        );
        assert!(!state.log().contains(op.id()));
    }

    #[test]
    fn duplicate_ingest_is_idempotent() {
        let codec = JsonV1Codec;
        let mut state = AntiEntropyState::new();
        let op = make_op(node(1), 1, b"hello");
        let raw = codec.encode_op(&op).unwrap();

        state.ingest_raw(&raw, &codec).unwrap();
        let outcome = state.ingest_raw(&raw, &codec).unwrap();
        assert_eq!(outcome, IngestOutcome::Duplicate);
        assert_eq!(state.log().len(), 1);
    }

    #[test]
    fn conflicting_content_rejected() {
        let codec = JsonV1Codec;
        let mut state = AntiEntropyState::new();
        let op1 = make_op(node(1), 1, b"hello");
        let raw1 = codec.encode_op(&op1).unwrap();
        state.ingest_raw(&raw1, &codec).unwrap();

        // Fabricate a different operation with the same OpId
        let op2 = touch_op(node(1), 1);
        let raw2 = codec.encode_op(&op2).unwrap();
        assert!(state.ingest_raw(&raw2, &codec).is_err());
    }

    #[test]
    fn record_local_stores_and_tracks() {
        let codec = JsonV1Codec;
        let mut state = AntiEntropyState::new();
        let op = make_op(node(1), 1, b"local");
        state.record_local(&op, &codec).unwrap();

        assert!(state.seen().contains(op.id()));
        assert!(state.log().contains(op.id()));
    }

    #[test]
    fn batch_contains_only_what_remote_needs() {
        let codec = JsonV1Codec;
        let mut state = AntiEntropyState::new();
        let n = node(1);
        for i in 1..=5 {
            let op = make_op(n, i, format!("op{i}").as_bytes());
            state.record_local(&op, &codec).unwrap();
        }

        // Remote has seen ops 1-3 contiguously
        let mut remote = SeenOps::default();
        for i in 1..=3 {
            remote.record(OpId::new(n, i).unwrap());
        }

        let batch = state.compute_batch(&remote, &BatchLimits::default());
        assert_eq!(batch.len(), 2); // ops 4 and 5
        assert!(!batch.has_more());
    }

    #[test]
    fn batch_respects_max_ops() {
        let codec = JsonV1Codec;
        let mut state = AntiEntropyState::new();
        let n = node(1);
        for i in 1..=10 {
            let op = make_op(n, i, format!("op{i}").as_bytes());
            state.record_local(&op, &codec).unwrap();
        }

        let limits = BatchLimits {
            max_ops: 3,
            max_bytes: usize::MAX,
        };
        let batch = state.compute_batch(&SeenOps::default(), &limits);
        assert_eq!(batch.len(), 3);
        assert!(batch.has_more());
    }

    #[test]
    fn batch_respects_max_bytes() {
        let codec = JsonV1Codec;
        let mut state = AntiEntropyState::new();
        let n = node(1);
        for i in 1..=5 {
            let op = make_op(n, i, &[b'x'; 100]);
            state.record_local(&op, &codec).unwrap();
        }

        // Each entry is well over 100 bytes (envelope + JSON overhead).
        // Set a limit that allows only a couple.
        let one_entry_size = state.log().get(OpId::new(n, 1).unwrap()).unwrap().len();
        let limits = BatchLimits {
            max_ops: usize::MAX,
            max_bytes: one_entry_size * 2 + 1,
        };
        let batch = state.compute_batch(&SeenOps::default(), &limits);
        assert_eq!(batch.len(), 2);
        assert!(batch.has_more());
    }

    #[test]
    fn batch_skips_ops_remote_has_in_gaps() {
        let codec = JsonV1Codec;
        let mut state = AntiEntropyState::new();
        let n = node(1);
        for i in 1..=5 {
            let op = make_op(n, i, format!("op{i}").as_bytes());
            state.record_local(&op, &codec).unwrap();
        }

        // Remote has frontier=2 but also has op 4 as a sparse entry
        let mut remote = SeenOps::default();
        remote.record(OpId::new(n, 1).unwrap());
        remote.record(OpId::new(n, 2).unwrap());
        remote.record(OpId::new(n, 4).unwrap());

        let batch = state.compute_batch(&remote, &BatchLimits::default());
        // Should send ops 3 and 5 (not 4, which remote already has)
        assert_eq!(batch.len(), 2);

        // Verify the actual ops by decoding
        let decoded: Vec<StampedOperation> = batch
            .entries()
            .iter()
            .map(|raw| codec.decode_op(raw).unwrap())
            .collect();
        let counters: Vec<u64> = decoded.iter().map(|op| op.id().counter()).collect();
        assert_eq!(counters, vec![3, 5]);
    }

    #[test]
    fn empty_batch_when_fully_synced() {
        let codec = JsonV1Codec;
        let mut state = AntiEntropyState::new();
        let n = node(1);
        let op = make_op(n, 1, b"hello");
        state.record_local(&op, &codec).unwrap();

        let batch = state.compute_batch(state.seen(), &BatchLimits::default());
        assert!(batch.is_empty());
        assert!(!batch.has_more());
    }

    #[test]
    fn missing_locally_detects_gaps() {
        // We have ops 1-3 from node 1
        let n = node(1);
        let mut local = SeenOps::default();
        for i in 1..=3 {
            local.record(OpId::new(n, i).unwrap());
        }
        let state = AntiEntropyState::restore(local, OpLog::default());

        // Remote has ops 1-5
        let mut remote = SeenOps::default();
        for i in 1..=5 {
            remote.record(OpId::new(n, i).unwrap());
        }

        let missing = state.missing_locally(&remote);
        let counters: Vec<u64> = missing.iter().map(|id| id.counter()).collect();
        assert_eq!(counters, vec![4, 5]);
    }

    #[test]
    fn first_entry_always_included_even_if_over_byte_limit() {
        let codec = JsonV1Codec;
        let mut state = AntiEntropyState::new();
        let op = make_op(node(1), 1, &vec![b'x'; 500]);
        state.record_local(&op, &codec).unwrap();

        let limits = BatchLimits {
            max_ops: 100,
            max_bytes: 1, // Absurdly small
        };
        let batch = state.compute_batch(&SeenOps::default(), &limits);
        // First entry is always included to guarantee progress
        assert_eq!(batch.len(), 1);
    }

    #[test]
    fn hostile_remote_frontier_expansion_is_bounded() {
        let n = node(1);
        let remote: SeenOps = serde_json::from_str(&format!(
            r#"{{"nodes":{{"{n}":{{"frontier":{},"gaps":[]}}}}}}"#,
            u64::MAX
        ))
        .unwrap();

        let missing = AntiEntropyState::new().missing_locally(&remote);
        assert_eq!(missing.len(), MAX_MISSING_LOCALLY);
        assert_eq!(missing.first().unwrap().counter(), 1);
        assert_eq!(
            missing.last().unwrap().counter(),
            u64::try_from(MAX_MISSING_LOCALLY).unwrap()
        );
    }
}
