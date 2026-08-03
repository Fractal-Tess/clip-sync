use std::collections::BTreeMap;

use serde::Serialize;

use clip_sync_core::{config::Config, transfer::TRANSFER_PROTOCOL_VERSION};

use crate::{
    history_search::HistoryQuery,
    mesh::{MESH_PROTOCOL_VERSION, MeshHandle},
};

use super::{
    protocol::{
        ConfigResponse, DiagnosticCheck, DiagnosticsResponse, ErrorResponse, HistoryRequest,
        HistoryResponse, IPC_PROTOCOL_VERSION, PeerItem, PeerStats, PeersResponse, Response,
        StatusResponse, response,
    },
    state::{DaemonState, PeerHistoryStats},
};

fn authenticated_peer_stats(
    discovered_hostname: &str,
    allow_short_match: bool,
    device_names: &BTreeMap<String, String>,
    history_stats: &BTreeMap<String, PeerHistoryStats>,
) -> Option<PeerStats> {
    let discovered = discovered_hostname
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let discovered_short = discovered.split('.').next().unwrap_or(&discovered);
    let mut matches = device_names.iter().filter(|(_, authenticated_hostname)| {
        let authenticated = authenticated_hostname
            .trim_end_matches('.')
            .to_ascii_lowercase();
        authenticated == discovered
            || (allow_short_match
                && !authenticated.contains('.')
                && discovered.contains('.')
                && authenticated == discovered_short)
    });
    let (node_id, _) = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    let stats = history_stats.get(node_id).copied().unwrap_or_default();
    Some(PeerStats {
        shared_items: stats.shared_items,
        shared_bytes: stats.shared_bytes,
        pinned_items: stats.pinned_items,
        last_shared_millis: stats.last_shared_millis,
    })
}

impl DaemonState {
    pub(super) async fn status_response(&self) -> response::Body {
        let (local_addresses, discovered_peers) = {
            let discovery = self.inner.discovery.read().await;
            discovery.as_ref().map_or_else(
                || (Vec::new(), 0),
                |snapshot| {
                    (
                        snapshot
                            .local_addresses
                            .iter()
                            .map(ToString::to_string)
                            .collect(),
                        u32::try_from(snapshot.peers.len()).unwrap_or(u32::MAX),
                    )
                },
            )
        };
        let connected_peers = self.inner.mesh.read().await.as_ref().map_or(0, |mesh| {
            u32::try_from(mesh.status().active_connections).unwrap_or(u32::MAX)
        });
        response::Body::Status(StatusResponse {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            hostname: self.inner.hostname.clone(),
            uptime_seconds: self.inner.started.elapsed().as_secs(),
            config_path: self.inner.config_path.display().to_string(),
            local_addresses,
            discovered_peers,
            connected_peers,
        })
    }

    pub(super) async fn config_response(
        &self,
        request_id: u64,
    ) -> Result<response::Body, Response> {
        #[derive(Serialize)]
        struct RedactedLocal<'a> {
            listen_port: u16,
            discovery_interval_seconds: u64,
            reconcile_interval_seconds: u64,
            reconnect_min_seconds: u64,
            reconnect_max_seconds: u64,
            peer_interfaces: &'a [String],
            mesh_key_file_configured: bool,
            config_path: &'a str,
        }

        #[derive(Serialize)]
        struct RedactedConfig<'a> {
            shared: &'a clip_sync_core::config::SharedConfig,
            local: RedactedLocal<'a>,
        }

        let config = self.inner.config.read().await;
        let config_path = self.inner.config_path.to_string_lossy();
        let redacted = RedactedConfig {
            shared: &config.shared,
            local: RedactedLocal {
                listen_port: config.local.listen_port,
                discovery_interval_seconds: config.local.discovery_interval_seconds,
                reconcile_interval_seconds: config.local.reconcile_interval_seconds,
                reconnect_min_seconds: config.local.reconnect_min_seconds,
                reconnect_max_seconds: config.local.reconnect_max_seconds,
                peer_interfaces: &config.local.peer_interfaces,
                mesh_key_file_configured: !config.local.mesh_key_file.as_os_str().is_empty(),
                config_path: &config_path,
            },
        };
        match serde_json::to_vec(&redacted) {
            Ok(redacted_json) => Ok(response::Body::Config(ConfigResponse { redacted_json })),
            Err(error) => Err(error_response(
                request_id,
                "serialization_failed",
                error.to_string(),
            )),
        }
    }

    pub(super) async fn history_response(
        &self,
        request_id: u64,
        history_request: HistoryRequest,
    ) -> Result<response::Body, Response> {
        let query = match HistoryQuery::parse(&history_request.query) {
            Ok(query) => query,
            Err(error) => {
                return Err(error_response(
                    request_id,
                    "invalid_history_query",
                    error.to_string(),
                ));
            }
        };
        let page = self.inner.history.read().await.page(
            &query,
            history_request.offset,
            history_request.limit,
        );
        Ok(response::Body::History(HistoryResponse {
            items: page.items,
            total: page.total,
        }))
    }

    pub(super) async fn peers_response(&self) -> response::Body {
        let discovery = self.inner.discovery.read().await;
        let discovery_error = self.inner.discovery_error.read().await.clone();
        let history_stats = self.inner.peer_history_stats.read().await;
        let device_names = self.inner.device_names.read().await;
        let mesh = self.inner.mesh.read().await.clone();
        let connected_peers = if let Some(mesh) = mesh {
            mesh.connected_peers().await
        } else {
            BTreeMap::new()
        };
        let mut short_name_counts = BTreeMap::<String, usize>::new();
        for hostname in connected_peers.values() {
            let normalized = hostname.trim_end_matches('.').to_ascii_lowercase();
            let short = normalized
                .split('.')
                .next()
                .unwrap_or(&normalized)
                .to_owned();
            *short_name_counts.entry(short).or_default() += 1;
        }
        let peers = connected_peers
            .into_iter()
            .map(|(address, hostname)| {
                let normalized = hostname.trim_end_matches('.').to_ascii_lowercase();
                let short = normalized.split('.').next().unwrap_or(&normalized);
                PeerItem {
                    address: address.to_string(),
                    connected: true,
                    stats: authenticated_peer_stats(
                        &hostname,
                        short_name_counts.get(short) == Some(&1),
                        &device_names,
                        &history_stats,
                    ),
                    hostname,
                }
            })
            .collect();
        response::Body::Peers(PeersResponse {
            local_hostname: discovery.as_ref().map_or_else(
                || self.inner.hostname.clone(),
                |snapshot| snapshot.local_hostname.clone(),
            ),
            local_addresses: discovery.as_ref().map_or_else(Vec::new, |snapshot| {
                snapshot
                    .local_addresses
                    .iter()
                    .map(ToString::to_string)
                    .collect()
            }),
            peers,
            discovery_error,
            devices: self.inner.devices.read().await.clone(),
        })
    }

    #[allow(clippy::too_many_lines)]
    pub(super) async fn diagnostics_response(&self) -> response::Body {
        let discovery = self.inner.discovery.read().await;
        let discovery_error = self.inner.discovery_error.read().await;
        let clipboard = self.inner.clipboard_status.read().await.clone();
        let active_config = self.inner.config.read().await.clone();
        let config_check = Config::load(&self.inner.config_path);
        let (config_ok, config_detail) = match config_check {
            Ok(on_disk) if on_disk == active_config => (
                true,
                format!("{} (loaded and current)", self.inner.config_path.display()),
            ),
            Ok(_) => (
                false,
                format!(
                    "{} differs from the daemon's active configuration",
                    self.inner.config_path.display()
                ),
            ),
            Err(error) => (false, error.to_string()),
        };
        let mesh = self
            .inner
            .mesh
            .read()
            .await
            .as_ref()
            .map(MeshHandle::status);
        let (listener_ok, listener_detail, connections_ok, connection_detail) =
            if let Some(mesh) = mesh {
                let listener_detail = if let Some(address) = mesh.listener_address {
                    format!("listening on {address}")
                } else if let Some(error) = mesh.last_listener_error {
                    format!("listener unavailable: {error}")
                } else {
                    "listener inactive while interface discovery is unavailable".to_owned()
                };
                (
                    mesh.listener_address.is_some(),
                    listener_detail,
                    true,
                    format!(
                        "{} active of {}/{} discovered addresses",
                        mesh.active_connections,
                        mesh.discovered_addresses,
                        crate::discovery::MAX_DISCOVERED_PEERS
                    ),
                )
            } else {
                (
                    false,
                    "mesh supervisor is not attached".to_owned(),
                    false,
                    "mesh supervisor is not attached".to_owned(),
                )
            };
        let discovery_ok = discovery.is_some() && discovery_error.is_none();
        let discovery_detail = if let Some(error) = discovery_error.as_deref() {
            error.to_owned()
        } else if let Some(snapshot) = discovery.as_ref() {
            format!(
                "{} peers visible on {} selected addresses",
                snapshot.peers.len(),
                snapshot.local_addresses.len()
            )
        } else {
            "waiting for the first interface discovery result".to_owned()
        };
        response::Body::Diagnostics(DiagnosticsResponse {
            checks: vec![
                DiagnosticCheck {
                    name: "daemon".to_owned(),
                    ok: true,
                    detail: format!(
                        "running for {} seconds",
                        self.inner.started.elapsed().as_secs()
                    ),
                },
                DiagnosticCheck {
                    name: "config".to_owned(),
                    ok: config_ok,
                    detail: config_detail,
                },
                DiagnosticCheck {
                    name: "encrypted_storage".to_owned(),
                    ok: true,
                    detail: "open and ready".to_owned(),
                },
                DiagnosticCheck {
                    name: "mesh_secret".to_owned(),
                    ok: true,
                    detail: "owner-only key file and encrypted keyslot authenticated".to_owned(),
                },
                DiagnosticCheck {
                    name: "clipboard".to_owned(),
                    ok: clipboard.ok,
                    detail: clipboard.detail,
                },
                DiagnosticCheck {
                    name: "interface_discovery".to_owned(),
                    ok: discovery_ok,
                    detail: discovery_detail,
                },
                DiagnosticCheck {
                    name: "mesh_listener".to_owned(),
                    ok: listener_ok,
                    detail: listener_detail,
                },
                DiagnosticCheck {
                    name: "mesh_connections".to_owned(),
                    ok: connections_ok,
                    detail: connection_detail,
                },
                DiagnosticCheck {
                    name: "protocol_versions".to_owned(),
                    ok: true,
                    detail: format!(
                        "ipc={IPC_PROTOCOL_VERSION}, mesh={MESH_PROTOCOL_VERSION}, transfer={TRANSFER_PROTOCOL_VERSION}"
                    ),
                },
            ],
        })
    }
}

pub(super) fn error_response(
    request_id: u64,
    code: impl Into<String>,
    message: impl Into<String>,
) -> Response {
    Response {
        protocol_version: IPC_PROTOCOL_VERSION,
        request_id,
        body: Some(response::Body::Error(ErrorResponse {
            code: code.into(),
            message: message.into(),
        })),
    }
}

pub(super) fn command_processor_unavailable(request_id: u64) -> Response {
    error_response(
        request_id,
        "daemon_unavailable",
        "daemon command processor is unavailable",
    )
}

pub(super) fn command_processor_stopped(request_id: u64) -> Response {
    error_response(
        request_id,
        "daemon_unavailable",
        "daemon command processor stopped",
    )
}
