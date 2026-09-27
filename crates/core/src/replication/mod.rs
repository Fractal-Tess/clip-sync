//! Operation codec and batch limits for anti-entropy replication.
//!
//! Peers exchange the operations the other side has not seen, bounded by
//! [`BatchLimits`]. The operations themselves live only in encrypted storage,
//! which computes each batch (`EncryptedStorage::operation_batch`); nothing
//! here keeps a copy of the log in memory.

mod codec;

pub use codec::{Codec, CodecError, Envelope, JsonV1Codec};

/// Resource limits for a single anti-entropy batch.
#[derive(Clone, Copy, Debug)]
pub struct BatchLimits {
    /// Maximum number of operations in one batch.
    pub max_ops: usize,
    /// Maximum total serialized bytes across all operations in one batch.
    pub max_bytes: usize,
}

impl Default for BatchLimits {
    fn default() -> Self {
        Self {
            max_ops: 256,
            max_bytes: 1024 * 1024, // 1 MiB
        }
    }
}
