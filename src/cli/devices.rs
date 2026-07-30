use crate::{
    config::AppPaths,
    ipc::protocol::{ForgetDeviceRequest, PeersRequest, request, response},
};

use super::{
    commands::{DeviceCommand, OutputArgs},
    support::{daemon_request, daemon_response_error, mutation_response, unexpected_response},
    views::print_json,
};

pub(super) async fn peers(paths: &AppPaths, output: OutputArgs) -> anyhow::Result<()> {
    let response =
        daemon_request(paths, 2, request::Body::Peers(PeersRequest {}), output.json).await?;
    match response.body {
        Some(response::Body::Peers(peers)) if output.json => {
            let peer_items = peers
                .peers
                .into_iter()
                .map(|peer| {
                    serde_json::json!({
                        "hostname": peer.hostname,
                        "address": peer.address,
                        "connected": peer.connected,
                    })
                })
                .collect::<Vec<_>>();
            print_json(&serde_json::json!({
                "local_hostname": peers.local_hostname,
                "local_address": peers.local_address,
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
                peers
                    .local_address
                    .as_deref()
                    .unwrap_or("address unavailable")
            );
            if let Some(error) = peers.discovery_error {
                println!("discovery: unavailable ({error})");
            }
            if peers.peers.is_empty() {
                println!("no peers discovered");
            }
            for peer in peers.peers {
                let status = if peer.connected {
                    "connected"
                } else {
                    "offline"
                };
                println!("{}  {}  {status}", peer.hostname, peer.address);
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
