use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    path::Path,
    sync::atomic::{AtomicU16, Ordering},
    time::Duration,
};

use clip_sync::{
    discovery::{DiscoveredPeer, DiscoverySnapshot},
    mesh::{MeshChunkCommand, MeshHandle, MeshRuntime, MeshRuntimeConfig, PersistBatch},
    model::{ContentId, Operation, Payload, Representation, StampedOperation},
    payload::{
        ChunkStore, ChunkStoreConfig, ChunkStoreKey, ExplicitSharePolicy, Materializer,
        MaterializerConfig,
    },
    replication::{Codec, JsonV1Codec},
    storage::{HistoryStore, StorageKey},
    transfer::{TransferCoordinator, TransferPhase, TransferStateLimits},
    transport::Psk,
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

const CONTENT_KEY: [u8; 32] = [0x14; 32];
const CHUNK_KEY: [u8; 32] = [0x25; 32];
const STORAGE_KEY: [u8; 32] = [0x36; 32];
const PSK: [u8; 32] = [0x47; 32];
const CHUNK_BYTES: usize = 64 * 1024;

enum Command {
    Share {
        bytes: Vec<u8>,
        reply: oneshot::Sender<ContentId>,
    },
    LocallyComplete {
        content_id: ContentId,
        reply: oneshot::Sender<bool>,
    },
}

struct RuntimeNode {
    address: IpAddr,
    handle: MeshHandle,
    runtime: MeshRuntime,
    shutdown: CancellationToken,
    commands: mpsc::Sender<Command>,
    worker: JoinHandle<()>,
}

impl RuntimeNode {
    fn start(root: &Path, address: IpAddr, port: u16) -> Self {
        std::fs::create_dir_all(root).expect("node root");
        let history = HistoryStore::open(
            root.join("history.db"),
            &StorageKey::from_bytes(STORAGE_KEY),
        )
        .expect("history");
        let operations = history.storage().load_operations().expect("operations");
        let store = ChunkStore::open(
            root.join("chunks"),
            &ChunkStoreKey::from_bytes(CHUNK_KEY),
            ChunkStoreConfig {
                chunk_bytes: CHUNK_BYTES,
                max_payload_bytes: 16 * 1024 * 1024,
                max_chunks_per_manifest: 256,
            },
        )
        .expect("chunks");
        let materializer = Materializer::new(
            root.join("runtime/materialized"),
            MaterializerConfig::default(),
        )
        .expect("materializer");
        let mut transfers = TransferCoordinator::new(
            store,
            materializer,
            ExplicitSharePolicy {
                automatic_capture_threshold_bytes: 1024,
                mesh_quota_bytes: 8 * 1024 * 1024,
                maximum_explicit_share_bytes: 16 * 1024 * 1024,
                free_space_reserve_bytes: 0,
            },
            TransferStateLimits {
                max_chunks: 256,
                max_peers: 8,
            },
        );
        transfers
            .reconcile_projection(history.projection())
            .expect("recover transfers");

        let shutdown = CancellationToken::new();
        let mut config =
            MeshRuntimeConfig::new(history.replica().node_id(), address.to_string(), port);
        config.reconcile_interval = Duration::from_millis(50);
        config.reconnect_min = Duration::from_millis(20);
        config.reconnect_max = Duration::from_millis(100);
        config.max_concurrent_chunk_streams = 2;
        let (runtime, persist, chunks) = MeshRuntime::spawn_with_transfers(
            config,
            Psk::new(&PSK).expect("PSK"),
            &operations,
            shutdown.clone(),
        )
        .expect("mesh runtime");
        let handle = runtime.handle();
        let (commands, command_rx) = mpsc::channel(16);
        let worker = tokio::spawn(run_worker(
            history,
            transfers,
            handle.clone(),
            persist,
            chunks,
            command_rx,
            shutdown.clone(),
        ));
        Self {
            address,
            handle,
            runtime,
            shutdown,
            commands,
            worker,
        }
    }

    fn discover(&self, peers: &[IpAddr]) {
        self.handle.update_discovery(DiscoverySnapshot {
            local_address: self.address,
            local_hostname: self.address.to_string(),
            peers: peers
                .iter()
                .map(|address| DiscoveredPeer {
                    hostname: address.to_string(),
                    address: *address,
                    connected: true,
                })
                .collect(),
        });
    }

    async fn share(&self, bytes: Vec<u8>) -> ContentId {
        let (reply, completed) = oneshot::channel();
        self.commands
            .send(Command::Share { bytes, reply })
            .await
            .expect("share command");
        completed.await.expect("share reply")
    }

    async fn locally_complete(&self, content_id: ContentId) -> bool {
        let (reply, completed) = oneshot::channel();
        self.commands
            .send(Command::LocallyComplete { content_id, reply })
            .await
            .expect("progress command");
        completed.await.expect("progress reply")
    }

    async fn stop(self) {
        self.shutdown.cancel();
        self.runtime.wait().await;
        self.worker.await.expect("worker");
    }
}

async fn run_worker(
    mut history: HistoryStore,
    mut transfers: TransferCoordinator,
    mesh: MeshHandle,
    mut persist: mpsc::Receiver<PersistBatch>,
    mut chunks: mpsc::Receiver<MeshChunkCommand>,
    mut commands: mpsc::Receiver<Command>,
    shutdown: CancellationToken,
) {
    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            batch = persist.recv() => {
                let Some(batch) = batch else { break };
                let result = persist_batch(batch.operations(), &mut history, &mut transfers);
                if result.is_ok() {
                    mesh.notify_transfers();
                }
                batch.complete(result.map_err(|error| error.to_string()));
            }
            command = chunks.recv() => {
                let Some(command) = command else { break };
                handle_chunk_command(command, &mut transfers, &mesh);
            }
            command = commands.recv() => {
                let Some(command) = command else { break };
                match command {
                    Command::Share { bytes, reply } => {
                        let payload = Payload::new(
                            &CONTENT_KEY,
                            vec![Representation::new("application/x-runtime-test", bytes)],
                        ).expect("payload");
                        let inspection = transfers
                            .inspect_payload(&payload, u64::MAX)
                            .expect("inspection");
                        let (transfer_id, begin) = transfers
                            .begin_payload_share(
                                &payload,
                                inspection,
                                true,
                                &mut history,
                                100,
                                &CancellationToken::new(),
                            )
                            .expect("begin");
                        mesh.record_local(&begin).await.expect("publish begin");
                        let complete = transfers
                            .complete_payload_share(transfer_id, &mut history, 101)
                            .expect("complete");
                        mesh.record_local(&complete).await.expect("publish complete");
                        mesh.notify_transfers();
                        let _ = reply.send(payload.descriptor().content_id());
                    }
                    Command::LocallyComplete { content_id, reply } => {
                        let complete = history
                            .projection()
                            .completed_manifest_for_content(content_id)
                            .is_some()
                            && transfers.progress().iter().any(|progress| {
                                progress.phase == TransferPhase::Complete
                                    && progress.verified_chunks == progress.expected_chunks
                            });
                        let _ = reply.send(complete);
                    }
                }
            }
        }
    }
}

fn persist_batch(
    raw: &[Vec<u8>],
    history: &mut HistoryStore,
    transfers: &mut TransferCoordinator,
) -> anyhow::Result<()> {
    let codec = JsonV1Codec;
    let operations = raw
        .iter()
        .map(|operation| codec.decode_op(operation))
        .collect::<Result<Vec<StampedOperation>, _>>()?;
    for operation in &operations {
        if let Operation::Add { payload, .. } | Operation::AddQuotaExempt { payload, .. } =
            operation.operation()
        {
            payload.validate(&CONTENT_KEY)?;
        }
    }
    history.ingest_batch(&operations, 10_000)?;
    transfers.reconcile_projection(history.projection())?;
    Ok(())
}

fn handle_chunk_command(
    command: MeshChunkCommand,
    transfers: &mut TransferCoordinator,
    mesh: &MeshHandle,
) {
    match command {
        MeshChunkCommand::Missing { maximum, reply } => {
            let _ = reply.send(
                transfers
                    .missing_chunks(maximum)
                    .map_err(|error| error.to_string()),
            );
        }
        MeshChunkCommand::Export { request, reply } => {
            let _ = reply.send(
                transfers
                    .export_chunk(request, &CancellationToken::new())
                    .map_err(|error| error.to_string()),
            );
        }
        MeshChunkCommand::Import {
            request,
            encrypted,
            reply,
        } => {
            let result = transfers
                .import_chunk(request, &encrypted, &CancellationToken::new())
                .map(|_| ())
                .map_err(|error| error.to_string());
            if result.is_ok() {
                mesh.notify_transfers();
            }
            let _ = reply.send(result);
        }
    }
}

fn loopback(index: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(127, 0, 0, index))
}

fn unused_port() -> u16 {
    static NEXT_PORT: AtomicU16 = AtomicU16::new(42_000);
    loop {
        let port = NEXT_PORT.fetch_add(1, Ordering::Relaxed);
        if (1..=2)
            .map(|index| UdpSocket::bind(SocketAddr::new(loopback(index), port)))
            .collect::<Result<Vec<_>, _>>()
            .is_ok()
        {
            return port;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn authenticated_quic_runtime_fetches_chunks_on_dedicated_streams() {
    let root = tempfile::tempdir().expect("root");
    let port = unused_port();
    let a_ip = loopback(1);
    let b_ip = loopback(2);
    let a = RuntimeNode::start(&root.path().join("a"), a_ip, port);
    let b = RuntimeNode::start(&root.path().join("b"), b_ip, port);
    a.discover(&[b_ip]);
    b.discover(&[a_ip]);

    let content_id = a.share(vec![0x8b; CHUNK_BYTES * 3 + 19]).await;
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if b.locally_complete(content_id).await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("authenticated chunk transfer timed out");

    a.stop().await;
    b.stop().await;
}
