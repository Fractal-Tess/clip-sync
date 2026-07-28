use std::collections::BTreeSet;

use clip_sync::{
    model::{
        Acknowledgements, ContentId, HlcTimestamp, NodeId, OpId, Operation, Payload, Projection,
        Representation, SharedSetting, StampedOperation,
    },
    replica::{Replica, ReplicaError},
    storage::{HistoryError, HistoryStore, StorageKey},
};
use uuid::Uuid;

const CONTENT_KEY: [u8; 32] = [41; 32];

fn node(value: u128) -> NodeId {
    NodeId::from_uuid(Uuid::from_u128(value))
}

fn payload(byte: u8, size: usize) -> Payload {
    Payload::new(
        &CONTENT_KEY,
        vec![Representation::new(
            "application/octet-stream",
            vec![byte; size],
        )],
    )
    .unwrap()
}

fn content_id(payload: &Payload) -> ContentId {
    payload.descriptor().content_id()
}

fn storage_key() -> StorageKey {
    StorageKey::derive_from_secret(
        b"history management integration secret",
        b"history management integration salt",
    )
    .unwrap()
}

#[test]
fn deterministic_quota_excludes_pins_and_explicit_oversized_shares() {
    let mut replica = Replica::new(node(1));
    replica
        .set_shared_setting(SharedSetting::MeshQuotaBytes, 5, 1)
        .unwrap();

    let pinned = payload(1, 4);
    let pinned_id = content_id(&pinned);
    replica.copy(pinned, 2).unwrap();
    replica.pin(pinned_id, 3).unwrap();

    let oldest_chargeable = payload(2, 4);
    let oldest_chargeable_id = content_id(&oldest_chargeable);
    replica.copy(oldest_chargeable, 4).unwrap();

    let exempt = payload(3, 6);
    let exempt_id = content_id(&exempt);
    let explicit = replica.share_explicit(exempt, 5).unwrap();
    assert!(matches!(
        explicit.operation(),
        Operation::AddQuotaExempt { .. }
    ));

    let newest_chargeable = payload(4, 4);
    let newest_chargeable_id = content_id(&newest_chargeable);
    replica.copy(newest_chargeable, 6).unwrap();

    let plan = replica.projection().effective_quota_plan();
    assert_eq!(plan.chargeable_bytes(), 8);
    assert_eq!(plan.excluded_bytes(), 10);
    assert_eq!(plan.evictions(), &[oldest_chargeable_id]);
    assert!(replica.projection().is_quota_exempt(exempt_id));

    let evictions = replica.enforce_quota(7).unwrap();
    assert_eq!(evictions.len(), 1);
    assert_eq!(
        evictions[0].operation().content_id(),
        Some(oldest_chargeable_id)
    );
    assert!(replica.projection().is_visible(pinned_id));
    assert!(replica.projection().is_visible(exempt_id));
    assert!(replica.projection().is_visible(newest_chargeable_id));
}

#[test]
fn independent_quota_authors_select_same_ids_and_converge() {
    let mut origin = Replica::new(node(1));
    origin
        .set_shared_setting(SharedSetting::MeshQuotaBytes, 5, 1)
        .unwrap();
    for (time, byte) in [(2, 1), (3, 2), (4, 3)] {
        origin.copy(payload(byte, 4), time).unwrap();
    }
    let projection = origin.projection().clone();
    let timestamp = origin.last_timestamp();

    let mut left = Replica::restore(node(2), 0, timestamp, projection.clone());
    let mut right = Replica::restore(node(3), 0, timestamp, projection);
    let left_deletes = left.enforce_quota(10).unwrap();
    let right_deletes = right.enforce_quota(10).unwrap();

    let left_ids = left_deletes
        .iter()
        .map(|operation| operation.operation().content_id().unwrap())
        .collect::<Vec<_>>();
    let right_ids = right_deletes
        .iter()
        .map(|operation| operation.operation().content_id().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(left_ids, right_ids);
    assert_eq!(left_ids.len(), 2);

    for operation in &right_deletes {
        left.ingest(operation, 20).unwrap();
    }
    for operation in &left_deletes {
        right.ingest(operation, 20).unwrap();
    }
    assert_eq!(left.projection(), right.projection());
    assert_eq!(left.projection().visible_items().len(), 1);
}

#[test]
fn deletion_blocks_old_replay_and_new_copy_reintroduces_unpinned_content() {
    let mut replica = Replica::new(node(1));
    let value = payload(7, 4);
    let id = content_id(&value);
    let old_add = replica.copy(value.clone(), 10).unwrap();
    replica.pin(id, 11).unwrap();
    let deletion = replica.delete(id, 12).unwrap();

    assert_eq!(
        replica.ingest(&old_add, 13).unwrap(),
        clip_sync::model::ApplyOutcome::Duplicate
    );
    assert!(!replica.projection().is_visible(id));

    let reintroduced = replica.copy(value, 14).unwrap();
    assert!(reintroduced.event_key() > deletion.event_key());
    assert!(replica.projection().is_visible(id));
    assert!(!replica.projection().is_pinned(id));
}

#[test]
fn malformed_and_unknown_ids_do_not_advance_replica() {
    let mut replica = Replica::new(node(1));
    let before = replica.last_counter();
    assert!(matches!(
        replica.pin_by_id("not-a-content-id", 1),
        Err(ReplicaError::InvalidContentId(_))
    ));
    assert_eq!(replica.last_counter(), before);

    let unknown = "00".repeat(32);
    assert!(matches!(
        replica.delete_by_id(&unknown, 2),
        Err(ReplicaError::ContentNotVisible(_))
    ));
    assert_eq!(replica.last_counter(), before);
}

#[test]
fn far_future_peer_timestamp_does_not_poison_local_clock() {
    let mut replica = Replica::new(node(1));
    let peer = node(2);
    let remote = StampedOperation::new(
        OpId::new(peer, 1).unwrap(),
        HlcTimestamp::new(clip_sync::replica::MAX_REMOTE_CLOCK_SKEW_MILLIS + 101, 0),
        Operation::SetSetting {
            key: "future-setting".to_owned(),
            value: clip_sync::model::SettingValue::Bool(true),
        },
    );

    assert!(matches!(
        replica.ingest(&remote, 100),
        Err(ReplicaError::RemoteClockTooFarAhead { .. })
    ));
    assert_eq!(replica.last_timestamp(), HlcTimestamp::default());
    assert!(replica.projection().setting("future-setting").is_none());
}

#[test]
fn history_store_persists_mutations_quota_and_restart_state() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("history.db");
    let key = storage_key();

    let (node_id, next_counter, retained_id, exempt_id) = {
        let mut history = HistoryStore::open(&path, &key).unwrap();
        history.set_mesh_quota_and_enforce(5, 1).unwrap();

        let pinned = payload(1, 4);
        let pinned_id = content_id(&pinned);
        history.copy(pinned, 2).unwrap();
        history.pin_by_id(&pinned_id.to_string(), 3).unwrap();

        let evicted = payload(2, 4);
        let evicted_id = content_id(&evicted);
        history.copy(evicted, 4).unwrap();
        let retained = payload(3, 4);
        let retained_id = content_id(&retained);
        let capture_batch = history.copy_and_enforce(retained, 5).unwrap();
        assert_eq!(capture_batch.len(), 2);
        assert_eq!(capture_batch[1].operation().content_id(), Some(evicted_id));
        let exempt = payload(4, 6);
        let exempt_id = content_id(&exempt);
        let share_batch = history.share_explicit_and_enforce(exempt, 6).unwrap();
        assert_eq!(share_batch.len(), 1);
        history.unpin(pinned_id, 8).unwrap();
        let second_delete = history.enforce_quota(9).unwrap();
        assert_eq!(second_delete.len(), 1);
        assert_eq!(second_delete[0].operation().content_id(), Some(pinned_id));

        let node_id = history.replica().node_id();
        let next_counter = history.replica().last_counter() + 1;
        (node_id, next_counter, retained_id, exempt_id)
    };

    let mut restarted = HistoryStore::open(&path, &key).unwrap();
    assert_eq!(restarted.replica().node_id(), node_id);
    assert_eq!(restarted.replica().last_counter() + 1, next_counter);
    assert_eq!(
        restarted
            .projection()
            .effective_shared_settings()
            .mesh_quota_bytes,
        5
    );
    assert!(restarted.projection().is_visible(retained_id));
    assert!(restarted.projection().is_visible(exempt_id));
    assert!(restarted.projection().is_quota_exempt(exempt_id));

    restarted.delete_by_id(&exempt_id.to_string(), 10).unwrap();
    drop(restarted);
    let mut restarted = HistoryStore::open(&path, &key).unwrap();
    assert!(!restarted.projection().is_visible(exempt_id));
    restarted.share_explicit(payload(4, 6), 11).unwrap();
    drop(restarted);
    let restarted = HistoryStore::open(&path, &key).unwrap();
    assert!(restarted.projection().is_visible(exempt_id));
    assert!(restarted.projection().is_quota_exempt(exempt_id));
}

#[test]
fn peer_batch_conflict_rolls_back_storage_and_live_projection() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("peer-batch.db");
    let key = storage_key();
    let peer = node(77);
    let first_payload = payload(1, 3);
    let first_id = content_id(&first_payload);
    let second_payload = payload(2, 3);
    let second_id = content_id(&second_payload);
    let operation_id = OpId::new(peer, 1).unwrap();
    let first = StampedOperation::new(
        operation_id,
        HlcTimestamp::new(100, 0),
        Operation::Add {
            content_id: first_id,
            payload: first_payload,
        },
    );
    let conflicting = StampedOperation::new(
        operation_id,
        HlcTimestamp::new(101, 0),
        Operation::Add {
            content_id: second_id,
            payload: second_payload,
        },
    );

    let mut history = HistoryStore::open(&path, &key).unwrap();
    assert!(history.ingest_batch(&[first, conflicting], 200).is_err());
    assert!(history.projection().visible_items().is_empty());
    assert!(history.storage().load_operations().unwrap().is_empty());

    drop(history);
    let restarted = HistoryStore::open(&path, &key).unwrap();
    assert!(restarted.projection().visible_items().is_empty());
}

#[test]
fn peer_ingest_hlc_and_acknowledgements_survive_restart() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("peer.db");
    let key = storage_key();
    let peer = node(99);
    let remote_payload = payload(8, 3);
    let remote_id = content_id(&remote_payload);
    let remote = StampedOperation::new(
        OpId::new(peer, 1).unwrap(),
        HlcTimestamp::new(1_000_000, 0),
        Operation::Add {
            content_id: remote_id,
            payload: remote_payload,
        },
    );

    {
        let mut history = HistoryStore::open(&path, &key).unwrap();
        history.ingest(&remote, 10).unwrap();
        let seen = history.projection().seen_ops().clone();
        history.record_peer_acknowledgement(peer, &seen).unwrap();
    }

    let mut restarted = HistoryStore::open(&path, &key).unwrap();
    assert!(restarted.projection().is_visible(remote_id));
    assert!(
        restarted
            .acknowledgements()
            .unwrap()
            .has_seen(peer, remote.id())
    );
    let local = restarted
        .set_shared_setting(SharedSetting::CaptureThresholdBytes, 20, 11)
        .unwrap();
    assert!(local.timestamp() > remote.timestamp());
}

#[test]
fn tombstones_require_all_active_member_acknowledgements_or_stable_forget() {
    let local = node(1);
    let peer = node(2);
    let mut local_replica = Replica::new(local);
    let value = payload(1, 3);
    let id = content_id(&value);
    local_replica.copy(value, 1).unwrap();

    let peer_setting = StampedOperation::new(
        OpId::new(peer, 1).unwrap(),
        HlcTimestamp::new(2, 0),
        Operation::SetSetting {
            key: "future_policy".to_owned(),
            value: clip_sync::model::SettingValue::Bool(true),
        },
    );
    local_replica.ingest(&peer_setting, 2).unwrap();
    let deletion = local_replica.delete(id, 3).unwrap();

    let mut acknowledgements = Acknowledgements::default();
    assert!(
        local_replica
            .projection()
            .collectable_tombstones(local, &acknowledgements)
            .is_empty()
    );

    acknowledgements.record(peer, local_replica.projection().seen_ops());
    let collectable = local_replica
        .projection()
        .collectable_tombstones(local, &acknowledgements);
    assert_eq!(collectable.len(), 1);
    assert_eq!(collectable[0].deletion().operation_id(), deletion.id());

    let mut without_ack = Replica::restore(
        local,
        local_replica.last_counter(),
        local_replica.last_timestamp(),
        local_replica.projection().clone(),
    );
    without_ack.forget_device(peer, 4).unwrap();
    assert_eq!(
        without_ack
            .projection()
            .collectable_tombstones(local, &Acknowledgements::default())
            .len(),
        1
    );
}

#[test]
fn invalid_store_id_is_typed_and_never_persisted() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("invalid.db");
    let key = storage_key();
    let mut history = HistoryStore::open(&path, &key).unwrap();
    let counter = history.replica().last_counter();

    assert!(matches!(
        history.delete_by_id("xyz", 1),
        Err(HistoryError::InvalidContentId(_))
    ));
    assert_eq!(history.replica().last_counter(), counter);
    drop(history);

    let history = HistoryStore::open(&path, &key).unwrap();
    assert_eq!(history.replica().last_counter(), counter);
    assert!(history.projection().visible_items().is_empty());
}

#[test]
fn invalid_known_setting_is_rejected_before_seen_state_changes() {
    let operation = StampedOperation::new(
        OpId::new(node(1), 1).unwrap(),
        HlcTimestamp::new(1, 0),
        Operation::SetSetting {
            key: SharedSetting::MeshQuotaBytes.key().to_owned(),
            value: clip_sync::model::SettingValue::Unsigned(0),
        },
    );
    let mut projection = Projection::default();
    assert!(projection.apply(&operation).is_err());
    assert!(!projection.seen_ops().contains(operation.id()));
}

#[test]
fn authenticated_empty_member_blocks_compaction_until_its_durable_ack() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("member-gate.db");
    let key = storage_key();
    let mut history = HistoryStore::open(&path, &key).unwrap();
    let peer = node(222);

    history
        .ingest_authenticated_batch(
            peer,
            &clip_sync::model::SeenOps::default(),
            &BTreeSet::from([peer]),
            &[],
            1,
        )
        .unwrap();
    let value = payload(8, 4);
    let id = content_id(&value);
    history.copy(value, 2).unwrap();
    let deletion = history.delete(id, 3).unwrap();

    assert!(
        history
            .compact_acknowledged_tombstones()
            .unwrap()
            .is_empty()
    );
    let frontier = history.projection().seen_ops().clone();
    history
        .record_peer_acknowledgement(peer, &frontier)
        .unwrap();
    let report = history.compact_acknowledged_tombstones().unwrap();
    assert_eq!(report.tombstones(), &[id]);
    assert!(report.operations().contains(&deletion.id()));
}

#[test]
fn peer_frontier_with_not_yet_durable_operations_blocks_compaction() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("frontier-race.db");
    let key = storage_key();
    let mut history = HistoryStore::open(&path, &key).unwrap();
    let peer = node(223);
    let peer_first = StampedOperation::new(
        OpId::new(peer, 1).unwrap(),
        HlcTimestamp::new(1, 0),
        Operation::SetSetting {
            key: "future_policy".to_owned(),
            value: clip_sync::model::SettingValue::Bool(true),
        },
    );
    history.ingest(&peer_first, 1).unwrap();
    let value = payload(9, 4);
    let id = content_id(&value);
    history.copy(value, 2).unwrap();
    history.delete(id, 3).unwrap();

    let mut advertised = history.projection().seen_ops().clone();
    let not_yet_durable = OpId::new(peer, 2).unwrap();
    advertised.record(not_yet_durable);
    history
        .record_peer_acknowledgement(peer, &advertised)
        .unwrap();
    assert!(
        history
            .compact_acknowledged_tombstones()
            .unwrap()
            .is_empty()
    );

    let delayed = StampedOperation::new(
        not_yet_durable,
        HlcTimestamp::new(2, 0),
        Operation::Touch { content_id: id },
    );
    history.ingest(&delayed, 4).unwrap();
    let report = history.compact_acknowledged_tombstones().unwrap();
    assert_eq!(report.tombstones(), &[id]);
}

#[test]
fn tombstone_operation_compaction_survives_restart_and_allows_newer_copy() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("compaction.db");
    let key = storage_key();
    let value = payload(4, 9);
    let id = content_id(&value);
    let deletion;

    {
        let mut history = HistoryStore::open(&path, &key).unwrap();
        history.copy(value.clone(), 1).unwrap();
        deletion = history.delete(id, 2).unwrap();
        let report = history.compact_acknowledged_tombstones().unwrap();
        assert_eq!(report.tombstones(), &[id]);
        assert_eq!(report.operations().len(), 2);
        assert!(history.storage().load_operations().unwrap().is_empty());
        assert!(history.projection().seen_ops().contains(deletion.id()));
    }

    let mut restarted = HistoryStore::open(&path, &key).unwrap();
    assert!(!restarted.projection().is_visible(id));
    assert!(restarted.projection().seen_ops().contains(deletion.id()));
    assert!(restarted.storage().load_operations().unwrap().is_empty());
    restarted.copy(value, 3).unwrap();
    assert!(restarted.projection().is_visible(id));
}

#[test]
fn compacted_operation_replay_does_not_restore_storage_rows() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("compacted-replay.db");
    let key = storage_key();
    let value = payload(14, 3);
    let id = content_id(&value);
    let mut history = HistoryStore::open(&path, &key).unwrap();
    let add = history.copy(value, 1).unwrap();
    history.delete(id, 2).unwrap();
    history.compact_acknowledged_tombstones().unwrap();
    assert!(history.storage().load_operations().unwrap().is_empty());

    let peer = node(77);
    let mut frontier = clip_sync::model::SeenOps::default();
    frontier.record(add.id());
    history
        .ingest_authenticated_batch(peer, &frontier, &BTreeSet::from([peer]), &[add], 3)
        .unwrap();

    assert!(history.storage().load_operations().unwrap().is_empty());
    assert!(!history.projection().is_visible(id));
}

#[test]
fn malformed_authenticated_setting_rolls_back_operation_and_acknowledgement() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("malformed-setting.db");
    let key = storage_key();
    let mut history = HistoryStore::open(&path, &key).unwrap();
    let peer = node(44);
    let operation = StampedOperation::new(
        OpId::new(peer, 1).unwrap(),
        HlcTimestamp::new(10, 0),
        Operation::SetSetting {
            key: SharedSetting::CaptureThresholdBytes.key().to_owned(),
            value: clip_sync::model::SettingValue::Unsigned(0),
        },
    );
    let mut advertised = clip_sync::model::SeenOps::default();
    advertised.record(operation.id());

    assert!(
        history
            .ingest_authenticated_batch(
                peer,
                &advertised,
                &BTreeSet::from([peer]),
                std::slice::from_ref(&operation),
                10,
            )
            .is_err()
    );
    drop(history);

    let restarted = HistoryStore::open(&path, &key).unwrap();
    assert!(!restarted.projection().seen_ops().contains(operation.id()));
    assert!(restarted.acknowledgements().unwrap().peer(peer).is_none());
}

#[test]
fn identity_reset_is_a_new_empty_member_not_key_revocation() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("reset.db");
    let key = storage_key();
    let mut history = HistoryStore::open(&path, &key).unwrap();
    let old = history.replica().node_id();
    history.copy(payload(1, 2), 1).unwrap();

    let replacement = history.reset_identity().unwrap();
    assert_ne!(replacement, old);
    assert!(history.projection().visible_items().is_empty());
    assert!(history.storage().load_operations().unwrap().is_empty());
    drop(history);

    let restarted = HistoryStore::open(&path, &key).unwrap();
    assert_eq!(restarted.replica().node_id(), replacement);
    assert!(restarted.projection().visible_items().is_empty());
}

#[test]
fn stable_forget_removes_old_ack_but_persists_rejection_state() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("forget-maintenance.db");
    let key = storage_key();
    let peer = node(55);
    let mut history = HistoryStore::open(&path, &key).unwrap();
    let peer_operation = StampedOperation::new(
        OpId::new(peer, 1).unwrap(),
        HlcTimestamp::new(1, 0),
        Operation::SetSetting {
            key: "future_policy".to_owned(),
            value: clip_sync::model::SettingValue::Bool(true),
        },
    );
    history.ingest(&peer_operation, 1).unwrap();
    let peer_frontier = history.projection().seen_ops().clone();
    history
        .record_peer_acknowledgement(peer, &peer_frontier)
        .unwrap();
    history.forget_device(peer, 2).unwrap();

    let report = history.compact_acknowledged_tombstones().unwrap();
    assert_eq!(report.removed_acknowledgements(), &[peer]);
    assert!(history.acknowledgements().unwrap().peer(peer).is_none());
    assert!(history.projection().is_device_forgotten(peer));
    drop(history);

    let restarted = HistoryStore::open(&path, &key).unwrap();
    assert!(restarted.projection().is_device_forgotten(peer));
    assert!(restarted.acknowledgements().unwrap().peer(peer).is_none());
}
