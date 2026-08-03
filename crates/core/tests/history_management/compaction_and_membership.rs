use super::support::*;

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
            value: clip_sync_core::model::SettingValue::Bool(true),
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
fn authenticated_empty_member_blocks_compaction_until_its_durable_ack() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("member-gate.db");
    let key = storage_key();
    let mut history = HistoryStore::open(&path, &key).unwrap();
    let peer = node(222);

    history
        .ingest_authenticated_batch(
            peer,
            &clip_sync_core::model::SeenOps::default(),
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
            value: clip_sync_core::model::SettingValue::Bool(true),
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
    let mut frontier = clip_sync_core::model::SeenOps::default();
    frontier.record(add.id());
    history
        .ingest_authenticated_batch(peer, &frontier, &BTreeSet::from([peer]), &[add], 3)
        .unwrap();

    assert!(history.storage().load_operations().unwrap().is_empty());
    assert!(!history.projection().is_visible(id));
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
            value: clip_sync_core::model::SettingValue::Bool(true),
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
