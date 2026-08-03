use prost::Message;
use thiserror::Error;
use uuid::Uuid;

use crate::payload::{ChunkId, ManifestId};

use super::TransferId;

pub const TRANSFER_PROTOCOL_VERSION: u32 = 1;
pub const MAX_CHUNKS_PER_REQUEST: usize = 256;
pub const MAX_TRANSFER_CONTROL_BYTES: usize = 64 * 1024;

/// Versioned bounded control-plane envelope. Chunk object bytes belong on a
/// dedicated QUIC stream and are deliberately absent from this type.
#[derive(Clone, PartialEq, Message)]
pub struct TransferControl {
    #[prost(uint32, tag = "1")]
    pub version: u32,
    #[prost(oneof = "transfer_control::Kind", tags = "2, 3, 4, 5, 6")]
    pub kind: Option<transfer_control::Kind>,
}

pub mod transfer_control {
    use prost::Oneof;

    use super::{BeginTransfer, CancelTransfer, ChunkAck, ChunkRequest, CompleteTransfer};

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Kind {
        #[prost(message, tag = "2")]
        Begin(BeginTransfer),
        #[prost(message, tag = "3")]
        Request(ChunkRequest),
        #[prost(message, tag = "4")]
        Ack(ChunkAck),
        #[prost(message, tag = "5")]
        Cancel(CancelTransfer),
        #[prost(message, tag = "6")]
        Complete(CompleteTransfer),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct BeginTransfer {
    #[prost(bytes = "vec", tag = "1")]
    pub transfer_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub manifest_id: Vec<u8>,
    #[prost(uint64, tag = "3")]
    pub logical_size: u64,
    #[prost(uint32, tag = "4")]
    pub unique_chunk_count: u32,
    #[prost(bool, tag = "5")]
    pub quota_exempt: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct ChunkRequest {
    #[prost(bytes = "vec", tag = "1")]
    pub transfer_id: Vec<u8>,
    #[prost(bytes = "vec", repeated, tag = "2")]
    pub chunk_ids: Vec<Vec<u8>>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ChunkAck {
    #[prost(bytes = "vec", tag = "1")]
    pub transfer_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub chunk_id: Vec<u8>,
    #[prost(uint32, tag = "3")]
    pub logical_size: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct CancelTransfer {
    #[prost(bytes = "vec", tag = "1")]
    pub transfer_id: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct CompleteTransfer {
    #[prost(bytes = "vec", tag = "1")]
    pub transfer_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub manifest_id: Vec<u8>,
}

impl TransferControl {
    #[must_use]
    pub fn begin(
        transfer_id: TransferId,
        manifest_id: ManifestId,
        logical_size: u64,
        unique_chunk_count: u32,
        quota_exempt: bool,
    ) -> Self {
        Self {
            version: TRANSFER_PROTOCOL_VERSION,
            kind: Some(transfer_control::Kind::Begin(BeginTransfer {
                transfer_id: transfer_id.as_uuid().as_bytes().to_vec(),
                manifest_id: manifest_id.as_bytes().to_vec(),
                logical_size,
                unique_chunk_count,
                quota_exempt,
            })),
        }
    }

    #[must_use]
    pub fn request(transfer_id: TransferId, chunk_ids: &[ChunkId]) -> Self {
        Self {
            version: TRANSFER_PROTOCOL_VERSION,
            kind: Some(transfer_control::Kind::Request(ChunkRequest {
                transfer_id: transfer_id.as_uuid().as_bytes().to_vec(),
                chunk_ids: chunk_ids.iter().map(|id| id.as_bytes().to_vec()).collect(),
            })),
        }
    }

    #[must_use]
    pub fn cancel(transfer_id: TransferId) -> Self {
        Self {
            version: TRANSFER_PROTOCOL_VERSION,
            kind: Some(transfer_control::Kind::Cancel(CancelTransfer {
                transfer_id: transfer_id.as_uuid().as_bytes().to_vec(),
            })),
        }
    }

    /// Decodes only after applying a hard envelope-size bound.
    ///
    /// # Errors
    ///
    /// Returns a typed error for oversized, malformed, unsupported, or
    /// semantically invalid messages.
    pub fn decode_bounded(bytes: &[u8]) -> Result<Self, TransferProtocolError> {
        if bytes.len() > MAX_TRANSFER_CONTROL_BYTES {
            return Err(TransferProtocolError::MessageTooLarge);
        }
        let message = Self::decode(bytes).map_err(TransferProtocolError::Decode)?;
        message.validate()?;
        Ok(message)
    }

    /// Validates IDs, nonzero fields, request fanout, and protocol version.
    ///
    /// # Errors
    ///
    /// Returns a typed protocol validation error.
    pub fn validate(&self) -> Result<(), TransferProtocolError> {
        if self.version != TRANSFER_PROTOCOL_VERSION {
            return Err(TransferProtocolError::UnsupportedVersion(self.version));
        }
        let kind = self
            .kind
            .as_ref()
            .ok_or(TransferProtocolError::MissingKind)?;
        match kind {
            transfer_control::Kind::Begin(begin) => {
                parse_transfer_id(&begin.transfer_id)?;
                parse_manifest_id(&begin.manifest_id)?;
                if begin.logical_size == 0 || begin.unique_chunk_count == 0 {
                    return Err(TransferProtocolError::InvalidField);
                }
            }
            transfer_control::Kind::Request(request) => {
                parse_transfer_id(&request.transfer_id)?;
                if request.chunk_ids.is_empty() || request.chunk_ids.len() > MAX_CHUNKS_PER_REQUEST
                {
                    return Err(TransferProtocolError::InvalidChunkRequest);
                }
                for id in &request.chunk_ids {
                    parse_chunk_id(id)?;
                }
            }
            transfer_control::Kind::Ack(ack) => {
                parse_transfer_id(&ack.transfer_id)?;
                parse_chunk_id(&ack.chunk_id)?;
                if ack.logical_size == 0 {
                    return Err(TransferProtocolError::InvalidField);
                }
            }
            transfer_control::Kind::Cancel(cancel) => {
                parse_transfer_id(&cancel.transfer_id)?;
            }
            transfer_control::Kind::Complete(complete) => {
                parse_transfer_id(&complete.transfer_id)?;
                parse_manifest_id(&complete.manifest_id)?;
            }
        }
        Ok(())
    }

    /// Encodes after validating and enforcing the control-plane bound.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid fields or excessive encoded size.
    pub fn encode_bounded(&self) -> Result<Vec<u8>, TransferProtocolError> {
        self.validate()?;
        if self.encoded_len() > MAX_TRANSFER_CONTROL_BYTES {
            return Err(TransferProtocolError::MessageTooLarge);
        }
        Ok(self.encode_to_vec())
    }
}

fn parse_transfer_id(bytes: &[u8]) -> Result<TransferId, TransferProtocolError> {
    let bytes: [u8; 16] = bytes
        .try_into()
        .map_err(|_| TransferProtocolError::InvalidIdentifier)?;
    Ok(TransferId::from_uuid(Uuid::from_bytes(bytes)))
}

fn parse_chunk_id(bytes: &[u8]) -> Result<ChunkId, TransferProtocolError> {
    hex::encode(bytes)
        .parse()
        .map_err(|_| TransferProtocolError::InvalidIdentifier)
}

fn parse_manifest_id(bytes: &[u8]) -> Result<ManifestId, TransferProtocolError> {
    hex::encode(bytes)
        .parse()
        .map_err(|_| TransferProtocolError::InvalidIdentifier)
}

#[derive(Debug, Error)]
pub enum TransferProtocolError {
    #[error("transfer control message exceeds its size limit")]
    MessageTooLarge,
    #[error("unsupported transfer protocol version {0}")]
    UnsupportedVersion(u32),
    #[error("transfer control message has no kind")]
    MissingKind,
    #[error("invalid transfer identifier")]
    InvalidIdentifier,
    #[error("invalid transfer control field")]
    InvalidField,
    #[error("chunk request is empty or exceeds its fanout limit")]
    InvalidChunkRequest,
    #[error("invalid protobuf transfer message: {0}")]
    Decode(prost::DecodeError),
}
