pub(super) use clip_sync_core::model::{
    HlcTimestamp, NodeId, OpId, Operation, Payload, Projection, Representation, SeenOps,
    StampedOperation,
};
pub(super) use clip_sync_core::replication::{
    AntiEntropyState, BatchLimits, Codec, IngestOutcome, JsonV1Codec, OpBatch,
};
pub(super) use uuid::Uuid;

// ── Helpers ────────────────────────────────────────────────────────────

pub(super) const CONTENT_KEY: [u8; 32] = [9; 32];

pub(super) fn node(id: u128) -> NodeId {
    NodeId::from_uuid(Uuid::from_u128(id))
}

pub(super) fn make_add(node_id: NodeId, counter: u64, text: &[u8]) -> StampedOperation {
    let id = OpId::new(node_id, counter).unwrap();
    let ts = HlcTimestamp::new(counter * 1000, 0);
    let payload =
        Payload::new(&CONTENT_KEY, vec![Representation::new("text/plain", text)]).expect("valid");
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

pub(super) fn make_touch(node_id: NodeId, counter: u64, content_text: &[u8]) -> StampedOperation {
    let id = OpId::new(node_id, counter).unwrap();
    let ts = HlcTimestamp::new(counter * 1000, 0);
    let payload = Payload::new(
        &CONTENT_KEY,
        vec![Representation::new("text/plain", content_text)],
    )
    .expect("valid");
    let content_id = payload.descriptor().content_id();
    StampedOperation::new(id, ts, Operation::Touch { content_id })
}

pub(super) fn make_delete(node_id: NodeId, counter: u64, content_text: &[u8]) -> StampedOperation {
    let id = OpId::new(node_id, counter).unwrap();
    let ts = HlcTimestamp::new(counter * 1000, 0);
    let payload = Payload::new(
        &CONTENT_KEY,
        vec![Representation::new("text/plain", content_text)],
    )
    .expect("valid");
    let content_id = payload.descriptor().content_id();
    StampedOperation::new(id, ts, Operation::Delete { content_id })
}

/// Encode an op, ingest it into a state, and apply to projection.
pub(super) fn ingest_and_apply(
    state: &mut AntiEntropyState,
    projection: &mut Projection,
    raw: &[u8],
    codec: &JsonV1Codec,
) {
    match state.ingest_raw(raw, codec).unwrap() {
        IngestOutcome::Applied(op) => {
            projection.apply(&op).unwrap();
        }
        IngestOutcome::Duplicate => {}
    }
}

/// Transfer a full batch from sender to receiver, applying to projection.
pub(super) fn sync_batch(
    sender: &AntiEntropyState,
    receiver: &mut AntiEntropyState,
    receiver_proj: &mut Projection,
    limits: &BatchLimits,
    codec: &JsonV1Codec,
) -> OpBatch {
    let batch = sender.compute_batch(receiver.seen(), limits);
    for entry in batch.entries() {
        ingest_and_apply(receiver, receiver_proj, entry, codec);
    }
    batch
}

/// Fully synchronize two nodes by exchanging batches until both are idle.
pub(super) fn full_sync(
    a: &mut AntiEntropyState,
    a_proj: &mut Projection,
    b: &mut AntiEntropyState,
    b_proj: &mut Projection,
    codec: &JsonV1Codec,
) {
    let limits = BatchLimits::default();
    loop {
        let ab = sync_batch(a, b, b_proj, &limits, codec);
        let ba = sync_batch(b, a, a_proj, &limits, codec);
        if ab.is_empty() && ba.is_empty() {
            break;
        }
    }
}
