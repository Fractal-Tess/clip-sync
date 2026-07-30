pub(super) use std::{fs, net::UdpSocket};

pub(super) use clip_sync::{
    clipboard::types::{ClipboardContent, ClipboardRepresentation, MimeType},
    daemon::{AutomaticClipboardCaptureResult, capture_automatic_clipboard},
    mesh::{MeshHandle, MeshRuntime, MeshRuntimeConfig},
    model::{ContentId, Payload, Projection, Representation, StampedOperation},
    payload::{
        ChunkStore, ChunkStoreConfig, ChunkStoreKey, ExplicitSharePolicy, FileSnapshotLimits,
        Materializer, MaterializerConfig, StoredManifest, snapshot_file_uris,
    },
    storage::{HistoryStore, StorageKey},
    transfer::{
        TransferCoordinator, TransferCoordinatorError, TransferPhase, TransferStateLimits,
        operation_transfer_id,
    },
    transport::Psk,
};
pub(super) use tempfile::TempDir;
pub(super) use tokio_util::sync::CancellationToken;
pub(super) use url::Url;

pub(super) const CHUNK_BYTES: usize = 64 * 1024;
pub(super) const CONTENT_KEY: [u8; 32] = [0x31; 32];
pub(super) const CHUNK_KEY: [u8; 32] = [0x52; 32];
pub(super) const STORAGE_KEY: [u8; 32] = [0x73; 32];
pub(super) const PSK: [u8; 32] = [0x94; 32];

pub(super) struct Node {
    pub(super) directory: TempDir,
    pub(super) history: HistoryStore,
    pub(super) transfers: TransferCoordinator,
}

impl Node {
    pub(super) fn new() -> Self {
        let directory = tempfile::tempdir().expect("node directory");
        let history = HistoryStore::open(
            directory.path().join("history.db"),
            &StorageKey::from_bytes(STORAGE_KEY),
        )
        .expect("history");
        let transfers = open_transfers(&directory);
        Self {
            directory,
            history,
            transfers,
        }
    }

    pub(super) fn restart_transfers(&mut self) {
        let replacement = open_transfers(&self.directory);
        self.transfers = replacement;
        self.transfers
            .reconcile_projection(self.history.projection())
            .expect("recover transfers");
    }

    pub(super) fn ingest(&mut self, operations: &[StampedOperation]) {
        self.history
            .ingest_batch(operations, 10_000)
            .expect("ingest operations");
        self.transfers
            .reconcile_projection(self.history.projection())
            .expect("reconcile transfers");
    }
}

pub(super) fn open_transfers(directory: &TempDir) -> TransferCoordinator {
    open_transfers_with_threshold(directory, 1024)
}

pub(super) fn open_transfers_with_threshold(
    directory: &TempDir,
    automatic_capture_threshold_bytes: u64,
) -> TransferCoordinator {
    let store = ChunkStore::open(
        directory.path().join("chunks"),
        &ChunkStoreKey::from_bytes(CHUNK_KEY),
        ChunkStoreConfig {
            chunk_bytes: CHUNK_BYTES,
            max_payload_bytes: 16 * 1024 * 1024,
            max_chunks_per_manifest: 256,
        },
    )
    .expect("chunk store");
    let materializer = Materializer::new(
        directory.path().join("runtime/materialized"),
        MaterializerConfig::default(),
    )
    .expect("materializer");
    TransferCoordinator::new(
        store,
        materializer,
        ExplicitSharePolicy {
            automatic_capture_threshold_bytes,
            mesh_quota_bytes: 8 * 1024 * 1024,
            maximum_explicit_share_bytes: 16 * 1024 * 1024,
            free_space_reserve_bytes: 0,
        },
        TransferStateLimits {
            max_chunks: 256,
            max_peers: 16,
        },
    )
}

pub(super) fn spawn_mesh(
    node_id: clip_sync::model::NodeId,
) -> (MeshRuntime, MeshHandle, CancellationToken) {
    let socket = UdpSocket::bind("127.0.0.1:0").expect("ephemeral UDP port");
    let port = socket.local_addr().expect("local address").port();
    drop(socket);
    let shutdown = CancellationToken::new();
    let config = MeshRuntimeConfig::new(node_id, "automatic-capture-test".to_owned(), port);
    let (runtime, _persist, _chunks) = MeshRuntime::spawn_with_transfers(
        config,
        Psk::new(&PSK).expect("PSK"),
        &[],
        shutdown.clone(),
    )
    .expect("mesh runtime");
    let handle = runtime.handle();
    (runtime, handle, shutdown)
}

pub(super) fn uri_clipboard(paths: &[&std::path::Path]) -> ClipboardContent {
    let mut uri_list = paths
        .iter()
        .map(|path| {
            Url::from_file_path(path)
                .expect("absolute file URL")
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\r\n");
    uri_list.push_str("\r\n");
    ClipboardContent::new_with_limit(
        vec![ClipboardRepresentation::new(
            MimeType::new("text/uri-list").expect("URI MIME"),
            uri_list,
        )],
        16 * 1024 * 1024,
    )
    .expect("URI clipboard")
}

pub(super) fn payload(bytes: Vec<u8>) -> Payload {
    Payload::new(
        &CONTENT_KEY,
        vec![
            Representation::new("application/x-test", bytes),
            Representation::new("text/plain;charset=utf-8", b"transfer fixture".to_vec()),
        ],
    )
    .expect("payload")
}

pub(super) fn clipboard(payload: &Payload) -> ClipboardContent {
    ClipboardContent::new_with_limit(
        payload
            .representations()
            .iter()
            .map(|representation| {
                ClipboardRepresentation::new(
                    MimeType::new(representation.mime()).expect("MIME"),
                    representation.bytes(),
                )
            })
            .collect(),
        16 * 1024 * 1024,
    )
    .expect("clipboard")
}

pub(super) fn begin_complete(
    origin: &mut Node,
    payload: &Payload,
) -> (ContentId, Vec<StampedOperation>) {
    let inspection = origin
        .transfers
        .inspect_payload(payload, u64::MAX)
        .expect("inspect");
    let (transfer_id, begin) = origin
        .transfers
        .begin_payload_share(
            payload,
            inspection,
            true,
            &mut origin.history,
            100,
            &CancellationToken::new(),
        )
        .expect("begin");
    let complete = origin
        .transfers
        .complete_payload_share(transfer_id, &mut origin.history, 101)
        .expect("complete");
    (payload.descriptor().content_id(), vec![begin, complete])
}

pub(super) fn move_chunks(source: &Node, destination: &mut Node, maximum: usize) -> usize {
    let requests = destination
        .transfers
        .missing_chunks(maximum)
        .expect("missing chunks");
    for request in &requests {
        let encrypted = source
            .transfers
            .export_chunk(*request, &CancellationToken::new())
            .expect("export");
        destination
            .transfers
            .import_chunk(*request, &encrypted, &CancellationToken::new())
            .expect("import");
    }
    requests.len()
}

pub(super) fn activate_file_snapshot(
    node: &Node,
    content_id: ContentId,
) -> (clip_sync::payload::ManifestId, std::path::PathBuf, String) {
    let activated = node
        .transfers
        .activate(
            content_id,
            node.history.projection(),
            &CONTENT_KEY,
            16 * 1024 * 1024,
            &CancellationToken::new(),
        )
        .expect("file snapshot activation");
    let manifest_id = activated
        .materialized_manifest()
        .expect("file materialization");
    let root = node
        .directory
        .path()
        .join("runtime/materialized")
        .join(manifest_id.to_string());
    let uri_bytes = activated
        .content()
        .bytes_for_mime("text/uri-list")
        .expect("runtime URIs");
    (
        manifest_id,
        root,
        String::from_utf8_lossy(&uri_bytes).into_owned(),
    )
}

pub(super) fn assert_automatic_snapshot_bytes(root: &std::path::Path) {
    assert_eq!(
        fs::read(root.join("single.txt")).expect("restored single file"),
        b"automatic file bytes"
    );
    assert_eq!(
        fs::read(root.join("folder/nested/data.bin")).expect("restored nested file"),
        b"automatic directory bytes"
    );
}

pub(super) fn cleanup_file_snapshot(
    node: &Node,
    manifest_id: clip_sync::payload::ManifestId,
    root: &std::path::Path,
) {
    node.transfers
        .cleanup_materialization(manifest_id)
        .expect("materialization cleanup");
    assert!(!root.exists());
}
