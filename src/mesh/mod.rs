//! Authenticated QUIC mesh runtime and bounded anti-entropy protocol.

mod protocol;
mod runtime;

pub use runtime::{MeshError, MeshHandle, MeshRuntime, MeshRuntimeConfig, PersistBatch};
