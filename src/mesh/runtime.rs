use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    hash::{Hash, Hasher},
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};

use quinn::{Connection, Endpoint};
use thiserror::Error;
use tokio::{
    sync::{Mutex, RwLock, Semaphore, mpsc, oneshot, watch},
    task::{JoinHandle, JoinSet},
    time::{MissedTickBehavior, timeout},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    discovery::DiscoverySnapshot,
    model::{NodeId, OpId, Operation, SeenOps, StampedOperation},
    replication::{AntiEntropyState, BatchLimits, Codec, JsonV1Codec, OpLog},
    transfer::{TransferChunk, TransferId},
    transport::{Psk, authenticate_client, authenticate_server, mesh_endpoint},
};

use super::protocol::{
    ChunkStreamRequest, ChunkStreamResponse, IdentityHello, MAX_BATCH_OPERATIONS,
    MAX_CHUNK_CONTROL_BYTES, MAX_ENCRYPTED_CHUNK_BYTES, MAX_FRONTIER_BYTES, MAX_HOSTNAME_BYTES,
    MAX_MEMBERSHIP_BYTES, PROTOCOL_VERSION, ProtocolError, STREAM_KIND_CHUNK, STREAM_KIND_SYNC,
    SyncRequest, SyncResponse, read_message, read_message_bounded, validate_batch, write_message,
};

const SERVER_NAME: &str = "clip-sync.mesh";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);
const PERSIST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RECONCILE_ROUNDS: usize = 1024;
const MAX_CONCURRENT_HANDSHAKES: usize = 32;
const MAX_CONCURRENT_CHUNK_STREAMS: usize = 4;
const MAX_MISSING_CHUNKS_PER_ROUND: usize = 64;
const CHUNK_BROKER_TIMEOUT: Duration = Duration::from_secs(30);
const CLOSE_DUPLICATE: u32 = 0x201;
const CLOSE_FORGOTTEN: u32 = 0x202;
const CLOSE_SHUTDOWN: u32 = 0x203;

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

type RuntimeSpawn = (
    MeshRuntime,
    mpsc::Receiver<PersistBatch>,
    Option<mpsc::Receiver<MeshChunkCommand>>,
);

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

/// Cloneable daemon-facing control surface.
#[derive(Clone, Debug)]
pub struct MeshHandle {
    discovery: watch::Sender<Option<DiscoverySnapshot>>,
    revision: watch::Sender<u64>,
    state: Arc<RwLock<AntiEntropyState>>,
    known_members: Arc<RwLock<BTreeSet<NodeId>>>,
    forgotten_devices: Arc<RwLock<BTreeSet<NodeId>>>,
    registry: Arc<Mutex<BTreeMap<NodeId, ActiveConnection>>>,
}

impl MeshHandle {
    /// Updates the `NetBird` bind address and dial set.
    pub fn update_discovery(&self, snapshot: DiscoverySnapshot) {
        self.discovery.send_replace(Some(snapshot));
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

    /// Wakes live authenticated sessions after transfer state changes.
    pub fn notify_transfers(&self) {
        bump_revision(&self.revision);
    }

    async fn forget_identity(&self, node_id: NodeId) {
        self.forgotten_devices.write().await.insert(node_id);
        if let Some(active) = self.registry.lock().await.remove(&node_id) {
            active
                .connection
                .close(CLOSE_FORGOTTEN.into(), b"device identity forgotten");
        }
    }
}

/// Owned background mesh supervisor.
#[derive(Debug)]
pub struct MeshRuntime {
    handle: MeshHandle,
    task: JoinHandle<()>,
}

impl MeshRuntime {
    /// Creates an initially unbound runtime. Call [`MeshHandle::update_discovery`]
    /// when a `NetBird` snapshot is available.
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
        validate_local_config(&config)?;
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
        let registry = Arc::new(Mutex::new(BTreeMap::new()));
        let (discovery, discovery_rx) = watch::channel(None);
        let (revision, _) = watch::channel(0_u64);
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
            state: state.clone(),
            known_members: known_members.clone(),
            forgotten_devices: forgotten_devices.clone(),
            registry: registry.clone(),
        };
        let context = Arc::new(RuntimeContext {
            config,
            psk: Arc::new(psk),
            state,
            revision,
            persist_tx,
            chunk_tx,
            registry,
            known_members,
            forgotten_devices,
        });
        let task = tokio::spawn(supervise(context, discovery_rx, shutdown));
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
    persist_tx: mpsc::Sender<PersistBatch>,
    chunk_tx: Option<mpsc::Sender<MeshChunkCommand>>,
    registry: Arc<Mutex<BTreeMap<NodeId, ActiveConnection>>>,
    known_members: Arc<RwLock<BTreeSet<NodeId>>>,
    forgotten_devices: Arc<RwLock<BTreeSet<NodeId>>>,
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

async fn supervise(
    context: Arc<RuntimeContext>,
    mut discovery: watch::Receiver<Option<DiscoverySnapshot>>,
    shutdown: CancellationToken,
) {
    let mut generation: Option<ListenerGeneration> = None;

    loop {
        let next = discovery.borrow_and_update().clone();
        let generation_stopped = generation
            .as_ref()
            .is_some_and(|generation| generation.task.is_finished());
        if let Some(snapshot) = next {
            let bind = SocketAddr::new(snapshot.local_address, context.config.listen_port);
            let must_restart = generation_stopped
                || generation
                    .as_ref()
                    .is_none_or(|generation| generation.bind != bind);
            if must_restart {
                stop_generation(&mut generation).await;
                match mesh_endpoint(bind) {
                    Ok(endpoint) => {
                        let generation_shutdown = shutdown.child_token();
                        let (peers, peer_updates) = watch::channel(discovered_addresses(
                            &snapshot,
                            context.config.listen_port,
                        ));
                        let task = tokio::spawn(run_generation(
                            endpoint,
                            peer_updates,
                            context.clone(),
                            generation_shutdown.clone(),
                        ));
                        generation = Some(ListenerGeneration {
                            bind,
                            shutdown: generation_shutdown,
                            peers,
                            task,
                        });
                        tracing::info!(%bind, "mesh QUIC listener active");
                    }
                    Err(error) => {
                        tracing::warn!(%bind, %error, "could not bind mesh QUIC listener");
                    }
                }
            } else if let Some(generation) = &generation {
                generation
                    .peers
                    .send_replace(discovered_addresses(&snapshot, context.config.listen_port));
            }
        }

        tokio::select! {
            () = shutdown.cancelled() => break,
            result = discovery.changed() => {
                if result.is_err() {
                    break;
                }
            }
            () = tokio::time::sleep(context.config.reconnect_min),
                if generation.is_none() && discovery.borrow().is_some() => {}
        }
    }

    stop_generation(&mut generation).await;
}

#[derive(Debug)]
struct ListenerGeneration {
    bind: SocketAddr,
    shutdown: CancellationToken,
    peers: watch::Sender<Vec<SocketAddr>>,
    task: JoinHandle<()>,
}

async fn stop_generation(generation: &mut Option<ListenerGeneration>) {
    if let Some(generation) = generation.take() {
        generation.shutdown.cancel();
        if let Err(error) = generation.task.await {
            tracing::warn!(%error, "mesh listener generation did not stop cleanly");
        }
    }
}

fn discovered_addresses(snapshot: &DiscoverySnapshot, port: u16) -> Vec<SocketAddr> {
    snapshot
        .peers
        .iter()
        .filter(|peer| peer.connected && peer.address != snapshot.local_address)
        .map(|peer| SocketAddr::new(peer.address, port))
        .collect()
}

async fn run_generation(
    endpoint: Endpoint,
    mut peer_updates: watch::Receiver<Vec<SocketAddr>>,
    context: Arc<RuntimeContext>,
    shutdown: CancellationToken,
) {
    let handshakes = Arc::new(Semaphore::new(MAX_CONCURRENT_HANDSHAKES));
    let mut tasks = JoinSet::new();
    let mut dialers = BTreeMap::<SocketAddr, CancellationToken>::new();
    update_dialers(
        &endpoint,
        &context,
        &shutdown,
        &mut tasks,
        &mut dialers,
        &peer_updates.borrow_and_update(),
    );

    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            result = peer_updates.changed() => {
                if result.is_err() {
                    break;
                }
                update_dialers(
                    &endpoint,
                    &context,
                    &shutdown,
                    &mut tasks,
                    &mut dialers,
                    &peer_updates.borrow_and_update(),
                );
            }
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else {
                    break;
                };
                let Ok(permit) = handshakes.clone().try_acquire_owned() else {
                    incoming.refuse();
                    continue;
                };
                spawn_incoming(
                    &mut tasks,
                    incoming,
                    permit,
                    context.clone(),
                    shutdown.clone(),
                );
            }
            completed = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Err(error)) = completed {
                    tracing::warn!(%error, "mesh connection task panicked");
                }
            }
        }
    }

    endpoint.close(CLOSE_SHUTDOWN.into(), b"mesh listener stopping");
    shutdown.cancel();
    for dialer in dialers.into_values() {
        dialer.cancel();
    }
    while let Some(result) = tasks.join_next().await {
        if let Err(error) = result {
            tracing::warn!(%error, "mesh connection task did not stop cleanly");
        }
    }
    let registered = {
        let mut registry = context.registry.lock().await;
        std::mem::take(&mut *registry)
    };
    for active in registered.into_values() {
        active
            .connection
            .close(CLOSE_SHUTDOWN.into(), b"mesh listener stopping");
    }
    if timeout(Duration::from_secs(5), endpoint.wait_idle())
        .await
        .is_err()
    {
        tracing::debug!("QUIC endpoint still had draining connections at shutdown");
    }
}

fn spawn_incoming(
    tasks: &mut JoinSet<()>,
    incoming: quinn::Incoming,
    permit: tokio::sync::OwnedSemaphorePermit,
    context: Arc<RuntimeContext>,
    shutdown: CancellationToken,
) {
    tasks.spawn(async move {
        let connection = tokio::select! {
            () = shutdown.cancelled() => return,
            result = incoming => match result {
                Ok(connection) => connection,
                Err(error) => {
                    tracing::debug!(%error, "incoming QUIC handshake failed");
                    return;
                }
            }
        };
        let authenticated = tokio::select! {
            () = shutdown.cancelled() => return,
            result = authenticate_server(connection, &context.psk) => match result {
                Ok(connection) => connection,
                Err(error) => {
                    tracing::debug!(%error, "incoming mesh authentication failed");
                    return;
                }
            }
        };
        drop(permit);
        if let Err(error) = run_connection(
            authenticated.into_inner(),
            Direction::Inbound,
            context,
            shutdown,
        )
        .await
        {
            tracing::debug!(%error, "incoming mesh session ended");
        }
    });
}

fn update_dialers(
    endpoint: &Endpoint,
    context: &Arc<RuntimeContext>,
    shutdown: &CancellationToken,
    tasks: &mut JoinSet<()>,
    dialers: &mut BTreeMap<SocketAddr, CancellationToken>,
    addresses: &[SocketAddr],
) {
    dialers.retain(|address, cancellation| {
        if addresses.contains(address) {
            true
        } else {
            cancellation.cancel();
            false
        }
    });
    for address in addresses {
        if dialers.contains_key(address) {
            continue;
        }
        let cancellation = shutdown.child_token();
        dialers.insert(*address, cancellation.clone());
        tasks.spawn(dial_peer(
            endpoint.clone(),
            *address,
            context.clone(),
            cancellation,
        ));
    }
}

async fn dial_peer(
    endpoint: Endpoint,
    address: SocketAddr,
    context: Arc<RuntimeContext>,
    shutdown: CancellationToken,
) {
    let mut attempt = 0_u32;
    loop {
        let connecting = match endpoint.connect(address, SERVER_NAME) {
            Ok(connecting) => connecting,
            Err(error) => {
                tracing::debug!(%address, %error, "could not start mesh connection");
                if wait_backoff(&context.config, address, attempt, &shutdown).await {
                    return;
                }
                attempt = attempt.saturating_add(1);
                continue;
            }
        };
        let connection = tokio::select! {
            () = shutdown.cancelled() => return,
            result = connecting => match result {
                Ok(connection) => connection,
                Err(error) => {
                    tracing::debug!(%address, %error, "mesh peer is unreachable");
                    if wait_backoff(&context.config, address, attempt, &shutdown).await {
                        return;
                    }
                    attempt = attempt.saturating_add(1);
                    continue;
                }
            }
        };
        let authenticated = tokio::select! {
            () = shutdown.cancelled() => return,
            result = authenticate_client(connection, &context.psk) => match result {
                Ok(connection) => connection,
                Err(error) => {
                    tracing::debug!(%address, %error, "outgoing mesh authentication failed");
                    if wait_backoff(&context.config, address, attempt, &shutdown).await {
                        return;
                    }
                    attempt = attempt.saturating_add(1);
                    continue;
                }
            }
        };

        attempt = 0;
        let mut duplicate = false;
        if let Err(error) = run_connection(
            authenticated.into_inner(),
            Direction::Outbound,
            context.clone(),
            shutdown.clone(),
        )
        .await
        {
            duplicate = matches!(&error, MeshError::DuplicateConnection(_));
            tracing::debug!(%address, %error, "outgoing mesh session ended");
        }
        let backoff_attempt = if duplicate { u32::MAX } else { attempt };
        if wait_backoff(&context.config, address, backoff_attempt, &shutdown).await {
            return;
        }
        attempt = attempt.saturating_add(1);
    }
}

async fn run_connection(
    connection: Connection,
    direction: Direction,
    context: Arc<RuntimeContext>,
    shutdown: CancellationToken,
) -> Result<(), MeshError> {
    let peer = timeout(
        HANDSHAKE_TIMEOUT,
        exchange_identity(&connection, direction, &context),
    )
    .await
    .map_err(|_| MeshError::HandshakeTimeout)??;

    if peer.node_id == context.config.node_id {
        connection.close(CLOSE_DUPLICATE.into(), b"duplicate node identity");
        return Err(MeshError::DuplicateNodeIdentity(peer.node_id));
    }
    if context
        .forgotten_devices
        .read()
        .await
        .contains(&peer.node_id)
    {
        connection.close(CLOSE_FORGOTTEN.into(), b"device identity forgotten");
        return Err(MeshError::ForgottenNodeIdentity(peer.node_id));
    }

    persist_and_record(
        &context,
        peer.node_id,
        peer.frontier.clone(),
        peer.known_members.clone(),
        Vec::new(),
    )
    .await?;

    // A forget may become durable while this handshake is waiting on the
    // daemon. Recheck before registering so that race cannot establish a
    // session for the retired identity.
    if context
        .forgotten_devices
        .read()
        .await
        .contains(&peer.node_id)
    {
        connection.close(CLOSE_FORGOTTEN.into(), b"device identity forgotten");
        return Err(MeshError::ForgottenNodeIdentity(peer.node_id));
    }

    let stable_id = connection.stable_id();
    if !register_connection(&context, peer.node_id, direction, &connection).await {
        connection.close(CLOSE_DUPLICATE.into(), b"duplicate connection");
        return Err(MeshError::DuplicateConnection(peer.node_id));
    }

    tracing::info!(
        peer_id = %peer.node_id,
        peer_hostname = %peer.hostname,
        ?direction,
        "authenticated mesh peer connected"
    );
    let peer_frontier = Arc::new(Mutex::new(peer.frontier));
    let result = session_loop(&connection, &context, peer.node_id, peer_frontier, shutdown).await;
    remove_connection(&context, peer.node_id, stable_id).await;
    connection.close(CLOSE_SHUTDOWN.into(), b"mesh session ended");
    tracing::info!(peer_id = %peer.node_id, "mesh peer disconnected");
    result
}

#[derive(Debug)]
struct PeerIdentity {
    node_id: NodeId,
    hostname: String,
    frontier: SeenOps,
    known_members: BTreeSet<NodeId>,
}

async fn exchange_identity(
    connection: &Connection,
    direction: Direction,
    context: &RuntimeContext,
) -> Result<PeerIdentity, MeshError> {
    let local = local_identity(context).await?;
    let peer = match direction {
        Direction::Outbound => {
            let (mut send, mut recv) = connection.open_bi().await?;
            write_message(&mut send, &local).await?;
            let peer = read_message(&mut recv).await?;
            send.finish()?;
            peer
        }
        Direction::Inbound => {
            let (mut send, mut recv) = connection.accept_bi().await?;
            let peer = read_message(&mut recv).await?;
            write_message(&mut send, &local).await?;
            send.finish()?;
            peer
        }
    };
    parse_identity(peer)
}

async fn local_identity(context: &RuntimeContext) -> Result<IdentityHello, MeshError> {
    let frontier = encode_frontier(context.state.read().await.seen())?;
    let members = context.known_members.read().await;
    let known_members = encode_membership(&members)?;
    Ok(IdentityHello {
        protocol_version: PROTOCOL_VERSION,
        node_id: context.config.node_id.as_uuid().as_bytes().to_vec(),
        hostname: context.config.hostname.clone(),
        frontier,
        known_members,
    })
}

fn parse_identity(hello: IdentityHello) -> Result<PeerIdentity, MeshError> {
    if hello.protocol_version != PROTOCOL_VERSION {
        return Err(MeshError::UnsupportedProtocol(hello.protocol_version));
    }
    if !valid_hostname(&hello.hostname) {
        return Err(MeshError::InvalidHostname);
    }
    if hello.frontier.len() > MAX_FRONTIER_BYTES {
        return Err(MeshError::Protocol(ProtocolError::FrontierTooLarge(
            hello.frontier.len(),
        )));
    }
    if hello.known_members.len() > MAX_MEMBERSHIP_BYTES {
        return Err(MeshError::Protocol(ProtocolError::MembershipTooLarge(
            hello.known_members.len(),
        )));
    }
    let uuid = Uuid::from_slice(&hello.node_id).map_err(|_| MeshError::InvalidNodeId)?;
    let frontier = decode_frontier(&hello.frontier)?;
    let known_members = decode_membership(&hello.known_members)?;
    Ok(PeerIdentity {
        node_id: NodeId::from_uuid(uuid),
        hostname: hello.hostname,
        frontier,
        known_members,
    })
}

async fn register_connection(
    context: &RuntimeContext,
    peer: NodeId,
    direction: Direction,
    connection: &Connection,
) -> bool {
    let preferred = preferred_direction(context.config.node_id, peer) == direction;
    let mut registry = context.registry.lock().await;
    if let Some(existing) = registry.get(&peer) {
        if existing.preferred || !preferred {
            return false;
        }
        existing
            .connection
            .close(CLOSE_DUPLICATE.into(), b"replaced by canonical connection");
    }
    registry.insert(
        peer,
        ActiveConnection {
            stable_id: connection.stable_id(),
            preferred,
            connection: connection.clone(),
        },
    );
    true
}

async fn remove_connection(context: &RuntimeContext, peer: NodeId, stable_id: usize) {
    let mut registry = context.registry.lock().await;
    if registry
        .get(&peer)
        .is_some_and(|active| active.stable_id == stable_id)
    {
        registry.remove(&peer);
    }
}

fn preferred_direction(local: NodeId, peer: NodeId) -> Direction {
    match local.cmp(&peer) {
        Ordering::Less => Direction::Outbound,
        Ordering::Equal | Ordering::Greater => Direction::Inbound,
    }
}

async fn session_loop(
    connection: &Connection,
    context: &Arc<RuntimeContext>,
    peer: NodeId,
    peer_frontier: Arc<Mutex<SeenOps>>,
    shutdown: CancellationToken,
) -> Result<(), MeshError> {
    let inbound = accept_session_streams(
        connection,
        context,
        peer,
        peer_frontier.clone(),
        shutdown.clone(),
    );
    let outbound =
        initiate_sync_streams(connection, context, peer, peer_frontier, shutdown.clone());
    let transfers = initiate_chunk_streams(connection, context, shutdown.clone());
    tokio::pin!(inbound);
    tokio::pin!(outbound);
    tokio::pin!(transfers);

    tokio::select! {
        result = &mut inbound => result,
        result = &mut outbound => result,
        result = &mut transfers => result,
        error = connection.closed() => Err(MeshError::Connection(error)),
        () = shutdown.cancelled() => Ok(()),
    }
}

async fn accept_session_streams(
    connection: &Connection,
    context: &Arc<RuntimeContext>,
    peer: NodeId,
    peer_frontier: Arc<Mutex<SeenOps>>,
    shutdown: CancellationToken,
) -> Result<(), MeshError> {
    loop {
        let streams = tokio::select! {
            () = shutdown.cancelled() => return Ok(()),
            streams = connection.accept_bi() => streams?,
        };
        timeout(
            EXCHANGE_TIMEOUT,
            answer_session(streams, context, peer, &peer_frontier),
        )
        .await
        .map_err(|_| MeshError::ExchangeTimeout)??;
    }
}

async fn answer_session(
    (mut send, mut recv): (quinn::SendStream, quinn::RecvStream),
    context: &RuntimeContext,
    peer: NodeId,
    peer_frontier: &Mutex<SeenOps>,
) -> Result<(), MeshError> {
    let mut kind = [0_u8; 1];
    recv.read_exact(&mut kind).await?;
    match kind[0] {
        STREAM_KIND_SYNC => answer_sync(&mut send, &mut recv, context, peer, peer_frontier).await,
        STREAM_KIND_CHUNK => answer_chunk(&mut send, &mut recv, context).await,
        kind => Err(MeshError::UnknownStreamKind(kind)),
    }
}

async fn answer_sync(
    send: &mut quinn::SendStream,
    recv: &mut quinn::RecvStream,
    context: &RuntimeContext,
    peer: NodeId,
    peer_frontier: &Mutex<SeenOps>,
) -> Result<(), MeshError> {
    let request: SyncRequest = read_message(recv).await?;
    validate_batch(
        &request.frontier,
        &request.known_members,
        &request.operations,
    )?;
    tracing::debug!(
        received_operations = request.operations.len(),
        "answering mesh reconciliation request"
    );
    let advertised = decode_frontier(&request.frontier)?;
    let known_members = decode_membership(&request.known_members)?;
    persist_and_record(
        context,
        peer,
        advertised.clone(),
        known_members,
        request.operations,
    )
    .await?;
    *peer_frontier.lock().await = advertised.clone();

    let (frontier, known_members, operations, has_more) =
        batch_for_peer(context, &advertised).await?;
    let response = SyncResponse {
        frontier,
        operations,
        has_more,
        known_members,
    };
    write_message(send, &response).await?;
    send.finish()?;
    Ok(())
}

async fn initiate_sync_streams(
    connection: &Connection,
    context: &Arc<RuntimeContext>,
    peer: NodeId,
    peer_frontier: Arc<Mutex<SeenOps>>,
    shutdown: CancellationToken,
) -> Result<(), MeshError> {
    let mut interval = tokio::time::interval(context.config.reconcile_interval);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut revision = context.revision.subscribe();

    loop {
        tokio::select! {
            () = shutdown.cancelled() => return Ok(()),
            _ = interval.tick() => {}
            result = revision.changed() => {
                if result.is_err() {
                    return Ok(());
                }
            }
        }

        for _ in 0..MAX_RECONCILE_ROUNDS {
            let more = timeout(
                EXCHANGE_TIMEOUT,
                initiate_sync(connection, context, peer, &peer_frontier),
            )
            .await
            .map_err(|_| MeshError::ExchangeTimeout)??;
            if !more {
                break;
            }
        }
    }
}

async fn initiate_sync(
    connection: &Connection,
    context: &RuntimeContext,
    peer: NodeId,
    peer_frontier: &Mutex<SeenOps>,
) -> Result<bool, MeshError> {
    let advertised = peer_frontier.lock().await.clone();
    let (frontier, known_members, operations, request_has_more) =
        batch_for_peer(context, &advertised).await?;
    let request = SyncRequest {
        frontier,
        operations,
        has_more: request_has_more,
        known_members,
    };
    let (mut send, mut recv) = connection.open_bi().await?;
    send.write_all(&[STREAM_KIND_SYNC]).await?;
    write_message(&mut send, &request).await?;
    let response: SyncResponse = read_message(&mut recv).await?;
    validate_batch(
        &response.frontier,
        &response.known_members,
        &response.operations,
    )?;
    tracing::debug!(
        pushed_operations = request.operations.len(),
        received_operations = response.operations.len(),
        "completed mesh reconciliation exchange"
    );
    let response_frontier = decode_frontier(&response.frontier)?;
    let known_members = decode_membership(&response.known_members)?;
    persist_and_record(
        context,
        peer,
        response_frontier.clone(),
        known_members,
        response.operations,
    )
    .await?;
    *peer_frontier.lock().await = response_frontier;
    send.finish()?;
    Ok(request_has_more || response.has_more)
}

async fn initiate_chunk_streams(
    connection: &Connection,
    context: &RuntimeContext,
    shutdown: CancellationToken,
) -> Result<(), MeshError> {
    let Some(_) = &context.chunk_tx else {
        std::future::pending::<()>().await;
        return Ok(());
    };
    let mut interval = tokio::time::interval(context.config.reconcile_interval);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut revision = context.revision.subscribe();
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return Ok(()),
            _ = interval.tick() => {}
            result = revision.changed() => {
                if result.is_err() {
                    return Ok(());
                }
            }
        }
        let missing = broker_missing(context, MAX_MISSING_CHUNKS_PER_ROUND).await?;
        let semaphore = Arc::new(Semaphore::new(context.config.max_concurrent_chunk_streams));
        let mut tasks = JoinSet::new();
        for request in missing {
            let permit = semaphore
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| MeshError::PersistenceUnavailable)?;
            let connection = connection.clone();
            let chunk_tx = context
                .chunk_tx
                .as_ref()
                .expect("checked transfer broker")
                .clone();
            tasks.spawn(async move {
                let _permit = permit;
                request_chunk(&connection, &chunk_tx, request).await
            });
        }
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::debug!(%error, "chunk request did not complete"),
                Err(error) => tracing::debug!(%error, "chunk request task failed"),
            }
        }
    }
}

async fn request_chunk(
    connection: &Connection,
    chunk_tx: &mpsc::Sender<MeshChunkCommand>,
    request: TransferChunk,
) -> Result<(), MeshError> {
    let (mut send, mut recv) = connection.open_bi().await?;
    send.write_all(&[STREAM_KIND_CHUNK]).await?;
    write_message(
        &mut send,
        &ChunkStreamRequest {
            transfer_id: request.transfer_id.as_uuid().as_bytes().to_vec(),
            manifest_id: request.manifest_id.as_bytes().to_vec(),
            chunk_id: request.chunk_id.as_bytes().to_vec(),
            logical_size: request.logical_size,
        },
    )
    .await?;
    let response: ChunkStreamResponse =
        read_message_bounded(&mut recv, MAX_CHUNK_CONTROL_BYTES).await?;
    validate_chunk_response(&response, request)?;
    if !response.available {
        send.finish()?;
        return Ok(());
    }
    let encrypted_size = usize::try_from(response.encrypted_size)
        .map_err(|_| MeshError::ChunkFrameTooLarge(usize::MAX))?;
    if encrypted_size == 0 || encrypted_size > MAX_ENCRYPTED_CHUNK_BYTES {
        return Err(MeshError::ChunkFrameTooLarge(encrypted_size));
    }
    let mut encrypted = vec![0_u8; encrypted_size];
    recv.read_exact(&mut encrypted).await?;
    send.finish()?;
    broker_import(chunk_tx, request, encrypted).await
}

async fn answer_chunk(
    send: &mut quinn::SendStream,
    recv: &mut quinn::RecvStream,
    context: &RuntimeContext,
) -> Result<(), MeshError> {
    let request: ChunkStreamRequest = read_message_bounded(recv, MAX_CHUNK_CONTROL_BYTES).await?;
    let request = parse_chunk_request(&request)?;
    let encrypted = match broker_export(context, request).await {
        Ok(encrypted) => encrypted,
        Err(error) => {
            tracing::debug!(%error, "requested chunk is unavailable");
            write_message(
                send,
                &ChunkStreamResponse {
                    available: false,
                    transfer_id: request.transfer_id.as_uuid().as_bytes().to_vec(),
                    chunk_id: request.chunk_id.as_bytes().to_vec(),
                    encrypted_size: 0,
                },
            )
            .await?;
            send.finish()?;
            return Ok(());
        }
    };
    if encrypted.is_empty() || encrypted.len() > MAX_ENCRYPTED_CHUNK_BYTES {
        return Err(MeshError::ChunkFrameTooLarge(encrypted.len()));
    }
    write_message(
        send,
        &ChunkStreamResponse {
            available: true,
            transfer_id: request.transfer_id.as_uuid().as_bytes().to_vec(),
            chunk_id: request.chunk_id.as_bytes().to_vec(),
            encrypted_size: u32::try_from(encrypted.len())
                .map_err(|_| MeshError::ChunkFrameTooLarge(encrypted.len()))?,
        },
    )
    .await?;
    send.write_all(&encrypted).await?;
    send.finish()?;
    Ok(())
}

async fn batch_for_peer(
    context: &RuntimeContext,
    peer_frontier: &SeenOps,
) -> Result<(Vec<u8>, Vec<u8>, Vec<Vec<u8>>, bool), MeshError> {
    let state = context.state.read().await;
    let batch = state.compute_batch(peer_frontier, &context.config.batch_limits);
    let frontier = encode_frontier(state.seen())?;
    let members = context.known_members.read().await;
    let known_members = encode_membership(&members)?;
    Ok((
        frontier,
        known_members,
        batch.entries().to_vec(),
        batch.has_more(),
    ))
}

async fn persist_and_record(
    context: &RuntimeContext,
    peer: NodeId,
    peer_frontier: SeenOps,
    known_members: BTreeSet<NodeId>,
    operations: Vec<Vec<u8>>,
) -> Result<(), MeshError> {
    let codec = JsonV1Codec;
    let decoded = operations
        .into_iter()
        .map(|operation| {
            let decoded = codec.decode_op(&operation)?;
            let canonical = codec.encode_op(&decoded)?;
            Ok((canonical, decoded))
        })
        .collect::<Result<Vec<_>, MeshError>>()?;
    let operations = decoded
        .iter()
        .map(|(operation, _)| operation.clone())
        .collect::<Vec<_>>();

    let (reply, completed) = oneshot::channel();
    context
        .persist_tx
        .send(PersistBatch {
            peer,
            peer_frontier,
            known_members: known_members.clone(),
            operations: operations.clone(),
            reply,
        })
        .await
        .map_err(|_| MeshError::PersistenceUnavailable)?;
    let persisted = timeout(PERSIST_TIMEOUT, completed)
        .await
        .map_err(|_| MeshError::PersistenceTimeout)?
        .map_err(|_| MeshError::PersistenceUnavailable)?
        .map_err(MeshError::PersistenceRejected)?;

    let mut state = context.state.write().await;
    for operation in &operations {
        state.ingest_raw(operation, &JsonV1Codec)?;
    }
    state.compact_log(persisted.compacted_operations());
    drop(state);

    let mut members = context.known_members.write().await;
    members.insert(peer);
    members.extend(known_members);
    for (_, operation) in &decoded {
        members.insert(operation.id().node());
    }
    drop(members);

    for (_, operation) in decoded {
        if let Operation::ForgetDevice { node_id } = operation.operation() {
            context.forgotten_devices.write().await.insert(*node_id);
            if let Some(active) = context.registry.lock().await.remove(node_id) {
                active
                    .connection
                    .close(CLOSE_FORGOTTEN.into(), b"device identity forgotten");
            }
        }
    }
    bump_revision(&context.revision);
    Ok(())
}

async fn broker_missing(
    context: &RuntimeContext,
    maximum: usize,
) -> Result<Vec<TransferChunk>, MeshError> {
    let sender = context
        .chunk_tx
        .as_ref()
        .ok_or(MeshError::ChunkBrokerUnavailable)?;
    let (reply, completed) = oneshot::channel();
    sender
        .send(MeshChunkCommand::Missing { maximum, reply })
        .await
        .map_err(|_| MeshError::ChunkBrokerUnavailable)?;
    timeout(CHUNK_BROKER_TIMEOUT, completed)
        .await
        .map_err(|_| MeshError::ChunkBrokerTimeout)?
        .map_err(|_| MeshError::ChunkBrokerUnavailable)?
        .map_err(MeshError::ChunkBrokerRejected)
}

async fn broker_export(
    context: &RuntimeContext,
    request: TransferChunk,
) -> Result<Vec<u8>, MeshError> {
    let sender = context
        .chunk_tx
        .as_ref()
        .ok_or(MeshError::ChunkBrokerUnavailable)?;
    let (reply, completed) = oneshot::channel();
    sender
        .send(MeshChunkCommand::Export { request, reply })
        .await
        .map_err(|_| MeshError::ChunkBrokerUnavailable)?;
    timeout(CHUNK_BROKER_TIMEOUT, completed)
        .await
        .map_err(|_| MeshError::ChunkBrokerTimeout)?
        .map_err(|_| MeshError::ChunkBrokerUnavailable)?
        .map_err(MeshError::ChunkBrokerRejected)
}

async fn broker_import(
    sender: &mpsc::Sender<MeshChunkCommand>,
    request: TransferChunk,
    encrypted: Vec<u8>,
) -> Result<(), MeshError> {
    let (reply, completed) = oneshot::channel();
    sender
        .send(MeshChunkCommand::Import {
            request,
            encrypted,
            reply,
        })
        .await
        .map_err(|_| MeshError::ChunkBrokerUnavailable)?;
    timeout(CHUNK_BROKER_TIMEOUT, completed)
        .await
        .map_err(|_| MeshError::ChunkBrokerTimeout)?
        .map_err(|_| MeshError::ChunkBrokerUnavailable)?
        .map_err(MeshError::ChunkBrokerRejected)
}

fn parse_chunk_request(message: &ChunkStreamRequest) -> Result<TransferChunk, MeshError> {
    if message.logical_size == 0 {
        return Err(MeshError::InvalidChunkRequest);
    }
    let transfer_id = TransferId::from_uuid(
        Uuid::from_slice(&message.transfer_id).map_err(|_| MeshError::InvalidChunkRequest)?,
    );
    let manifest_id = hex::encode(&message.manifest_id)
        .parse()
        .map_err(|_| MeshError::InvalidChunkRequest)?;
    let chunk_id = hex::encode(&message.chunk_id)
        .parse()
        .map_err(|_| MeshError::InvalidChunkRequest)?;
    Ok(TransferChunk {
        transfer_id,
        manifest_id,
        chunk_id,
        logical_size: message.logical_size,
    })
}

fn validate_chunk_response(
    response: &ChunkStreamResponse,
    request: TransferChunk,
) -> Result<(), MeshError> {
    if response.transfer_id != request.transfer_id.as_uuid().as_bytes()
        || response.chunk_id != request.chunk_id.as_bytes()
        || response.available != (response.encrypted_size != 0)
    {
        return Err(MeshError::InvalidChunkResponse);
    }
    Ok(())
}

fn encode_frontier(frontier: &SeenOps) -> Result<Vec<u8>, MeshError> {
    let encoded = serde_json::to_vec(frontier)?;
    if encoded.len() > MAX_FRONTIER_BYTES {
        return Err(MeshError::Protocol(ProtocolError::FrontierTooLarge(
            encoded.len(),
        )));
    }
    Ok(encoded)
}

fn decode_frontier(encoded: &[u8]) -> Result<SeenOps, MeshError> {
    if encoded.len() > MAX_FRONTIER_BYTES {
        return Err(MeshError::Protocol(ProtocolError::FrontierTooLarge(
            encoded.len(),
        )));
    }
    serde_json::from_slice(encoded).map_err(MeshError::Frontier)
}

fn encode_membership(members: &BTreeSet<NodeId>) -> Result<Vec<u8>, MeshError> {
    let encoded = serde_json::to_vec(members)?;
    if encoded.len() > MAX_MEMBERSHIP_BYTES {
        return Err(MeshError::Protocol(ProtocolError::MembershipTooLarge(
            encoded.len(),
        )));
    }
    Ok(encoded)
}

fn decode_membership(encoded: &[u8]) -> Result<BTreeSet<NodeId>, MeshError> {
    if encoded.len() > MAX_MEMBERSHIP_BYTES {
        return Err(MeshError::Protocol(ProtocolError::MembershipTooLarge(
            encoded.len(),
        )));
    }
    serde_json::from_slice(encoded).map_err(MeshError::Membership)
}

fn bump_revision(revision: &watch::Sender<u64>) {
    revision.send_modify(|value| *value = value.wrapping_add(1));
}

async fn wait_backoff(
    config: &MeshRuntimeConfig,
    address: SocketAddr,
    attempt: u32,
    shutdown: &CancellationToken,
) -> bool {
    let shift = attempt.min(20);
    let multiplier = 1_u32 << shift;
    let base = config
        .reconnect_min
        .saturating_mul(multiplier)
        .min(config.reconnect_max);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    address.hash(&mut hasher);
    attempt.hash(&mut hasher);
    let jitter_limit = (base.as_millis() / 4).max(1);
    let jitter = u64::try_from(u128::from(hasher.finish()) % jitter_limit).unwrap_or(0);
    let delay = base.saturating_add(Duration::from_millis(jitter));
    tokio::select! {
        () = shutdown.cancelled() => true,
        () = tokio::time::sleep(delay) => false,
    }
}

fn validate_local_config(config: &MeshRuntimeConfig) -> Result<(), MeshError> {
    if !valid_hostname(&config.hostname) {
        return Err(MeshError::InvalidHostname);
    }
    if config.reconcile_interval.is_zero()
        || config.reconnect_min.is_zero()
        || config.reconnect_max < config.reconnect_min
        || config.listen_port == 0
        || config.batch_limits.max_ops == 0
        || config.batch_limits.max_ops > MAX_BATCH_OPERATIONS
        || config.batch_limits.max_bytes == 0
        || config.batch_limits.max_bytes > super::protocol::MAX_CONTROL_FRAME_BYTES
        || config.max_concurrent_chunk_streams == 0
        || config.max_concurrent_chunk_streams > 32
    {
        return Err(MeshError::InvalidConfig);
    }
    Ok(())
}

fn valid_hostname(hostname: &str) -> bool {
    !hostname.is_empty()
        && hostname.len() <= MAX_HOSTNAME_BYTES
        && !hostname.chars().any(char::is_control)
}

#[derive(Debug, Error)]
pub enum MeshError {
    #[error("mesh runtime configuration is invalid")]
    InvalidConfig,
    #[error("mesh hostname is invalid")]
    InvalidHostname,
    #[error("peer node identity is invalid")]
    InvalidNodeId,
    #[error("peer uses unsupported protocol version {0}")]
    UnsupportedProtocol(u32),
    #[error("peer duplicated the local active node identity {0}")]
    DuplicateNodeIdentity(NodeId),
    #[error("forgotten peer node identity {0} was rejected")]
    ForgottenNodeIdentity(NodeId),
    #[error("peer {0} already has a canonical active connection")]
    DuplicateConnection(NodeId),
    #[error("identity handshake timed out")]
    HandshakeTimeout,
    #[error("replication exchange timed out")]
    ExchangeTimeout,
    #[error("daemon persistence timed out")]
    PersistenceTimeout,
    #[error("daemon persistence service is unavailable")]
    PersistenceUnavailable,
    #[error("daemon rejected a remote operation batch: {0}")]
    PersistenceRejected(String),
    #[error("authenticated chunk broker timed out")]
    ChunkBrokerTimeout,
    #[error("authenticated chunk broker is unavailable")]
    ChunkBrokerUnavailable,
    #[error("daemon rejected chunk work: {0}")]
    ChunkBrokerRejected(String),
    #[error("unknown authenticated stream kind {0}")]
    UnknownStreamKind(u8),
    #[error("chunk stream frame is too large ({0} bytes)")]
    ChunkFrameTooLarge(usize),
    #[error("chunk request is invalid")]
    InvalidChunkRequest,
    #[error("chunk response is invalid")]
    InvalidChunkResponse,
    #[error("frontier is malformed: {0}")]
    Frontier(serde_json::Error),
    #[error("frontier serialization failed: {0}")]
    FrontierSerialization(#[from] serde_json::Error),
    #[error("membership advertisement is malformed: {0}")]
    Membership(serde_json::Error),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error(transparent)]
    Codec(#[from] crate::replication::CodecError),
    #[error(transparent)]
    Replication(#[from] crate::replication::AntiEntropyError),
    #[error("QUIC connection failed: {0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error("could not finish a QUIC stream: {0}")]
    Finish(#[from] quinn::ClosedStream),
    #[error("could not write a QUIC stream: {0}")]
    StreamWrite(#[from] quinn::WriteError),
    #[error("could not read a QUIC stream: {0}")]
    StreamRead(#[from] quinn::ReadExactError),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(version: u32) -> IdentityHello {
        IdentityHello {
            protocol_version: version,
            node_id: Uuid::from_u128(7).as_bytes().to_vec(),
            hostname: "node".to_owned(),
            frontier: serde_json::to_vec(&SeenOps::default()).unwrap(),
            known_members: serde_json::to_vec(&BTreeSet::<NodeId>::new()).unwrap(),
        }
    }

    #[test]
    fn rolling_protocol_version_mismatch_is_rejected_before_session_state() {
        assert!(matches!(
            parse_identity(hello(PROTOCOL_VERSION - 1)),
            Err(MeshError::UnsupportedProtocol(version)) if version == PROTOCOL_VERSION - 1
        ));
        assert!(matches!(
            parse_identity(hello(PROTOCOL_VERSION + 1)),
            Err(MeshError::UnsupportedProtocol(version)) if version == PROTOCOL_VERSION + 1
        ));
    }

    #[test]
    fn malformed_membership_advertisement_is_rejected() {
        let mut malformed = hello(PROTOCOL_VERSION);
        malformed.known_members = b"not-json".to_vec();
        assert!(matches!(
            parse_identity(malformed),
            Err(MeshError::Membership(_))
        ));
    }
}
