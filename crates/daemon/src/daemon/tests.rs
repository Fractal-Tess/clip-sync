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
    cancel_transfer,
    clipboard::spawn_clipboard_watch,
    commands::{forget_device, share_clipboard_command, transfer_items},
    config_supervision::{
        config_change_requires_reload, restart_required_local_change, update_shared_setting,
    },
    views::device_items,
};
use clip_sync_core::{
    clipboard::{
        backend::{BackendError, ClipboardBackend, ClipboardEvent},
        types::{
            ClipboardContent, ClipboardRepresentation, CurrentClipboardInspection, FeedbackMarker,
            Generation, MimeType, OfferMimeList, ProbeResult,
        },
        wayland::WaylandBackend,
    },
    config::Config,
    model::{NodeId, Payload, Representation},
    payload::{
        ChunkStore, ChunkStoreConfig, ChunkStoreKey, ExplicitSharePolicy, Materializer,
        MaterializerConfig,
    },
    storage::{HistoryStore, StorageKey},
    transfer::{TransferCoordinator, TransferStateLimits},
    transport::Psk,
};
use clip_sync_ipc::protocol::SharedSettingKind;

use crate::{
    ipc::DaemonState,
    mesh::{MeshHandle, MeshRuntime, MeshRuntimeConfig},
};

mod image;

const STORAGE_KEY: [u8; 32] = [0x31; 32];
const CHUNK_KEY: [u8; 32] = [0x42; 32];
const CONTENT_KEY: [u8; 32] = [0x53; 32];
const PSK: [u8; 32] = [0x64; 32];

#[derive(Clone)]
struct TestClipboard {
    inspection: CurrentClipboardInspection,
    content: ClipboardContent,
}

#[async_trait]
impl ClipboardBackend for TestClipboard {
    async fn probe(&self) -> Result<ProbeResult, BackendError> {
        Err(BackendError::NoDisplay)
    }

    async fn watch(
        &self,
        _shutdown: CancellationToken,
        _on_event: Box<dyn Fn(ClipboardEvent) + Send + Sync>,
    ) -> Result<(), BackendError> {
        Err(BackendError::NoDisplay)
    }

    async fn set_clipboard_content(
        &self,
        _content: ClipboardContent,
    ) -> Result<FeedbackMarker, BackendError> {
        Err(BackendError::WatchNotRunning)
    }

    async fn inspect_current_clipboard(
        &self,
        _maximum_bytes: u64,
    ) -> Result<CurrentClipboardInspection, BackendError> {
        Ok(self.inspection.clone())
    }

    async fn capture_current_clipboard(
        &self,
        inspection: &CurrentClipboardInspection,
    ) -> Result<ClipboardContent, BackendError> {
        if inspection.generation() != self.inspection.generation() {
            return Err(BackendError::CurrentOfferChanged);
        }
        Ok(self.content.clone())
    }
}

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

    async fn inspect_current_clipboard(
        &self,
        _maximum_bytes: u64,
    ) -> Result<CurrentClipboardInspection, BackendError> {
        Err(BackendError::WatchNotRunning)
    }

    async fn capture_current_clipboard(
        &self,
        _inspection: &CurrentClipboardInspection,
    ) -> Result<ClipboardContent, BackendError> {
        Err(BackendError::WatchNotRunning)
    }
}

fn clipboard(bytes: &[u8]) -> TestClipboard {
    let mime = MimeType::new("text/plain").unwrap();
    let content = ClipboardContent::new_with_max(
        vec![ClipboardRepresentation::new(mime.clone(), bytes.to_vec())],
        u64::MAX,
    )
    .unwrap();
    let inspection = CurrentClipboardInspection::new(
        Generation::from_value(7),
        OfferMimeList::new(vec![mime]).unwrap(),
        u64::try_from(bytes.len()).unwrap(),
    );
    TestClipboard {
        inspection,
        content,
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

#[test]
fn config_watcher_suppresses_daemon_revisions_but_not_external_edits() {
    let mut observed = Config::default();
    observed.shared.revision = "a1".to_owned();

    for revision in 2..10_000 {
        let mut daemon_write = observed.clone();
        daemon_write.shared.mesh_quota_bytes += 1;
        daemon_write.shared.revision = format!("{revision:x}");
        assert!(!config_change_requires_reload(&observed, &daemon_write));
        observed = daemon_write;
    }

    let mut external_shared = observed.clone();
    external_shared.shared.mesh_quota_bytes += 1;
    assert!(config_change_requires_reload(&observed, &external_shared));

    let mut external_local = observed.clone();
    external_local.local.listen_port += 1;
    external_local.shared.revision = "ffff".to_owned();
    assert!(config_change_requires_reload(&observed, &external_local));
    assert!(restart_required_local_change(
        &observed.local,
        &external_local.local
    ));

    let mut live_policy = observed.local.clone();
    live_policy.maximum_explicit_share_bytes += 1;
    live_policy.transfer_free_space_reserve_bytes += 1;
    assert!(!restart_required_local_change(
        &observed.local,
        &live_policy
    ));
}

fn open_state(
    root: &std::path::Path,
    threshold: u64,
    quota: u64,
) -> (HistoryStore, TransferCoordinator) {
    let history = HistoryStore::open(
        root.join("history.db"),
        &StorageKey::from_bytes(STORAGE_KEY),
    )
    .unwrap();
    let store = ChunkStore::open(
        root.join("chunks"),
        &ChunkStoreKey::from_bytes(CHUNK_KEY),
        ChunkStoreConfig {
            chunk_bytes: 64 * 1024,
            max_payload_bytes: 1024 * 1024,
            max_chunks_per_manifest: 1024,
        },
    )
    .unwrap();
    let materializer =
        Materializer::new(root.join("materialized"), MaterializerConfig::default()).unwrap();
    let transfers = TransferCoordinator::new(
        store,
        materializer,
        ExplicitSharePolicy {
            automatic_capture_threshold_bytes: threshold,
            mesh_quota_bytes: quota,
            maximum_explicit_share_bytes: 1024 * 1024,
            free_space_reserve_bytes: 0,
        },
        TransferStateLimits::default(),
    );
    (history, transfers)
}

fn spawn_mesh(node_id: NodeId) -> (MeshRuntime, MeshHandle, CancellationToken) {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = socket.local_addr().unwrap().port();
    drop(socket);
    let shutdown = CancellationToken::new();
    let config = MeshRuntimeConfig::new(node_id, "test-node".to_owned(), port);
    let (runtime, _persist, _chunks) =
        MeshRuntime::spawn_with_transfers(config, Psk::new(&PSK).unwrap(), &[], shutdown.clone())
            .unwrap();
    let mesh = runtime.handle();
    (runtime, mesh, shutdown)
}

#[tokio::test]
async fn share_command_inspects_then_requires_confirmation_and_returns_ids() {
    let temporary = tempfile::tempdir().unwrap();
    let (mut history, mut transfers) = open_state(temporary.path(), 4, 1024);
    let (runtime, mesh, shutdown) = spawn_mesh(history.replica().node_id());
    let clipboard = clipboard(b"explicit payload");

    let inspection = share_clipboard_command(
        &clipboard,
        false,
        &CONTENT_KEY,
        &mut transfers,
        &mut history,
        &mesh,
        1024 * 1024,
    )
    .await
    .unwrap();
    assert!(!inspection.shared);
    assert!(inspection.confirmation_required);
    assert!(transfers.progress().is_empty());

    let shared = share_clipboard_command(
        &clipboard,
        true,
        &CONTENT_KEY,
        &mut transfers,
        &mut history,
        &mesh,
        1024 * 1024,
    )
    .await
    .unwrap();
    assert!(shared.shared);
    assert!(shared.transfer_id.is_some());
    assert!(shared.content_id.is_some());
    assert_eq!(transfers.progress().len(), 1);
    assert_eq!(history.projection().transfers().len(), 1);

    shutdown.cancel();
    runtime.wait().await;
}

#[tokio::test]
async fn transfer_cancel_mutates_history_and_coordinator() {
    let temporary = tempfile::tempdir().unwrap();
    let (mut history, mut transfers) = open_state(temporary.path(), 1024, 4096);
    let (runtime, mesh, shutdown) = spawn_mesh(history.replica().node_id());
    let payload = Payload::new(
        &CONTENT_KEY,
        vec![Representation::new("text/plain", b"pending".to_vec())],
    )
    .unwrap();
    let available = fs2::available_space(transfers.store().root()).unwrap();
    let inspection = transfers.inspect_payload(&payload, available).unwrap();
    let (transfer_id, begin) = transfers
        .begin_payload_share(
            &payload,
            inspection,
            false,
            &mut history,
            10,
            &CancellationToken::new(),
        )
        .unwrap();
    mesh.record_local(&begin).await.unwrap();
    let listed = transfer_items(&history, &transfers);
    assert_eq!(listed[0].transfer_id, transfer_id.to_string());
    assert_eq!(
        listed[0].content_id,
        payload.descriptor().content_id().to_string()
    );
    assert_eq!(listed[0].peer, history.replica().node_id().to_string());
    assert_eq!(listed[0].total_bytes, 7);

    cancel_transfer(transfer_id, &mut transfers, &mut history, &mesh, 11)
        .await
        .unwrap();
    assert_eq!(
        history.projection().transfer(transfer_id).unwrap().phase(),
        clip_sync_core::transfer::TransferPhase::Cancelled
    );
    assert_eq!(
        transfers.progress()[0].phase,
        clip_sync_core::transfer::TransferPhase::Cancelled
    );

    shutdown.cancel();
    runtime.wait().await;
}

#[tokio::test]
async fn forget_device_persists_and_publishes_known_member_rejection() {
    let temporary = tempfile::tempdir().unwrap();
    let (mut history, _transfers) = open_state(temporary.path(), 1024, 4096);
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

#[cfg(unix)]
#[tokio::test]
async fn setting_update_replicates_enforces_quota_and_preserves_config_symlink() {
    use std::os::unix::fs::symlink;

    let temporary = tempfile::tempdir().unwrap();
    let (mut history, mut transfers) = open_state(temporary.path(), 1024, 4096);
    let payload = Payload::new(
        &CONTENT_KEY,
        vec![Representation::new("text/plain", vec![7; 64])],
    )
    .unwrap();
    history.copy_and_enforce(payload, 1).unwrap();
    let target = temporary.path().join("managed.toml");
    let link = temporary.path().join("config.toml");
    let mut config = Config::default();
    config.local.maximum_explicit_share_bytes = 32 * 1024 * 1024;
    config.local.transfer_free_space_reserve_bytes = 0;
    config.save(&target).unwrap();
    symlink("managed.toml", &link).unwrap();
    let clipboard = WaylandBackend::new();
    let (runtime, mesh, shutdown) = spawn_mesh(history.replica().node_id());

    update_shared_setting(
        SharedSettingKind::MeshQuotaBytes,
        1,
        &mut history,
        &clipboard,
        &mut transfers,
        &mesh,
        &link,
        &mut config,
    )
    .await
    .unwrap();
    assert_eq!(
        history
            .projection()
            .effective_shared_settings()
            .mesh_quota_bytes,
        1
    );
    assert!(history.projection().visible_items().is_empty());
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(Config::load(&target).unwrap().shared.mesh_quota_bytes, 1);

    update_shared_setting(
        SharedSettingKind::CaptureThresholdBytes,
        512,
        &mut history,
        &clipboard,
        &mut transfers,
        &mesh,
        &link,
        &mut config,
    )
    .await
    .unwrap();
    assert_eq!(config.shared.capture_threshold_bytes, 512);

    shutdown.cancel();
    runtime.wait().await;
}
