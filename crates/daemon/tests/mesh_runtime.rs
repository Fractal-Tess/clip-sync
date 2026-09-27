use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    path::Path,
    sync::atomic::{AtomicU16, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use clip_sync_core::{
    files,
    model::{ContentId, ItemKind, NodeId, OpId, Operation, Payload, Reference, Representation},
    storage::{HistoryStore, LocalSource, StorageKey},
    transport::Psk,
};
use clip_sync_daemon::{
    discovery::{DiscoveredPeer, DiscoverySnapshot},
    mesh::{
        Fetched, MeshError, MeshHandle, MeshRuntime, MeshRuntimeConfig, MeshStoreRequest,
        PersistBatch, SourceRequest,
    },
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

const CONTENT_KEY: [u8; 32] = [0x51; 32];
const PSK: [u8; 32] = [0xa4; 32];

enum NodeCommand {
    Copy {
        text: String,
        reply: oneshot::Sender<()>,
    },
    VisibleCount {
        reply: oneshot::Sender<usize>,
    },
    CopyFiles {
        paths: Vec<std::path::PathBuf>,
        reply: oneshot::Sender<ContentId>,
    },
    Reference {
        content_id: ContentId,
        reply: oneshot::Sender<Option<(OpId, Reference)>>,
    },
}

struct TestNode {
    node_id: NodeId,
    address: IpAddr,
    port: u16,
    handle: MeshHandle,
    runtime: MeshRuntime,
    shutdown: CancellationToken,
    commands: mpsc::Sender<NodeCommand>,
    worker: JoinHandle<()>,
}

impl TestNode {
    fn start(path: &Path, address: IpAddr, port: u16) -> Self {
        Self::start_with_forgotten(path, address, port, std::collections::BTreeSet::new())
    }

    fn start_with_forgotten(
        path: &Path,
        address: IpAddr,
        port: u16,
        forgotten_devices: std::collections::BTreeSet<NodeId>,
    ) -> Self {
        let history = HistoryStore::open(path, &storage_key()).unwrap();
        let node_id = history.replica().node_id();
        let shutdown = CancellationToken::new();
        let mut config = MeshRuntimeConfig::new(node_id, address.to_string(), port);
        config.reconcile_interval = Duration::from_millis(75);
        config.reconnect_min = Duration::from_millis(25);
        config.reconnect_max = Duration::from_millis(200);
        config.initial_seen = history.projection().seen_ops().clone();
        config.forgotten_devices = forgotten_devices;
        let (runtime, store) =
            MeshRuntime::spawn(config, Psk::new(&PSK).unwrap(), shutdown.clone()).unwrap();
        let handle = runtime.handle();
        let (commands, command_rx) = mpsc::channel(16);
        let worker = tokio::spawn(run_storage_worker(
            history,
            handle.clone(),
            store,
            command_rx,
            shutdown.clone(),
        ));
        Self {
            node_id,
            address,
            port,
            handle,
            runtime,
            shutdown,
            commands,
            worker,
        }
    }

    fn discover(&self, peers: &[IpAddr]) {
        self.handle
            .update_discovery(snapshot(self.address, peers, self.port));
    }

    fn clear_discovery(&self) {
        self.handle.clear_discovery();
    }

    async fn copy(&self, text: &str) {
        let (reply, complete) = oneshot::channel();
        self.commands
            .send(NodeCommand::Copy {
                text: text.to_owned(),
                reply,
            })
            .await
            .unwrap();
        complete.await.unwrap();
    }

    async fn visible_count(&self) -> usize {
        let (reply, count) = oneshot::channel();
        self.commands
            .send(NodeCommand::VisibleCount { reply })
            .await
            .unwrap();
        count.await.unwrap()
    }

    async fn copy_files(&self, paths: &[&Path]) -> ContentId {
        let (reply, copied) = oneshot::channel();
        self.commands
            .send(NodeCommand::CopyFiles {
                paths: paths.iter().map(|path| path.to_path_buf()).collect(),
                reply,
            })
            .await
            .unwrap();
        copied.await.unwrap()
    }

    /// Waits for a reference to sync here, then fetches it from its origin.
    async fn fetch(&self, content_id: ContentId, destination: &Path) -> Result<Fetched, MeshError> {
        let (operation, reference) = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let (reply, item) = oneshot::channel();
                self.commands
                    .send(NodeCommand::Reference { content_id, reply })
                    .await
                    .unwrap();
                if let Some(item) = item.await.unwrap() {
                    return item;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("reference did not sync");
        self.handle
            .fetch(content_id, operation, &reference, destination)
            .await
    }

    async fn stop(self) {
        self.shutdown.cancel();
        self.runtime.wait().await;
        self.worker.await.unwrap();
    }
}

async fn run_storage_worker(
    mut history: HistoryStore,
    mesh: MeshHandle,
    mut store: mpsc::Receiver<MeshStoreRequest>,
    mut commands: mpsc::Receiver<NodeCommand>,
    shutdown: CancellationToken,
) {
    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            request = store.recv() => {
                let Some(request) = request else {
                    break;
                };
                match request {
                    MeshStoreRequest::Persist(batch) => {
                        let result = persist_remote(&batch, &mut history);
                        batch.complete(result.map_err(|error| error.to_string()));
                    }
                    MeshStoreRequest::Batch(request) => {
                        let result = history
                            .operation_batch(request.remote(), request.limits())
                            .map_err(|error| error.to_string());
                        request.complete(result);
                    }
                    MeshStoreRequest::Source(request) => answer_source(request, &history),
                }
            }
            command = commands.recv() => {
                let Some(command) = command else {
                    break;
                };
                match command {
                    NodeCommand::Copy { text, reply } => {
                        let payload = Payload::new(
                            &CONTENT_KEY,
                            vec![Representation::new("text/plain", text.into_bytes())],
                        ).unwrap();
                        let operation = history.copy(payload, now_millis()).unwrap();
                        mesh.record_local(&operation).await.unwrap();
                        let _ = reply.send(());
                    }
                    NodeCommand::VisibleCount { reply } => {
                        let _ = reply.send(history.projection().visible_items().len());
                    }
                    NodeCommand::CopyFiles { paths, reply } => {
                        let uri_list = files::uri_list_for_paths(
                            paths.iter().map(|path| (path.as_path(), path.is_dir())),
                        )
                        .unwrap();
                        let (reference, sources) = files::describe_files(&paths).unwrap();
                        let content_id = ContentId::from_file_reference(
                            &CONTENT_KEY,
                            history.replica().node_id(),
                            &uri_list,
                        );
                        let operation = history
                            .add_reference(
                                content_id,
                                reference,
                                &LocalSource::Files(sources),
                                now_millis(),
                            )
                            .unwrap();
                        mesh.record_local(&operation).await.unwrap();
                        let _ = reply.send(content_id);
                    }
                    NodeCommand::Reference { content_id, reply } => {
                        let projection = history.projection();
                        let item = match projection.item(content_id) {
                            Some(ItemKind::Reference(reference)) => projection
                                .item_operation(content_id)
                                .map(|operation| (operation, reference.clone())),
                            _ => None,
                        };
                        let _ = reply.send(item);
                    }
                }
            }
        }
    }
}

/// Mirrors the daemon: serve only the exact version the peer holds.
fn answer_source(request: SourceRequest, history: &HistoryStore) {
    let current =
        history.projection().item_operation(request.content_id()) == Some(request.operation());
    let result = match history.local_source(request.content_id()) {
        Ok(Some(source)) if current => Ok(source),
        _ => Err("not available".to_owned()),
    };
    request.complete(result);
}

fn persist_remote(batch: &PersistBatch, history: &mut HistoryStore) -> anyhow::Result<()> {
    for operation in batch.operations() {
        if let Operation::Add { payload, .. } = operation.operation() {
            payload.validate(&CONTENT_KEY)?;
        }
    }
    history.ingest_authenticated_batch(
        batch.peer(),
        batch.peer_frontier(),
        batch.known_members(),
        batch.operations(),
        now_millis(),
    )?;
    Ok(())
}

fn snapshot(local: IpAddr, peers: &[IpAddr], port: u16) -> DiscoverySnapshot {
    DiscoverySnapshot {
        local_addresses: vec![local],
        local_hostname: local.to_string(),
        peers: peers
            .iter()
            .map(|address| DiscoveredPeer {
                hostname: address.to_string(),
                address: *address,
                port,
                local_address: local,
                connected: true,
            })
            .collect(),
    }
}

async fn wait_for_count(node: &TestNode, expected: usize, stage: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if node.visible_count().await == expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("mesh convergence timed out during {stage}"));
}

async fn wait_for_listener(node: &TestNode, expected: Option<SocketAddr>, stage: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if node.handle.status().listener_address == expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("mesh listener transition timed out during {stage}"));
}

fn storage_key() -> StorageKey {
    StorageKey::derive_from_secret(b"mesh runtime test storage", b"mesh runtime test salt").unwrap()
}

fn now_millis() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

fn loopback(index: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(127, 0, 0, index))
}

fn unused_port() -> u16 {
    static NEXT_PORT: AtomicU16 = AtomicU16::new(38_000);
    loop {
        let port = NEXT_PORT.fetch_add(1, Ordering::Relaxed);
        let sockets = (1..=3)
            .map(|index| UdpSocket::bind(SocketAddr::new(loopback(index), port)))
            .collect::<Result<Vec<_>, _>>();
        if sockets.is_ok() {
            return port;
        }
    }
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("clip_sync=info")
        .with_test_writer()
        .try_init();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_nodes_exchange_both_directions_and_reconcile_after_offline_restart() {
    init_tracing();
    let temp = tempfile::tempdir().unwrap();
    let a_path = temp.path().join("a.db");
    let b_path = temp.path().join("b.db");
    let port = unused_port();
    let a_ip = loopback(1);
    let b_ip = loopback(2);

    let a = TestNode::start(&a_path, a_ip, port);
    let b = TestNode::start(&b_path, b_ip, port);
    a.discover(&[b_ip]);
    b.discover(&[a_ip]);

    a.copy("from-a").await;
    wait_for_count(&b, 1, "two-node A to B").await;
    b.copy("from-b").await;
    wait_for_count(&a, 2, "two-node B to A").await;

    b.stop().await;
    a.copy("while-b-offline").await;
    let b = TestNode::start(&b_path, b_ip, port);
    b.discover(&[a_ip]);
    wait_for_count(&b, 3, "two-node offline restart").await;

    a.stop().await;
    b.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_nodes_store_forward_with_origin_offline_and_later_converge() {
    init_tracing();
    let temp = tempfile::tempdir().unwrap();
    let a_path = temp.path().join("a.db");
    let b_path = temp.path().join("b.db");
    let c_path = temp.path().join("c.db");
    let port = unused_port();
    let a_ip = loopback(1);
    let b_ip = loopback(2);
    let c_ip = loopback(3);

    let a = TestNode::start(&a_path, a_ip, port);
    let b = TestNode::start(&b_path, b_ip, port);
    a.discover(&[b_ip]);
    b.discover(&[a_ip]);
    a.copy("origin-a").await;
    wait_for_count(&b, 1, "three-node A to B").await;
    a.stop().await;

    let c = TestNode::start(&c_path, c_ip, port);
    b.discover(&[c_ip]);
    c.discover(&[b_ip]);
    wait_for_count(&c, 1, "three-node store-forward B to C").await;
    c.stop().await;

    b.copy("created-while-c-offline").await;
    let c = TestNode::start(&c_path, c_ip, port);
    b.discover(&[c_ip]);
    c.discover(&[b_ip]);
    wait_for_count(&c, 2, "three-node C offline restart").await;

    b.stop().await;
    c.stop().await;
}

#[tokio::test]
async fn runtime_rejects_control_characters_in_logged_hostname() {
    let config = MeshRuntimeConfig::new(NodeId::new(), "peer\nforged-log-line", 24_892);
    assert!(matches!(
        MeshRuntime::spawn(config, Psk::new(&PSK).unwrap(), CancellationToken::new()),
        Err(MeshError::InvalidHostname)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forgotten_identity_is_rejected_but_reset_identity_joins_as_new() {
    init_tracing();
    let temp = tempfile::tempdir().unwrap();
    let a_path = temp.path().join("forgotten-a.db");
    let b_path = temp.path().join("forgotten-b.db");
    let port = unused_port();
    let a_ip = loopback(1);
    let b_ip = loopback(2);

    let a = TestNode::start(&a_path, a_ip, port);
    let old_identity = a.node_id;
    let b = TestNode::start_with_forgotten(
        &b_path,
        b_ip,
        port,
        std::collections::BTreeSet::from([old_identity]),
    );
    a.copy("must-not-cross-forgotten-session").await;
    a.discover(&[b_ip]);
    b.discover(&[a_ip]);
    tokio::time::sleep(Duration::from_millis(750)).await;
    assert_eq!(b.visible_count().await, 0);

    a.stop().await;
    {
        let mut reset = HistoryStore::open(&a_path, &storage_key()).unwrap();
        let replacement = reset.reset_identity().unwrap();
        assert_ne!(replacement, old_identity);
    }

    let a = TestNode::start(&a_path, a_ip, port);
    assert_ne!(a.node_id, old_identity);
    a.discover(&[b_ip]);
    b.discover(&[a_ip]);
    a.copy("new-identity-is-accepted").await;
    wait_for_count(&b, 1, "reset identity join").await;

    a.stop().await;
    b.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn selected_interface_addresses_each_receive_a_listener() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("multiple-listeners.db");
    let port = unused_port();
    let first = loopback(1);
    let second = loopback(2);
    let node = TestNode::start(&path, first, port);
    let mut discovery = snapshot(first, &[], port);
    discovery.local_addresses = vec![first, second];
    node.handle.update_discovery(discovery);

    wait_for_listener(
        &node,
        Some(SocketAddr::new(first, port)),
        "first selected interface bind",
    )
    .await;
    assert!(
        UdpSocket::bind(SocketAddr::new(second, port)).is_err(),
        "the second selected interface must also own the QUIC listen port"
    );

    node.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn discovery_loss_stops_stale_listener_and_rebinds_after_resume() {
    init_tracing();
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("resume.db");
    let port = unused_port();
    let first = loopback(1);
    let second = loopback(2);
    let mut node = TestNode::start(&path, first, port);

    node.discover(&[]);
    wait_for_listener(
        &node,
        Some(SocketAddr::new(first, port)),
        "initial interface bind",
    )
    .await;

    node.clear_discovery();
    wait_for_listener(&node, None, "discovery outage").await;
    assert_eq!(node.handle.status().active_connections, 0);

    node.address = second;
    node.discover(&[]);
    wait_for_listener(
        &node,
        Some(SocketAddr::new(second, port)),
        "post-resume rebind",
    )
    .await;

    node.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repeated_suspend_resume_churn_keeps_runtime_state_bounded() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("churn.db");
    let port = unused_port();
    let node = TestNode::start(&path, loopback(1), port);

    for _ in 0..250 {
        node.discover(&[loopback(2), loopback(3)]);
        node.clear_discovery();
    }
    node.discover(&[]);
    wait_for_listener(
        &node,
        Some(SocketAddr::new(loopback(1), port)),
        "listener churn recovery",
    )
    .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let status = loop {
        let status = node.handle.status();
        if status.listener_address == Some(SocketAddr::new(loopback(1), port))
            && status.discovered_addresses == 0
            && status.active_connections == 0
            && status.last_listener_error.is_none()
        {
            break status;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "mesh runtime did not quiesce after discovery churn: {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert!(status.last_listener_error.is_none());

    node.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn copied_files_are_fetched_from_their_origin_on_demand() {
    init_tracing();
    let temp = tempfile::tempdir().unwrap();
    let port = unused_port();
    let (a_ip, b_ip) = (loopback(1), loopback(2));
    let a = TestNode::start(&temp.path().join("a.db"), a_ip, port);
    let b = TestNode::start(&temp.path().join("b.db"), b_ip, port);
    a.discover(&[b_ip]);
    b.discover(&[a_ip]);

    let originals = temp.path().join("originals");
    let album = originals.join("album");
    std::fs::create_dir_all(album.join("raw")).unwrap();
    let large = (0..3_000_000_u32)
        .map(|value| (value % 251).to_le_bytes()[0])
        .collect::<Vec<_>>();
    std::fs::write(album.join("a.jpg"), &large).unwrap();
    std::fs::write(album.join("raw/b.cr3"), b"raw bytes").unwrap();
    std::fs::write(album.join("empty"), b"").unwrap();
    let notes = originals.join("notes.txt");
    std::fs::write(&notes, b"notes").unwrap();

    let content_id = a.copy_files(&[&album, &notes]).await;
    let destination = temp.path().join("cache/fetched/item");
    let Fetched::Files(root) = b.fetch(content_id, &destination).await.unwrap() else {
        panic!("expected files");
    };
    assert_eq!(std::fs::read(root.join("album/a.jpg")).unwrap(), large);
    assert_eq!(
        std::fs::read(root.join("album/raw/b.cr3")).unwrap(),
        b"raw bytes"
    );
    assert!(std::fs::read(root.join("album/empty")).unwrap().is_empty());
    assert_eq!(std::fs::read(root.join("notes.txt")).unwrap(), b"notes");
    // The origin was only read, never copied.
    assert_eq!(std::fs::read(album.join("a.jpg")).unwrap(), large);

    a.stop().await;
    b.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_changed_after_copying_is_refused_not_mixed() {
    init_tracing();
    let temp = tempfile::tempdir().unwrap();
    let port = unused_port();
    let (a_ip, b_ip) = (loopback(1), loopback(2));
    let a = TestNode::start(&temp.path().join("a.db"), a_ip, port);
    let b = TestNode::start(&temp.path().join("b.db"), b_ip, port);
    a.discover(&[b_ip]);
    b.discover(&[a_ip]);

    let file = temp.path().join("draft.txt");
    std::fs::write(&file, b"first version").unwrap();
    let content_id = a.copy_files(&[&file]).await;
    std::fs::write(&file, b"edited after copying").unwrap();

    let destination = temp.path().join("cache/fetched/item");
    assert!(matches!(
        b.fetch(content_id, &destination).await,
        Err(MeshError::SourceUnavailable(_))
    ));
    assert!(
        !destination.exists(),
        "a failed fetch must leave nothing behind"
    );

    a.stop().await;
    b.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fetching_from_an_offline_origin_fails_cleanly() {
    init_tracing();
    let temp = tempfile::tempdir().unwrap();
    let port = unused_port();
    let (a_ip, b_ip) = (loopback(1), loopback(2));
    let a = TestNode::start(&temp.path().join("a.db"), a_ip, port);
    let b = TestNode::start(&temp.path().join("b.db"), b_ip, port);
    a.discover(&[b_ip]);
    b.discover(&[a_ip]);

    let file = temp.path().join("big.iso");
    std::fs::write(&file, vec![9_u8; 1024]).unwrap();
    let content_id = a.copy_files(&[&file]).await;
    wait_for_count(&b, 1, "reference sync").await;
    a.stop().await;

    let destination = temp.path().join("cache/fetched/item");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match b.fetch(content_id, &destination).await {
                Err(MeshError::OriginOffline) => return,
                Err(_) | Ok(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    })
    .await
    .expect("the origin should be reported offline");

    b.stop().await;
}
