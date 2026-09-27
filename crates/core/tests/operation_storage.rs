use clip_sync_core::model::{
    ContentId, HlcTimestamp, NodeId, OpId, Operation, Payload, Representation, StampedOperation,
};
use clip_sync_core::storage::{AppendOutcome, EncryptedStorage, StorageError, StorageKey};
use rusqlite::Connection;
use uuid::Uuid;

fn storage_key() -> StorageKey {
    StorageKey::derive_from_secret(
        b"operation storage integration test secret",
        b"operation storage integration test salt",
    )
    .unwrap()
}

/// A touch whose content ID encodes `key` and `value`, so different values
/// give different bytes under the same operation ID.
fn marker_operation(
    node: NodeId,
    counter: u64,
    timestamp: HlcTimestamp,
    key: &str,
    value: i64,
) -> StampedOperation {
    StampedOperation::new(
        OpId::new(node, counter).unwrap(),
        timestamp,
        Operation::Touch {
            content_id: marker(key, value),
        },
    )
}

fn marker(key: &str, value: i64) -> ContentId {
    ContentId::from_bytes(*blake3::hash(format!("{key}={value}").as_bytes()).as_bytes())
}

#[test]
fn version_one_database_migrates_and_keeps_its_new_replica_identity() {
    let temp_dir = tempfile::tempdir().unwrap();
    let path = temp_dir.path().join("migration.db");
    let key_bytes = [17_u8; 32];
    let key = StorageKey::from_bytes(key_bytes);
    let key_hex = hex::encode(key_bytes);

    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(&format!(
            "PRAGMA key = \"x'{key_hex}'\";
             CREATE TABLE storage_meta (
                 key TEXT PRIMARY KEY NOT NULL CHECK (length(key) > 0),
                 value TEXT NOT NULL
             ) STRICT;
             INSERT INTO storage_meta (key, value) VALUES ('schema_version', '1');
             PRAGMA user_version = 1;"
        ))
        .unwrap();
    connection.close().unwrap();

    let metadata = {
        let storage = EncryptedStorage::open(&path, &key).unwrap();
        assert_eq!(
            storage.meta_value("schema_version").unwrap().as_deref(),
            Some("6")
        );
        assert!(storage.load_operations().unwrap().is_empty());
        storage.replica_metadata().unwrap()
    };

    let storage = EncryptedStorage::open(&path, &key).unwrap();
    assert_eq!(storage.replica_metadata().unwrap(), metadata);
}

/// Builds a database exactly as 0.3 left it: schema 4, operations as JSON.
#[test]
fn a_0_3_database_is_converted_to_protobuf_and_indexed() {
    let temp_dir = tempfile::tempdir().unwrap();
    let path = temp_dir.path().join("legacy.db");
    let key_bytes = [29_u8; 32];
    let key = StorageKey::from_bytes(key_bytes);
    let remote = NodeId::from_uuid(Uuid::from_u128(7));
    let payload = Payload::new(
        &[3; 32],
        vec![Representation::new("text/plain", b"kept".to_vec())],
    )
    .unwrap();
    let content_id = payload.descriptor().content_id();
    let legacy_add = format!(
        r#"{{"id":{{"node":"{remote}","counter":1}},"timestamp":{{"physical_millis":1000,"logical":0}},"operation":{{"type":"add","content_id":"{content_id}","payload":{}}}}}"#,
        serde_json::to_string(&payload).unwrap()
    );
    let legacy_setting = format!(
        r#"{{"id":{{"node":"{remote}","counter":2}},"timestamp":{{"physical_millis":1001,"logical":0}},"operation":{{"type":"set_setting","key":"mesh_quota_bytes","value":{{"type":"unsigned","value":5}}}}}}"#
    );
    EncryptedStorage::open(&path, &key)
        .unwrap()
        .close()
        .unwrap();

    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(&format!(
            "PRAGMA key = \"x'{}'\";
             DROP TABLE operations;
             DROP TABLE local_sources;
             CREATE TABLE operations (
                 origin_node BLOB NOT NULL CHECK (length(origin_node) = 16),
                 counter INTEGER NOT NULL,
                 hlc_physical_millis INTEGER NOT NULL,
                 hlc_logical INTEGER NOT NULL,
                 encoding_version INTEGER NOT NULL CHECK (encoding_version = 1),
                 payload BLOB NOT NULL,
                 PRIMARY KEY (origin_node, counter)
             ) STRICT, WITHOUT ROWID;
             CREATE INDEX operations_event_order
                 ON operations (hlc_physical_millis, hlc_logical, origin_node, counter);
             UPDATE storage_meta SET value = '4' WHERE key = 'schema_version';
             PRAGMA user_version = 4;",
            hex::encode(key_bytes)
        ))
        .unwrap();
    for (counter, millis, json) in [(1, 1000, &legacy_add), (2, 1001, &legacy_setting)] {
        connection
            .execute(
                "INSERT INTO operations VALUES (?1, ?2, ?3, 0, 1, ?4)",
                rusqlite::params![
                    &remote.as_uuid().as_bytes()[..],
                    counter,
                    millis,
                    json.as_bytes()
                ],
            )
            .unwrap();
    }
    connection.close().unwrap();

    let storage = EncryptedStorage::open(&path, &key).unwrap();
    assert_eq!(
        storage.meta_value("schema_version").unwrap().as_deref(),
        Some("6")
    );
    let operations = storage.load_operations().unwrap();
    assert_eq!(
        operations[0].operation(),
        &Operation::Add {
            content_id,
            payload
        }
    );
    assert_eq!(operations[1].operation(), &Operation::Retired);
    assert!(storage.rebuild_projection().unwrap().is_visible(content_id));
    storage.close().unwrap();

    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(&format!("PRAGMA key = \"x'{}'\";", hex::encode(key_bytes)))
        .unwrap();
    let indexed: Vec<u8> = connection
        .query_row(
            "SELECT content_id FROM operations WHERE counter = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(indexed, content_id.as_bytes());
}

#[test]
fn future_schema_version_is_rejected_without_downgrade() {
    let temp_dir = tempfile::tempdir().unwrap();
    let path = temp_dir.path().join("future-schema.db");
    let key_bytes = [23_u8; 32];
    let key = StorageKey::from_bytes(key_bytes);
    EncryptedStorage::open(&path, &key)
        .unwrap()
        .close()
        .unwrap();

    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(&format!(
            "PRAGMA key = \"x'{}'\";
             UPDATE storage_meta SET value = '999' WHERE key = 'schema_version';
             PRAGMA user_version = 999;",
            hex::encode(key_bytes)
        ))
        .unwrap();
    connection.close().unwrap();

    assert!(matches!(
        EncryptedStorage::open(&path, &key),
        Err(StorageError::IncompatibleSchema(message))
            if message.contains("unsupported schema_version 999")
    ));

    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(&format!("PRAGMA key = \"x'{}'\";", hex::encode(key_bytes)))
        .unwrap();
    let version: String = connection
        .query_row(
            "SELECT value FROM storage_meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(version, "999");
}

#[test]
fn restart_recovers_operations_projection_and_replica_metadata() {
    let temp_dir = tempfile::tempdir().unwrap();
    let path = temp_dir.path().join("operations.db");
    let key = storage_key();

    let (operation, expected_metadata) = {
        let mut storage = EncryptedStorage::open(&path, &key).unwrap();
        let metadata = storage.replica_metadata().unwrap();
        let operation = marker_operation(
            metadata.node_id(),
            metadata.next_operation_counter(),
            HlcTimestamp::new(1_000, 0),
            "history_limit",
            250,
        );
        assert_eq!(
            storage.append_local_operation(&operation).unwrap(),
            AppendOutcome::Inserted
        );
        let expected_metadata = storage.replica_metadata().unwrap();
        storage.close().unwrap();
        (operation, expected_metadata)
    };

    let storage = EncryptedStorage::open(&path, &key).unwrap();
    assert_eq!(storage.replica_metadata().unwrap(), expected_metadata);
    assert_eq!(storage.load_operations().unwrap(), vec![operation]);
    assert!(
        storage
            .rebuild_projection()
            .unwrap()
            .is_visible(marker("history_limit", 250))
    );
}

#[test]
fn exact_operation_replay_is_idempotent_across_restart() {
    let temp_dir = tempfile::tempdir().unwrap();
    let path = temp_dir.path().join("idempotent.db");
    let key = storage_key();
    let node = NodeId::from_uuid(Uuid::from_u128(42));
    let operation = marker_operation(node, 7, HlcTimestamp::new(500, 3), "quota", 1024);

    {
        let mut storage = EncryptedStorage::open(&path, &key).unwrap();
        assert_eq!(
            storage.append_operation(&operation).unwrap(),
            AppendOutcome::Inserted
        );
        assert_eq!(
            storage.append_operation(&operation).unwrap(),
            AppendOutcome::AlreadyPresent
        );
    }

    let mut storage = EncryptedStorage::open(&path, &key).unwrap();
    assert_eq!(
        storage.append_operation(&operation).unwrap(),
        AppendOutcome::AlreadyPresent
    );
    assert_eq!(storage.load_operations().unwrap(), vec![operation]);
}

#[test]
fn reusing_an_operation_id_with_different_bytes_is_rejected() {
    let temp_dir = tempfile::tempdir().unwrap();
    let path = temp_dir.path().join("conflict.db");
    let key = storage_key();
    let node = NodeId::from_uuid(Uuid::from_u128(7));
    let original = marker_operation(node, 1, HlcTimestamp::new(10, 0), "theme", 1);
    let conflicting = marker_operation(node, 1, HlcTimestamp::new(10, 0), "theme", 2);

    let mut storage = EncryptedStorage::open(&path, &key).unwrap();
    storage.append_operation(&original).unwrap();
    assert!(matches!(
        storage.append_operation(&conflicting),
        Err(StorageError::OperationConflict(id)) if id == original.id()
    ));
    assert_eq!(storage.load_operations().unwrap(), vec![original]);
}

#[test]
fn payload_reconstructs_with_exact_mime_names_and_bytes() {
    const CONTENT_KEY: [u8; 32] = [91; 32];

    let temp_dir = tempfile::tempdir().unwrap();
    let path = temp_dir.path().join("payload.db");
    let key = storage_key();
    let node = NodeId::from_uuid(Uuid::from_u128(99));
    let payload = Payload::new(
        &CONTENT_KEY,
        vec![
            Representation::new("text/plain;charset=utf-8", vec![0, 1, 2, 0, 255]),
            Representation::new("text/html", b"<p>exact \0 bytes</p>".to_vec()),
        ],
    )
    .unwrap();
    let content_id = payload.descriptor().content_id();
    let operation = StampedOperation::new(
        OpId::new(node, 3).unwrap(),
        HlcTimestamp::new(700, 4),
        Operation::Add {
            content_id,
            payload,
        },
    );

    {
        let mut storage = EncryptedStorage::open(&path, &key).unwrap();
        storage.append_operation(&operation).unwrap();
    }

    let storage = EncryptedStorage::open(&path, &key).unwrap();
    let loaded = storage.load_operations().unwrap();
    assert_eq!(loaded, vec![operation.clone()]);
    let Operation::Add { payload, .. } = loaded[0].operation() else {
        panic!("loaded operation changed variant");
    };
    assert_eq!(payload.representations()[0].mime(), "text/html");
    assert_eq!(
        payload.representations()[0].bytes(),
        b"<p>exact \0 bytes</p>"
    );
    assert_eq!(payload.representations()[1].bytes(), &[0, 1, 2, 0, 255]);
}

#[test]
fn local_append_persists_counter_and_hlc_atomically() {
    let temp_dir = tempfile::tempdir().unwrap();
    let path = temp_dir.path().join("atomic.db");
    let key = storage_key();

    let expected = {
        let mut storage = EncryptedStorage::open(&path, &key).unwrap();
        let initial = storage.replica_metadata().unwrap();
        let first = marker_operation(
            initial.node_id(),
            initial.next_operation_counter(),
            HlcTimestamp::new(100, 2),
            "first",
            1,
        );
        storage.append_local_operation(&first).unwrap();

        let advanced = storage.replica_metadata().unwrap();
        assert_eq!(advanced.node_id(), initial.node_id());
        assert_eq!(advanced.next_operation_counter(), 2);
        assert_eq!(advanced.last_hlc(), HlcTimestamp::new(100, 2));

        let invalid = marker_operation(
            advanced.node_id(),
            advanced.next_operation_counter(),
            advanced.last_hlc(),
            "must_rollback",
            2,
        );
        assert!(matches!(
            storage.append_local_operation(&invalid),
            Err(StorageError::HlcRegression { .. })
        ));
        assert_eq!(storage.replica_metadata().unwrap(), advanced);
        assert_eq!(storage.load_operations().unwrap(), vec![first]);
        advanced
    };

    let storage = EncryptedStorage::open(&path, &key).unwrap();
    assert_eq!(storage.replica_metadata().unwrap(), expected);
    assert_eq!(storage.load_operations().unwrap().len(), 1);
}

#[test]
fn operations_outside_sqlite_integer_bounds_are_rejected() {
    let temp_dir = tempfile::tempdir().unwrap();
    let path = temp_dir.path().join("bounds.db");
    let key = storage_key();
    let mut storage = EncryptedStorage::open(&path, &key).unwrap();
    let node = NodeId::from_uuid(Uuid::from_u128(123));
    let operation = marker_operation(
        node,
        i64::MAX as u64 + 1,
        HlcTimestamp::new(1, 0),
        "too_large",
        1,
    );

    assert!(matches!(
        storage.append_operation(&operation),
        Err(StorageError::IntegerOutOfRange {
            field: "operation counter",
            ..
        })
    ));
    assert!(storage.load_operations().unwrap().is_empty());
}

#[test]
fn remote_batch_is_atomic_on_operation_conflict() {
    let temp_dir = tempfile::tempdir().unwrap();
    let path = temp_dir.path().join("remote-batch-atomic.db");
    let key = storage_key();
    let node = NodeId::from_uuid(Uuid::from_u128(700));
    let original = marker_operation(node, 1, HlcTimestamp::new(10, 0), "value", 1);
    let new_operation = marker_operation(node, 2, HlcTimestamp::new(11, 0), "next", 2);
    let conflict = marker_operation(node, 1, HlcTimestamp::new(10, 0), "value", 99);

    let mut storage = EncryptedStorage::open(&path, &key).unwrap();
    storage.append_operation(&original).unwrap();
    let metadata_before = storage.local_replica_metadata().unwrap();
    assert!(matches!(
        storage.append_remote_operations(
            &[new_operation.clone(), conflict],
            HlcTimestamp::new(20, 0),
        ),
        Err(StorageError::OperationConflict(id)) if id == original.id()
    ));

    assert_eq!(storage.load_operations().unwrap(), vec![original]);
    assert_eq!(storage.local_replica_metadata().unwrap(), metadata_before);
}

#[test]
fn remote_batch_persists_observed_clock_without_advancing_local_counter() {
    let temp_dir = tempfile::tempdir().unwrap();
    let path = temp_dir.path().join("remote-clock.db");
    let key = storage_key();
    let remote = NodeId::from_uuid(Uuid::from_u128(701));
    let operation = marker_operation(remote, 1, HlcTimestamp::new(500, 3), "remote", 1);

    {
        let mut storage = EncryptedStorage::open(&path, &key).unwrap();
        let initial = storage.local_replica_metadata().unwrap();
        assert_eq!(
            storage
                .append_remote_operations(
                    std::slice::from_ref(&operation),
                    HlcTimestamp::new(500, 4),
                )
                .unwrap(),
            1
        );
        let advanced = storage.local_replica_metadata().unwrap();
        assert_eq!(
            advanced.next_operation_counter(),
            initial.next_operation_counter()
        );
        assert_eq!(advanced.last_hlc(), HlcTimestamp::new(500, 4));
    }

    let storage = EncryptedStorage::open(&path, &key).unwrap();
    assert_eq!(
        storage.local_replica_metadata().unwrap().last_hlc(),
        HlcTimestamp::new(500, 4)
    );
    assert_eq!(storage.load_operations().unwrap(), vec![operation]);
}

#[test]
fn remote_batch_cannot_claim_a_new_local_operation_id() {
    let temp_dir = tempfile::tempdir().unwrap();
    let path = temp_dir.path().join("remote-local-id.db");
    let key = storage_key();
    let mut storage = EncryptedStorage::open(&path, &key).unwrap();
    let metadata = storage.local_replica_metadata().unwrap();
    let forged = marker_operation(
        metadata.node_id(),
        metadata.next_operation_counter(),
        HlcTimestamp::new(100, 0),
        "forged",
        1,
    );

    assert!(matches!(
        storage.append_remote_operations(
            std::slice::from_ref(&forged),
            HlcTimestamp::new(100, 1),
        ),
        Err(StorageError::RemoteOperationClaimsLocalIdentity(node))
            if node == metadata.node_id()
    ));
    assert!(storage.load_operations().unwrap().is_empty());
    assert_eq!(storage.local_replica_metadata().unwrap(), metadata);
}
