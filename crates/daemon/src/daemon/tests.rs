use std::{
    collections::BTreeSet,
    net::UdpSocket,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::{
    CaptureLimits, capture_clipboard, clipboard::spawn_clipboard_watch, commands::forget_device,
    views::device_items,
};
use clip_sync_core::{
    clipboard::{
        backend::{BackendError, ClipboardBackend, ClipboardEvent},
        types::{ClipboardContent, ClipboardRepresentation, FeedbackMarker, MimeType, ProbeResult},
    },
    config::Config,
    files,
    model::{ContentId, ItemKind, NodeId, Reference},
    storage::{HistoryStore, LocalSource, StorageKey},
    transport::Psk,
};

use crate::{
    ipc::DaemonState,
    mesh::{MeshHandle, MeshRuntime, MeshRuntimeConfig},
};

mod image;

const STORAGE_KEY: [u8; 32] = [0x31; 32];
const CONTENT_KEY: [u8; 32] = [0x53; 32];
const PSK: [u8; 32] = [0x64; 32];
const LIMITS: CaptureLimits = CaptureLimits {
    inline_limit_bytes: 16,
    history_quota_bytes: 1024,
};

#[derive(Clone, Default)]
struct RecoveringClipboard {
    watches: Arc<AtomicUsize>,
}

#[async_trait]
impl ClipboardBackend for RecoveringClipboard {
    async fn probe(&self) -> Result<ProbeResult, BackendError> {
        Err(BackendError::NoDisplay)
    }

    async fn watch(
        &self,
        shutdown: CancellationToken,
        on_event: Box<dyn Fn(ClipboardEvent) + Send + Sync>,
    ) -> Result<(), BackendError> {
        let attempt = self.watches.fetch_add(1, Ordering::SeqCst);
        std::thread::spawn(move || on_event(ClipboardEvent::Ready))
            .join()
            .expect("scripted clipboard callback");
        if attempt == 0 {
            return Err(BackendError::Connection("injected disconnect".to_owned()));
        }
        shutdown.cancelled().await;
        Ok(())
    }

    async fn set_clipboard_content(
        &self,
        _content: ClipboardContent,
    ) -> Result<FeedbackMarker, BackendError> {
        Err(BackendError::WatchNotRunning)
    }
}

#[tokio::test]
async fn clipboard_supervisor_reconnects_after_injected_disconnect() {
    let temporary = tempfile::tempdir().unwrap();
    let (commands, _command_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = DaemonState::new(
        "test".to_owned(),
        temporary.path().join("config.toml"),
        Config::default(),
        commands,
    );
    let backend = RecoveringClipboard::default();
    let watches = backend.watches.clone();
    let shutdown = CancellationToken::new();
    let (events, mut received) = tokio::sync::mpsc::channel(4);
    let task = spawn_clipboard_watch(backend, state, events, shutdown.clone());

    tokio::time::timeout(Duration::from_secs(3), async {
        while watches.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("clipboard watcher did not reconnect");
    assert!(matches!(received.recv().await, Some(ClipboardEvent::Ready)));
    assert!(matches!(received.recv().await, Some(ClipboardEvent::Ready)));

    shutdown.cancel();
    drop(received);
    task.await.expect("clipboard supervisor");
}

fn open_history(root: &std::path::Path) -> HistoryStore {
    HistoryStore::open(
        root.join("history.db"),
        &StorageKey::from_bytes(STORAGE_KEY),
    )
    .unwrap()
}

fn spawn_mesh(node_id: NodeId) -> (MeshRuntime, MeshHandle, CancellationToken) {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = socket.local_addr().unwrap().port();
    drop(socket);
    let shutdown = CancellationToken::new();
    let config = MeshRuntimeConfig::new(node_id, "test-node".to_owned(), port);
    let (runtime, _store) =
        MeshRuntime::spawn(config, Psk::new(&PSK).unwrap(), shutdown.clone()).unwrap();
    let mesh = runtime.handle();
    (runtime, mesh, shutdown)
}

fn content(representations: &[(&str, &[u8])]) -> ClipboardContent {
    ClipboardContent::new_with_max(
        representations
            .iter()
            .map(|(mime, bytes)| {
                ClipboardRepresentation::new(MimeType::new(*mime).unwrap(), bytes.to_vec())
            })
            .collect(),
        u64::MAX,
    )
    .unwrap()
}

async fn capture(
    history: &mut HistoryStore,
    mesh: &MeshHandle,
    content: &ClipboardContent,
    now: u64,
) -> ContentId {
    capture_clipboard(content, &CONTENT_KEY, LIMITS, history, mesh, now)
        .await
        .unwrap()
}

#[tokio::test]
async fn small_copies_replicate_inline_with_every_representation() {
    let temporary = tempfile::tempdir().unwrap();
    let mut history = open_history(temporary.path());
    let (runtime, mesh, shutdown) = spawn_mesh(history.replica().node_id());

    let copied = content(&[("text/plain", b"hello"), ("text/html", b"<b>hi</b>")]);
    let content_id = capture(&mut history, &mesh, &copied, 10).await;

    assert!(matches!(
        history.projection().item(content_id),
        Some(ItemKind::Inline { .. })
    ));
    let payload = history.load_payload(content_id).unwrap().unwrap();
    assert_eq!(payload.representations().len(), 2);
    assert!(history.local_source(content_id).unwrap().is_none());

    shutdown.cancel();
    runtime.wait().await;
}

#[tokio::test]
async fn large_copies_stay_local_and_publish_only_a_description() {
    let temporary = tempfile::tempdir().unwrap();
    let mut history = open_history(temporary.path());
    let (runtime, mesh, shutdown) = spawn_mesh(history.replica().node_id());

    let large = vec![7_u8; 64];
    let content_id = capture(&mut history, &mesh, &content(&[("image/png", &large)]), 10).await;

    let Some(ItemKind::Reference(Reference::Data(descriptors))) =
        history.projection().item(content_id)
    else {
        panic!("a large copy should be a reference");
    };
    assert_eq!(descriptors[0].byte_len(), 64);
    let operation = history
        .storage()
        .load_operation(history.projection().item_operation(content_id).unwrap())
        .unwrap()
        .unwrap();
    assert!(
        clip_sync_core::replication::encode_operation(&operation).len() < 200,
        "the replicated operation must not carry the bytes"
    );
    let Some(LocalSource::Data(payload)) = history.local_source(content_id).unwrap() else {
        panic!("the bytes should be in the local store");
    };
    assert_eq!(payload.representations()[0].bytes(), large.as_slice());

    shutdown.cancel();
    runtime.wait().await;
}

#[tokio::test]
async fn copied_files_are_described_in_place_and_republished_on_every_copy() {
    let temporary = tempfile::tempdir().unwrap();
    let mut history = open_history(temporary.path());
    let (runtime, mesh, shutdown) = spawn_mesh(history.replica().node_id());
    let file = temporary.path().join("report.pdf");
    std::fs::write(&file, vec![1_u8; 40]).unwrap();
    let uri_list = files::uri_list_for_paths([(file.as_path(), false)]).unwrap();

    let first = capture(
        &mut history,
        &mesh,
        &content(&[("text/uri-list", &uri_list)]),
        10,
    )
    .await;
    let Some(ItemKind::Reference(Reference::Files(entries))) = history.projection().item(first)
    else {
        panic!("copied files should be a reference");
    };
    assert_eq!(entries[0].path, "report.pdf");
    assert_eq!(entries[0].size, 40);

    // Editing the file in place keeps its identity but must refresh the
    // description peers fetch against.
    std::fs::write(&file, vec![2_u8; 55]).unwrap();
    let second = capture(
        &mut history,
        &mesh,
        &content(&[("text/uri-list", &uri_list)]),
        20,
    )
    .await;
    assert_eq!(first, second);
    let Some(ItemKind::Reference(Reference::Files(entries))) = history.projection().item(second)
    else {
        panic!("copied files should be a reference");
    };
    assert_eq!(entries[0].size, 55);
    let Some(LocalSource::Files(sources)) = history.local_source(second).unwrap() else {
        panic!("the file location should be recorded");
    };
    assert!(files::open_source(&sources[0]).is_ok());

    shutdown.cancel();
    runtime.wait().await;
}

#[tokio::test]
async fn the_same_path_on_two_devices_is_two_items() {
    let temporary = tempfile::tempdir().unwrap();
    let uri_list = b"file:///home/user/notes.txt\r\n";
    let one = ContentId::from_file_reference(&CONTENT_KEY, NodeId::new(), uri_list);
    let other = ContentId::from_file_reference(&CONTENT_KEY, NodeId::new(), uri_list);
    assert_ne!(one, other);
    drop(temporary);
}

#[tokio::test]
async fn forget_device_persists_and_publishes_known_member_rejection() {
    let temporary = tempfile::tempdir().unwrap();
    let mut history = open_history(temporary.path());
    let remote = NodeId::new();
    history
        .ingest_authenticated_batch(
            remote,
            &clip_sync_core::model::SeenOps::default(),
            &BTreeSet::from([remote]),
            &[],
            1,
        )
        .unwrap();
    let (runtime, mesh, shutdown) = spawn_mesh(history.replica().node_id());

    forget_device(&remote.to_string(), &mut history, &mesh)
        .await
        .unwrap();
    assert!(history.projection().is_device_forgotten(remote));
    assert!(
        device_items(&history)
            .iter()
            .any(|device| device.device_id == remote.to_string() && device.forgotten)
    );

    shutdown.cancel();
    runtime.wait().await;
}
