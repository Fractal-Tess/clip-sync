pub(super) use clip_sync_core::model::{
    HlcTimestamp, NodeId, OpId, Operation, Payload, Projection, Representation, SeenOps,
    StampedOperation,
};
pub(super) use clip_sync_core::replication::{BatchLimits, decode_operation};
use clip_sync_core::storage::{HistoryStore, OperationBatch, StorageKey};
use tempfile::TempDir;
pub(super) use uuid::Uuid;

// ── Helpers ────────────────────────────────────────────────────────────

pub(super) const CONTENT_KEY: [u8; 32] = [9; 32];

/// Far enough past every synthetic timestamp that ingest never sees skew.
const NOW_MILLIS: u64 = 1_000_000_000;

pub(super) fn node(id: u128) -> NodeId {
    NodeId::from_uuid(Uuid::from_u128(id))
}

fn text_payload(text: &[u8]) -> Payload {
    Payload::new(&CONTENT_KEY, vec![Representation::new("text/plain", text)]).expect("valid")
}

fn stamped(node_id: NodeId, counter: u64, operation: Operation) -> StampedOperation {
    let id = OpId::new(node_id, counter).unwrap();
    StampedOperation::new(id, HlcTimestamp::new(counter * 1000, 0), operation)
}

pub(super) fn make_add(node_id: NodeId, counter: u64, text: &[u8]) -> StampedOperation {
    let payload = text_payload(text);
    let content_id = payload.descriptor().content_id();
    stamped(
        node_id,
        counter,
        Operation::Add {
            content_id,
            payload,
        },
    )
}

pub(super) fn make_touch(node_id: NodeId, counter: u64, content_text: &[u8]) -> StampedOperation {
    let content_id = text_payload(content_text).descriptor().content_id();
    stamped(node_id, counter, Operation::Touch { content_id })
}

pub(super) fn make_delete(node_id: NodeId, counter: u64, content_text: &[u8]) -> StampedOperation {
    let content_id = text_payload(content_text).descriptor().content_id();
    stamped(node_id, counter, Operation::Delete { content_id })
}

/// One mesh member backed by a real encrypted store. Operations are authored
/// by synthetic node IDs, so every peer relays them exactly as it would relay
/// operations from another device.
pub(super) struct Peer {
    store: HistoryStore,
    _directory: TempDir,
}

impl Peer {
    pub(super) fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let key = StorageKey::derive_from_secret(b"anti-entropy test secret", b"salt").unwrap();
        let store = HistoryStore::open(directory.path().join("history.db"), &key).unwrap();
        Self {
            store,
            _directory: directory,
        }
    }

    /// Stores operations as if they had just arrived from another device.
    pub(super) fn receive(&mut self, operations: &[StampedOperation]) {
        self.store.ingest_batch(operations, NOW_MILLIS).unwrap();
    }

    /// Stores a batch exactly as it arrives over the network.
    pub(super) fn receive_encoded(&mut self, encoded: &[Vec<u8>]) {
        let operations = encoded
            .iter()
            .map(|bytes| decode_operation(bytes).unwrap())
            .collect::<Vec<_>>();
        self.receive(&operations);
    }

    pub(super) fn seen(&self) -> &SeenOps {
        self.store.projection().seen_ops()
    }

    pub(super) fn projection(&self) -> &Projection {
        self.store.projection()
    }

    pub(super) fn batch_for(&self, remote: &SeenOps, limits: &BatchLimits) -> OperationBatch {
        self.store.operation_batch(remote, limits).unwrap()
    }

    /// Every stored operation, in event order.
    pub(super) fn operations(&self) -> Vec<StampedOperation> {
        self.store.storage().load_operations().unwrap()
    }

    pub(super) fn operation(&self, id: OpId) -> Option<StampedOperation> {
        self.store.storage().load_operation(id).unwrap()
    }
}

/// Sends one batch from `sender` to `receiver` and returns it.
pub(super) fn sync_batch(sender: &Peer, receiver: &mut Peer, limits: &BatchLimits) -> usize {
    let batch = sender.batch_for(receiver.seen(), limits);
    receiver.receive_encoded(&batch.operations);
    batch.operations.len()
}

/// Fully synchronizes two peers by exchanging batches until both are idle.
pub(super) fn full_sync(a: &mut Peer, b: &mut Peer) {
    let limits = BatchLimits::default();
    loop {
        let ab = sync_batch(a, b, &limits);
        let ba = sync_batch(b, a, &limits);
        if ab == 0 && ba == 0 {
            break;
        }
    }
}
