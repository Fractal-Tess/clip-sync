use std::{fs, net::UdpSocket};

use clip_sync::{
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
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use url::Url;

const CHUNK_BYTES: usize = 64 * 1024;
const CONTENT_KEY: [u8; 32] = [0x31; 32];
const CHUNK_KEY: [u8; 32] = [0x52; 32];
const STORAGE_KEY: [u8; 32] = [0x73; 32];
const PSK: [u8; 32] = [0x94; 32];

struct Node {
    directory: TempDir,
    history: HistoryStore,
    transfers: TransferCoordinator,
}

impl Node {
    fn new() -> Self {
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

    fn restart_transfers(&mut self) {
        let replacement = open_transfers(&self.directory);
        self.transfers = replacement;
        self.transfers
            .reconcile_projection(self.history.projection())
            .expect("recover transfers");
    }

    fn ingest(&mut self, operations: &[StampedOperation]) {
        self.history
            .ingest_batch(operations, 10_000)
            .expect("ingest operations");
        self.transfers
            .reconcile_projection(self.history.projection())
            .expect("reconcile transfers");
    }
}

fn open_transfers(directory: &TempDir) -> TransferCoordinator {
    open_transfers_with_threshold(directory, 1024)
}

fn open_transfers_with_threshold(
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

fn spawn_mesh(node_id: clip_sync::model::NodeId) -> (MeshRuntime, MeshHandle, CancellationToken) {
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

fn uri_clipboard(paths: &[&std::path::Path]) -> ClipboardContent {
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

fn payload(bytes: Vec<u8>) -> Payload {
    Payload::new(
        &CONTENT_KEY,
        vec![
            Representation::new("application/x-test", bytes),
            Representation::new("text/plain;charset=utf-8", b"transfer fixture".to_vec()),
        ],
    )
    .expect("payload")
}

fn clipboard(payload: &Payload) -> ClipboardContent {
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

fn begin_complete(origin: &mut Node, payload: &Payload) -> (ContentId, Vec<StampedOperation>) {
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

fn move_chunks(source: &Node, destination: &mut Node, maximum: usize) -> usize {
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

fn activate_file_snapshot(
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

fn assert_automatic_snapshot_bytes(root: &std::path::Path) {
    assert_eq!(
        fs::read(root.join("single.txt")).expect("restored single file"),
        b"automatic file bytes"
    );
    assert_eq!(
        fs::read(root.join("folder/nested/data.bin")).expect("restored nested file"),
        b"automatic directory bytes"
    );
}

fn cleanup_file_snapshot(
    node: &Node,
    manifest_id: clip_sync::payload::ManifestId,
    root: &std::path::Path,
) {
    node.transfers
        .cleanup_materialization(manifest_id)
        .expect("materialization cleanup");
    assert!(!root.exists());
}

#[tokio::test]
async fn automatic_file_and_directory_capture_is_private_durable_and_remotely_materialized() {
    let mut origin = Node::new();
    let mut remote = Node::new();
    let source_file = origin.directory.path().join("single.txt");
    let source_directory = origin.directory.path().join("folder");
    let source_file_path = source_file.to_string_lossy().into_owned();
    let source_directory_path = source_directory.to_string_lossy().into_owned();
    fs::create_dir(&source_directory).expect("source directory");
    fs::create_dir(source_directory.join("nested")).expect("nested directory");
    fs::write(&source_file, b"automatic file bytes").expect("source file");
    fs::write(
        source_directory.join("nested/data.bin"),
        b"automatic directory bytes",
    )
    .expect("nested source file");
    let clipboard = uri_clipboard(&[&source_file, &source_directory]);
    let origin_path = origin.directory.path().to_string_lossy().into_owned();
    let (runtime, mesh, shutdown) = spawn_mesh(origin.history.replica().node_id());

    let result = capture_automatic_clipboard(
        &clipboard,
        &CONTENT_KEY,
        &mut origin.transfers,
        &mut origin.history,
        &mesh,
        100,
        &CancellationToken::new(),
    )
    .await
    .expect("automatic file capture");
    let (transfer_id, content_id) = match result {
        AutomaticClipboardCaptureResult::Files {
            transfer_id,
            content_id,
        } => (transfer_id, content_id),
        other => panic!("expected file capture, got {other:?}"),
    };
    assert_eq!(
        origin
            .history
            .projection()
            .transfer(transfer_id)
            .expect("transfer")
            .phase(),
        TransferPhase::Complete
    );
    assert!(
        origin.history.projection().payload(content_id).is_none(),
        "origin URI bytes must not be retained as inline history"
    );
    let operations = origin
        .history
        .storage()
        .load_operations()
        .expect("captured operations");
    for operation in &operations {
        let encoded = serde_json::to_string(operation).expect("encode operation");
        assert!(
            !encoded.contains(&origin_path),
            "replicated operation leaked the origin path: {encoded}"
        );
    }

    fs::remove_file(&source_file).expect("delete source file after capture");
    fs::remove_dir_all(&source_directory).expect("delete source directory after capture");
    let (local_manifest, local_root, local_uri) = activate_file_snapshot(&origin, content_id);
    assert!(!local_uri.contains(&source_file_path));
    assert!(!local_uri.contains(&source_directory_path));
    assert_automatic_snapshot_bytes(&local_root);
    cleanup_file_snapshot(&origin, local_manifest, &local_root);

    remote.ingest(&operations);
    assert!(move_chunks(&origin, &mut remote, 64) > 0);
    let (remote_manifest, remote_root, remote_uri) = activate_file_snapshot(&remote, content_id);
    assert_automatic_snapshot_bytes(&remote_root);
    assert!(remote_uri.contains(remote_root.to_str().expect("UTF-8 runtime root")));
    assert!(!remote_uri.contains(&origin_path));
    cleanup_file_snapshot(&remote, remote_manifest, &remote_root);

    shutdown.cancel();
    runtime.wait().await;
}

#[cfg(unix)]
#[tokio::test]
async fn automatic_file_capture_rejects_unsafe_and_oversize_offers_without_history() {
    use std::os::unix::{fs::symlink, net::UnixListener};

    let directory = tempfile::tempdir().expect("node directory");
    let mut history = HistoryStore::open(
        directory.path().join("history.db"),
        &StorageKey::from_bytes(STORAGE_KEY),
    )
    .expect("history");
    let mut transfers = open_transfers_with_threshold(&directory, 32);
    let (runtime, mesh, shutdown) = spawn_mesh(history.replica().node_id());
    let outside = directory.path().join("outside.txt");
    fs::write(&outside, b"outside").expect("outside file");
    let symlink_root = directory.path().join("symlink.txt");
    symlink(&outside, &symlink_root).expect("root symlink");
    let escaping_directory = directory.path().join("escaping");
    fs::create_dir(&escaping_directory).expect("escaping directory");
    symlink(&outside, escaping_directory.join("escape")).expect("child symlink");
    let socket_path = directory.path().join("special.sock");
    let _listener = UnixListener::bind(&socket_path).expect("Unix socket");
    let oversized = directory.path().join("oversized.bin");
    fs::write(&oversized, vec![0x5a; 33]).expect("oversized source");

    for (index, path) in [
        symlink_root.as_path(),
        escaping_directory.as_path(),
        socket_path.as_path(),
        oversized.as_path(),
    ]
    .into_iter()
    .enumerate()
    {
        let outcome = capture_automatic_clipboard(
            &uri_clipboard(&[path]),
            &CONTENT_KEY,
            &mut transfers,
            &mut history,
            &mesh,
            u64::try_from(index + 1).expect("timestamp"),
            &CancellationToken::new(),
        )
        .await
        .expect("safe rejection");
        assert_eq!(
            outcome,
            AutomaticClipboardCaptureResult::RejectedFiles,
            "unsafe path was accepted: {}",
            path.display()
        );
        assert!(history.projection().visible_items().is_empty());
        assert!(transfers.progress().is_empty());
    }
    assert!(
        history
            .storage()
            .load_operations()
            .expect("operation log")
            .is_empty()
    );

    shutdown.cancel();
    runtime.wait().await;
}

#[tokio::test]
async fn automatic_non_file_capture_preserves_all_mime_representations() {
    let mut node = Node::new();
    let content = ClipboardContent::new_with_limit(
        vec![
            ClipboardRepresentation::new(
                MimeType::new("text/plain;charset=utf-8").expect("plain MIME"),
                b"plain".to_vec(),
            ),
            ClipboardRepresentation::new(
                MimeType::new("text/html").expect("HTML MIME"),
                b"<b>plain</b>".to_vec(),
            ),
            ClipboardRepresentation::new(
                MimeType::new("application/x-clip-sync-test").expect("custom MIME"),
                vec![0, 1, 2, 3],
            ),
        ],
        1024,
    )
    .expect("multi-MIME clipboard");
    let (runtime, mesh, shutdown) = spawn_mesh(node.history.replica().node_id());

    let outcome = capture_automatic_clipboard(
        &content,
        &CONTENT_KEY,
        &mut node.transfers,
        &mut node.history,
        &mesh,
        300,
        &CancellationToken::new(),
    )
    .await
    .expect("automatic non-file capture");
    let content_id = match outcome {
        AutomaticClipboardCaptureResult::Payload { content_id } => content_id,
        other => panic!("expected payload capture, got {other:?}"),
    };
    let retained = node
        .history
        .projection()
        .payload(content_id)
        .expect("retained payload");
    assert_eq!(retained.representations().len(), 3);
    for representation in content.representations() {
        assert_eq!(
            retained
                .representations()
                .iter()
                .find(|retained| retained.mime() == representation.mime_type().as_str())
                .expect("retained MIME")
                .bytes(),
            representation.bytes()
        );
    }
    assert!(node.transfers.progress().is_empty());

    shutdown.cancel();
    runtime.wait().await;
}

#[test]
fn interrupted_transfer_resumes_after_coordinator_restart() {
    let mut origin = Node::new();
    let mut remote = Node::new();
    let source_payload = payload(vec![0xa5; CHUNK_BYTES * 3 + 17]);
    let (content_id, operations) = begin_complete(&mut origin, &source_payload);
    remote.ingest(&operations);

    assert_eq!(move_chunks(&origin, &mut remote, 1), 1);
    let before = remote.transfers.progress()[0];
    assert_eq!(before.verified_chunks, 1);
    remote.restart_transfers();
    assert_eq!(remote.transfers.progress()[0].verified_chunks, 1);

    assert!(move_chunks(&origin, &mut remote, 64) >= 2);
    let progress = remote.transfers.progress()[0];
    assert_eq!(progress.phase, TransferPhase::Complete);
    let activated = remote
        .transfers
        .activate(
            content_id,
            remote.history.projection(),
            &CONTENT_KEY,
            16 * 1024 * 1024,
            &CancellationToken::new(),
        )
        .expect("activate");
    assert_eq!(activated.content(), &clipboard(&source_payload));
}

#[test]
fn cancellation_dominates_pending_partial_and_locally_complete_phases() {
    for chunks_before_cancel in [0_usize, 1, usize::MAX] {
        let mut origin = Node::new();
        let mut remote = Node::new();
        let source_payload = payload(vec![0x44; CHUNK_BYTES * 2 + 9]);
        let (_, operations) = begin_complete(&mut origin, &source_payload);
        remote.ingest(&operations);

        if chunks_before_cancel != 0 {
            move_chunks(&origin, &mut remote, chunks_before_cancel);
        }
        let transfer_id = origin.transfers.progress()[0].transfer_id;
        let cancel = origin
            .transfers
            .cancel(transfer_id, &mut origin.history, 102)
            .expect("cancel");
        remote.ingest(&[cancel]);

        assert_eq!(
            remote.transfers.progress()[0].phase,
            TransferPhase::Cancelled
        );
        assert!(
            remote
                .transfers
                .missing_chunks(16)
                .expect("missing")
                .is_empty()
        );
        let objects = fs::read_dir(remote.transfers.store().root().join("objects"))
            .expect("objects")
            .count();
        assert_eq!(objects, 0, "cancel must reclaim remote partial chunks");
    }
}

#[test]
fn cancelling_one_duplicate_manifest_share_preserves_the_other_transfer() {
    let mut origin = Node::new();
    let mut remote = Node::new();
    let source_payload = payload(vec![0x61; CHUNK_BYTES * 2 + 11]);
    let (content_id, first_operations) = begin_complete(&mut origin, &source_payload);
    let (_, second_operations) = begin_complete(&mut origin, &source_payload);
    let first_transfer = operation_transfer_id(&first_operations[0]).expect("first transfer");
    let second_transfer = operation_transfer_id(&second_operations[0]).expect("second transfer");
    remote.ingest(&first_operations);
    remote.ingest(&second_operations);

    assert_eq!(move_chunks(&origin, &mut remote, 1), 1);
    let cancellation = origin
        .transfers
        .cancel(first_transfer, &mut origin.history, 102)
        .expect("cancel first transfer");
    remote.ingest(&[cancellation]);
    origin.restart_transfers();
    remote.restart_transfers();

    move_chunks(&origin, &mut remote, 64);
    assert!(remote.history.projection().is_visible(content_id));
    let second = remote
        .transfers
        .progress()
        .into_iter()
        .find(|progress| progress.transfer_id == second_transfer)
        .expect("second transfer progress");
    assert_eq!(second.phase, TransferPhase::Complete);
    let activated = remote
        .transfers
        .activate(
            content_id,
            remote.history.projection(),
            &CONTENT_KEY,
            16 * 1024 * 1024,
            &CancellationToken::new(),
        )
        .expect("activate surviving duplicate share");
    assert_eq!(activated.content(), &clipboard(&source_payload));
}

#[test]
fn remote_file_snapshot_materializes_beneath_private_runtime_root() {
    let mut origin = Node::new();
    let mut remote = Node::new();
    let source_root = origin.directory.path().join("source");
    fs::create_dir(&source_root).expect("source root");
    fs::write(source_root.join("data.bin"), b"remote exact file bytes").expect("source file");
    let snapshot = snapshot_file_uris(
        std::slice::from_ref(&source_root),
        origin.transfers.store_mut(),
        FileSnapshotLimits::default(),
        &CancellationToken::new(),
    )
    .expect("snapshot");
    let manifest = StoredManifest::Files(snapshot);
    let manifest_id = origin
        .transfers
        .store_mut()
        .commit_manifest(&manifest)
        .expect("manifest");
    let original_uri = Url::from_file_path(&source_root)
        .expect("file URL")
        .to_string();
    let content_id = Payload::new(
        &CONTENT_KEY,
        vec![Representation::new("text/uri-list", original_uri)],
    )
    .expect("URI payload")
    .descriptor()
    .content_id();
    let (transfer_id, begin) = origin
        .transfers
        .begin_committed_manifest_share(
            content_id,
            manifest_id,
            &manifest,
            false,
            &mut origin.history,
            200,
        )
        .expect("begin files");
    let complete = origin
        .transfers
        .complete_payload_share(transfer_id, &mut origin.history, 201)
        .expect("complete files");
    remote.ingest(&[begin, complete]);
    move_chunks(&origin, &mut remote, 64);

    let activated = remote
        .transfers
        .activate(
            content_id,
            remote.history.projection(),
            &CONTENT_KEY,
            16 * 1024 * 1024,
            &CancellationToken::new(),
        )
        .expect("remote file activation");
    let materialized = activated
        .materialized_manifest()
        .expect("materialized manifest");
    let runtime_root = remote.directory.path().join("runtime/materialized");
    let restored = runtime_root
        .join(materialized.to_string())
        .join("source/data.bin");
    assert_eq!(
        fs::read(restored).expect("restored bytes"),
        b"remote exact file bytes"
    );
    let uri_bytes = activated
        .content()
        .bytes_for_mime("text/uri-list")
        .expect("URI representation");
    assert!(
        String::from_utf8_lossy(&uri_bytes)
            .contains(runtime_root.to_str().expect("runtime path UTF-8"))
    );
    remote
        .transfers
        .cleanup_materialization(materialized)
        .expect("cleanup");
    assert!(!runtime_root.join(materialized.to_string()).exists());
}

#[test]
fn corrupted_encrypted_chunk_is_rejected_without_progress() {
    let mut origin = Node::new();
    let mut remote = Node::new();
    let source_payload = payload(vec![0x99; CHUNK_BYTES + 1]);
    let (_, operations) = begin_complete(&mut origin, &source_payload);
    remote.ingest(&operations);
    let request = remote.transfers.missing_chunks(1).expect("missing")[0];
    let mut encrypted = origin
        .transfers
        .export_chunk(request, &CancellationToken::new())
        .expect("export");
    *encrypted.last_mut().expect("encrypted byte") ^= 1;

    assert!(matches!(
        remote
            .transfers
            .import_chunk(request, &encrypted, &CancellationToken::new()),
        Err(TransferCoordinatorError::ChunkStore(_))
    ));
    assert_eq!(remote.transfers.progress()[0].verified_chunks, 0);
    assert_eq!(
        fs::read_dir(remote.transfers.store().root().join("staging"))
            .expect("staging")
            .count(),
        0
    );
}

#[test]
fn offline_peer_catches_up_from_non_origin_replica() {
    let mut origin = Node::new();
    let mut relay = Node::new();
    let mut offline = Node::new();
    let source_payload = payload(vec![0x2a; CHUNK_BYTES * 2 + 77]);
    let (content_id, operations) = begin_complete(&mut origin, &source_payload);
    relay.ingest(&operations);
    move_chunks(&origin, &mut relay, 64);
    assert_eq!(relay.transfers.progress()[0].phase, TransferPhase::Complete);

    // The origin is not consulted after this point: the offline peer consumes
    // the forwarded operation log and encrypted chunks from the relay.
    let forwarded = relay
        .history
        .storage()
        .load_operations()
        .expect("forwarded operations");
    offline.ingest(&forwarded);
    move_chunks(&relay, &mut offline, 64);
    let activated = offline
        .transfers
        .activate(
            content_id,
            offline.history.projection(),
            &CONTENT_KEY,
            16 * 1024 * 1024,
            &CancellationToken::new(),
        )
        .expect("offline activation");
    assert_eq!(activated.content(), &clipboard(&source_payload));
}

#[test]
fn replicated_cancellation_converges_when_terminal_operations_are_reordered() {
    let mut origin = Node::new();
    let source_payload = payload(vec![0x18; CHUNK_BYTES + 3]);
    let (content_id, mut operations) = begin_complete(&mut origin, &source_payload);
    let transfer_id = origin.transfers.progress()[0].transfer_id;
    operations.push(
        origin
            .transfers
            .cancel(transfer_id, &mut origin.history, 102)
            .expect("cancel"),
    );

    let mut forward = Projection::default();
    forward.apply_all(&operations).expect("forward projection");
    for order in [
        [0_usize, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ] {
        let mut reordered = Projection::default();
        reordered
            .apply_all(order.map(|index| &operations[index]))
            .expect("reordered projection");
        assert_eq!(forward, reordered);
    }
    assert!(!forward.is_visible(content_id));
    assert_eq!(
        forward.transfer(transfer_id).expect("transfer").phase(),
        TransferPhase::Cancelled
    );
}
