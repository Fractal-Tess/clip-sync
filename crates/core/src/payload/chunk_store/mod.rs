mod catalog;
mod error;
mod manifests;
mod object;
mod recovery;
mod support;
mod transfer;
mod types;
mod validation;

use std::path::PathBuf;

use rusqlite::Connection;
use zeroize::Zeroizing;

pub use error::ChunkStoreError;
pub use types::{
    BlobManifest, ChunkId, ChunkRef, ChunkStoreConfig, ChunkStoreKey, ManifestId, MimeBlob,
    MimeBundleManifest, StoredManifest,
};

const CHUNK_DOMAIN: &[u8] = b"clip-sync/chunk-id/v1\0";
const MANIFEST_DOMAIN: &[u8] = b"clip-sync/manifest-id/v1\0";
const CHUNK_MAGIC: &[u8; 8] = b"CSCHUNK1";
const CHUNK_HEADER_BYTES: usize = 8 + 24;
const AEAD_TAG_BYTES: usize = 16;
const MIN_CHUNK_BYTES: usize = 64 * 1024;
const MAX_CHUNK_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_CHUNK_BYTES: usize = 1024 * 1024;
const DEFAULT_MAX_PAYLOAD_BYTES: u64 = 1024 * 1024 * 1024 * 1024;
const DEFAULT_MAX_CHUNKS: usize = 1_048_576;

/// Single-owner encrypted fixed-size chunk store.
pub struct ChunkStore {
    root: PathBuf,
    chunks_dir: PathBuf,
    staging_dir: PathBuf,
    connection: Connection,
    chunk_key: Zeroizing<[u8; 32]>,
    id_key: Zeroizing<[u8; 32]>,
    config: ChunkStoreConfig,
}
