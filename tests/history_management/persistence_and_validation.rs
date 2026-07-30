use super::support::*;

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
