use std::path::Path;

use clap::{Parser, error::ErrorKind};

use super::{
    Cli, LaunchKind, ParsedInvocation,
    views::{StatusOutput, error_json, history_item_json, peer_json},
};

fn invocation(arguments: &[&str]) -> ParsedInvocation {
    ParsedInvocation {
        cli: Cli::try_parse_from(arguments).expect("invocation should parse"),
    }
}

#[test]
fn desktop_is_the_default_and_explicit_desktop_mode() {
    let default = invocation(&["clip-sync"]);
    assert_eq!(default.kind(), LaunchKind::Desktop);
    assert_eq!(default.config_override(), None);
    assert!(!default.desktop_control());

    let explicit = invocation(&["clip-sync", "desktop"]);
    assert_eq!(explicit.kind(), LaunchKind::Desktop);
    assert!(!explicit.desktop_control());

    let control = invocation(&["clip-sync", "desktop", "--control"]);
    assert_eq!(control.kind(), LaunchKind::Desktop);
    assert!(control.desktop_control());

    let configured = invocation(&["clip-sync", "--config", "/tmp/config.toml", "desktop"]);
    assert_eq!(configured.kind(), LaunchKind::Desktop);
    assert_eq!(
        configured.config_override(),
        Some(Path::new("/tmp/config.toml"))
    );
}

#[test]
fn daemon_is_the_only_daemon_launch_mode() {
    let daemon = invocation(&["clip-sync", "--config", "/tmp/config.toml", "daemon"]);
    assert_eq!(daemon.kind(), LaunchKind::Daemon);
    assert_eq!(
        daemon.config_override(),
        Some(Path::new("/tmp/config.toml"))
    );

    assert_eq!(invocation(&["clip-sync"]).kind(), LaunchKind::Desktop);
    assert_eq!(
        invocation(&["clip-sync", "status"]).kind(),
        LaunchKind::Client
    );
}

#[test]
fn every_client_command_routes_to_client_mode() {
    let commands = [
        vec!["clip-sync", "status"],
        vec!["clip-sync", "peers"],
        vec!["clip-sync", "history", "list"],
        vec!["clip-sync", "history", "search", "needle"],
        vec!["clip-sync", "history", "activate", "content"],
        vec!["clip-sync", "history", "pin", "content"],
        vec!["clip-sync", "history", "unpin", "content"],
        vec!["clip-sync", "history", "delete", "content"],
        vec!["clip-sync", "doctor"],
        vec!["clip-sync", "device", "forget", "device"],
    ];

    for arguments in commands {
        assert_eq!(invocation(&arguments).kind(), LaunchKind::Client);
    }
}

#[test]
fn help_and_version_still_short_circuit_through_clap() {
    assert_eq!(
        Cli::try_parse_from(["clip-sync", "--help"])
            .expect_err("help should short-circuit")
            .kind(),
        ErrorKind::DisplayHelp
    );
    assert_eq!(
        Cli::try_parse_from(["clip-sync", "--version"])
            .expect_err("version should short-circuit")
            .kind(),
        ErrorKind::DisplayVersion
    );
}

#[test]
fn cli_exposes_history_search_and_mutations() {
    for arguments in [
        vec!["clip-sync", "history", "search", "needle", "--json"],
        vec!["clip-sync", "history", "pin", "content", "--json"],
        vec!["clip-sync", "history", "unpin", "content"],
        vec!["clip-sync", "history", "delete", "content"],
        vec!["clip-sync", "device", "forget", "device-id", "--json"],
    ] {
        Cli::try_parse_from(arguments).expect("command should parse");
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
        history_item_json(&clip_sync_ipc::protocol::HistoryItem {
            content_id: "content".to_owned(),
            preview: "preview".to_owned(),
            mime_types: vec!["text/plain".to_owned()],
            logical_size: 42,
            source_node: "device-id".to_owned(),
            source_device: "device".to_owned(),
            pinned: true,
            physical_millis: 1_704_067_200_000,
            origin_millis: Some(1_704_067_200_000),
            remote: true,
            pinned_millis: None,
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
            "remote": true,
            "pinned_millis": null,
        })
    );
}

#[test]
fn status_json_fields_are_stable() {
    let addresses = vec!["192.168.10.4".to_owned()];
    let output = StatusOutput {
        version: "0.1.0",
        hostname: "kiwi",
        uptime_seconds: 42,
        config_path: "/config.toml",
        local_addresses: &addresses,
        discovered_peers: 2,
    };

    assert_eq!(
        serde_json::to_value(output).expect("serialize status"),
        serde_json::json!({
            "version": "0.1.0",
            "hostname": "kiwi",
            "uptime_seconds": 42,
            "config_path": "/config.toml",
            "local_addresses": ["192.168.10.4"],
            "discovered_peers": 2,
        })
    );
}

#[test]
fn peer_json_stats_are_additive_and_explicitly_unavailable() {
    use clip_sync_ipc::protocol::{PeerItem, PeerStats};

    let mut peer = PeerItem {
        hostname: "kiwi.mesh.local".to_owned(),
        address: "100.64.0.2".to_owned(),
        connected: true,
        stats: None,
    };
    assert_eq!(
        peer_json(&peer),
        serde_json::json!({
            "hostname": "kiwi.mesh.local",
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
