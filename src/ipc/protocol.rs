use prost::{Message, Oneof};

pub const IPC_PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, PartialEq, Message)]
pub struct Request {
    #[prost(uint32, tag = "1")]
    pub protocol_version: u32,
    #[prost(uint64, tag = "2")]
    pub request_id: u64,
    #[prost(oneof = "request::Body", tags = "10, 11, 12, 13")]
    pub body: Option<request::Body>,
}

pub mod request {
    use super::{ActivateRequest, ConfigRequest, HistoryRequest, Oneof, StatusRequest};

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
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Message)]
pub struct StatusRequest {}

#[derive(Clone, Copy, PartialEq, Eq, Message)]
pub struct ConfigRequest {}

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

#[derive(Clone, PartialEq, Message)]
pub struct Response {
    #[prost(uint32, tag = "1")]
    pub protocol_version: u32,
    #[prost(uint64, tag = "2")]
    pub request_id: u64,
    #[prost(oneof = "response::Body", tags = "10, 11, 12, 13, 14")]
    pub body: Option<response::Body>,
}

pub mod response {
    use super::{
        ConfigResponse, ErrorResponse, HistoryResponse, MutationResponse, Oneof, StatusResponse,
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
}

#[derive(Clone, PartialEq, Message)]
pub struct ErrorResponse {
    #[prost(string, tag = "1")]
    pub code: String,
    #[prost(string, tag = "2")]
    pub message: String,
}
