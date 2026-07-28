use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    path::Path,
    sync::atomic::{AtomicU16, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use clip_sync::{
    discovery::{DiscoveredPeer, DiscoverySnapshot},
    mesh::{MeshError, MeshHandle, MeshRuntime, MeshRuntimeConfig, PersistBatch, PersistResult},
    model::{NodeId, Operation, Payload, Representation, StampedOperation},
    replica::Replica,
    replication::{Codec, JsonV1Codec},
    storage::{EncryptedStorage, HistoryStore, StorageKey},
    transport::Psk,
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
}

struct TestNode {
    node_id: NodeId,
    address: IpAddr,
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
        let key = storage_key();
        let storage = EncryptedStorage::open(path, &key).unwrap();
        let metadata = storage.local_replica_metadata().unwrap();
        let operations = storage.load_operations().unwrap();
        let projection = storage.rebuild_projection().unwrap();
        let replica = Replica::restore(
            metadata.node_id(),
            metadata.next_operation_counter() - 1,
            metadata.last_hlc(),
            projection,
        );
        let shutdown = CancellationToken::new();
        let mut config = MeshRuntimeConfig::new(metadata.node_id(), address.to_string(), port);
        config.reconcile_interval = Duration::from_millis(75);
        config.reconnect_min = Duration::from_millis(25);
        config.reconnect_max = Duration::from_millis(200);
        config.initial_seen = replica.projection().seen_ops().clone();
        config.forgotten_devices = forgotten_devices;
        let (runtime, persist) = MeshRuntime::spawn(
            config,
            Psk::new(&PSK).unwrap(),
            &operations,
            shutdown.clone(),
        )
        .unwrap();
        let handle = runtime.handle();
        let (commands, command_rx) = mpsc::channel(16);
        let worker = tokio::spawn(run_storage_worker(
            storage,
            replica,
            handle.clone(),
            persist,
            command_rx,
            shutdown.clone(),
        ));
        Self {
            node_id: metadata.node_id(),
            address,
            handle,
            runtime,
            shutdown,
            commands,
            worker,
        }
    }

    fn discover(&self, peers: &[IpAddr]) {
        self.handle.update_discovery(snapshot(self.address, peers));
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

    async fn stop(self) {
        self.shutdown.cancel();
        self.runtime.wait().await;
        self.worker.await.unwrap();
    }
}

async fn run_storage_worker(
    mut storage: EncryptedStorage,
    mut replica: Replica,
    mesh: MeshHandle,
    mut persist: mpsc::Receiver<PersistBatch>,
    mut commands: mpsc::Receiver<NodeCommand>,
    shutdown: CancellationToken,
) {
    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            request = persist.recv() => {
                let Some(request) = request else {
                    break;
                };
                let result = persist_remote(
                    request.operations(),
                    request.peer(),
                    request.peer_frontier(),
                    request.known_members(),
                    &mut storage,
                    &mut replica,
                );
                request.complete(
                    result
                        .map(|()| PersistResult::default())
                        .map_err(|error| error.to_string()),
                );
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
                        let mut next = replica.clone();
                        let operation = next.copy(payload, now_millis()).unwrap();
                        storage.append_local_operation(&operation).unwrap();
                        replica = next;
                        mesh.record_local(&operation).await.unwrap();
                        let _ = reply.send(());
                    }
                    NodeCommand::VisibleCount { reply } => {
                        let _ = reply.send(replica.projection().visible_items().len());
                    }
                }
            }
        }
    }
}

fn persist_remote(
    raw_operations: &[Vec<u8>],
    peer: NodeId,
    peer_frontier: &clip_sync::model::SeenOps,
    known_members: &std::collections::BTreeSet<NodeId>,
    storage: &mut EncryptedStorage,
    replica: &mut Replica,
) -> anyhow::Result<()> {
    let codec = JsonV1Codec;
    let operations = raw_operations
        .iter()
        .map(|raw| codec.decode_op(raw))
        .collect::<Result<Vec<StampedOperation>, _>>()?;
    for operation in &operations {
        if let Operation::Add { payload, .. } = operation.operation() {
            payload.validate(&CONTENT_KEY)?;
        }
    }
    let mut next = replica.clone();
    for operation in &operations {
        next.ingest(operation, now_millis())?;
    }
    storage.append_authenticated_peer_batch(
        peer,
        peer_frontier,
        known_members,
        &operations,
        next.last_timestamp(),
    )?;
    *replica = next;
    Ok(())
}

fn snapshot(local: IpAddr, peers: &[IpAddr]) -> DiscoverySnapshot {
    DiscoverySnapshot {
        local_address: local,
        local_hostname: local.to_string(),
        peers: peers
            .iter()
            .map(|address| DiscoveredPeer {
                hostname: address.to_string(),
                address: *address,
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
        MeshRuntime::spawn(
            config,
            Psk::new(&PSK).unwrap(),
            &[],
            CancellationToken::new()
        ),
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
        "initial NetBird bind",
    )
    .await;

    node.clear_discovery();
    wait_for_listener(&node, None, "NetBird outage").await;
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
    let status = node.handle.status();
    assert_eq!(status.discovered_addresses, 0);
    assert_eq!(status.active_connections, 0);
    assert!(status.last_listener_error.is_none());

    node.stop().await;
}
