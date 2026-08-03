use serde::{Deserialize, Serialize};
use specta::Type;

#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StatusView {
    pub(crate) version: String,
    pub(crate) hostname: String,
    pub(crate) uptime_seconds: u64,
    pub(crate) config_path: String,
    pub(crate) local_addresses: Vec<String>,
    pub(crate) discovered_peers: u32,
    pub(crate) connected_peers: u32,
}

#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HistoryItemView {
    pub(crate) content_id: String,
    pub(crate) preview: String,
    pub(crate) mime_types: Vec<String>,
    pub(crate) logical_size: u64,
    pub(crate) source_node: String,
    pub(crate) source_device: String,
    pub(crate) pinned: bool,
    pub(crate) physical_millis: u64,
    pub(crate) origin_millis: Option<u64>,
}

#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HistoryPageView {
    pub(crate) items: Vec<HistoryItemView>,
    pub(crate) total: u64,
}

#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MutationView {
    pub(crate) ok: bool,
    pub(crate) message: String,
    pub(crate) resource_id: Option<String>,
}

#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PeerStatsView {
    pub(crate) shared_items: u64,
    pub(crate) shared_bytes: u64,
    pub(crate) pinned_items: u64,
    pub(crate) last_shared_millis: Option<u64>,
}

#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PeerView {
    pub(crate) hostname: String,
    pub(crate) address: String,
    pub(crate) connected: bool,
    pub(crate) stats: Option<PeerStatsView>,
}

#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeviceView {
    pub(crate) device_id: String,
    pub(crate) local: bool,
    pub(crate) forgotten: bool,
}

#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PeersView {
    pub(crate) local_hostname: String,
    pub(crate) local_addresses: Vec<String>,
    pub(crate) peers: Vec<PeerView>,
    pub(crate) discovery_error: Option<String>,
    pub(crate) devices: Vec<DeviceView>,
}

#[derive(Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SharedSettingsView {
    #[serde(alias = "mesh_quota_bytes")]
    pub(crate) mesh_quota_bytes: u64,
    #[serde(alias = "capture_threshold_bytes")]
    pub(crate) capture_threshold_bytes: u64,
    pub(crate) revision: String,
}

#[derive(Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalSettingsView {
    #[serde(alias = "listen_port")]
    pub(crate) listen_port: u16,
    #[serde(alias = "discovery_interval_seconds")]
    pub(crate) discovery_interval_seconds: u64,
    #[serde(alias = "reconcile_interval_seconds")]
    pub(crate) reconcile_interval_seconds: u64,
    #[serde(alias = "reconnect_min_seconds")]
    pub(crate) reconnect_min_seconds: u64,
    #[serde(alias = "reconnect_max_seconds")]
    pub(crate) reconnect_max_seconds: u64,
    #[serde(alias = "peer_interfaces")]
    pub(crate) peer_interfaces: Vec<String>,
    #[serde(alias = "mesh_key_file_configured")]
    pub(crate) mesh_key_file_configured: bool,
    #[serde(alias = "config_path")]
    pub(crate) config_path: String,
}

#[derive(Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SettingsView {
    pub(crate) shared: SharedSettingsView,
    pub(crate) local: LocalSettingsView,
}

#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DiagnosticView {
    pub(crate) name: String,
    pub(crate) ok: bool,
    pub(crate) detail: String,
}

#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TransferView {
    pub(crate) transfer_id: String,
    pub(crate) content_id: String,
    pub(crate) peer: String,
    pub(crate) state: String,
    pub(crate) completed_bytes: u64,
    pub(crate) total_bytes: u64,
}

#[derive(Debug, Clone, Copy, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub(crate) enum SharedSettingView {
    MeshQuotaBytes,
    CaptureThresholdBytes,
}

#[derive(Debug, Clone, Copy, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub(crate) enum HistoryUpdateView {
    Pin,
    Unpin,
    Delete,
}

#[derive(Debug, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImagePreviewView {
    pub(crate) content_id: String,
    pub(crate) mime_type: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) rgba: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::SettingsView;

    #[test]
    fn deserializes_daemon_redacted_settings() {
        let settings: SettingsView = serde_json::from_value(serde_json::json!({
            "shared": {
                "mesh_quota_bytes": 1024,
                "capture_threshold_bytes": 512,
                "revision": "rev"
            },
            "local": {
                "listen_port": 47822,
                "discovery_interval_seconds": 15,
                "reconcile_interval_seconds": 5,
                "reconnect_min_seconds": 1,
                "reconnect_max_seconds": 30,
                "peer_interfaces": ["wt0", "tun0"],
                "mesh_key_file_configured": true,
                "config_path": "/tmp/config.toml"
            }
        }))
        .expect("redacted settings");

        assert_eq!(settings.shared.mesh_quota_bytes, 1024);
        assert_eq!(settings.local.listen_port, 47822);
        assert_eq!(settings.local.peer_interfaces, ["wt0", "tun0"]);
        assert!(settings.local.mesh_key_file_configured);
    }
}
