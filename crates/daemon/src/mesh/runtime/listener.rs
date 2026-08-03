use std::{
    collections::{BTreeMap, BTreeSet},
    hash::{Hash, Hasher},
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use quinn::Endpoint;
use tokio::{
    sync::{Semaphore, watch},
    task::{JoinHandle, JoinSet},
    time::timeout,
};
use tokio_util::sync::CancellationToken;

use clip_sync_core::transport::{authenticate_client, authenticate_server, mesh_endpoint};

use crate::discovery::{DiscoverySnapshot, MAX_DISCOVERED_PEERS};

use super::Direction;
use super::{
    CLOSE_SHUTDOWN, MAX_ACTIVE_CONNECTIONS, MAX_CONCURRENT_HANDSHAKES, MAX_GENERATION_TASKS,
    MeshError, MeshRuntimeConfig, RuntimeContext, SERVER_NAME, session::run_connection,
};

pub(super) async fn supervise(
    context: Arc<RuntimeContext>,
    mut discovery: watch::Receiver<Option<DiscoverySnapshot>>,
    shutdown: CancellationToken,
) {
    let mut generations = Vec::<ListenerGeneration>::new();

    loop {
        let next = discovery.borrow_and_update().clone();
        if let Some(snapshot) = next {
            let desired_binds = snapshot
                .local_addresses
                .iter()
                .map(|address| SocketAddr::new(*address, context.config.listen_port))
                .collect::<BTreeSet<_>>();
            let active_binds = generations
                .iter()
                .map(|generation| generation.bind)
                .collect::<BTreeSet<_>>();
            let must_restart = active_binds != desired_binds
                || generations
                    .iter()
                    .any(|generation| generation.task.is_finished());
            if must_restart {
                stop_generations(&mut generations).await;
                let mut errors = Vec::new();
                for bind in desired_binds {
                    match mesh_endpoint(bind) {
                        Ok(endpoint) => {
                            let generation_shutdown = shutdown.child_token();
                            let (peers, peer_updates) =
                                watch::channel(discovered_addresses(&snapshot, bind.ip()));
                            let task = tokio::spawn(run_generation(
                                endpoint,
                                peer_updates,
                                context.clone(),
                                generation_shutdown.clone(),
                            ));
                            generations.push(ListenerGeneration {
                                bind,
                                shutdown: generation_shutdown,
                                peers,
                                task,
                            });
                            tracing::info!(%bind, "mesh QUIC listener active");
                        }
                        Err(error) => {
                            errors.push(format!("{bind}: {error}"));
                            tracing::warn!(%bind, %error, "could not bind mesh QUIC listener");
                        }
                    }
                }
                context.status.send_modify(|status| {
                    status.listener_address = generations.first().map(|generation| generation.bind);
                    status.discovered_addresses = snapshot
                        .peers
                        .iter()
                        .map(|peer| SocketAddr::new(peer.address, peer.port))
                        .collect::<BTreeSet<_>>()
                        .len();
                    status.last_listener_error = (!errors.is_empty()).then(|| errors.join("; "));
                });
            } else {
                for generation in &generations {
                    generation
                        .peers
                        .send_replace(discovered_addresses(&snapshot, generation.bind.ip()));
                }
                context.status.send_modify(|status| {
                    status.discovered_addresses = snapshot
                        .peers
                        .iter()
                        .map(|peer| SocketAddr::new(peer.address, peer.port))
                        .collect::<BTreeSet<_>>()
                        .len();
                });
            }
        } else {
            stop_generations(&mut generations).await;
            context.status.send_modify(|status| {
                status.listener_address = None;
                status.discovered_addresses = 0;
                status.active_connections = 0;
            });
        }

        tokio::select! {
            () = shutdown.cancelled() => break,
            result = discovery.changed() => {
                if result.is_err() {
                    break;
                }
            }
            () = tokio::time::sleep(context.config.reconnect_min) => {}
        }
    }

    stop_generations(&mut generations).await;
    context.status.send_modify(|status| {
        status.listener_address = None;
        status.discovered_addresses = 0;
        status.active_connections = 0;
    });
}

#[derive(Debug)]
struct ListenerGeneration {
    bind: SocketAddr,
    shutdown: CancellationToken,
    peers: watch::Sender<Vec<SocketAddr>>,
    task: JoinHandle<()>,
}

async fn stop_generations(generations: &mut Vec<ListenerGeneration>) {
    for generation in generations.drain(..) {
        generation.shutdown.cancel();
        if let Err(error) = generation.task.await {
            tracing::warn!(%error, "mesh listener generation did not stop cleanly");
        }
    }
}

fn discovered_addresses(snapshot: &DiscoverySnapshot, local_address: IpAddr) -> Vec<SocketAddr> {
    snapshot
        .peers
        .iter()
        .filter(|peer| {
            peer.connected && peer.local_address == local_address && peer.address != local_address
        })
        .map(|peer| SocketAddr::new(peer.address, peer.port))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .take(MAX_DISCOVERED_PEERS)
        .collect()
}

async fn run_generation(
    endpoint: Endpoint,
    mut peer_updates: watch::Receiver<Vec<SocketAddr>>,
    context: Arc<RuntimeContext>,
    shutdown: CancellationToken,
) {
    let handshakes = Arc::new(Semaphore::new(MAX_CONCURRENT_HANDSHAKES));
    let sessions = Arc::new(Semaphore::new(MAX_ACTIVE_CONNECTIONS));
    let mut tasks = JoinSet::new();
    let mut dialers = BTreeMap::<SocketAddr, CancellationToken>::new();
    update_dialers(
        &endpoint,
        &context,
        &shutdown,
        &mut tasks,
        &mut dialers,
        &peer_updates.borrow_and_update(),
        &sessions,
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
                    &sessions,
                );
            }
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else {
                    break;
                };
                if tasks.len() >= MAX_GENERATION_TASKS {
                    incoming.refuse();
                    continue;
                }
                let Ok(permit) = handshakes.clone().try_acquire_owned() else {
                    incoming.refuse();
                    continue;
                };
                spawn_incoming(
                    &mut tasks,
                    incoming,
                    permit,
                    sessions.clone(),
                    context.clone(),
                    shutdown.clone(),
                );
            }
            completed = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Err(error)) = completed {
                    tracing::warn!(%error, "mesh connection task panicked");
                }
                update_dialers(
                    &endpoint,
                    &context,
                    &shutdown,
                    &mut tasks,
                    &mut dialers,
                    &peer_updates.borrow(),
                    &sessions,
                );
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
    sessions: Arc<Semaphore>,
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
        let Ok(session_permit) = sessions.try_acquire_owned() else {
            authenticated
                .connection()
                .close(CLOSE_SHUTDOWN.into(), b"mesh connection limit reached");
            return;
        };
        if let Err(error) = run_connection(
            authenticated.into_inner(),
            Direction::Inbound,
            context,
            shutdown,
            session_permit,
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
    sessions: &Arc<Semaphore>,
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
        if dialers.contains_key(address) || tasks.len() >= MAX_GENERATION_TASKS {
            continue;
        }
        let cancellation = shutdown.child_token();
        dialers.insert(*address, cancellation.clone());
        tasks.spawn(dial_peer(
            endpoint.clone(),
            *address,
            context.clone(),
            cancellation,
            sessions.clone(),
        ));
    }
}

async fn dial_peer(
    endpoint: Endpoint,
    address: SocketAddr,
    context: Arc<RuntimeContext>,
    shutdown: CancellationToken,
    sessions: Arc<Semaphore>,
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
        let Ok(session_permit) = sessions.clone().try_acquire_owned() else {
            authenticated
                .connection()
                .close(CLOSE_SHUTDOWN.into(), b"mesh connection limit reached");
            if wait_backoff(&context.config, address, attempt, &shutdown).await {
                return;
            }
            attempt = attempt.saturating_add(1);
            continue;
        };

        attempt = 0;
        let mut duplicate = false;
        if let Err(error) = run_connection(
            authenticated.into_inner(),
            Direction::Outbound,
            context.clone(),
            shutdown.clone(),
            session_permit,
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
