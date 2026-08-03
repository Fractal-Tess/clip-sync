pub(super) use std::{collections::BTreeSet, fs, io::Cursor, time::Duration};

pub(super) use clip_sync_core::{
    model::NodeId,
    payload::{
        ChunkStore, ChunkStoreConfig, ChunkStoreError, ChunkStoreKey, ExplicitShareError,
        ExplicitSharePolicy, FileSnapshotError, FileSnapshotLimits, MaterializationError,
        Materializer, MaterializerConfig, StoredManifest, parse_file_uri_list, snapshot_file_uris,
    },
    transfer::{TransferControl, TransferId, TransferPhase, TransferRecord, TransferStateLimits},
};
pub(super) use prost::Message;
pub(super) use tempfile::TempDir;
pub(super) use tokio_util::sync::CancellationToken;
pub(super) use url::Url;

pub(super) const CHUNK_BYTES: usize = 64 * 1024;

pub(super) fn store(directory: &TempDir) -> ChunkStore {
    ChunkStore::open(
        directory.path().join("chunks"),
        &ChunkStoreKey::from_bytes([0x42; 32]),
        ChunkStoreConfig {
            chunk_bytes: CHUNK_BYTES,
            max_payload_bytes: 16 * 1024 * 1024,
            max_chunks_per_manifest: 256,
        },
    )
    .expect("open chunk store")
}

pub(super) fn walk_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(path) = pending.pop() {
        for entry in fs::read_dir(path).expect("read dir") {
            let entry = entry.expect("entry");
            let kind = entry.file_type().expect("file type");
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() {
                files.push(entry.path());
            }
        }
    }
    files
}
