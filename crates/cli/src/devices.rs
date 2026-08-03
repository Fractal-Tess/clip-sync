use clip_sync_core::config::AppPaths;
use clip_sync_ipc::protocol::{ForgetDeviceRequest, PeersRequest, request, response};

use super::{
    commands::{DeviceCommand, OutputArgs},
    support::{daemon_request, daemon_response_error, mutation_response, unexpected_response},
    views::{peer_json, print_json},
};

pub(super) async fn peers(paths: &AppPaths, output: OutputArgs) -> anyhow::Result<()> {
    let response =
        daemon_request(paths, 2, request::Body::Peers(PeersRequest {}), output.json).await?;
    match response.body {
        Some(response::Body::Peers(peers)) if output.json => {
            let peer_items = peers.peers.iter().map(peer_json).collect::<Vec<_>>();
            print_json(&serde_json::json!({
                "local_hostname": peers.local_hostname,
                "local_addresses": peers.local_addresses,
                "peers": peer_items,
                "discovery_error": peers.discovery_error,
                "devices": peers.devices.into_iter().map(|device| serde_json::json!({
                    "device_id": device.device_id,
                    "local": device.local,
                    "forgotten": device.forgotten,
                })).collect::<Vec<_>>(),
            }))
        }
        Some(response::Body::Peers(peers)) => {
            println!(
                "local: {} ({})",
                peers.local_hostname,
                if peers.local_addresses.is_empty() {
                    "address unavailable".to_owned()
                } else {
                    peers.local_addresses.join(", ")
                }
            );
            if let Some(error) = peers.discovery_error {
                println!("discovery: unavailable ({error})");
            }
            if peers.peers.is_empty() {
                println!("no connected peers");
            }
            for peer in peers.peers {
                let status = if peer.connected {
                    "connected"
                } else {
                    "offline"
                };
                if let Some(stats) = peer.stats {
                    println!(
                        "{}  {}  {status}  {} items  {} bytes  {} pinned",
                        peer.hostname,
                        peer.address,
                        stats.shared_items,
                        stats.shared_bytes,
                        stats.pinned_items
                    );
                } else {
                    println!(
                        "{}  {}  {status}  stats unavailable",
                        peer.hostname, peer.address
                    );
                }
            }
            if !peers.devices.is_empty() {
                println!("mesh devices:");
                for device in peers.devices {
                    let state = if device.local {
                        "local"
                    } else if device.forgotten {
                        "forgotten"
                    } else {
                        "remembered"
                    };
                    println!("{}  {state}", device.device_id);
                }
            }
            Ok(())
        }
        Some(response::Body::Error(error)) => Err(daemon_response_error(&error, output.json)),
        _ => Err(unexpected_response(output.json, "peers")),
    }
}

pub(super) async fn device_command(paths: &AppPaths, command: DeviceCommand) -> anyhow::Result<()> {
    match command {
        DeviceCommand::Forget { device_id, json } => {
            let response = daemon_request(
                paths,
                11,
                request::Body::ForgetDevice(ForgetDeviceRequest { device_id }),
                json,
            )
            .await?;
            mutation_response(response, json, "device forgotten")
        }
    }
}
