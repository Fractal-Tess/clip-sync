use clip_sync::model::{
    HlcTimestamp, NodeId, OpId, Operation, Payload, Representation, SettingValue, StampedOperation,
};
use clip_sync::storage::{AppendOutcome, EncryptedStorage, StorageError, StorageKey};
use rusqlite::Connection;
use uuid::Uuid;

fn storage_key() -> StorageKey {
    StorageKey::derive_from_secret(
        b"operation storage integration test secret",
        b"operation storage integration test salt",
    )
    .unwrap()
}

fn setting_operation(
    node: NodeId,
    counter: u64,
    timestamp: HlcTimestamp,
    key: &str,
    value: i64,
) -> StampedOperation {
    StampedOperation::new(
        OpId::new(node, counter).unwrap(),
        timestamp,
        Operation::SetSetting {
            key: key.to_owned(),
            value: SettingValue::Integer(value),
        },
    )
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
            Some("3")
        );
        assert!(storage.load_operations().unwrap().is_empty());
        storage.replica_metadata().unwrap()
    };

    let storage = EncryptedStorage::open(&path, &key).unwrap();
    assert_eq!(storage.replica_metadata().unwrap(), metadata);
}

#[test]
fn restart_recovers_operations_projection_and_replica_metadata() {
    let temp_dir = tempfile::tempdir().unwrap();
    let path = temp_dir.path().join("operations.db");
    let key = storage_key();

    let (operation, expected_metadata) = {
        let mut storage = EncryptedStorage::open(&path, &key).unwrap();
        let metadata = storage.replica_metadata().unwrap();
        let operation = setting_operation(
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
    assert_eq!(
        storage
            .rebuild_projection()
            .unwrap()
            .setting("history_limit"),
        Some(&SettingValue::Integer(250))
    );
}

#[test]
fn exact_operation_replay_is_idempotent_across_restart() {
    let temp_dir = tempfile::tempdir().unwrap();
    let path = temp_dir.path().join("idempotent.db");
    let key = storage_key();
    let node = NodeId::from_uuid(Uuid::from_u128(42));
    let operation = setting_operation(node, 7, HlcTimestamp::new(500, 3), "quota", 1024);

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
    let original = setting_operation(node, 1, HlcTimestamp::new(10, 0), "theme", 1);
    let conflicting = setting_operation(node, 1, HlcTimestamp::new(10, 0), "theme", 2);

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
        let first = setting_operation(
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

        let invalid = setting_operation(
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
    let operation = setting_operation(
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
