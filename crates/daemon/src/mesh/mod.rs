//! Authenticated QUIC mesh runtime and bounded anti-entropy protocol.

mod protocol;
mod runtime;

pub const MESH_PROTOCOL_VERSION: u32 = protocol::PROTOCOL_VERSION;

pub use runtime::{
    MeshChunkCommand, MeshError, MeshHandle, MeshRuntime, MeshRuntimeConfig, MeshRuntimeStatus,
    PersistBatch, PersistResult,
};
