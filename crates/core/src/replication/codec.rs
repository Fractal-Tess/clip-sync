//! Protobuf encoding of operations, shared by storage and the network.
//!
//! One canonical encoding means a peer is sent exactly the bytes that are
//! stored, and a relayed operation compares equal byte for byte with the
//! original. Payload bytes are carried as length-delimited fields, so
//! decoding costs about as much memory as the payload itself.

use prost::{Message, Oneof};
use thiserror::Error;
use uuid::Uuid;

use crate::model::{
    ContentId, FileEntry, HlcTimestamp, NodeId, OpId, Operation, Payload, Projection,
    ProjectionError, Reference, Representation, RepresentationDescriptor, StampedOperation,
};

/// Encodes an operation canonically.
#[must_use]
pub fn encode_operation(operation: &StampedOperation) -> Vec<u8> {
    OperationMessage::from(operation).encode_to_vec()
}

/// Decodes and structurally validates an operation.
///
/// # Errors
///
/// Returns an error for malformed bytes, invalid identities, or an operation
/// that fails projection validation.
pub fn decode_operation(bytes: &[u8]) -> Result<StampedOperation, CodecError> {
    let message = OperationMessage::decode(bytes)?;
    let operation = StampedOperation::try_from(message)?;
    Projection::validate_operation(&operation)?;
    Ok(operation)
}

#[derive(Debug, Error)]
pub enum CodecError {
    #[error("operation bytes are not valid Protobuf: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error("operation is malformed: {0}")]
    Malformed(&'static str),
    #[error("operation payload is invalid: {0}")]
    Payload(#[from] crate::model::ContentError),
    #[error("operation is invalid: {0}")]
    InvalidOperation(#[from] ProjectionError),
}

#[derive(Clone, PartialEq, Message)]
struct OperationMessage {
    #[prost(bytes = "vec", tag = "1")]
    node: Vec<u8>,
    #[prost(uint64, tag = "2")]
    counter: u64,
    #[prost(uint64, tag = "3")]
    physical_millis: u64,
    #[prost(uint32, tag = "4")]
    logical: u32,
    #[prost(oneof = "Body", tags = "10, 11, 12, 13, 14, 15, 16")]
    body: Option<Body>,
}

#[derive(Clone, PartialEq, Oneof)]
enum Body {
    #[prost(message, tag = "10")]
    Add(AddMessage),
    #[prost(message, tag = "11")]
    AddReference(ReferenceMessage),
    #[prost(bytes, tag = "12")]
    Touch(Vec<u8>),
    #[prost(bytes, tag = "13")]
    Delete(Vec<u8>),
    #[prost(message, tag = "14")]
    SetPin(SetPinMessage),
    #[prost(bytes, tag = "15")]
    ForgetDevice(Vec<u8>),
    #[prost(message, tag = "16")]
    Retired(RetiredMessage),
}

#[derive(Clone, PartialEq, Message)]
struct AddMessage {
    #[prost(bytes = "vec", tag = "1")]
    content_id: Vec<u8>,
    #[prost(message, repeated, tag = "2")]
    representations: Vec<RepresentationMessage>,
}

#[derive(Clone, PartialEq, Message)]
struct RepresentationMessage {
    #[prost(string, tag = "1")]
    mime: String,
    #[prost(bytes = "vec", tag = "2")]
    bytes: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
struct ReferenceMessage {
    #[prost(bytes = "vec", tag = "1")]
    content_id: Vec<u8>,
    #[prost(message, repeated, tag = "2")]
    files: Vec<FileEntryMessage>,
    #[prost(message, repeated, tag = "3")]
    data: Vec<DescriptorMessage>,
}

#[derive(Clone, PartialEq, Message)]
struct FileEntryMessage {
    #[prost(string, tag = "1")]
    path: String,
    #[prost(bool, tag = "2")]
    directory: bool,
    #[prost(bool, tag = "3")]
    executable: bool,
    #[prost(uint64, tag = "4")]
    size: u64,
}

#[derive(Clone, PartialEq, Message)]
struct DescriptorMessage {
    #[prost(string, tag = "1")]
    mime: String,
    #[prost(uint64, tag = "2")]
    byte_len: u64,
}

#[derive(Clone, PartialEq, Message)]
struct SetPinMessage {
    #[prost(bytes = "vec", tag = "1")]
    content_id: Vec<u8>,
    #[prost(bool, tag = "2")]
    pinned: bool,
}

#[derive(Clone, PartialEq, Message)]
struct RetiredMessage {}

impl From<&StampedOperation> for OperationMessage {
    fn from(stamped: &StampedOperation) -> Self {
        let id = |content_id: &ContentId| content_id.as_bytes().to_vec();
        let body = match stamped.operation() {
            Operation::Add {
                content_id,
                payload,
            } => Body::Add(AddMessage {
                content_id: id(content_id),
                representations: payload
                    .representations()
                    .iter()
                    .map(|representation| RepresentationMessage {
                        mime: representation.mime().to_owned(),
                        bytes: representation.bytes().to_vec(),
                    })
                    .collect(),
            }),
            Operation::AddReference {
                content_id,
                reference,
            } => {
                let (files, data) = match reference {
                    Reference::Files(entries) => (
                        entries
                            .iter()
                            .map(|entry| FileEntryMessage {
                                path: entry.path.clone(),
                                directory: entry.directory,
                                executable: entry.executable,
                                size: entry.size,
                            })
                            .collect(),
                        Vec::new(),
                    ),
                    Reference::Data(representations) => (
                        Vec::new(),
                        representations
                            .iter()
                            .map(|representation| DescriptorMessage {
                                mime: representation.mime().to_owned(),
                                byte_len: representation.byte_len(),
                            })
                            .collect(),
                    ),
                };
                Body::AddReference(ReferenceMessage {
                    content_id: id(content_id),
                    files,
                    data,
                })
            }
            Operation::Touch { content_id } => Body::Touch(id(content_id)),
            Operation::Delete { content_id } => Body::Delete(id(content_id)),
            Operation::SetPin { content_id, pinned } => Body::SetPin(SetPinMessage {
                content_id: id(content_id),
                pinned: *pinned,
            }),
            Operation::ForgetDevice { node_id } => {
                Body::ForgetDevice(node_id.as_uuid().as_bytes().to_vec())
            }
            Operation::Retired => Body::Retired(RetiredMessage {}),
        };
        Self {
            node: stamped.id().node().as_uuid().as_bytes().to_vec(),
            counter: stamped.id().counter(),
            physical_millis: stamped.timestamp().physical_millis(),
            logical: stamped.timestamp().logical(),
            body: Some(body),
        }
    }
}

impl TryFrom<OperationMessage> for StampedOperation {
    type Error = CodecError;

    fn try_from(message: OperationMessage) -> Result<Self, CodecError> {
        let id = OpId::new(node_id(&message.node)?, message.counter)
            .map_err(|_| CodecError::Malformed("operation counter"))?;
        let timestamp = HlcTimestamp::new(message.physical_millis, message.logical);
        let operation = match message
            .body
            .ok_or(CodecError::Malformed("operation body"))?
        {
            Body::Add(add) => {
                let content_id = content_id(&add.content_id)?;
                let representations = add
                    .representations
                    .into_iter()
                    .map(|representation| {
                        Representation::new(representation.mime, representation.bytes)
                    })
                    .collect();
                Operation::Add {
                    content_id,
                    payload: Payload::from_parts(content_id, representations)?,
                }
            }
            Body::AddReference(reference) => {
                let kind = match (reference.files.is_empty(), reference.data.is_empty()) {
                    (false, true) => Reference::Files(
                        reference
                            .files
                            .into_iter()
                            .map(|entry| FileEntry {
                                path: entry.path,
                                directory: entry.directory,
                                executable: entry.executable,
                                size: entry.size,
                            })
                            .collect(),
                    ),
                    (true, false) => Reference::Data(
                        reference
                            .data
                            .into_iter()
                            .map(|entry| RepresentationDescriptor::new(entry.mime, entry.byte_len))
                            .collect(),
                    ),
                    _ => return Err(CodecError::Malformed("reference must be files or data")),
                };
                Operation::AddReference {
                    content_id: content_id(&reference.content_id)?,
                    reference: kind,
                }
            }
            Body::Touch(bytes) => Operation::Touch {
                content_id: content_id(&bytes)?,
            },
            Body::Delete(bytes) => Operation::Delete {
                content_id: content_id(&bytes)?,
            },
            Body::SetPin(pin) => Operation::SetPin {
                content_id: content_id(&pin.content_id)?,
                pinned: pin.pinned,
            },
            Body::ForgetDevice(bytes) => Operation::ForgetDevice {
                node_id: node_id(&bytes)?,
            },
            Body::Retired(RetiredMessage {}) => Operation::Retired,
        };
        Ok(Self::new(id, timestamp, operation))
    }
}

fn node_id(bytes: &[u8]) -> Result<NodeId, CodecError> {
    Uuid::from_slice(bytes)
        .map(NodeId::from_uuid)
        .map_err(|_| CodecError::Malformed("node ID"))
}

fn content_id(bytes: &[u8]) -> Result<ContentId, CodecError> {
    bytes
        .try_into()
        .map(ContentId::from_bytes)
        .map_err(|_| CodecError::Malformed("content ID"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 32] = [4; 32];

    fn stamped(operation: Operation) -> StampedOperation {
        StampedOperation::new(
            OpId::new(NodeId::from_uuid(Uuid::from_u128(7)), 3).unwrap(),
            HlcTimestamp::new(1_700_000_000_000, 2),
            operation,
        )
    }

    fn round_trip(operation: Operation) {
        let original = stamped(operation);
        let encoded = encode_operation(&original);
        let decoded = decode_operation(&encoded).unwrap();
        assert_eq!(decoded, original);
        assert_eq!(encode_operation(&decoded), encoded, "encoding is canonical");
    }

    #[test]
    fn every_operation_kind_round_trips_canonically() {
        let payload = Payload::new(
            &KEY,
            vec![
                Representation::new("text/plain", b"hi".to_vec()),
                Representation::new("image/png", vec![0, 255, 1]),
            ],
        )
        .unwrap();
        let content_id = payload.descriptor().content_id();
        round_trip(Operation::Add {
            content_id,
            payload,
        });
        round_trip(Operation::AddReference {
            content_id,
            reference: Reference::Files(vec![
                FileEntry {
                    path: "album".to_owned(),
                    directory: true,
                    executable: false,
                    size: 0,
                },
                FileEntry {
                    path: "album/a.jpg".to_owned(),
                    directory: false,
                    executable: false,
                    size: 5_000_000_000,
                },
            ]),
        });
        round_trip(Operation::AddReference {
            content_id,
            reference: Reference::Data(vec![RepresentationDescriptor::new("image/png", 9)]),
        });
        round_trip(Operation::Touch { content_id });
        round_trip(Operation::Delete { content_id });
        round_trip(Operation::SetPin {
            content_id,
            pinned: true,
        });
        round_trip(Operation::ForgetDevice {
            node_id: NodeId::from_uuid(Uuid::from_u128(9)),
        });
        round_trip(Operation::Retired);
    }

    #[test]
    fn payload_bytes_are_stored_as_bytes() {
        let bytes = vec![0xab; 100_000];
        let payload = Payload::new(&KEY, vec![Representation::new("image/png", bytes)]).unwrap();
        let content_id = payload.descriptor().content_id();
        let encoded = encode_operation(&stamped(Operation::Add {
            content_id,
            payload,
        }));
        assert!(encoded.len() < 100_200, "{} bytes", encoded.len());
    }

    #[test]
    fn malformed_bytes_and_bodies_are_rejected() {
        assert!(decode_operation(&[0xff, 0xff, 0xff]).is_err());
        let mut message = OperationMessage::from(&stamped(Operation::Retired));
        message.body = None;
        assert!(decode_operation(&message.encode_to_vec()).is_err());
    }
}
