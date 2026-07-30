use super::support::*;

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
