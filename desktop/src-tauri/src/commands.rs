use clip_sync_ipc::protocol::{
    ActivateRequest, ConfigRequest, DiagnosticsRequest, ForgetDeviceRequest, HistoryRequest,
    HistoryUpdateAction, HistoryUpdateRequest, ImagePreviewRequest, MutationResponse,
    PeerInterfacesUpdateRequest, PeersRequest, SharedSettingKind, SharedSettingUpdateRequest,
    StatusRequest, TransferCancelRequest, TransfersRequest, request, response,
};
use tauri::State;

use crate::{
    image_preview::image_preview_view,
    state::{AppState, daemon_request},
    views::{
        DeviceView, DiagnosticView, HistoryItemView, HistoryPageView, HistoryUpdateView,
        ImagePreviewView, MutationView, PeerStatsView, PeerView, PeersView, SettingsView,
        SharedSettingView, StatusView, TransferView,
    },
};

#[tauri::command]
#[specta::specta]
pub(crate) async fn get_status(state: State<'_, AppState>) -> Result<StatusView, String> {
    match daemon_request(&state, request::Body::Status(StatusRequest {})).await? {
        response::Body::Status(status) => Ok(StatusView {
            version: status.version,
            hostname: status.hostname,
            uptime_seconds: status.uptime_seconds,
            config_path: status.config_path,
            local_addresses: status.local_addresses,
            discovered_peers: status.discovered_peers,
            connected_peers: status.connected_peers,
        }),
        _ => Err("ClipSync daemon returned the wrong response to status".to_owned()),
    }
}

#[tauri::command]
#[specta::specta]
pub(crate) async fn get_history(
    query: String,
    offset: u32,
    limit: u32,
    state: State<'_, AppState>,
) -> Result<HistoryPageView, String> {
    let limit = limit.clamp(1, 200);
    match daemon_request(
        &state,
        request::Body::History(HistoryRequest {
            query,
            limit,
            offset,
        }),
    )
    .await?
    {
        response::Body::History(history) if history.total == 0 && !history.items.is_empty() => Err(
            "The running ClipSync daemon does not support paged history yet; restart it with the updated build"
                .to_owned(),
        ),
        response::Body::History(history) => Ok(HistoryPageView {
            items: history
                .items
                .into_iter()
                .map(|item| HistoryItemView {
                    content_id: item.content_id,
                    preview: item.preview,
                    mime_types: item.mime_types,
                    logical_size: item.logical_size,
                    source_node: item.source_node,
                    source_device: item.source_device,
                    pinned: item.pinned,
                    physical_millis: item.physical_millis,
                    origin_millis: item.origin_millis,
                })
                .collect(),
            total: history.total,
        }),
        _ => Err("ClipSync daemon returned the wrong response to history".to_owned()),
    }
}

#[tauri::command]
#[specta::specta]
pub(crate) async fn get_image_preview(
    content_id: String,
    state: State<'_, AppState>,
) -> Result<ImagePreviewView, String> {
    let requested_content_id = content_id.clone();
    match daemon_request(
        &state,
        request::Body::ImagePreview(ImagePreviewRequest { content_id }),
    )
    .await?
    {
        response::Body::ImagePreview(preview) => image_preview_view(&requested_content_id, preview),
        _ => Err("ClipSync daemon returned the wrong response to image preview".to_owned()),
    }
}

#[tauri::command]
#[specta::specta]
pub(crate) async fn get_peers(state: State<'_, AppState>) -> Result<PeersView, String> {
    match daemon_request(&state, request::Body::Peers(PeersRequest {})).await? {
        response::Body::Peers(peers) => Ok(PeersView {
            local_hostname: peers.local_hostname,
            local_addresses: peers.local_addresses,
            peers: peers
                .peers
                .into_iter()
                .map(|peer| PeerView {
                    hostname: peer.hostname,
                    address: peer.address,
                    connected: peer.connected,
                    stats: peer.stats.map(|stats| PeerStatsView {
                        shared_items: stats.shared_items,
                        shared_bytes: stats.shared_bytes,
                        pinned_items: stats.pinned_items,
                        last_shared_millis: stats.last_shared_millis,
                    }),
                })
                .collect(),
            discovery_error: peers.discovery_error,
            devices: peers
                .devices
                .into_iter()
                .map(|device| DeviceView {
                    device_id: device.device_id,
                    local: device.local,
                    forgotten: device.forgotten,
                })
                .collect(),
        }),
        _ => Err("ClipSync daemon returned the wrong response to peers".to_owned()),
    }
}

#[tauri::command]
#[specta::specta]
pub(crate) async fn get_settings(state: State<'_, AppState>) -> Result<SettingsView, String> {
    match daemon_request(&state, request::Body::Config(ConfigRequest {})).await? {
        response::Body::Config(config) => serde_json::from_slice(&config.redacted_json)
            .map_err(|error| format!("ClipSync daemon returned invalid settings: {error}")),
        _ => Err("ClipSync daemon returned the wrong response to settings".to_owned()),
    }
}

#[tauri::command]
#[specta::specta]
pub(crate) async fn get_diagnostics(
    state: State<'_, AppState>,
) -> Result<Vec<DiagnosticView>, String> {
    match daemon_request(&state, request::Body::Diagnostics(DiagnosticsRequest {})).await? {
        response::Body::Diagnostics(diagnostics) => Ok(diagnostics
            .checks
            .into_iter()
            .map(|check| DiagnosticView {
                name: check.name,
                ok: check.ok,
                detail: check.detail,
            })
            .collect()),
        _ => Err("ClipSync daemon returned the wrong response to diagnostics".to_owned()),
    }
}

#[tauri::command]
#[specta::specta]
pub(crate) async fn get_transfers(state: State<'_, AppState>) -> Result<Vec<TransferView>, String> {
    match daemon_request(&state, request::Body::Transfers(TransfersRequest {})).await? {
        response::Body::Transfers(transfers) => Ok(transfers
            .transfers
            .into_iter()
            .map(|transfer| TransferView {
                transfer_id: transfer.transfer_id,
                content_id: transfer.content_id,
                peer: transfer.peer,
                state: transfer.state,
                completed_bytes: transfer.completed_bytes,
                total_bytes: transfer.total_bytes,
            })
            .collect()),
        _ => Err("ClipSync daemon returned the wrong response to transfers".to_owned()),
    }
}

fn mutation_view(mutation: MutationResponse) -> MutationView {
    MutationView {
        ok: mutation.ok,
        message: mutation.message,
        resource_id: mutation.resource_id,
    }
}

#[tauri::command]
#[specta::specta]
pub(crate) async fn cancel_transfer(
    transfer_id: String,
    state: State<'_, AppState>,
) -> Result<MutationView, String> {
    match daemon_request(
        &state,
        request::Body::TransferCancel(TransferCancelRequest { transfer_id }),
    )
    .await?
    {
        response::Body::Mutation(mutation) => Ok(mutation_view(mutation)),
        _ => Err("ClipSync daemon returned the wrong response to transfer cancellation".to_owned()),
    }
}

#[tauri::command]
#[specta::specta]
pub(crate) async fn forget_device(
    device_id: String,
    state: State<'_, AppState>,
) -> Result<MutationView, String> {
    match daemon_request(
        &state,
        request::Body::ForgetDevice(ForgetDeviceRequest { device_id }),
    )
    .await?
    {
        response::Body::Mutation(mutation) => Ok(mutation_view(mutation)),
        _ => Err("ClipSync daemon returned the wrong response to device forget".to_owned()),
    }
}

#[tauri::command]
#[specta::specta]
pub(crate) async fn update_shared_setting(
    setting: SharedSettingView,
    value: u64,
    state: State<'_, AppState>,
) -> Result<MutationView, String> {
    let setting = match setting {
        SharedSettingView::MeshQuotaBytes => SharedSettingKind::MeshQuotaBytes,
        SharedSettingView::CaptureThresholdBytes => SharedSettingKind::CaptureThresholdBytes,
    };
    match daemon_request(
        &state,
        request::Body::SharedSettingUpdate(SharedSettingUpdateRequest {
            setting: setting as i32,
            value,
        }),
    )
    .await?
    {
        response::Body::Mutation(mutation) => Ok(mutation_view(mutation)),
        _ => Err("ClipSync daemon returned the wrong response to settings update".to_owned()),
    }
}

#[tauri::command]
#[specta::specta]
pub(crate) async fn update_peer_interfaces(
    interfaces: Vec<String>,
    state: State<'_, AppState>,
) -> Result<MutationView, String> {
    match daemon_request(
        &state,
        request::Body::PeerInterfacesUpdate(PeerInterfacesUpdateRequest { interfaces }),
    )
    .await?
    {
        response::Body::Mutation(mutation) => Ok(mutation_view(mutation)),
        _ => Err("ClipSync daemon returned the wrong response to peer interface update".to_owned()),
    }
}

#[tauri::command]
#[specta::specta]
pub(crate) async fn update_history(
    content_id: String,
    action: HistoryUpdateView,
    state: State<'_, AppState>,
) -> Result<MutationView, String> {
    let action = match action {
        HistoryUpdateView::Pin => HistoryUpdateAction::Pin,
        HistoryUpdateView::Unpin => HistoryUpdateAction::Unpin,
        HistoryUpdateView::Delete => HistoryUpdateAction::Delete,
    };
    match daemon_request(
        &state,
        request::Body::HistoryUpdate(HistoryUpdateRequest {
            content_id,
            action: action as i32,
        }),
    )
    .await?
    {
        response::Body::Mutation(mutation) => Ok(mutation_view(mutation)),
        _ => Err("ClipSync daemon returned the wrong response to history update".to_owned()),
    }
}

#[tauri::command]
#[specta::specta]
pub(crate) async fn activate_history(
    content_id: String,
    state: State<'_, AppState>,
) -> Result<MutationView, String> {
    match daemon_request(
        &state,
        request::Body::Activate(ActivateRequest { content_id }),
    )
    .await?
    {
        response::Body::Mutation(mutation) => Ok(mutation_view(mutation)),
        _ => Err("ClipSync daemon returned the wrong response to activation".to_owned()),
    }
}
