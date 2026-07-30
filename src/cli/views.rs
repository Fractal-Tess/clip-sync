use serde::{Deserialize, Serialize};

#[derive(Serialize)]
pub(super) struct StatusOutput<'a> {
    pub(super) version: &'a str,
    pub(super) hostname: &'a str,
    pub(super) uptime_seconds: u64,
    pub(super) config_path: &'a str,
    pub(super) netbird_address: &'a Option<String>,
    pub(super) discovered_peers: u32,
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct SafeConfig {
    shared: crate::config::SharedConfig,
    local: SafeLocal,
}

#[derive(Debug, Deserialize, Serialize)]
struct SafeLocal {
    listen_port: u16,
    discovery_interval_seconds: u64,
    reconcile_interval_seconds: u64,
    reconnect_min_seconds: u64,
    reconnect_max_seconds: u64,
    netbird_command: String,
    mesh_key_file_configured: bool,
    config_path: String,
}

pub(super) fn history_item_json(item: &crate::ipc::protocol::HistoryItem) -> serde_json::Value {
    serde_json::json!({
        "content_id": item.content_id,
        "preview": item.preview,
        "mime_types": item.mime_types,
        "logical_size": item.logical_size,
        "source_node": item.source_node,
        "source_device": item.source_device,
        "pinned": item.pinned,
        "physical_millis": item.physical_millis,
    })
}

pub(super) fn share_json(
    result: &crate::ipc::protocol::ShareClipboardResponse,
) -> serde_json::Value {
    serde_json::json!({
        "ok": result.shared,
        "shared": result.shared,
        "confirmation_required": result.confirmation_required,
        "logical_size": result.logical_size,
        "mime_types": &result.mime_types,
        "quota_exempt": result.quota_exempt,
        "transfer_id": &result.transfer_id,
        "content_id": &result.content_id,
        "message": &result.message,
    })
}

pub(super) fn transfer_json(transfer: &crate::ipc::protocol::TransferItem) -> serde_json::Value {
    serde_json::json!({
        "transfer_id": transfer.transfer_id,
        "content_id": transfer.content_id,
        "peer": transfer.peer,
        "state": transfer.state,
        "completed_bytes": transfer.completed_bytes,
        "total_bytes": transfer.total_bytes,
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
