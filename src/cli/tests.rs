use clap::Parser;

use super::{
    Cli,
    views::{StatusOutput, error_json, history_item_json, peer_json, share_json, transfer_json},
};

#[cfg(feature = "ui")]
use super::commands::{Command, UiCommand};

#[test]
fn cli_exposes_history_search_and_mutations() {
    for arguments in [
        vec!["clip-sync", "history", "search", "needle", "--json"],
        vec!["clip-sync", "history", "pin", "content", "--json"],
        vec!["clip-sync", "history", "unpin", "content"],
        vec!["clip-sync", "history", "delete", "content"],
        vec!["clip-sync", "transfer", "cancel", "transfer-id", "--json"],
        vec!["clip-sync", "device", "forget", "device-id", "--json"],
        vec![
            "clip-sync",
            "config",
            "set",
            "mesh-quota",
            "1048576",
            "--json",
        ],
        vec!["clip-sync", "share-clipboard", "--confirm", "--json"],
        vec![
            "clip-sync",
            "rekey",
            "--old-key-file",
            "old.key",
            "--new-key-file",
            "new.key",
        ],
    ] {
        Cli::try_parse_from(arguments).expect("command should parse");
    }
}

#[cfg(feature = "ui")]
#[test]
fn cli_preserves_unified_ui_tray_and_global_close_routes() {
    for (route, expected) in [
        ("switcher", UiCommand::Switcher),
        ("control", UiCommand::Control),
        ("close-quick", UiCommand::CloseQuick),
        ("tray", UiCommand::Tray),
    ] {
        let cli = Cli::try_parse_from(["clip-sync", "ui", route]).expect("UI command should parse");
        let Command::Ui { command } = cli.command else {
            panic!("expected UI command");
        };
        assert_eq!(
            std::mem::discriminant(&command),
            std::mem::discriminant(&expected)
        );
    }
}

#[test]
fn json_error_shape_is_stable() {
    assert_eq!(
        error_json("daemon_unavailable", "not running"),
        serde_json::json!({
            "ok": false,
            "error": {
                "code": "daemon_unavailable",
                "message": "not running",
            }
        })
    );
}

#[test]
fn history_json_fields_are_stable() {
    assert_eq!(
        history_item_json(&crate::ipc::protocol::HistoryItem {
            content_id: "content".to_owned(),
            preview: "preview".to_owned(),
            mime_types: vec!["text/plain".to_owned()],
            logical_size: 42,
            source_node: "device-id".to_owned(),
            source_device: "device".to_owned(),
            pinned: true,
            physical_millis: 1_704_067_200_000,
            origin_millis: Some(1_704_067_200_000),
        }),
        serde_json::json!({
            "content_id": "content",
            "preview": "preview",
            "mime_types": ["text/plain"],
            "logical_size": 42,
            "source_node": "device-id",
            "source_device": "device",
            "pinned": true,
            "physical_millis": 1_704_067_200_000_u64,
            "origin_millis": 1_704_067_200_000_u64,
        })
    );
}

#[test]
fn status_json_fields_are_stable() {
    let address = Some("100.64.0.1".to_owned());
    let output = StatusOutput {
        version: "0.1.0",
        hostname: "kiwi",
        uptime_seconds: 42,
        config_path: "/config.toml",
        netbird_address: &address,
        discovered_peers: 2,
    };

    assert_eq!(
        serde_json::to_value(output).expect("serialize status"),
        serde_json::json!({
            "version": "0.1.0",
            "hostname": "kiwi",
            "uptime_seconds": 42,
            "config_path": "/config.toml",
            "netbird_address": "100.64.0.1",
            "discovered_peers": 2,
        })
    );
}

#[test]
fn peer_json_stats_are_additive_and_explicitly_unavailable() {
    use crate::ipc::protocol::{PeerItem, PeerStats};

    let mut peer = PeerItem {
        hostname: "kiwi.netbird.cloud".to_owned(),
        address: "100.64.0.2".to_owned(),
        connected: true,
        stats: None,
    };
    assert_eq!(
        peer_json(&peer),
        serde_json::json!({
            "hostname": "kiwi.netbird.cloud",
            "address": "100.64.0.2",
            "connected": true,
            "stats": null,
        })
    );
    peer.stats = Some(PeerStats {
        shared_items: 12,
        shared_bytes: 4_096,
        pinned_items: 2,
        last_shared_millis: Some(99),
    });
    assert_eq!(peer_json(&peer)["stats"]["shared_items"], 12);
    assert_eq!(peer_json(&peer)["stats"]["last_shared_millis"], 99);
}

#[test]
fn share_json_fields_and_resource_ids_are_stable() {
    let value = share_json(&crate::ipc::protocol::ShareClipboardResponse {
        shared: true,
        confirmation_required: true,
        logical_size: 42,
        mime_types: vec!["text/plain".to_owned()],
        quota_exempt: false,
        transfer_id: Some("transfer-id".to_owned()),
        content_id: Some("content-id".to_owned()),
        message: "clipboard shared".to_owned(),
    });
    assert_eq!(
        value,
        serde_json::json!({
            "ok": true,
            "shared": true,
            "confirmation_required": true,
            "logical_size": 42,
            "mime_types": ["text/plain"],
            "quota_exempt": false,
            "transfer_id": "transfer-id",
            "content_id": "content-id",
            "message": "clipboard shared",
        })
    );
}

#[test]
fn transfer_progress_json_fields_are_stable() {
    assert_eq!(
        transfer_json(&crate::ipc::protocol::TransferItem {
            transfer_id: "transfer-id".to_owned(),
            content_id: "content-id".to_owned(),
            peer: "peer-id".to_owned(),
            state: "replicating".to_owned(),
            completed_bytes: 10,
            total_bytes: 20,
        }),
        serde_json::json!({
            "transfer_id": "transfer-id",
            "content_id": "content-id",
            "peer": "peer-id",
            "state": "replicating",
            "completed_bytes": 10,
            "total_bytes": 20,
        })
    );
}
