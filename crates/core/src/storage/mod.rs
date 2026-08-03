mod database;
mod error;
mod frontier;
mod history;
mod history_sync;
mod key;
mod metadata;
mod operations;
mod queries;
mod schema;

pub use database::EncryptedStorage;
pub use error::{Result, StorageError};
pub use history::{HistoryError, HistoryStore};
pub use history_sync::CompactionReport;
pub use key::StorageKey;
pub use metadata::ReplicaMetadata;
pub use operations::AppendOutcome;
