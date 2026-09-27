use serde::Serialize;

#[derive(Serialize)]
pub(super) struct StatusOutput<'a> {
    pub(super) version: &'a str,
    pub(super) hostname: &'a str,
    pub(super) uptime_seconds: u64,
    pub(super) config_path: &'a str,
    pub(super) local_addresses: &'a [String],
    pub(super) discovered_peers: u32,
}

pub(super) fn history_item_json(item: &clip_sync_ipc::protocol::HistoryItem) -> serde_json::Value {
    serde_json::json!({
        "content_id": item.content_id,
        "preview": item.preview,
        "mime_types": item.mime_types,
        "logical_size": item.logical_size,
        "source_node": item.source_node,
        "source_device": item.source_device,
        "pinned": item.pinned,
        "physical_millis": item.physical_millis,
        "origin_millis": item.origin_millis,
        "remote": item.remote,
    })
}

pub(super) fn peer_json(peer: &clip_sync_ipc::protocol::PeerItem) -> serde_json::Value {
    serde_json::json!({
        "hostname": peer.hostname,
        "address": peer.address,
        "connected": peer.connected,
        "stats": peer.stats.map(|stats| serde_json::json!({
            "shared_items": stats.shared_items,
            "shared_bytes": stats.shared_bytes,
            "pinned_items": stats.pinned_items,
            "last_shared_millis": stats.last_shared_millis,
        })),
    })
}

pub(super) fn error_json(code: &str, message: &str) -> serde_json::Value {
    serde_json::json!({
        "ok": false,
        "error": {
            "code": code,
            "message": message,
        }
    })
}

pub(super) fn print_json(value: &impl Serialize) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
