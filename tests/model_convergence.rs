use clip_sync::model::{
    ApplyOutcome, ContentId, HlcTimestamp, NodeId, OpId, Operation, Payload, Projection,
    Representation, SettingValue, StampedOperation,
};
use proptest::prelude::*;
use uuid::Uuid;

const CONTENT_KEY: [u8; blake3::KEY_LEN] = [19; blake3::KEY_LEN];

fn node(value: u128) -> NodeId {
    NodeId::from_uuid(Uuid::from_u128(value))
}

fn payload(mime: &str, bytes: &[u8]) -> Payload {
    Payload::new(
        &CONTENT_KEY,
        vec![Representation::new(mime, bytes.to_vec())],
    )
    .unwrap()
}

fn stamp(
    node_id: NodeId,
    counter: u64,
    physical_millis: u64,
    logical: u32,
    operation: Operation,
) -> StampedOperation {
    StampedOperation::new(
        OpId::new(node_id, counter).unwrap(),
        HlcTimestamp::new(physical_millis, logical),
        operation,
    )
}

#[allow(clippy::too_many_lines)]
fn convergence_fixture() -> (Vec<StampedOperation>, ContentId, ContentId) {
    let first_node = node(1);
    let second_node = node(2);
    let third_node = node(3);
    let alpha = payload("text/plain;charset=utf-8", b"alpha");
    let beta = payload("image/png", &[0x89, b'P', b'N', b'G']);
    let alpha_id = alpha.descriptor().content_id();
    let beta_id = beta.descriptor().content_id();

    let operations = vec![
        stamp(
            first_node,
            1,
            10,
            0,
            Operation::Add {
                content_id: alpha_id,
                payload: alpha.clone(),
            },
        ),
        stamp(
            second_node,
            1,
            11,
            0,
            Operation::Touch {
                content_id: alpha_id,
            },
        ),
        stamp(
            third_node,
            1,
            12,
            0,
            Operation::Delete {
                content_id: alpha_id,
            },
        ),
        stamp(
            first_node,
            2,
            13,
            0,
            Operation::Add {
                content_id: alpha_id,
                payload: alpha,
            },
        ),
        stamp(
            second_node,
            2,
            14,
            0,
            Operation::SetPin {
                content_id: alpha_id,
                pinned: true,
            },
        ),
        stamp(
            first_node,
            3,
            9,
            0,
            Operation::SetPin {
                content_id: alpha_id,
                pinned: false,
            },
        ),
        stamp(
            third_node,
            2,
            15,
            0,
            Operation::Add {
                content_id: beta_id,
                payload: beta,
            },
        ),
        stamp(
            first_node,
            4,
            16,
            0,
            Operation::SetSetting {
                key: "mesh_quota_bytes".into(),
                value: SettingValue::Unsigned(512),
            },
        ),
        stamp(
            second_node,
            3,
            16,
            0,
            Operation::SetSetting {
                key: "mesh_quota_bytes".into(),
                value: SettingValue::Unsigned(1_024),
            },
        ),
        stamp(
            third_node,
            3,
            17,
            0,
            Operation::ForgetDevice { node_id: node(99) },
        ),
    ];

    (operations, alpha_id, beta_id)
}

#[test]
fn replayed_operations_are_idempotent() {
    let (operations, _, _) = convergence_fixture();
    let mut projection = Projection::default();
    projection.apply_all(&operations).unwrap();
    let once = projection.clone();

    for operation in &operations {
        assert_eq!(projection.apply(operation), Ok(ApplyOutcome::Duplicate));
    }

    assert_eq!(projection, once);
}

#[test]
fn add_rejects_a_payload_with_a_different_content_id() {
    let first = payload("text/plain", b"first");
    let second = payload("text/plain", b"second");
    let operation = stamp(
        node(1),
        1,
        1,
        0,
        Operation::Add {
            content_id: first.descriptor().content_id(),
            payload: second,
        },
    );
    let mut projection = Projection::default();

    assert!(projection.apply(&operation).is_err());
    assert!(!projection.seen_ops().contains(operation.id()));
}

#[test]
fn old_add_cannot_resurrect_but_new_activity_can() {
    let origin = node(1);
    let peer = node(2);
    let content = payload("text/plain", b"value");
    let content_id = content.descriptor().content_id();
    let old_add = stamp(
        origin,
        1,
        10,
        0,
        Operation::Add {
            content_id,
            payload: content,
        },
    );
    let deletion = stamp(peer, 1, 20, 0, Operation::Delete { content_id });
    let new_touch = stamp(origin, 2, 21, 0, Operation::Touch { content_id });
    let mut projection = Projection::default();

    projection.apply(&deletion).unwrap();
    projection.apply(&old_add).unwrap();
    assert!(!projection.is_visible(content_id));

    projection.apply(&new_touch).unwrap();
    assert!(projection.is_visible(content_id));
}

#[test]
fn equal_hlc_settings_use_node_and_counter_tie_break() {
    let low_node = node(1);
    let high_node = node(2);
    let low = stamp(
        low_node,
        1,
        100,
        0,
        Operation::SetSetting {
            key: "capture_threshold".into(),
            value: SettingValue::Unsigned(10),
        },
    );
    let high = stamp(
        high_node,
        1,
        100,
        0,
        Operation::SetSetting {
            key: "capture_threshold".into(),
            value: SettingValue::Unsigned(20),
        },
    );

    for order in [[&low, &high], [&high, &low]] {
        let mut projection = Projection::default();
        projection.apply_all(order).unwrap();
        assert_eq!(
            projection.setting("capture_threshold"),
            Some(&SettingValue::Unsigned(20))
        );
    }
}

#[test]
fn fixture_has_expected_merged_timeline_and_registers() {
    let (operations, alpha_id, beta_id) = convergence_fixture();
    let mut projection = Projection::default();
    projection.apply_all(operations.iter().rev()).unwrap();

    let visible = projection.visible_items();
    assert_eq!(
        visible
            .iter()
            .map(|item| item.content_id())
            .collect::<Vec<_>>(),
        vec![beta_id, alpha_id]
    );
    assert!(projection.is_pinned(alpha_id));
    assert_eq!(
        projection.setting("mesh_quota_bytes"),
        Some(&SettingValue::Unsigned(1_024))
    );
    assert!(projection.is_device_forgotten(node(99)));
}

#[test]
fn partitioned_shared_settings_and_membership_converge_after_heal() {
    let a = node(10);
    let b = node(20);
    let retired = node(30);
    let operations = [
        stamp(
            a,
            1,
            100,
            0,
            Operation::SetSetting {
                key: "mesh_quota_bytes".to_owned(),
                value: SettingValue::Unsigned(100),
            },
        ),
        stamp(
            b,
            1,
            101,
            0,
            Operation::SetSetting {
                key: "capture_threshold_bytes".to_owned(),
                value: SettingValue::Unsigned(50),
            },
        ),
        stamp(
            retired,
            1,
            102,
            0,
            Operation::SetSetting {
                key: "mesh_quota_bytes".to_owned(),
                value: SettingValue::Unsigned(200),
            },
        ),
        stamp(a, 2, 103, 0, Operation::ForgetDevice { node_id: retired }),
    ];

    let deliveries = [[0_usize, 3, 1, 2], [1, 2, 3, 0], [2, 1, 0, 3]];
    let projections = deliveries.map(|delivery| {
        let mut projection = Projection::default();
        for index in delivery {
            projection.apply(&operations[index]).unwrap();
        }
        projection
    });

    assert_eq!(projections[0], projections[1]);
    assert_eq!(projections[1], projections[2]);
    assert_eq!(
        projections[0].effective_shared_settings().mesh_quota_bytes,
        200
    );
    assert!(projections[0].is_device_forgotten(retired));
}

proptest! {
    #[test]
    fn replicas_converge_under_reordering_and_duplication(
        left_delivery in prop::collection::vec(0_usize..10, 0..100),
        right_delivery in prop::collection::vec(0_usize..10, 0..100),
    ) {
        let (operations, _, _) = convergence_fixture();
        let mut left = Projection::default();
        let mut right = Projection::default();

        for index in left_delivery.into_iter().chain(0..operations.len()) {
            left.apply(&operations[index]).unwrap();
        }
        for index in right_delivery
            .into_iter()
            .chain((0..operations.len()).rev())
        {
            right.apply(&operations[index]).unwrap();
        }

        prop_assert_eq!(left, right);
    }
}
