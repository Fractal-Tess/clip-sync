pub(super) use std::collections::BTreeSet;

pub(super) use clip_sync_core::{
    model::{
        Acknowledgements, ContentId, HlcTimestamp, NodeId, OpId, Operation, Payload, Projection,
        Representation, SharedSetting, StampedOperation,
    },
    replica::{Replica, ReplicaError},
    storage::{HistoryError, HistoryStore, StorageKey},
};
pub(super) use uuid::Uuid;

pub(super) const CONTENT_KEY: [u8; 32] = [41; 32];

pub(super) fn node(value: u128) -> NodeId {
    NodeId::from_uuid(Uuid::from_u128(value))
}

pub(super) fn payload(byte: u8, size: usize) -> Payload {
    Payload::new(
        &CONTENT_KEY,
        vec![Representation::new(
            "application/octet-stream",
            vec![byte; size],
        )],
    )
    .unwrap()
}

pub(super) fn content_id(payload: &Payload) -> ContentId {
    payload.descriptor().content_id()
}

pub(super) fn storage_key() -> StorageKey {
    StorageKey::derive_from_secret(
        b"history management integration secret",
        b"history management integration salt",
    )
    .unwrap()
}
