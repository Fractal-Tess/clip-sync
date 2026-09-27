//! Resolves where this host listens and which peers it dials.
//!
//! Peers are the fixed `peer_addresses` from configuration; nothing is
//! broadcast or probed. Listeners bind only to the global addresses of the
//! configured `peer_interfaces`, and a peer is dialled from the local address
//! whose network contains it. Every connection still authenticates with the
//! mesh secret, so an address only says where to try.

use std::{collections::BTreeSet, net::IpAddr, path::PathBuf, time::Duration};

use ipnet::IpNet;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{process::Command, time::timeout};

pub const MAX_DISCOVERED_PEERS: usize = 512;
const MAX_INTERFACE_OUTPUT_BYTES: usize = 1024 * 1024;

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
    peer_addresses: Vec<IpAddr>,
    local_hostname: String,
    listen_port: u16,
    command_timeout: Duration,
}

impl InterfaceDiscovery {
    #[must_use]
    pub fn new(
        peer_interfaces: Vec<String>,
        peer_addresses: Vec<IpAddr>,
        local_hostname: String,
        listen_port: u16,
    ) -> Self {
        Self {
            ip_command: PathBuf::from("ip"),
            peer_interfaces,
            peer_addresses,
            local_hostname,
            listen_port,
            command_timeout: Duration::from_secs(5),
        }
    }

    #[cfg(test)]
    fn with_ip_command(mut self, ip_command: impl Into<PathBuf>) -> Self {
        self.ip_command = ip_command.into();
        self
    }

    /// Reads the configured interfaces' current addresses and pairs each
    /// configured peer with the local address that can reach it.
    ///
    /// # Errors
    ///
    /// Returns an error if no configured interface is up with a global
    /// address, or if interface inspection fails.
    pub async fn discover(&self) -> Result<DiscoverySnapshot, DiscoveryError> {
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
        let peers = configured_peers(&endpoints, &self.peer_addresses, self.listen_port);
        Ok(DiscoverySnapshot {
            local_addresses,
            local_hostname: self.local_hostname.clone(),
            peers: peers.into_iter().take(MAX_DISCOVERED_PEERS).collect(),
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
    address: IpAddr,
    network: IpNet,
}

#[derive(Debug, Deserialize)]
struct InterfaceStatus {
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

fn configured_peers(
    endpoints: &[InterfaceEndpoint],
    peer_addresses: &[IpAddr],
    listen_port: u16,
) -> BTreeSet<DiscoveredPeer> {
    peer_addresses
        .iter()
        .filter_map(|address| {
            endpoints
                .iter()
                .find(|endpoint| endpoint.address != *address && endpoint.network.contains(address))
                .map(|endpoint| DiscoveredPeer {
                    hostname: address.to_string(),
                    address: *address,
                    port: listen_port,
                    local_address: endpoint.address,
                    connected: true,
                })
        })
        .collect()
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
    #[error("interface inspection I/O failed: {0}")]
    Io(#[from] std::io::Error),
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
            Vec::new(),
            "host".to_owned(),
            24_892,
        )
        .with_ip_command(command_path);
        let endpoints = discovery.interface_endpoints().await.expect("endpoints");
        assert_eq!(endpoints.len(), 1);
        assert_eq!(endpoints[0].name, "wt0");
    }

    #[test]
    fn configured_peers_remain_in_every_snapshot() {
        let endpoints =
            parse_interface_endpoints(INTERFACES, &["wt0".to_owned()]).expect("valid interfaces");
        let peers = configured_peers(
            &endpoints,
            &[
                "100.91.0.2".parse().expect("local IP"),
                "100.91.126.8".parse().expect("peer IP"),
                "192.168.10.8".parse().expect("other network"),
            ],
            24_892,
        );
        assert_eq!(
            peers,
            BTreeSet::from([DiscoveredPeer {
                hostname: "100.91.126.8".to_owned(),
                address: "100.91.126.8".parse().expect("peer IP"),
                port: 24_892,
                local_address: "100.91.0.2".parse().expect("local IP"),
                connected: true,
            }])
        );
    }
}
