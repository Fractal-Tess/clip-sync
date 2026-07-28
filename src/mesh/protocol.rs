//! Bounded Protobuf control protocol used after transport authentication.

use prost::Message;
use quinn::{RecvStream, SendStream};
use thiserror::Error;

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_CONTROL_FRAME_BYTES: usize = 24 * 1024 * 1024;
pub const MAX_FRONTIER_BYTES: usize = 1024 * 1024;
pub const MAX_HOSTNAME_BYTES: usize = 255;
pub const MAX_BATCH_OPERATIONS: usize = 128;
pub const STREAM_KIND_SYNC: u8 = 1;
pub const STREAM_KIND_CHUNK: u8 = 2;
pub const MAX_ENCRYPTED_CHUNK_BYTES: usize = 4 * 1024 * 1024 + 48;
pub const MAX_CHUNK_CONTROL_BYTES: usize = 512;

#[derive(Clone, PartialEq, Message)]
pub struct IdentityHello {
    #[prost(uint32, tag = "1")]
    pub protocol_version: u32,
    #[prost(bytes = "vec", tag = "2")]
    pub node_id: Vec<u8>,
    #[prost(string, tag = "3")]
    pub hostname: String,
    #[prost(bytes = "vec", tag = "4")]
    pub frontier: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SyncRequest {
    #[prost(bytes = "vec", tag = "1")]
    pub frontier: Vec<u8>,
    #[prost(bytes = "vec", repeated, tag = "2")]
    pub operations: Vec<Vec<u8>>,
    #[prost(bool, tag = "3")]
    pub has_more: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct SyncResponse {
    #[prost(bytes = "vec", tag = "1")]
    pub frontier: Vec<u8>,
    #[prost(bytes = "vec", repeated, tag = "2")]
    pub operations: Vec<Vec<u8>>,
    #[prost(bool, tag = "3")]
    pub has_more: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct ChunkStreamRequest {
    #[prost(bytes = "vec", tag = "1")]
    pub transfer_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub manifest_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "3")]
    pub chunk_id: Vec<u8>,
    #[prost(uint32, tag = "4")]
    pub logical_size: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct ChunkStreamResponse {
    #[prost(bool, tag = "1")]
    pub available: bool,
    #[prost(bytes = "vec", tag = "2")]
    pub transfer_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "3")]
    pub chunk_id: Vec<u8>,
    #[prost(uint32, tag = "4")]
    pub encrypted_size: u32,
}

pub async fn write_message<M: Message>(
    send: &mut SendStream,
    message: &M,
) -> Result<(), ProtocolError> {
    let encoded_len = message.encoded_len();
    if encoded_len > MAX_CONTROL_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge(encoded_len));
    }
    let encoded_len_u32 =
        u32::try_from(encoded_len).map_err(|_| ProtocolError::FrameTooLarge(encoded_len))?;
    send.write_all(&encoded_len_u32.to_be_bytes()).await?;
    let mut encoded = Vec::with_capacity(encoded_len);
    message.encode(&mut encoded)?;
    send.write_all(&encoded).await?;
    Ok(())
}

pub async fn read_message<M: Message + Default>(recv: &mut RecvStream) -> Result<M, ProtocolError> {
    read_message_bounded(recv, MAX_CONTROL_FRAME_BYTES).await
}

pub async fn read_message_bounded<M: Message + Default>(
    recv: &mut RecvStream,
    maximum_bytes: usize,
) -> Result<M, ProtocolError> {
    let mut length = [0; 4];
    recv.read_exact(&mut length).await?;
    let length = usize::try_from(u32::from_be_bytes(length))
        .map_err(|_| ProtocolError::FrameTooLarge(usize::MAX))?;
    if length > maximum_bytes {
        return Err(ProtocolError::FrameTooLarge(length));
    }
    let mut encoded = vec![0; length];
    recv.read_exact(&mut encoded).await?;
    M::decode(encoded.as_slice()).map_err(ProtocolError::Decode)
}

pub fn validate_batch(frontier: &[u8], operations: &[Vec<u8>]) -> Result<(), ProtocolError> {
    if frontier.len() > MAX_FRONTIER_BYTES {
        return Err(ProtocolError::FrontierTooLarge(frontier.len()));
    }
    if operations.len() > MAX_BATCH_OPERATIONS {
        return Err(ProtocolError::TooManyOperations(operations.len()));
    }
    let total = operations
        .iter()
        .try_fold(0_usize, |total, operation| {
            total.checked_add(operation.len())
        })
        .ok_or(ProtocolError::BatchTooLarge(usize::MAX))?;
    if total > MAX_CONTROL_FRAME_BYTES {
        return Err(ProtocolError::BatchTooLarge(total));
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("control frame is too large ({0} bytes)")]
    FrameTooLarge(usize),
    #[error("frontier is too large ({0} bytes)")]
    FrontierTooLarge(usize),
    #[error("batch contains too many operations ({0})")]
    TooManyOperations(usize),
    #[error("operation batch is too large ({0} bytes)")]
    BatchTooLarge(usize),
    #[error("could not encode a control frame: {0}")]
    Encode(#[from] prost::EncodeError),
    #[error("could not decode a control frame: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error("could not write a control frame: {0}")]
    Write(#[from] quinn::WriteError),
    #[error("could not read a control frame: {0}")]
    Read(#[from] quinn::ReadExactError),
}
