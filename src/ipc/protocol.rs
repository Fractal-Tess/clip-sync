use prost::{Message, Oneof};

pub const IPC_PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, PartialEq, Message)]
pub struct Request {
    #[prost(uint32, tag = "1")]
    pub protocol_version: u32,
    #[prost(uint64, tag = "2")]
    pub request_id: u64,
    #[prost(oneof = "request::Body", tags = "10, 11")]
    pub body: Option<request::Body>,
}

pub mod request {
    use super::{ConfigRequest, Oneof, StatusRequest};

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Body {
        #[prost(message, tag = "10")]
        Status(StatusRequest),
        #[prost(message, tag = "11")]
        Config(ConfigRequest),
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Message)]
pub struct StatusRequest {}

#[derive(Clone, Copy, PartialEq, Eq, Message)]
pub struct ConfigRequest {}

#[derive(Clone, PartialEq, Message)]
pub struct Response {
    #[prost(uint32, tag = "1")]
    pub protocol_version: u32,
    #[prost(uint64, tag = "2")]
    pub request_id: u64,
    #[prost(oneof = "response::Body", tags = "10, 11, 12")]
    pub body: Option<response::Body>,
}

pub mod response {
    use super::{ConfigResponse, ErrorResponse, Oneof, StatusResponse};

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Body {
        #[prost(message, tag = "10")]
        Status(StatusResponse),
        #[prost(message, tag = "11")]
        Config(ConfigResponse),
        #[prost(message, tag = "12")]
        Error(ErrorResponse),
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
pub struct ErrorResponse {
    #[prost(string, tag = "1")]
    pub code: String,
    #[prost(string, tag = "2")]
    pub message: String,
}
