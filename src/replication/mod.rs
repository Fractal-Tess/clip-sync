//! Transport-independent anti-entropy replication core.
//!
//! This module implements an immutable operation log with content-integrity
//! checking, a versioned codec boundary (JSON v1 today, Protobuf later), and
//! deterministic bounded batch computation for peer synchronization.
//!
//! # Limitations
//!
//! - **No networking.** This is a pure state machine; transport is the caller's
//!   responsibility.
//! - **No persistence.** State lives in memory; the caller must snapshot and
//!   restore across restarts.
//! - **JSON v1 only.** The codec trait is designed for a future Protobuf backend
//!   but only a `serde_json` implementation exists today.
//! - **No back-pressure or flow control.** Batch limits bound individual
//!   responses but there is no session-level rate limiting.
//! - **No cryptographic authentication of batches.** Integrity relies on the
//!   transport layer (QUIC/TLS) established elsewhere.
//! - **`SeenOps` grows with the number of gap entries.** Under sustained
//!   out-of-order delivery the sparse set can grow; this is bounded in practice
//!   by the batch-size limits on the sender side.

mod codec;
mod op_log;
mod state;

pub use codec::{Codec, CodecError, Envelope, JsonV1Codec};
pub use op_log::{OpLog, OpLogError};
pub use state::{AntiEntropyError, AntiEntropyState, BatchLimits, IngestOutcome, OpBatch};
