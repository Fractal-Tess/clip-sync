//! Bounded, encrypted storage and safe runtime reconstruction for large payloads.

mod chunk_store;
mod file_snapshot;
mod materialize;
mod share;

pub use chunk_store::{
    BlobManifest, ChunkId, ChunkRef, ChunkStore, ChunkStoreConfig, ChunkStoreError, ChunkStoreKey,
    ManifestId, MimeBlob, MimeBundleManifest, StoredManifest,
};
pub use file_snapshot::{
    FileSnapshot, FileSnapshotEntry, FileSnapshotError, FileSnapshotLimits, SnapshotEntryKind,
    inspect_file_uris, parse_file_uri_list, snapshot_file_uris,
};
pub use materialize::{Materialization, MaterializationError, Materializer, MaterializerConfig};
pub use share::{
    CapturedExplicitShare, ExplicitShareCaptureError, ExplicitShareDecision, ExplicitShareError,
    ExplicitShareInspection, ExplicitSharePolicy,
};
