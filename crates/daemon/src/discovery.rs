use std::{
    collections::{BTreeMap, BTreeSet},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6},
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use hmac::{Hmac, Mac};
use ipnet::IpNet;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use socket2::{Domain, Protocol, Socket, Type};
use thiserror::Error;
use tokio::{net::UdpSocket, process::Command, task::JoinSet, time::timeout};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

pub const MAX_DISCOVERED_PEERS: usize = 512;
const DISCOVERY_PORT: u16 = 24_891;
const MULTICAST_V4: Ipv4Addr = Ipv4Addr::new(239, 255, 67, 83);
const MULTICAST_V6: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0x4353, 0);
const BEACON_INTERVAL: Duration = Duration::from_secs(2);
const UNICAST_PROBE_INTERVAL: Duration = Duration::from_millis(2);
const MAX_UNICAST_PROBES_PER_WINDOW: u64 = 4_096;
const MAX_CLOCK_SKEW: Duration = Duration::from_mins(2);
const BEACON_MAGIC: &[u8; 4] = b"CSD0";
const BEACON_VERSION: u8 = 1;
const NONCE_LEN: usize = 16;
const MAC_LEN: usize = 32;
const BEACON_BODY_LEN: usize = 4 + 1 + 1 + 2 + 8 + NONCE_LEN;
const BEACON_LEN: usize = BEACON_BODY_LEN + MAC_LEN;
const MAX_INTERFACE_OUTPUT_BYTES: usize = 1024 * 1024;

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum BeaconKind {
    Probe = 1,
    Response = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Beacon {
    kind: BeaconKind,
    port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiscoverySnapshot {
    pub local_addresses: Vec<IpAddr>,
    pub local_hostname: String,
    pub peers: Vec<DiscoveredPeer>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct DiscoveredPeer {
    pub hostname: String,
    pub address: IpAddr,
    pub port: u16,
    pub local_address: IpAddr,
    pub connected: bool,
}

pub struct InterfaceDiscovery {
    ip_command: PathBuf,
    peer_interfaces: Vec<String>,
    local_hostname: String,
    listen_port: u16,
    key: Zeroizing<[u8; 32]>,
    command_timeout: Duration,
    discovery_window: Duration,
}

impl InterfaceDiscovery {
    #[must_use]
    pub fn new(
        peer_interfaces: Vec<String>,
        local_hostname: String,
        listen_port: u16,
        key: Zeroizing<[u8; 32]>,
        discovery_window: Duration,
    ) -> Self {
        Self {
            ip_command: PathBuf::from("ip"),
            peer_interfaces,
            local_hostname,
            listen_port,
            key,
            command_timeout: Duration::from_secs(5),
            discovery_window,
        }
    }

    #[cfg(test)]
    fn with_ip_command(mut self, ip_command: impl Into<PathBuf>) -> Self {
        self.ip_command = ip_command.into();
        self
    }

    /// Runs one continuous authenticated multicast discovery window.
    ///
    /// # Errors
    ///
    /// Returns an error if interface enumeration, socket setup, beacon I/O, or
    /// bounded worker collection fails.
    pub async fn discover(
        &self,
        shutdown: CancellationToken,
    ) -> Result<DiscoverySnapshot, DiscoveryError> {
        let endpoints = self.interface_endpoints().await?;
        if endpoints.is_empty() {
            return Err(DiscoveryError::InterfacesHaveNoAddresses);
        }
        let local_addresses = endpoints
            .iter()
            .map(|endpoint| endpoint.address)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let local_address_set = local_addresses.iter().copied().collect::<BTreeSet<_>>();
        let mut tasks = JoinSet::new();
        for endpoint in endpoints {
            let key = *self.key;
            let local_addresses = local_address_set.clone();
            let child_shutdown = shutdown.child_token();
            let listen_port = self.listen_port;
            let discovery_window = self.discovery_window;
            tasks.spawn(async move {
                discover_on_endpoint(
                    endpoint,
                    &key,
                    listen_port,
                    discovery_window,
                    &local_addresses,
                    child_shutdown,
                )
                .await
            });
        }

        let mut peers = BTreeSet::new();
        while let Some(result) = tasks.join_next().await {
            let discovered = result.map_err(DiscoveryError::Task)??;
            peers.extend(discovered);
            if peers.len() > MAX_DISCOVERED_PEERS {
                return Err(DiscoveryError::TooManyPeers {
                    observed: peers.len(),
                    maximum: MAX_DISCOVERED_PEERS,
                });
            }
        }

        Ok(DiscoverySnapshot {
            local_addresses,
            local_hostname: self.local_hostname.clone(),
            peers: peers.into_iter().collect(),
        })
    }

    async fn interface_endpoints(&self) -> Result<Vec<InterfaceEndpoint>, DiscoveryError> {
        if self.peer_interfaces.is_empty() {
            return Err(DiscoveryError::NoInterfacesConfigured);
        }
        let mut command = Command::new(&self.ip_command);
        command.args(["-j", "address", "show"]).kill_on_drop(true);
        let output = timeout(self.command_timeout, command.output())
            .await
            .map_err(|_| DiscoveryError::InterfaceTimeout)??;
        if !output.status.success() {
            return Err(DiscoveryError::InterfaceCommandFailed(output.status.code()));
        }
        if output.stdout.len() > MAX_INTERFACE_OUTPUT_BYTES {
            return Err(DiscoveryError::InterfaceOutputTooLarge);
        }
        parse_interface_endpoints(&output.stdout, &self.peer_interfaces)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct InterfaceEndpoint {
    name: String,
    index: u32,
    address: IpAddr,
    network: IpNet,
}

#[derive(Debug, Deserialize)]
struct InterfaceStatus {
    ifindex: u32,
    ifname: String,
    #[serde(default)]
    addr_info: Vec<InterfaceAddress>,
}

#[derive(Debug, Deserialize)]
struct InterfaceAddress {
    family: String,
    local: String,
    prefixlen: u8,
    scope: String,
}

fn parse_interface_endpoints(
    source: &[u8],
    selected: &[String],
) -> Result<Vec<InterfaceEndpoint>, DiscoveryError> {
    let interfaces: Vec<InterfaceStatus> = serde_json::from_slice(source)?;
    let selected = selected.iter().collect::<BTreeSet<_>>();
    let available = interfaces
        .iter()
        .map(|interface| interface.ifname.as_str())
        .collect::<BTreeSet<_>>();
    if !selected
        .iter()
        .any(|name| available.contains(name.as_str()))
    {
        return Err(DiscoveryError::InterfacesUnavailable(
            selected.into_iter().cloned().collect::<Vec<_>>().join(", "),
        ));
    }

    let mut endpoints = Vec::new();
    for interface in interfaces
        .into_iter()
        .filter(|interface| selected.contains(&interface.ifname))
    {
        for address in interface.addr_info {
            if address.scope != "global" || !matches!(address.family.as_str(), "inet" | "inet6") {
                continue;
            }
            let local: IpAddr = address
                .local
                .parse()
                .map_err(|_| DiscoveryError::InvalidAddress(address.local.clone()))?;
            let network = IpNet::new(local, address.prefixlen)
                .map_err(|_| DiscoveryError::InvalidPrefix(address.prefixlen))?;
            endpoints.push(InterfaceEndpoint {
                name: interface.ifname.clone(),
                index: interface.ifindex,
                address: local,
                network,
            });
        }
    }
    endpoints.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.address.cmp(&right.address))
    });
    endpoints.dedup_by(|left, right| left.address == right.address);
    Ok(endpoints)
}

async fn discover_on_endpoint(
    endpoint: InterfaceEndpoint,
    key: &[u8; 32],
    listen_port: u16,
    discovery_window: Duration,
    local_addresses: &BTreeSet<IpAddr>,
    shutdown: CancellationToken,
) -> Result<Vec<DiscoveredPeer>, DiscoveryError> {
    let (unicast_socket, multicast) = discovery_sockets(&endpoint)?;
    let (multicast_socket, mut multicast_target) = multicast
        .map_or((None, None), |(socket, target)| {
            (Some(socket), Some(target))
        });
    let probe_interval = if multicast_target.is_some() {
        BEACON_INTERVAL
    } else {
        UNICAST_PROBE_INTERVAL
    };
    let mut interval = tokio::time::interval(probe_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let now = unix_time_seconds()?;
    let unicast_targets = unicast_probe_targets(&endpoint, now, discovery_window);
    let mut unicast_targets = unicast_targets.into_iter();
    let deadline = tokio::time::sleep(discovery_window);
    tokio::pin!(deadline);
    let mut unicast_frame = [0_u8; 128];
    let mut multicast_frame = [0_u8; 128];
    let mut peers = BTreeMap::<IpAddr, DiscoveredPeer>::new();

    loop {
        let event = tokio::select! {
            () = shutdown.cancelled() => DiscoveryEvent::Stop,
            () = &mut deadline => DiscoveryEvent::Stop,
            _ = interval.tick() => DiscoveryEvent::Tick,
            received = unicast_socket.recv_from(&mut unicast_frame) => {
                DiscoveryEvent::Unicast(received)
            }
            received = receive_optional(multicast_socket.as_ref(), &mut multicast_frame) => {
                DiscoveryEvent::Multicast(received)
            }
        };
        let (received, frame) = match event {
            DiscoveryEvent::Stop => break,
            DiscoveryEvent::Tick => {
                let target = multicast_target.or_else(|| unicast_targets.next());
                if let Some(target) = target {
                    let beacon =
                        encode_beacon(BeaconKind::Probe, listen_port, unix_time_seconds()?, key)?;
                    let sender = multicast_target
                        .and(multicast_socket.as_ref())
                        .unwrap_or(&unicast_socket);
                    if let Err(error) = sender.send_to(&beacon, target).await {
                        if multicast_target.take().is_some() {
                            tracing::debug!(
                                interface = %endpoint.name,
                                %error,
                                "multicast probes cannot be routed; using bounded authenticated unicast probes"
                            );
                            interval = tokio::time::interval(UNICAST_PROBE_INTERVAL);
                            interval
                                .set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                        } else {
                            tracing::trace!(interface = %endpoint.name, %target, %error, "discovery probe could not be sent");
                        }
                    }
                }
                continue;
            }
            DiscoveryEvent::Unicast(received) => (received, &unicast_frame[..]),
            DiscoveryEvent::Multicast(received) => (received, &multicast_frame[..]),
        };
        let (length, source) = received?;
        let source_address = source.ip();
        if local_addresses.contains(&source_address) || !endpoint.network.contains(&source_address)
        {
            continue;
        }
        let beacon = match decode_beacon(&frame[..length], unix_time_seconds()?, key) {
            Ok(beacon) => beacon,
            Err(DiscoveryError::InvalidBeacon | DiscoveryError::StaleBeacon) => continue,
            Err(error) => return Err(error),
        };
        if beacon.kind == BeaconKind::Probe {
            let response =
                encode_beacon(BeaconKind::Response, listen_port, unix_time_seconds()?, key)?;
            if let Err(error) = unicast_socket.send_to(&response, source).await {
                tracing::trace!(interface = %endpoint.name, %source, %error, "discovery response could not be sent");
            }
        }
        peers.insert(
            source_address,
            DiscoveredPeer {
                hostname: source_address.to_string(),
                address: source_address,
                port: beacon.port,
                local_address: endpoint.address,
                connected: true,
            },
        );
        if peers.len() > MAX_DISCOVERED_PEERS {
            return Err(DiscoveryError::TooManyPeers {
                observed: peers.len(),
                maximum: MAX_DISCOVERED_PEERS,
            });
        }
    }
    Ok(peers.into_values().collect())
}

enum DiscoveryEvent {
    Stop,
    Tick,
    Unicast(std::io::Result<(usize, SocketAddr)>),
    Multicast(std::io::Result<(usize, SocketAddr)>),
}

async fn receive_optional(
    socket: Option<&UdpSocket>,
    frame: &mut [u8],
) -> std::io::Result<(usize, SocketAddr)> {
    match socket {
        Some(socket) => socket.recv_from(frame).await,
        None => std::future::pending().await,
    }
}

fn discovery_sockets(
    endpoint: &InterfaceEndpoint,
) -> Result<(UdpSocket, Option<(UdpSocket, SocketAddr)>), DiscoveryError> {
    let unicast = unicast_socket(endpoint)?;
    match multicast_socket(endpoint) {
        Ok(multicast) => Ok((unicast, Some(multicast))),
        Err(error) => {
            tracing::debug!(
                interface = %endpoint.name,
                %error,
                "multicast is unavailable; using bounded authenticated unicast probes"
            );
            Ok((unicast, None))
        }
    }
}

fn base_socket(domain: Domain) -> Result<Socket, std::io::Error> {
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    socket.set_nonblocking(true)?;
    Ok(socket)
}

fn multicast_socket(
    endpoint: &InterfaceEndpoint,
) -> Result<(UdpSocket, SocketAddr), DiscoveryError> {
    let domain = if endpoint.address.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let socket = base_socket(domain)?;
    let target = match endpoint.address {
        IpAddr::V4(local) => {
            socket.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, DISCOVERY_PORT).into())?;
            socket.join_multicast_v4(&MULTICAST_V4, &local)?;
            socket.set_multicast_if_v4(&local)?;
            socket.set_multicast_loop_v4(false)?;
            socket.set_multicast_ttl_v4(1)?;
            SocketAddr::V4(SocketAddrV4::new(MULTICAST_V4, DISCOVERY_PORT))
        }
        IpAddr::V6(_) => {
            socket.set_only_v6(true)?;
            socket.bind(&SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, DISCOVERY_PORT, 0, 0).into())?;
            socket.join_multicast_v6(&MULTICAST_V6, endpoint.index)?;
            socket.set_multicast_if_v6(endpoint.index)?;
            socket.set_multicast_loop_v6(false)?;
            SocketAddr::V6(SocketAddrV6::new(
                MULTICAST_V6,
                DISCOVERY_PORT,
                0,
                endpoint.index,
            ))
        }
    };
    let standard: std::net::UdpSocket = socket.into();
    Ok((UdpSocket::from_std(standard)?, target))
}

fn unicast_socket(endpoint: &InterfaceEndpoint) -> Result<UdpSocket, DiscoveryError> {
    let domain = if endpoint.address.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let socket = base_socket(domain)?;
    socket.bind(&SocketAddr::new(endpoint.address, DISCOVERY_PORT).into())?;
    let standard: std::net::UdpSocket = socket.into();
    Ok(UdpSocket::from_std(standard)?)
}

fn unicast_probe_targets(
    endpoint: &InterfaceEndpoint,
    timestamp: u64,
    discovery_window: Duration,
) -> Vec<SocketAddr> {
    let IpNet::V4(network) = endpoint.network else {
        return Vec::new();
    };
    let prefix = network.prefix_len();
    let address_count = 1_u64 << (32 - prefix);
    let (first, host_count) = if prefix <= 30 {
        (u32::from(network.network()) + 1, address_count - 2)
    } else {
        (u32::from(network.network()), address_count)
    };
    let cycle = timestamp / discovery_window.as_secs().max(1);
    let start = cycle.saturating_mul(MAX_UNICAST_PROBES_PER_WINDOW) % host_count;
    let maximum = host_count.min(MAX_UNICAST_PROBES_PER_WINDOW);
    (0..maximum)
        .map(|offset| first + u32::try_from((start + offset) % host_count).unwrap_or(0))
        .map(Ipv4Addr::from)
        .filter(|address| IpAddr::V4(*address) != endpoint.address)
        .map(|address| SocketAddr::new(IpAddr::V4(address), DISCOVERY_PORT))
        .collect()
}

fn encode_beacon(
    kind: BeaconKind,
    listen_port: u16,
    timestamp: u64,
    key: &[u8; 32],
) -> Result<[u8; BEACON_LEN], DiscoveryError> {
    let mut beacon = [0_u8; BEACON_LEN];
    beacon[..4].copy_from_slice(BEACON_MAGIC);
    beacon[4] = BEACON_VERSION;
    beacon[5] = kind as u8;
    beacon[6..8].copy_from_slice(&listen_port.to_be_bytes());
    beacon[8..16].copy_from_slice(&timestamp.to_be_bytes());
    getrandom::fill(&mut beacon[16..16 + NONCE_LEN])?;
    let mut mac = <HmacSha256 as hmac::digest::KeyInit>::new_from_slice(key)
        .map_err(|_| DiscoveryError::InvalidDiscoveryKey)?;
    mac.update(&beacon[..BEACON_BODY_LEN]);
    beacon[BEACON_BODY_LEN..].copy_from_slice(&mac.finalize().into_bytes());
    Ok(beacon)
}

fn decode_beacon(
    beacon: &[u8],
    current_timestamp: u64,
    key: &[u8; 32],
) -> Result<Beacon, DiscoveryError> {
    if beacon.len() != BEACON_LEN || &beacon[..4] != BEACON_MAGIC || beacon[4] != BEACON_VERSION {
        return Err(DiscoveryError::InvalidBeacon);
    }
    let mut mac = <HmacSha256 as hmac::digest::KeyInit>::new_from_slice(key)
        .map_err(|_| DiscoveryError::InvalidDiscoveryKey)?;
    mac.update(&beacon[..BEACON_BODY_LEN]);
    mac.verify_slice(&beacon[BEACON_BODY_LEN..])
        .map_err(|_| DiscoveryError::InvalidBeacon)?;

    let kind = match beacon[5] {
        value if value == BeaconKind::Probe as u8 => BeaconKind::Probe,
        value if value == BeaconKind::Response as u8 => BeaconKind::Response,
        _ => return Err(DiscoveryError::InvalidBeacon),
    };
    let timestamp = u64::from_be_bytes(
        beacon[8..16]
            .try_into()
            .map_err(|_| DiscoveryError::InvalidBeacon)?,
    );
    if timestamp.abs_diff(current_timestamp) > MAX_CLOCK_SKEW.as_secs() {
        return Err(DiscoveryError::StaleBeacon);
    }
    Ok(Beacon {
        kind,
        port: u16::from_be_bytes(
            beacon[6..8]
                .try_into()
                .map_err(|_| DiscoveryError::InvalidBeacon)?,
        ),
    })
}

fn unix_time_seconds() -> Result<u64, DiscoveryError> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error("no peer interfaces are configured")]
    NoInterfacesConfigured,
    #[error("none of the configured peer interfaces are available: {0}")]
    InterfacesUnavailable(String),
    #[error("configured peer interfaces have no global addresses")]
    InterfacesHaveNoAddresses,
    #[error("interface inspection timed out")]
    InterfaceTimeout,
    #[error("interface inspection exited unsuccessfully ({0:?})")]
    InterfaceCommandFailed(Option<i32>),
    #[error("interface inspection output exceeded the safety limit")]
    InterfaceOutputTooLarge,
    #[error("interface inspection returned invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("interface inspection returned an invalid address: {0}")]
    InvalidAddress(String),
    #[error("interface inspection returned an invalid prefix length: {0}")]
    InvalidPrefix(u8),
    #[error("discovery beacon is invalid")]
    InvalidBeacon,
    #[error("discovery beacon is stale")]
    StaleBeacon,
    #[error("could not initialize the discovery authentication key")]
    InvalidDiscoveryKey,
    #[error("system clock is before the Unix epoch: {0}")]
    SystemTime(#[from] std::time::SystemTimeError),
    #[error("could not generate discovery nonce: {0}")]
    Random(#[from] getrandom::Error),
    #[error("discovery I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("discovery worker failed: {0}")]
    Task(#[from] tokio::task::JoinError),
    #[error("discovery returned {observed} peers, exceeding the {maximum}-peer safety limit")]
    TooManyPeers { observed: usize, maximum: usize },
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt};

    use tempfile::tempdir;

    use super::*;

    const INTERFACES: &[u8] = br#"[
      {"ifindex": 2, "ifname": "eth0", "addr_info": [
        {"family":"inet","local":"192.168.10.4","prefixlen":24,"scope":"global"},
        {"family":"inet6","local":"fe80::1","prefixlen":64,"scope":"link"}
      ]},
      {"ifindex": 8, "ifname": "wt0", "addr_info": [
        {"family":"inet","local":"100.91.0.2","prefixlen":16,"scope":"global"}
      ]}
    ]"#;

    #[test]
    fn selects_only_global_addresses_from_configured_interfaces() {
        let endpoints =
            parse_interface_endpoints(INTERFACES, &["wt0".to_owned()]).expect("valid interfaces");
        assert_eq!(endpoints.len(), 1);
        assert_eq!(endpoints[0].name, "wt0");
        assert_eq!(
            endpoints[0].address,
            "100.91.0.2".parse::<IpAddr>().expect("IP")
        );
        assert!(
            endpoints[0]
                .network
                .contains(&"100.91.126.8".parse::<IpAddr>().expect("IP"))
        );
    }

    #[test]
    fn unicast_probe_windows_are_bounded_and_advance() {
        let endpoint = InterfaceEndpoint {
            name: "wt0".to_owned(),
            index: 8,
            address: "100.91.0.2".parse().expect("local IP"),
            network: "100.91.0.0/16".parse().expect("network"),
        };
        let first = unicast_probe_targets(&endpoint, 1_700_000_000, Duration::from_secs(15));
        let second = unicast_probe_targets(&endpoint, 1_700_000_015, Duration::from_secs(15));
        assert!(
            u64::try_from(first.len()).expect("probe count fits u64")
                <= MAX_UNICAST_PROBES_PER_WINDOW
        );
        assert!(first.iter().all(|target| target.ip() != endpoint.address));
        assert_ne!(first.first(), second.first());
    }

    #[test]
    fn authenticated_beacon_round_trips() {
        let key = [7; 32];
        let beacon = encode_beacon(BeaconKind::Probe, 24_892, 1_700_000_000, &key).expect("beacon");
        assert_eq!(
            decode_beacon(&beacon, 1_700_000_001, &key).expect("valid beacon"),
            Beacon {
                kind: BeaconKind::Probe,
                port: 24_892,
            }
        );
    }

    #[test]
    fn beacon_rejects_wrong_key_and_stale_timestamp() {
        let beacon =
            encode_beacon(BeaconKind::Probe, 24_892, 1_700_000_000, &[7; 32]).expect("beacon");
        assert!(matches!(
            decode_beacon(&beacon, 1_700_000_000, &[8; 32]),
            Err(DiscoveryError::InvalidBeacon)
        ));
        assert!(matches!(
            decode_beacon(&beacon, 1_700_001_000, &[7; 32]),
            Err(DiscoveryError::StaleBeacon)
        ));
    }

    #[tokio::test]
    async fn command_output_is_bounded_and_parsed() {
        let directory = tempdir().expect("temporary directory");
        let command_path = directory.path().join("ip");
        fs::write(
            &command_path,
            format!(
                "#!/bin/sh\nprintf '%s' '{}'\n",
                String::from_utf8_lossy(INTERFACES)
            ),
        )
        .expect("write command");
        fs::set_permissions(&command_path, fs::Permissions::from_mode(0o700))
            .expect("command permissions");
        let discovery = InterfaceDiscovery::new(
            vec!["wt0".to_owned()],
            "host".to_owned(),
            24_892,
            Zeroizing::new([1; 32]),
            Duration::from_millis(1),
        )
        .with_ip_command(command_path);
        let endpoints = discovery.interface_endpoints().await.expect("endpoints");
        assert_eq!(endpoints.len(), 1);
        assert_eq!(endpoints[0].name, "wt0");
    }
}
