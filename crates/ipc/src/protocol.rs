use prost::{Enumeration, Message, Oneof};

pub const IPC_PROTOCOL_VERSION: u32 = 6;

#[derive(Clone, PartialEq, Message)]
pub struct Request {
    #[prost(uint32, tag = "1")]
    pub protocol_version: u32,
    #[prost(uint64, tag = "2")]
    pub request_id: u64,
    #[prost(
        oneof = "request::Body",
        tags = "10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23"
    )]
    pub body: Option<request::Body>,
}

pub mod request {
    use super::{
        ActivateRequest, ConfigRequest, DiagnosticsRequest, ForgetDeviceRequest, HistoryRequest,
        HistoryUpdateRequest, ImagePreviewRequest, Oneof, PeerInterfacesUpdateRequest,
        PeersRequest, ShareClipboardRequest, SharedSettingUpdateRequest, StatusRequest,
        TransferCancelRequest, TransfersRequest,
    };

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Body {
        #[prost(message, tag = "10")]
        Status(StatusRequest),
        #[prost(message, tag = "11")]
        Config(ConfigRequest),
        #[prost(message, tag = "12")]
        History(HistoryRequest),
        #[prost(message, tag = "13")]
        Activate(ActivateRequest),
        #[prost(message, tag = "14")]
        Peers(PeersRequest),
        #[prost(message, tag = "15")]
        HistoryUpdate(HistoryUpdateRequest),
        #[prost(message, tag = "16")]
        Diagnostics(DiagnosticsRequest),
        #[prost(message, tag = "17")]
        ShareClipboard(ShareClipboardRequest),
        #[prost(message, tag = "18")]
        Transfers(TransfersRequest),
        #[prost(message, tag = "19")]
        TransferCancel(TransferCancelRequest),
        #[prost(message, tag = "20")]
        ForgetDevice(ForgetDeviceRequest),
        #[prost(message, tag = "21")]
        SharedSettingUpdate(SharedSettingUpdateRequest),
        #[prost(message, tag = "22")]
        ImagePreview(ImagePreviewRequest),
        #[prost(message, tag = "23")]
        PeerInterfacesUpdate(PeerInterfacesUpdateRequest),
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Message)]
pub struct StatusRequest {}

#[derive(Clone, Copy, PartialEq, Eq, Message)]
pub struct ConfigRequest {}

#[derive(Clone, Copy, PartialEq, Eq, Message)]
pub struct PeersRequest {}

#[derive(Clone, Copy, PartialEq, Eq, Message)]
pub struct DiagnosticsRequest {}

#[derive(Clone, Copy, PartialEq, Eq, Message)]
pub struct ShareClipboardRequest {
    #[prost(bool, tag = "1")]
    pub confirmed: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Message)]
pub struct TransfersRequest {}

#[derive(Clone, PartialEq, Eq, Message)]
pub struct HistoryRequest {
    #[prost(string, tag = "1")]
    pub query: String,
    #[prost(uint32, tag = "2")]
    pub limit: u32,
    #[prost(uint32, tag = "3")]
    pub offset: u32,
}

#[derive(Clone, PartialEq, Eq, Message)]
pub struct ActivateRequest {
    #[prost(string, tag = "1")]
    pub content_id: String,
}

#[derive(Clone, PartialEq, Eq, Message)]
pub struct ImagePreviewRequest {
    #[prost(string, tag = "1")]
    pub content_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Enumeration)]
#[repr(i32)]
pub enum HistoryUpdateAction {
    Unspecified = 0,
    Pin = 1,
    Unpin = 2,
    Delete = 3,
}

#[derive(Clone, PartialEq, Eq, Message)]
pub struct HistoryUpdateRequest {
    #[prost(string, tag = "1")]
    pub content_id: String,
    #[prost(enumeration = "HistoryUpdateAction", tag = "2")]
    pub action: i32,
}

#[derive(Clone, PartialEq, Eq, Message)]
pub struct TransferCancelRequest {
    #[prost(string, tag = "1")]
    pub transfer_id: String,
}

#[derive(Clone, PartialEq, Eq, Message)]
pub struct ForgetDeviceRequest {
    #[prost(string, tag = "1")]
    pub device_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Enumeration)]
#[repr(i32)]
pub enum SharedSettingKind {
    Unspecified = 0,
    MeshQuotaBytes = 1,
    CaptureThresholdBytes = 2,
}

#[derive(Clone, Copy, PartialEq, Eq, Message)]
pub struct SharedSettingUpdateRequest {
    #[prost(enumeration = "SharedSettingKind", tag = "1")]
    pub setting: i32,
    #[prost(uint64, tag = "2")]
    pub value: u64,
}

#[derive(Clone, PartialEq, Eq, Message)]
pub struct PeerInterfacesUpdateRequest {
    #[prost(string, repeated, tag = "1")]
    pub interfaces: Vec<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Response {
    #[prost(uint32, tag = "1")]
    pub protocol_version: u32,
    #[prost(uint64, tag = "2")]
    pub request_id: u64,
    #[prost(
        oneof = "response::Body",
        tags = "10, 11, 12, 13, 14, 15, 16, 17, 18, 19"
    )]
    pub body: Option<response::Body>,
}

pub mod response {
    use super::{
        ConfigResponse, DiagnosticsResponse, ErrorResponse, HistoryResponse, ImagePreviewResponse,
        MutationResponse, Oneof, PeersResponse, ShareClipboardResponse, StatusResponse,
        TransfersResponse,
    };

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Body {
        #[prost(message, tag = "10")]
        Status(StatusResponse),
        #[prost(message, tag = "11")]
        Config(ConfigResponse),
        #[prost(message, tag = "12")]
        Error(ErrorResponse),
        #[prost(message, tag = "13")]
        History(HistoryResponse),
        #[prost(message, tag = "14")]
        Mutation(MutationResponse),
        #[prost(message, tag = "15")]
        Peers(PeersResponse),
        #[prost(message, tag = "16")]
        Diagnostics(DiagnosticsResponse),
        #[prost(message, tag = "17")]
        Transfers(TransfersResponse),
        #[prost(message, tag = "18")]
        ShareClipboard(ShareClipboardResponse),
        #[prost(message, tag = "19")]
        ImagePreview(ImagePreviewResponse),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct StatusResponse {
    #[prost(string, tag = "1")]
    pub version: String,
    #[prost(string, tag = "2")]
    pub hostname: String,
    #[prost(uint64, tag = "3")]
    pub uptime_seconds: u64,
    #[prost(string, tag = "4")]
    pub config_path: String,
    #[prost(string, repeated, tag = "5")]
    pub local_addresses: Vec<String>,
    #[prost(uint32, tag = "6")]
    pub discovered_peers: u32,
    #[prost(uint32, tag = "8")]
    pub connected_peers: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct ConfigResponse {
    #[prost(bytes = "vec", tag = "1")]
    pub redacted_json: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct HistoryResponse {
    #[prost(message, repeated, tag = "1")]
    pub items: Vec<HistoryItem>,
    #[prost(uint64, tag = "2")]
    pub total: u64,
}

#[derive(Clone, PartialEq, Eq, Message)]
pub struct HistoryItem {
    #[prost(string, tag = "1")]
    pub content_id: String,
    #[prost(string, tag = "2")]
    pub preview: String,
    #[prost(string, repeated, tag = "3")]
    pub mime_types: Vec<String>,
    #[prost(uint64, tag = "4")]
    pub logical_size: u64,
    #[prost(string, tag = "5")]
    pub source_node: String,
    #[prost(bool, tag = "6")]
    pub pinned: bool,
    #[prost(uint64, tag = "7")]
    pub physical_millis: u64,
    #[prost(string, tag = "8")]
    pub source_device: String,
    #[prost(uint64, optional, tag = "9")]
    pub origin_millis: Option<u64>,
}

#[derive(Clone, PartialEq, Eq, Message)]
pub struct ImagePreviewResponse {
    #[prost(string, tag = "1")]
    pub content_id: String,
    #[prost(string, tag = "2")]
    pub mime_type: String,
    #[prost(uint32, tag = "3")]
    pub width: u32,
    #[prost(uint32, tag = "4")]
    pub height: u32,
    #[prost(bytes = "vec", tag = "5")]
    pub rgba: Vec<u8>,
}

#[derive(Clone, PartialEq, Eq, Message)]
pub struct MutationResponse {
    #[prost(bool, tag = "1")]
    pub ok: bool,
    #[prost(string, tag = "2")]
    pub message: String,
    #[prost(string, optional, tag = "3")]
    pub resource_id: Option<String>,
}

#[derive(Clone, PartialEq, Eq, Message)]
pub struct ShareClipboardResponse {
    #[prost(bool, tag = "1")]
    pub shared: bool,
    #[prost(bool, tag = "2")]
    pub confirmation_required: bool,
    #[prost(uint64, tag = "3")]
    pub logical_size: u64,
    #[prost(string, repeated, tag = "4")]
    pub mime_types: Vec<String>,
    #[prost(bool, tag = "5")]
    pub quota_exempt: bool,
    #[prost(string, optional, tag = "6")]
    pub transfer_id: Option<String>,
    #[prost(string, optional, tag = "7")]
    pub content_id: Option<String>,
    #[prost(string, tag = "8")]
    pub message: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct PeersResponse {
    #[prost(string, tag = "1")]
    pub local_hostname: String,
    #[prost(string, repeated, tag = "2")]
    pub local_addresses: Vec<String>,
    #[prost(message, repeated, tag = "3")]
    pub peers: Vec<PeerItem>,
    #[prost(string, optional, tag = "4")]
    pub discovery_error: Option<String>,
    #[prost(message, repeated, tag = "5")]
    pub devices: Vec<DeviceItem>,
}

#[derive(Clone, PartialEq, Eq, Message)]
pub struct DeviceItem {
    #[prost(string, tag = "1")]
    pub device_id: String,
    #[prost(bool, tag = "2")]
    pub local: bool,
    #[prost(bool, tag = "3")]
    pub forgotten: bool,
}

#[derive(Clone, PartialEq, Eq, Message)]
pub struct PeerItem {
    #[prost(string, tag = "1")]
    pub hostname: String,
    #[prost(string, tag = "2")]
    pub address: String,
    #[prost(bool, tag = "3")]
    pub connected: bool,
    #[prost(message, optional, tag = "4")]
    pub stats: Option<PeerStats>,
}

#[derive(Clone, Copy, PartialEq, Eq, Message)]
pub struct PeerStats {
    #[prost(uint64, tag = "1")]
    pub shared_items: u64,
    #[prost(uint64, tag = "2")]
    pub shared_bytes: u64,
    #[prost(uint64, tag = "3")]
    pub pinned_items: u64,
    #[prost(uint64, optional, tag = "4")]
    pub last_shared_millis: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct DiagnosticsResponse {
    #[prost(message, repeated, tag = "1")]
    pub checks: Vec<DiagnosticCheck>,
}

#[derive(Clone, PartialEq, Eq, Message)]
pub struct DiagnosticCheck {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(bool, tag = "2")]
    pub ok: bool,
    #[prost(string, tag = "3")]
    pub detail: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct TransfersResponse {
    #[prost(message, repeated, tag = "1")]
    pub transfers: Vec<TransferItem>,
}

#[derive(Clone, PartialEq, Eq, Message)]
pub struct TransferItem {
    #[prost(string, tag = "1")]
    pub transfer_id: String,
    #[prost(string, tag = "2")]
    pub content_id: String,
    #[prost(string, tag = "3")]
    pub peer: String,
    #[prost(string, tag = "4")]
    pub state: String,
    #[prost(uint64, tag = "5")]
    pub completed_bytes: u64,
    #[prost(uint64, tag = "6")]
    pub total_bytes: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct ErrorResponse {
    #[prost(string, tag = "1")]
    pub code: String,
    #[prost(string, tag = "2")]
    pub message: String,
}
