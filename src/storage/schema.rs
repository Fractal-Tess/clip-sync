use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::path::Path;

use rusqlite::{Connection, OptionalExtension};
use zeroize::Zeroizing;

use super::{Result, StorageError, StorageKey, key::SQLCIPHER_KEY_HEX_CHARS};

const SCHEMA_VERSION: u32 = 4;

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
    Ok(())
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
