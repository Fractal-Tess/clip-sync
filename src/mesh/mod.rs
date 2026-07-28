//! Authenticated QUIC mesh runtime and bounded anti-entropy protocol.

mod protocol;
mod runtime;

pub use runtime::{
    MeshChunkCommand, MeshError, MeshHandle, MeshRuntime, MeshRuntimeConfig, PersistBatch,
    PersistResult,
};
