use std::{net::IpAddr, path::PathBuf, time::Duration};

use async_trait::async_trait;
use ipnet::IpNet;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{process::Command, time::timeout};

/// Hard cap on peer-derived dial tasks and retained discovery diagnostics.
pub const MAX_DISCOVERED_PEERS: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiscoverySnapshot {
    pub local_address: IpAddr,
    pub local_hostname: String,
    pub peers: Vec<DiscoveredPeer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiscoveredPeer {
    pub hostname: String,
    pub address: IpAddr,
    pub connected: bool,
}

#[async_trait]
pub trait PeerDiscovery: Send + Sync {
    async fn discover(&self) -> Result<DiscoverySnapshot, DiscoveryError>;
}

#[derive(Debug, Clone)]
pub struct NetbirdDiscovery {
    command: PathBuf,
    command_timeout: Duration,
}

impl NetbirdDiscovery {
    pub fn new(command: impl Into<PathBuf>) -> Self {
        Self {
            command: command.into(),
            command_timeout: Duration::from_secs(5),
        }
    }

    #[must_use]
    pub fn with_timeout(mut self, command_timeout: Duration) -> Self {
        self.command_timeout = command_timeout;
        self
    }

    /// Parses the stable subset of `netbird status --json` used for discovery.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed JSON or invalid local/peer addresses.
    pub fn parse(source: &[u8]) -> Result<DiscoverySnapshot, DiscoveryError> {
        let status: NetbirdStatus = serde_json::from_slice(source)?;
        if status.peers.details.len() > MAX_DISCOVERED_PEERS {
            return Err(DiscoveryError::TooManyPeers {
                observed: status.peers.details.len(),
                maximum: MAX_DISCOVERED_PEERS,
            });
        }
        let local_network: IpNet = status
            .netbird_ip
            .parse()
            .map_err(|_| DiscoveryError::InvalidAddress(status.netbird_ip.clone()))?;
        let mut peers = status
            .peers
            .details
            .into_iter()
            .map(|peer| {
                let address = peer
                    .netbird_ip
                    .parse()
                    .map_err(|_| DiscoveryError::InvalidAddress(peer.netbird_ip.clone()))?;
                Ok(DiscoveredPeer {
                    hostname: peer.fqdn,
                    address,
                    connected: peer.status.eq_ignore_ascii_case("connected"),
                })
            })
            .collect::<Result<Vec<_>, DiscoveryError>>()?;
        peers.sort_by(|left, right| {
            left.hostname
                .cmp(&right.hostname)
                .then_with(|| left.address.cmp(&right.address))
        });

        Ok(DiscoverySnapshot {
            local_address: local_network.addr(),
            local_hostname: status.fqdn,
            peers,
        })
    }
}

#[async_trait]
impl PeerDiscovery for NetbirdDiscovery {
    async fn discover(&self) -> Result<DiscoverySnapshot, DiscoveryError> {
        let mut command = Command::new(&self.command);
        command.args(["status", "--json"]).kill_on_drop(true);
        let output = timeout(self.command_timeout, command.output())
            .await
            .map_err(|_| DiscoveryError::Timeout)??;

        if !output.status.success() {
            return Err(DiscoveryError::CommandFailed(output.status.code()));
        }
        Self::parse(&output.stdout)
    }
}

#[derive(Debug, Deserialize)]
struct NetbirdStatus {
    #[serde(rename = "netbirdIp")]
    netbird_ip: String,
    fqdn: String,
    peers: NetbirdPeers,
}

#[derive(Debug, Deserialize)]
struct NetbirdPeers {
    #[serde(default)]
    details: Vec<NetbirdPeer>,
}

#[derive(Debug, Deserialize)]
struct NetbirdPeer {
    fqdn: String,
    #[serde(rename = "netbirdIp")]
    netbird_ip: String,
    status: String,
}

#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error("NetBird discovery command timed out")]
    Timeout,
    #[error("NetBird discovery command exited unsuccessfully ({0:?})")]
    CommandFailed(Option<i32>),
    #[error("could not run NetBird discovery: {0}")]
    Io(#[from] std::io::Error),
    #[error("could not decode NetBird status JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("NetBird returned an invalid address: {0}")]
    InvalidAddress(String),
    #[error("NetBird returned {observed} peers, exceeding the {maximum}-peer safety limit")]
    TooManyPeers { observed: usize, maximum: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &[u8] = br#"{
      "netbirdIp": "100.91.0.2/16",
      "fqdn": "vd.netbird.cloud",
      "peers": {
        "details": [
          {"fqdn":"kiwi.netbird.cloud","netbirdIp":"100.91.126.8","status":"Connected"},
          {"fqdn":"neo.netbird.cloud","netbirdIp":"100.91.0.3","status":"Connecting"}
        ]
      }
    }"#;

    #[test]
    fn parses_and_sorts_netbird_status() {
        let snapshot = NetbirdDiscovery::parse(FIXTURE).expect("valid fixture");

        assert_eq!(
            snapshot.local_address,
            "100.91.0.2".parse::<IpAddr>().unwrap()
        );
        assert_eq!(snapshot.local_hostname, "vd.netbird.cloud");
        assert_eq!(snapshot.peers.len(), 2);
        assert_eq!(snapshot.peers[0].hostname, "kiwi.netbird.cloud");
        assert!(snapshot.peers[0].connected);
        assert!(!snapshot.peers[1].connected);
    }

    #[test]
    fn rejects_invalid_peer_address() {
        let source = String::from_utf8_lossy(FIXTURE).replace("100.91.126.8", "not-an-ip");
        assert!(matches!(
            NetbirdDiscovery::parse(source.as_bytes()),
            Err(DiscoveryError::InvalidAddress(_))
        ));
    }

    #[test]
    fn rejects_peer_sets_that_would_create_unbounded_dial_tasks() {
        let peers = (0..=MAX_DISCOVERED_PEERS)
            .map(|index| {
                format!(
                    r#"{{"fqdn":"peer-{index}","netbirdIp":"100.64.0.1","status":"Connected"}}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let source = format!(
            r#"{{"netbirdIp":"100.91.0.2/16","fqdn":"vd","peers":{{"details":[{peers}]}}}}"#
        );
        assert!(matches!(
            NetbirdDiscovery::parse(source.as_bytes()),
            Err(DiscoveryError::TooManyPeers {
                observed,
                maximum: MAX_DISCOVERED_PEERS
            }) if observed == MAX_DISCOVERED_PEERS + 1
        ));
    }
}
