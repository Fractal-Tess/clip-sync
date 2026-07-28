use prost::{Enumeration, Message, Oneof};

pub const IPC_PROTOCOL_VERSION: u32 = 2;

#[derive(Clone, PartialEq, Message)]
pub struct Request {
    #[prost(uint32, tag = "1")]
    pub protocol_version: u32,
    #[prost(uint64, tag = "2")]
    pub request_id: u64,
    #[prost(
        oneof = "request::Body",
        tags = "10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20"
    )]
    pub body: Option<request::Body>,
}

pub mod request {
    use super::{
        ActivateRequest, ConfigRequest, DiagnosticsRequest, ForgetDeviceRequest, HistoryRequest,
        HistoryUpdateRequest, Oneof, PeersRequest, ShareClipboardRequest, StatusRequest,
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
pub struct ShareClipboardRequest {}

#[derive(Clone, Copy, PartialEq, Eq, Message)]
pub struct TransfersRequest {}

#[derive(Clone, PartialEq, Eq, Message)]
pub struct HistoryRequest {
    #[prost(string, tag = "1")]
    pub query: String,
    #[prost(uint32, tag = "2")]
    pub limit: u32,
}

#[derive(Clone, PartialEq, Eq, Message)]
pub struct ActivateRequest {
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

#[derive(Clone, PartialEq, Message)]
pub struct Response {
    #[prost(uint32, tag = "1")]
    pub protocol_version: u32,
    #[prost(uint64, tag = "2")]
    pub request_id: u64,
    #[prost(oneof = "response::Body", tags = "10, 11, 12, 13, 14, 15, 16, 17")]
    pub body: Option<response::Body>,
}

pub mod response {
    use super::{
        ConfigResponse, DiagnosticsResponse, ErrorResponse, HistoryResponse, MutationResponse,
        Oneof, PeersResponse, StatusResponse, TransfersResponse,
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
    #[prost(string, optional, tag = "5")]
    pub netbird_address: Option<String>,
    #[prost(uint32, tag = "6")]
    pub discovered_peers: u32,
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

#[derive(Clone, PartialEq, Message)]
pub struct PeersResponse {
    #[prost(string, tag = "1")]
    pub local_hostname: String,
    #[prost(string, optional, tag = "2")]
    pub local_address: Option<String>,
    #[prost(message, repeated, tag = "3")]
    pub peers: Vec<PeerItem>,
    #[prost(string, optional, tag = "4")]
    pub discovery_error: Option<String>,
}

#[derive(Clone, PartialEq, Eq, Message)]
pub struct PeerItem {
    #[prost(string, tag = "1")]
    pub hostname: String,
    #[prost(string, tag = "2")]
    pub address: String,
    #[prost(bool, tag = "3")]
    pub connected: bool,
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
