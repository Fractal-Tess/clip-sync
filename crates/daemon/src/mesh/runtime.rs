use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};

use quinn::Connection;
use tokio::{
    sync::{Mutex, RwLock, mpsc, oneshot, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use clip_sync_core::{
    model::{NodeId, OpId, Operation, SeenOps, StampedOperation},
    replication::{AntiEntropyState, BatchLimits, JsonV1Codec, OpLog},
    transfer::TransferChunk,
    transport::Psk,
};

use crate::discovery::{DiscoverySnapshot, MAX_DISCOVERED_PEERS};

use super::protocol::MAX_BATCH_OPERATIONS;

mod control;
mod error;
mod handshake;
mod listener;
mod session;
mod transfer;

pub use error::MeshError;

const SERVER_NAME: &str = "clip-sync.mesh";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);
const PERSIST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RECONCILE_ROUNDS: usize = 1024;
const MAX_CONCURRENT_HANDSHAKES: usize = 32;
const MAX_ACTIVE_CONNECTIONS: usize = 128;
const MAX_GENERATION_TASKS: usize =
    MAX_DISCOVERED_PEERS + MAX_ACTIVE_CONNECTIONS + MAX_CONCURRENT_HANDSHAKES;
const MAX_CONCURRENT_CHUNK_STREAMS: usize = 4;
const MAX_MISSING_CHUNKS_PER_ROUND: usize = 64;
const CHUNK_BROKER_TIMEOUT: Duration = Duration::from_secs(30);
const CLOSE_DUPLICATE: u32 = 0x201;
const CLOSE_FORGOTTEN: u32 = 0x202;
const CLOSE_SHUTDOWN: u32 = 0x203;
const CLOSE_PROTOCOL: u32 = 0x204;

/// Runtime tuning for one mesh member.
#[derive(Clone, Debug)]
pub struct MeshRuntimeConfig {
    pub node_id: NodeId,
    pub hostname: String,
    pub listen_port: u16,
    pub reconcile_interval: Duration,
    pub reconnect_min: Duration,
    pub reconnect_max: Duration,
    pub batch_limits: BatchLimits,
    pub max_concurrent_chunk_streams: usize,
    /// Durable seen summary, including operation IDs whose payload rows were
    /// safely compacted.
    pub initial_seen: SeenOps,
    pub known_members: BTreeSet<NodeId>,
    pub forgotten_devices: BTreeSet<NodeId>,
}

impl MeshRuntimeConfig {
    #[must_use]
    pub fn new(node_id: NodeId, hostname: impl Into<String>, listen_port: u16) -> Self {
        Self {
            node_id,
            hostname: hostname.into(),
            listen_port,
            reconcile_interval: Duration::from_secs(5),
            reconnect_min: Duration::from_secs(1),
            reconnect_max: Duration::from_mins(1),
            batch_limits: BatchLimits {
                max_ops: MAX_BATCH_OPERATIONS,
                max_bytes: 4 * 1024 * 1024,
            },
            max_concurrent_chunk_streams: MAX_CONCURRENT_CHUNK_STREAMS,
            initial_seen: SeenOps::default(),
            known_members: BTreeSet::from([node_id]),
            forgotten_devices: BTreeSet::new(),
        }
    }
}

/// A batch which must become durable before the network peer is acknowledged.
#[derive(Debug)]
pub struct PersistBatch {
    peer: NodeId,
    peer_frontier: SeenOps,
    known_members: BTreeSet<NodeId>,
    operations: Vec<Vec<u8>>,
    reply: oneshot::Sender<Result<PersistResult, String>>,
}

impl PersistBatch {
    #[must_use]
    pub const fn peer(&self) -> NodeId {
        self.peer
    }

    #[must_use]
    pub const fn peer_frontier(&self) -> &SeenOps {
        &self.peer_frontier
    }

    #[must_use]
    pub const fn known_members(&self) -> &BTreeSet<NodeId> {
        &self.known_members
    }

    #[must_use]
    pub fn operations(&self) -> &[Vec<u8>] {
        &self.operations
    }

    pub fn complete(self, result: Result<PersistResult, String>) {
        let _ = self.reply.send(result);
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PersistResult {
    compacted_operations: Vec<OpId>,
}

impl PersistResult {
    #[must_use]
    pub fn new(compacted_operations: Vec<OpId>) -> Self {
        Self {
            compacted_operations,
        }
    }

    #[must_use]
    pub fn compacted_operations(&self) -> &[OpId] {
        &self.compacted_operations
    }
}

/// Daemon-owned chunk-store work requested only by authenticated sessions.
#[derive(Debug)]
pub enum MeshChunkCommand {
    Missing {
        maximum: usize,
        reply: oneshot::Sender<Result<Vec<TransferChunk>, String>>,
    },
    Export {
        request: TransferChunk,
        reply: oneshot::Sender<Result<Vec<u8>, String>>,
    },
    Import {
        request: TransferChunk,
        encrypted: Vec<u8>,
        reply: oneshot::Sender<Result<(), String>>,
    },
}

/// Cloneable daemon-facing control surface.
#[derive(Clone, Debug)]
pub struct MeshHandle {
    discovery: watch::Sender<Option<DiscoverySnapshot>>,
    revision: watch::Sender<u64>,
    status: watch::Sender<MeshRuntimeStatus>,
    state: Arc<RwLock<AntiEntropyState>>,
    known_members: Arc<RwLock<BTreeSet<NodeId>>>,
    forgotten_devices: Arc<RwLock<BTreeSet<NodeId>>>,
    device_hostnames: Arc<RwLock<BTreeMap<NodeId, String>>>,
    registry: Arc<Mutex<BTreeMap<NodeId, ActiveConnection>>>,
}

impl MeshHandle {
    /// Updates the selected interface bind addresses and discovered dial set.
    pub fn update_discovery(&self, snapshot: DiscoverySnapshot) {
        self.discovery.send_replace(Some(snapshot));
    }

    /// Removes a stale bind/dial set when discovery becomes unavailable.
    pub fn clear_discovery(&self) {
        self.discovery.send_replace(None);
    }

    /// Returns current supervisor-owned listener and connection state.
    #[must_use]
    pub fn status(&self) -> MeshRuntimeStatus {
        self.status.borrow().clone()
    }

    /// Records an already-durable local operation and wakes every live session.
    ///
    /// # Errors
    ///
    /// Returns an error if deterministic operation encoding or log insertion
    /// fails.
    pub async fn record_local(&self, operation: &StampedOperation) -> Result<(), MeshError> {
        self.state
            .write()
            .await
            .record_local(operation, &JsonV1Codec)?;
        self.known_members
            .write()
            .await
            .insert(operation.id().node());
        if let Operation::ForgetDevice { node_id } = operation.operation() {
            self.forget_identity(*node_id).await;
        }
        bump_revision(&self.revision);
        Ok(())
    }

    #[must_use]
    pub async fn frontier(&self) -> SeenOps {
        self.state.read().await.seen().clone()
    }

    /// Returns remote addresses with a live authenticated mesh session.
    #[must_use]
    pub async fn connected_addresses(&self) -> BTreeSet<std::net::IpAddr> {
        self.registry
            .lock()
            .await
            .values()
            .map(|active| active.connection.remote_address().ip())
            .collect()
    }

    /// Returns remote addresses and authenticated hostnames for live mesh sessions.
    #[must_use]
    pub async fn connected_peers(&self) -> BTreeMap<std::net::IpAddr, String> {
        let connections = self
            .registry
            .lock()
            .await
            .iter()
            .map(|(node_id, active)| (*node_id, active.connection.remote_address().ip()))
            .collect::<Vec<_>>();
        let hostnames = self.device_hostnames.read().await;
        connections
            .into_iter()
            .filter_map(|(node_id, address)| {
                hostnames
                    .get(&node_id)
                    .cloned()
                    .map(|hostname| (address, hostname))
            })
            .collect()
    }

    /// Returns authenticated device names observed during this daemon run.
    #[must_use]
    pub async fn device_hostnames(&self) -> BTreeMap<NodeId, String> {
        self.device_hostnames.read().await.clone()
    }

    /// Wakes live authenticated sessions after transfer state changes.
    pub fn notify_transfers(&self) {
        bump_revision(&self.revision);
    }

    async fn forget_identity(&self, node_id: NodeId) {
        self.forgotten_devices.write().await.insert(node_id);
        self.device_hostnames.write().await.remove(&node_id);
        if let Some(active) = self.registry.lock().await.remove(&node_id) {
            active
                .connection
                .close(CLOSE_FORGOTTEN.into(), b"device identity forgotten");
        }
    }
}

/// Live, redacted mesh runtime state used by diagnostics and soak tests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MeshRuntimeStatus {
    pub listener_address: Option<SocketAddr>,
    pub discovered_addresses: usize,
    pub active_connections: usize,
    pub last_listener_error: Option<String>,
}

/// Owned background mesh supervisor.
#[derive(Debug)]
pub struct MeshRuntime {
    handle: MeshHandle,
    task: JoinHandle<()>,
}

type RuntimeSpawn = (
    MeshRuntime,
    mpsc::Receiver<PersistBatch>,
    Option<mpsc::Receiver<MeshChunkCommand>>,
);

impl MeshRuntime {
    /// Creates an initially unbound runtime. Call [`MeshHandle::update_discovery`]
    /// when an interface discovery snapshot is available.
    ///
    /// # Errors
    ///
    /// Returns an error if the hostname is invalid or persisted operations
    /// cannot initialize the forwarding log.
    pub fn spawn(
        config: MeshRuntimeConfig,
        psk: Psk,
        persisted_operations: &[StampedOperation],
        shutdown: CancellationToken,
    ) -> Result<(Self, mpsc::Receiver<PersistBatch>), MeshError> {
        let (runtime, persist_rx, _) =
            Self::spawn_inner(config, psk, persisted_operations, shutdown, false)?;
        Ok((runtime, persist_rx))
    }

    /// Creates a runtime with a daemon-owned encrypted chunk broker.
    ///
    /// # Errors
    ///
    /// Returns the same configuration/operation errors as [`Self::spawn`].
    #[allow(clippy::type_complexity)]
    pub fn spawn_with_transfers(
        config: MeshRuntimeConfig,
        psk: Psk,
        persisted_operations: &[StampedOperation],
        shutdown: CancellationToken,
    ) -> Result<
        (
            Self,
            mpsc::Receiver<PersistBatch>,
            mpsc::Receiver<MeshChunkCommand>,
        ),
        MeshError,
    > {
        let (runtime, persist_rx, chunk_rx) =
            Self::spawn_inner(config, psk, persisted_operations, shutdown, true)?;
        let chunk_rx = chunk_rx.ok_or(MeshError::ChunkBrokerUnavailable)?;
        Ok((runtime, persist_rx, chunk_rx))
    }

    fn spawn_inner(
        config: MeshRuntimeConfig,
        psk: Psk,
        persisted_operations: &[StampedOperation],
        shutdown: CancellationToken,
        transfers: bool,
    ) -> Result<RuntimeSpawn, MeshError> {
        handshake::validate_local_config(&config)?;
        let mut state = AntiEntropyState::restore(config.initial_seen.clone(), OpLog::default());
        for operation in persisted_operations {
            state.record_local(operation, &JsonV1Codec)?;
        }

        let state = Arc::new(RwLock::new(state));
        let mut initial_members = config.known_members.clone();
        initial_members.insert(config.node_id);
        for operation in persisted_operations {
            initial_members.insert(operation.id().node());
        }
        let known_members = Arc::new(RwLock::new(initial_members));
        let forgotten_devices = Arc::new(RwLock::new(config.forgotten_devices.clone()));
        let device_hostnames = Arc::new(RwLock::new(BTreeMap::from([(
            config.node_id,
            config.hostname.clone(),
        )])));
        let registry = Arc::new(Mutex::new(BTreeMap::new()));
        let (discovery, discovery_rx) = watch::channel(None);
        let (revision, _) = watch::channel(0_u64);
        let (status, _) = watch::channel(MeshRuntimeStatus::default());
        let (persist_tx, persist_rx) = mpsc::channel(32);
        let (chunk_tx, chunk_rx) = if transfers {
            let (tx, rx) = mpsc::channel(32);
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let handle = MeshHandle {
            discovery,
            revision: revision.clone(),
            status: status.clone(),
            state: state.clone(),
            known_members: known_members.clone(),
            forgotten_devices: forgotten_devices.clone(),
            device_hostnames: device_hostnames.clone(),
            registry: registry.clone(),
        };
        let context = Arc::new(RuntimeContext {
            config,
            psk: Arc::new(psk),
            state,
            revision,
            status,
            persist_tx,
            chunk_tx,
            registry,
            known_members,
            forgotten_devices,
            device_hostnames,
        });
        let task = tokio::spawn(listener::supervise(context, discovery_rx, shutdown));
        Ok((
            Self {
                handle: handle.clone(),
                task,
            },
            persist_rx,
            chunk_rx,
        ))
    }

    #[must_use]
    pub fn handle(&self) -> MeshHandle {
        self.handle.clone()
    }

    pub async fn wait(self) {
        if let Err(error) = self.task.await {
            tracing::warn!(%error, "mesh supervisor did not stop cleanly");
        }
    }
}

#[derive(Debug)]
struct RuntimeContext {
    config: MeshRuntimeConfig,
    psk: Arc<Psk>,
    state: Arc<RwLock<AntiEntropyState>>,
    revision: watch::Sender<u64>,
    status: watch::Sender<MeshRuntimeStatus>,
    persist_tx: mpsc::Sender<PersistBatch>,
    chunk_tx: Option<mpsc::Sender<MeshChunkCommand>>,
    registry: Arc<Mutex<BTreeMap<NodeId, ActiveConnection>>>,
    known_members: Arc<RwLock<BTreeSet<NodeId>>>,
    forgotten_devices: Arc<RwLock<BTreeSet<NodeId>>>,
    device_hostnames: Arc<RwLock<BTreeMap<NodeId, String>>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Direction {
    Outbound,
    Inbound,
}

#[derive(Debug)]
struct ActiveConnection {
    stable_id: usize,
    preferred: bool,
    connection: Connection,
}

fn bump_revision(revision: &watch::Sender<u64>) {
    revision.send_modify(|value| *value = value.wrapping_add(1));
}

#[cfg(test)]
mod tests;
