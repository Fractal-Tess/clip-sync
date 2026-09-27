use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::path::Path;

use rusqlite::{Connection, OptionalExtension};
use zeroize::Zeroizing;

use super::{
    Result, StorageError, StorageKey, key::SQLCIPHER_KEY_HEX_CHARS, legacy,
    operations::OPERATION_ENCODING_VERSION,
};
use crate::replication::encode_operation;

const SCHEMA_VERSION: u32 = 6;

const MIGRATION_1: &str = "
    BEGIN IMMEDIATE;
    CREATE TABLE storage_meta (
        key TEXT PRIMARY KEY NOT NULL CHECK (length(key) > 0),
        value TEXT NOT NULL
    ) STRICT;
    INSERT INTO storage_meta (key, value) VALUES ('schema_version', '1');
    PRAGMA user_version = 1;
    COMMIT;
";

const MIGRATION_2: &str = "
    BEGIN IMMEDIATE;
    CREATE TABLE operations (
        origin_node BLOB NOT NULL CHECK (length(origin_node) = 16),
        counter INTEGER NOT NULL CHECK (counter BETWEEN 1 AND 9223372036854775807),
        hlc_physical_millis INTEGER NOT NULL
            CHECK (hlc_physical_millis BETWEEN 0 AND 9223372036854775807),
        hlc_logical INTEGER NOT NULL CHECK (hlc_logical BETWEEN 0 AND 4294967295),
        encoding_version INTEGER NOT NULL CHECK (encoding_version = 1),
        payload BLOB NOT NULL,
        PRIMARY KEY (origin_node, counter)
    ) STRICT, WITHOUT ROWID;
    CREATE INDEX operations_event_order
        ON operations (hlc_physical_millis, hlc_logical, origin_node, counter);
    CREATE TABLE local_replica (
        singleton INTEGER PRIMARY KEY NOT NULL CHECK (singleton = 1),
        node_id BLOB NOT NULL UNIQUE CHECK (length(node_id) = 16),
        next_operation_counter INTEGER NOT NULL
            CHECK (next_operation_counter BETWEEN 1 AND 9223372036854775807),
        last_hlc_physical_millis INTEGER NOT NULL
            CHECK (last_hlc_physical_millis BETWEEN 0 AND 9223372036854775807),
        last_hlc_logical INTEGER NOT NULL CHECK (last_hlc_logical BETWEEN 0 AND 4294967295)
    ) STRICT;
    UPDATE storage_meta SET value = '2' WHERE key = 'schema_version';
    PRAGMA user_version = 2;
    COMMIT;
";

const MIGRATION_3: &str = "
    BEGIN IMMEDIATE;
    CREATE TABLE peer_acknowledgements (
        peer_node BLOB PRIMARY KEY NOT NULL CHECK (length(peer_node) = 16),
        frontier BLOB NOT NULL
    ) STRICT, WITHOUT ROWID;
    UPDATE storage_meta SET value = '3' WHERE key = 'schema_version';
    PRAGMA user_version = 3;
    COMMIT;
";

const MIGRATION_4: &str = "
    BEGIN IMMEDIATE;
    CREATE TABLE known_members (
        node_id BLOB PRIMARY KEY NOT NULL CHECK (length(node_id) = 16)
    ) STRICT, WITHOUT ROWID;
    CREATE TABLE compacted_seen (
        singleton INTEGER PRIMARY KEY NOT NULL CHECK (singleton = 1),
        encoding_version INTEGER NOT NULL CHECK (encoding_version = 1),
        payload BLOB NOT NULL
    ) STRICT;
    UPDATE storage_meta SET value = '4' WHERE key = 'schema_version';
    PRAGMA user_version = 4;
    COMMIT;
";

/// Indexes operations by the history item they touch, so compaction and
/// payload loads are lookups instead of decoding the whole log. Existing rows
/// are backfilled from their own serialized operation.
const MIGRATION_5: &str = "
    ALTER TABLE operations ADD COLUMN content_id BLOB
        CHECK (content_id IS NULL OR length(content_id) = 32);
    CREATE INDEX operations_content ON operations (content_id)
        WHERE content_id IS NOT NULL;
";

pub(super) fn should_initialize(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                return Err(StorageError::UnsafeDatabaseFile);
            }
            restrict_database_permissions(path)?;
            Ok(metadata.len() == 0)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            create_private_database(path)?;
            Ok(true)
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn create_private_database(path: &Path) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn create_private_database(path: &Path) -> Result<()> {
    let file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn restrict_database_permissions(path: &Path) -> Result<()> {
    use rustix::fs::{FileType, Mode, OFlags, fchmod, fstat, open};

    let fd = open(
        path,
        OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)?;
    let stat = fstat(&fd).map_err(std::io::Error::from)?;
    if !FileType::from_raw_mode(stat.st_mode).is_file()
        || stat.st_uid != rustix::process::getuid().as_raw()
    {
        return Err(StorageError::UnsafeDatabaseFile);
    }
    fchmod(&fd, Mode::RUSR | Mode::WUSR).map_err(std::io::Error::from)?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_database_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

pub(super) fn apply_key(connection: &Connection, key: &StorageKey) -> Result<()> {
    let mut pragma = Zeroizing::new(String::with_capacity(
        "PRAGMA cipher_log_level = NONE; PRAGMA key = \"x''\";".len() + SQLCIPHER_KEY_HEX_CHARS,
    ));
    pragma.push_str("PRAGMA cipher_log_level = NONE; PRAGMA key = \"x'");
    for byte in key.as_bytes() {
        write!(pragma, "{byte:02x}").map_err(|_| StorageError::KeyDerivation)?;
    }
    pragma.push_str("'\";");

    connection.execute_batch(pragma.as_str())?;
    Ok(())
}

pub(super) fn configure_connection(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "
        PRAGMA temp_store = MEMORY;
        PRAGMA foreign_keys = ON;
        ",
    )?;

    let temp_store: i64 = connection.query_row("PRAGMA temp_store;", [], |row| row.get(0))?;
    if temp_store != 2 {
        return Err(StorageError::IncompatibleSchema(
            "memory temp_store could not be enabled".to_owned(),
        ));
    }

    let foreign_keys_enabled: i64 =
        connection.query_row("PRAGMA foreign_keys;", [], |row| row.get(0))?;
    if foreign_keys_enabled != 1 {
        return Err(StorageError::IncompatibleSchema(
            "foreign key enforcement could not be enabled".to_owned(),
        ));
    }

    Ok(())
}

pub(super) fn verify_sqlcipher(connection: &Connection) -> Result<()> {
    let version = cipher_version(connection)?;
    if version.trim().is_empty() {
        return Err(StorageError::CipherUnavailable);
    }
    Ok(())
}

pub(super) fn cipher_version(connection: &Connection) -> Result<String> {
    connection
        .query_row("PRAGMA cipher_version;", [], |row| row.get(0))
        .map_err(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => StorageError::CipherUnavailable,
            error => error.into(),
        })
}

pub(super) fn verify_fts5(connection: &Connection) -> Result<()> {
    connection
        .execute_batch(
            "
            CREATE VIRTUAL TABLE temp.storage_fts5_probe USING fts5(value);
            DROP TABLE temp.storage_fts5_probe;
            ",
        )
        .map_err(|_| StorageError::Fts5Unavailable)
}

pub(super) fn existing_schema_version(connection: &Connection) -> Result<u32> {
    force_schema_read(connection)?;
    let schema_version = connection
        .query_row(
            "SELECT value FROM storage_meta WHERE key = 'schema_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(normalize_key_error)?
        .ok_or_else(|| StorageError::IncompatibleSchema("missing schema_version".to_owned()))?;
    let schema_version = schema_version.parse::<u32>().map_err(|_| {
        StorageError::IncompatibleSchema(format!(
            "schema_version {schema_version:?} is not an integer"
        ))
    })?;
    let user_version: u32 = connection.query_row("PRAGMA user_version;", [], |row| row.get(0))?;
    if schema_version != user_version {
        return Err(StorageError::IncompatibleSchema(format!(
            "schema_version {schema_version} disagrees with user_version {user_version}"
        )));
    }
    Ok(schema_version)
}

pub(super) fn apply_migrations(connection: &Connection, current_version: u32) -> Result<()> {
    if current_version > SCHEMA_VERSION {
        return Err(StorageError::IncompatibleSchema(format!(
            "unsupported schema_version {current_version}"
        )));
    }

    if current_version < 1 {
        connection.execute_batch(MIGRATION_1)?;
    }
    if current_version < 2 {
        connection.execute_batch(MIGRATION_2)?;
    }
    if current_version < 3 {
        connection.execute_batch(MIGRATION_3)?;
    }
    if current_version < 4 {
        connection.execute_batch(MIGRATION_4)?;
    }
    if current_version < 5 {
        migrate_to_5(connection)?;
    }
    if current_version < 6 {
        migrate_to_6(connection)?;
    }
    Ok(())
}

fn migrate_to_5(connection: &Connection) -> Result<()> {
    connection.execute_batch("BEGIN IMMEDIATE;")?;
    let result = (|| {
        connection.execute_batch(MIGRATION_5)?;
        let mut rows = Vec::new();
        {
            let mut statement =
                connection.prepare("SELECT origin_node, counter, payload FROM operations")?;
            let mut query = statement.query([])?;
            while let Some(row) = query.next()? {
                let node: Vec<u8> = row.get(0)?;
                let counter: i64 = row.get(1)?;
                let encoded = Zeroizing::new(row.get::<_, Vec<u8>>(2)?);
                let operation =
                    legacy::convert(encoded.as_slice()).map_err(StorageError::LegacyOperation)?;
                if let Some(content_id) = operation.operation().content_id() {
                    rows.push((node, counter, *content_id.as_bytes()));
                }
            }
        }
        for (node, counter, content_id) in rows {
            connection.execute(
                "UPDATE operations SET content_id = ?1 WHERE origin_node = ?2 AND counter = ?3",
                (&content_id[..], node, counter),
            )?;
        }
        connection.execute_batch(
            "UPDATE storage_meta SET value = '5' WHERE key = 'schema_version';
             PRAGMA user_version = 5;",
        )?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            connection.execute_batch("COMMIT;")?;
            Ok(())
        }
        Err(error) => {
            let _ = connection.execute_batch("ROLLBACK;");
            Err(error)
        }
    }
}

/// Converts every operation from the 0.3 JSON encoding to Protobuf and adds
/// the table recording where locally authored references live.
///
/// The table is rebuilt because its encoding-version constraint changes.
/// Settings and manifest-share operations become `Retired`; see
/// [`legacy::convert`]. Rows are converted one at a time, so memory stays
/// bounded by the largest single operation.
const MIGRATION_6_TABLES: &str = "
    CREATE TABLE operations_v6 (
        origin_node BLOB NOT NULL CHECK (length(origin_node) = 16),
        counter INTEGER NOT NULL CHECK (counter BETWEEN 1 AND 9223372036854775807),
        hlc_physical_millis INTEGER NOT NULL
            CHECK (hlc_physical_millis BETWEEN 0 AND 9223372036854775807),
        hlc_logical INTEGER NOT NULL CHECK (hlc_logical BETWEEN 0 AND 4294967295),
        encoding_version INTEGER NOT NULL CHECK (encoding_version = 2),
        payload BLOB NOT NULL,
        content_id BLOB CHECK (content_id IS NULL OR length(content_id) = 32),
        PRIMARY KEY (origin_node, counter)
    ) STRICT, WITHOUT ROWID;
    CREATE TABLE local_sources (
        content_id BLOB PRIMARY KEY NOT NULL CHECK (length(content_id) = 32),
        source BLOB NOT NULL
    ) STRICT, WITHOUT ROWID;
";

const MIGRATION_6_SWAP: &str = "
    DROP TABLE operations;
    ALTER TABLE operations_v6 RENAME TO operations;
    CREATE INDEX operations_event_order
        ON operations (hlc_physical_millis, hlc_logical, origin_node, counter);
    CREATE INDEX operations_content ON operations (content_id)
        WHERE content_id IS NOT NULL;
    UPDATE storage_meta SET value = '6' WHERE key = 'schema_version';
    PRAGMA user_version = 6;
";

fn migrate_to_6(connection: &Connection) -> Result<()> {
    connection.execute_batch("BEGIN IMMEDIATE;")?;
    let result = (|| {
        connection.execute_batch(MIGRATION_6_TABLES)?;
        {
            let mut rows = connection.prepare(
                "SELECT origin_node, counter, hlc_physical_millis, hlc_logical, payload
                 FROM operations",
            )?;
            let mut insert = connection.prepare(
                "INSERT INTO operations_v6 (
                     origin_node, counter, hlc_physical_millis, hlc_logical,
                     encoding_version, payload, content_id
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            let mut query = rows.query([])?;
            while let Some(row) = query.next()? {
                let encoded = Zeroizing::new(row.get::<_, Vec<u8>>(4)?);
                let operation =
                    legacy::convert(encoded.as_slice()).map_err(StorageError::LegacyOperation)?;
                let converted = Zeroizing::new(encode_operation(&operation));
                let content_id = operation
                    .operation()
                    .content_id()
                    .map(|content_id| content_id.as_bytes().to_vec());
                insert.execute(rusqlite::params![
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    OPERATION_ENCODING_VERSION,
                    converted.as_slice(),
                    content_id,
                ])?;
            }
        }
        connection.execute_batch(MIGRATION_6_SWAP)?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            connection.execute_batch("COMMIT;")?;
            Ok(())
        }
        Err(error) => {
            let _ = connection.execute_batch("ROLLBACK;");
            Err(error)
        }
    }
}

pub(super) fn verify_current_schema(connection: &Connection) -> Result<()> {
    let version = existing_schema_version(connection)?;
    if version != SCHEMA_VERSION {
        return Err(StorageError::IncompatibleSchema(format!(
            "unsupported schema_version {version}"
        )));
    }

    for table in [
        "operations",
        "local_replica",
        "peer_acknowledgements",
        "known_members",
        "compacted_seen",
        "local_sources",
    ] {
        let exists = connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1
             )",
            [table],
            |row| row.get::<_, bool>(0),
        )?;
        if !exists {
            return Err(StorageError::IncompatibleSchema(format!(
                "missing {table} table"
            )));
        }
    }
    Ok(())
}

fn force_schema_read(connection: &Connection) -> Result<()> {
    connection
        .query_row("SELECT count(*) FROM sqlite_master;", [], |row| {
            row.get::<_, i64>(0)
        })
        .map(|_| ())
        .map_err(normalize_key_error)
}

pub(super) fn normalize_key_error(error: rusqlite::Error) -> StorageError {
    match &error {
        rusqlite::Error::SqliteFailure(sqlite_error, message)
            if sqlite_error.code == rusqlite::ErrorCode::NotADatabase
                || sqlite_error.code == rusqlite::ErrorCode::DatabaseCorrupt
                || message
                    .as_deref()
                    .is_some_and(|message| message.contains("file is not a database")) =>
        {
            StorageError::InvalidKey
        }
        _ => error.into(),
    }
}
