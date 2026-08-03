use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use prost::Message;
use tokio::sync::mpsc;

use clip_sync_core::config::Config;
use clip_sync_ipc::protocol::{
    self, HistoryItem, HistoryRequest, IPC_PROTOCOL_VERSION, PeersRequest, Request, StatusRequest,
    request, response,
};

use crate::{
    discovery::{DiscoveredPeer, DiscoverySnapshot},
    ipc::DaemonState,
};

#[test]
fn legacy_v5_peer_items_decode_with_unavailable_stats() {
    let mut legacy = Vec::new();
    legacy.extend([0x0a, 0x04]);
    legacy.extend(b"kiwi");
    legacy.extend([0x12, 0x0a]);
    legacy.extend(b"100.64.0.2");
    legacy.extend([0x18, 0x01]);
    let peer = protocol::PeerItem::decode(legacy.as_slice()).expect("legacy peer item");
    assert!(peer.connected);
    assert!(peer.stats.is_none());
}

#[tokio::test]
async fn history_search_is_bounded_and_case_insensitive() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let (commands, _command_rx) = mpsc::unbounded_channel();
    let state = DaemonState::new(
        "test-node".to_owned(),
        temporary.path().join("config.toml"),
        Config::default(),
        commands,
    );
    state
        .set_history(vec![
            HistoryItem {
                content_id: "alpha".to_owned(),
                preview: "Build Finished".to_owned(),
                mime_types: vec!["text/plain".to_owned()],
                logical_size: 14,
                source_node: "kiwi".to_owned(),
                source_device: "kiwi".to_owned(),
                pinned: false,
                physical_millis: 2,
                origin_millis: Some(2),
            },
            HistoryItem {
                content_id: "beta".to_owned(),
                preview: "unrelated".to_owned(),
                mime_types: vec!["image/png".to_owned()],
                logical_size: 20,
                source_node: "vd".to_owned(),
                source_device: "vd".to_owned(),
                pinned: false,
                physical_millis: 1,
                origin_millis: Some(1),
            },
        ])
        .await;

    let response = state
        .handle(Request {
            protocol_version: IPC_PROTOCOL_VERSION,
            request_id: 8,
            body: Some(request::Body::History(HistoryRequest {
                query: "FINISHED".to_owned(),
                limit: 1,
                offset: 0,
            })),
        })
        .await;
    let Some(response::Body::History(history)) = response.body else {
        panic!("expected history response");
    };
    assert_eq!(history.total, 1);
    assert_eq!(history.items.len(), 1);
    assert_eq!(history.items[0].content_id, "alpha");
}

#[tokio::test]
async fn history_response_honors_offset_and_reports_total() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let (commands, _command_rx) = mpsc::unbounded_channel();
    let state = DaemonState::new(
        "test-node".to_owned(),
        temporary.path().join("config.toml"),
        Config::default(),
        commands,
    );
    let items = (0_u64..5)
        .map(|index| HistoryItem {
            content_id: format!("content-{index}"),
            preview: "matching preview".to_owned(),
            mime_types: vec!["text/plain".to_owned()],
            logical_size: index,
            source_node: "node".to_owned(),
            source_device: "device".to_owned(),
            pinned: false,
            physical_millis: index,
            origin_millis: Some(index),
        })
        .collect();
    state.set_history(items).await;

    let response = state
        .handle(Request {
            protocol_version: IPC_PROTOCOL_VERSION,
            request_id: 9,
            body: Some(request::Body::History(HistoryRequest {
                query: "matching".to_owned(),
                limit: 2,
                offset: 2,
            })),
        })
        .await;
    let Some(response::Body::History(history)) = response.body else {
        panic!("expected history response");
    };
    assert_eq!(history.total, 5);
    assert_eq!(
        history
            .items
            .iter()
            .map(|item| item.content_id.as_str())
            .collect::<Vec<_>>(),
        ["content-2", "content-1"]
    );
}

#[tokio::test]
async fn status_reports_interface_addresses_and_authenticated_connections() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let (commands, _command_rx) = mpsc::unbounded_channel();
    let state = DaemonState::new(
        "test-node".to_owned(),
        temporary.path().join("config.toml"),
        Config::default(),
        commands,
    );
    state
        .set_discovery(DiscoverySnapshot {
            local_addresses: vec!["100.64.0.1".parse().expect("local IP")],
            local_hostname: "vd.mesh.local".to_owned(),
            peers: vec![
                DiscoveredPeer {
                    hostname: "online.mesh.local".to_owned(),
                    address: "100.64.0.2".parse().expect("peer IP"),
                    port: 24_892,
                    local_address: "100.64.0.1".parse().expect("local IP"),
                    connected: true,
                },
                DiscoveredPeer {
                    hostname: "offline.mesh.local".to_owned(),
                    address: "100.64.0.3".parse().expect("peer IP"),
                    port: 24_892,
                    local_address: "100.64.0.1".parse().expect("local IP"),
                    connected: false,
                },
            ],
        })
        .await;

    let response = state
        .handle(Request {
            protocol_version: IPC_PROTOCOL_VERSION,
            request_id: 80,
            body: Some(request::Body::Status(StatusRequest {})),
        })
        .await;
    let Some(response::Body::Status(status)) = response.body else {
        panic!("expected status response");
    };
    assert_eq!(status.local_addresses, ["100.64.0.1"]);
    assert_eq!(status.discovered_peers, 2);
    assert_eq!(status.connected_peers, 0);
}

#[tokio::test]
async fn peer_response_requires_a_live_authenticated_connection() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let (commands, _command_rx) = mpsc::unbounded_channel();
    let state = DaemonState::new(
        "test-node".to_owned(),
        temporary.path().join("config.toml"),
        Config::default(),
        commands,
    );
    state
        .set_discovery(DiscoverySnapshot {
            local_addresses: vec!["192.168.10.4".parse().expect("local IP")],
            local_hostname: "local-host".to_owned(),
            peers: vec![DiscoveredPeer {
                hostname: "192.168.10.9".to_owned(),
                address: "192.168.10.9".parse().expect("peer IP"),
                port: 24_892,
                local_address: "192.168.10.4".parse().expect("local IP"),
                connected: true,
            }],
        })
        .await;

    let response = state
        .handle(Request {
            protocol_version: IPC_PROTOCOL_VERSION,
            request_id: 81,
            body: Some(request::Body::Peers(PeersRequest {})),
        })
        .await;
    let Some(response::Body::Peers(peers)) = response.body else {
        panic!("expected peers response");
    };
    assert!(
        peers.peers.is_empty(),
        "a discovery beacon alone must not be presented as an authenticated peer"
    );
}

#[tokio::test]
async fn history_search_uses_authenticated_device_name_aliases() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let (commands, _command_rx) = mpsc::unbounded_channel();
    let state = DaemonState::new(
        "test-node".to_owned(),
        temporary.path().join("config.toml"),
        Config::default(),
        commands,
    );
    state
        .set_device_names(BTreeMap::from([("node-id".to_owned(), "vd".to_owned())]))
        .await;
    state
        .set_history(vec![HistoryItem {
            content_id: "content".to_owned(),
            preview: "Screenshot".to_owned(),
            mime_types: vec!["image/png".to_owned()],
            logical_size: 10,
            source_node: "node-id".to_owned(),
            source_device: String::new(),
            pinned: true,
            physical_millis: 1,
            origin_millis: Some(1),
        }])
        .await;

    let response = state
        .handle(Request {
            protocol_version: IPC_PROTOCOL_VERSION,
            request_id: 80,
            body: Some(request::Body::History(HistoryRequest {
                query: "D:vd,T:image,P:true".to_owned(),
                limit: 100,
                offset: 0,
            })),
        })
        .await;
    let Some(response::Body::History(history)) = response.body else {
        panic!("expected history response");
    };
    assert_eq!(history.items.len(), 1);
    assert_eq!(history.items[0].source_device, "vd");
}

#[tokio::test]
async fn history_search_applies_typed_filters_in_newest_first_order() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let (commands, _command_rx) = mpsc::unbounded_channel();
    let state = DaemonState::new(
        "test-node".to_owned(),
        temporary.path().join("config.toml"),
        Config::default(),
        commands,
    );
    state
        .set_history(vec![
            HistoryItem {
                content_id: "old".to_owned(),
                preview: "Release Notes".to_owned(),
                mime_types: vec!["text/markdown".to_owned()],
                logical_size: 4_096,
                source_node: "office-node".to_owned(),
                source_device: "Office Laptop".to_owned(),
                pinned: true,
                physical_millis: 1_704_067_199_000,
                origin_millis: Some(1_704_067_199_000),
            },
            HistoryItem {
                content_id: "new".to_owned(),
                preview: "Release Notes".to_owned(),
                mime_types: vec!["text/markdown".to_owned()],
                logical_size: 4_500,
                source_node: "office-node".to_owned(),
                source_device: "Office Laptop".to_owned(),
                pinned: true,
                physical_millis: 1_704_067_199_500,
                origin_millis: Some(1_704_067_199_500),
            },
            HistoryItem {
                content_id: "wrong-device".to_owned(),
                preview: "Release Notes".to_owned(),
                mime_types: vec!["text/markdown".to_owned()],
                logical_size: 4_500,
                source_node: "phone-node".to_owned(),
                source_device: "Phone".to_owned(),
                pinned: true,
                physical_millis: 1_704_067_199_900,
                origin_millis: Some(1_704_067_199_900),
            },
        ])
        .await;

    let response = state
        .handle(Request {
            protocol_version: IPC_PROTOCOL_VERSION,
            request_id: 81,
            body: Some(request::Body::History(HistoryRequest {
                query: concat!(
                    r#""release notes" device:"office laptop" type:markdown "#,
                    "pinned:true min-size:4KiB max-size:5KB ",
                    "before:2024-01-01T00:00:00Z"
                )
                .to_owned(),
                limit: 500,
                offset: 0,
            })),
        })
        .await;
    let Some(response::Body::History(history)) = response.body else {
        panic!("expected history response");
    };
    assert_eq!(
        history
            .items
            .iter()
            .map(|item| item.content_id.as_str())
            .collect::<Vec<_>>(),
        ["new", "old"]
    );
}

#[tokio::test]
async fn invalid_history_query_error_is_stable_and_does_not_echo_value() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let (commands, _command_rx) = mpsc::unbounded_channel();
    let state = DaemonState::new(
        "test-node".to_owned(),
        temporary.path().join("config.toml"),
        Config::default(),
        commands,
    );
    let response = state
        .handle(Request {
            protocol_version: IPC_PROTOCOL_VERSION,
            request_id: 82,
            body: Some(request::Body::History(HistoryRequest {
                query: "pinned:private-value".to_owned(),
                limit: 100,
                offset: 0,
            })),
        })
        .await;
    let Some(response::Body::Error(error)) = response.body else {
        panic!("expected error response");
    };
    assert_eq!(error.code, "invalid_history_query");
    assert_eq!(
        error.message,
        "invalid query at byte 0: pinned expects true or false"
    );
    assert!(!error.message.contains("private-value"));
}

#[tokio::test]
async fn large_history_search_stays_responsive_and_bounded() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let (commands, _command_rx) = mpsc::unbounded_channel();
    let state = DaemonState::new(
        "test-node".to_owned(),
        temporary.path().join("config.toml"),
        Config::default(),
        commands,
    );
    let items = (0_u64..50_000)
        .map(|index| HistoryItem {
            content_id: format!("content-{index:05}"),
            preview: format!("ordinary clipboard preview {index}"),
            mime_types: vec!["text/plain".to_owned()],
            logical_size: index,
            source_node: format!("device-{}", index % 8),
            source_device: format!("host-{}", index % 8),
            pinned: index % 10 == 0,
            physical_millis: index,
            origin_millis: Some(index),
        })
        .collect();
    state.set_history(items).await;

    let started = Instant::now();
    let response = state
        .handle(Request {
            protocol_version: IPC_PROTOCOL_VERSION,
            request_id: 83,
            body: Some(request::Body::History(HistoryRequest {
                query: "not-present pinned:false type:text".to_owned(),
                limit: u32::MAX,
                offset: 0,
            })),
        })
        .await;
    let elapsed = started.elapsed();
    let Some(response::Body::History(history)) = response.body else {
        panic!("expected history response");
    };
    assert!(history.items.is_empty());
    assert!(
        elapsed < Duration::from_secs(1),
        "50k-entry metadata search took {elapsed:?}"
    );
}

#[tokio::test]
async fn config_response_is_redacted_and_complete_for_local_ui_fields() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let (commands, _command_rx) = mpsc::unbounded_channel();
    let state = DaemonState::new(
        "test-node".to_owned(),
        temporary.path().join("config.toml"),
        Config::default(),
        commands,
    );

    let response = state
        .handle(Request {
            protocol_version: IPC_PROTOCOL_VERSION,
            request_id: 11,
            body: Some(request::Body::Config(protocol::ConfigRequest {})),
        })
        .await;
    let Some(response::Body::Config(config)) = response.body else {
        panic!("expected config response");
    };
    let value: serde_json::Value =
        serde_json::from_slice(&config.redacted_json).expect("valid config JSON");
    let local = value
        .get("local")
        .and_then(serde_json::Value::as_object)
        .expect("local config object");

    for field in [
        "listen_port",
        "discovery_interval_seconds",
        "reconcile_interval_seconds",
        "reconnect_min_seconds",
        "reconnect_max_seconds",
        "peer_interfaces",
        "mesh_key_file_configured",
        "config_path",
    ] {
        assert!(local.contains_key(field), "missing {field}");
    }
    assert!(!local.contains_key("mesh_key_file"));
    assert!(!String::from_utf8_lossy(&config.redacted_json).contains("/run/secrets"));
}
