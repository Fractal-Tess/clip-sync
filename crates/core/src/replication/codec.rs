//! Versioned codec boundary for operation serialization.
//!
//! The [`Codec`] trait abstracts encoding so that the anti-entropy state
//! machine is independent of the wire format. [`JsonV1Codec`] wraps
//! `serde_json` inside a versioned [`Envelope`] for forward compatibility.
//! A Protobuf backend can be added later by implementing the same trait.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::model::{Projection, StampedOperation};

/// Version tag for the current JSON encoding.
const JSON_V1: u32 = 1;

/// Encode/decode boundary for replication messages.
///
/// Implementors must produce deterministic output for the same input so that
/// content-integrity checks in the [`super::OpLog`] work correctly.
pub trait Codec: Send + Sync {
    /// Serialize a stamped operation into raw bytes.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] if serialization fails.
    fn encode_op(&self, op: &StampedOperation) -> Result<Vec<u8>, CodecError>;

    /// Deserialize raw bytes into a stamped operation.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] if the bytes are malformed or use an
    /// unsupported version.
    fn decode_op(&self, bytes: &[u8]) -> Result<StampedOperation, CodecError>;
}

/// Versioned wrapper so that peers can detect format changes before
/// attempting deserialization of the inner payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope<T> {
    /// Format version. Only `1` is supported today.
    pub v: u32,
    /// Inner payload.
    pub d: T,
}

/// JSON v1 codec that wraps [`StampedOperation`] in an [`Envelope`].
#[derive(Clone, Copy, Debug, Default)]
pub struct JsonV1Codec;

impl Codec for JsonV1Codec {
    fn encode_op(&self, op: &StampedOperation) -> Result<Vec<u8>, CodecError> {
        let envelope = Envelope { v: JSON_V1, d: op };
        serde_json::to_vec(&envelope).map_err(CodecError::Serialize)
    }

    fn decode_op(&self, bytes: &[u8]) -> Result<StampedOperation, CodecError> {
        let envelope: Envelope<StampedOperation> =
            serde_json::from_slice(bytes).map_err(CodecError::Deserialize)?;
        if envelope.v != JSON_V1 {
            return Err(CodecError::UnsupportedVersion(envelope.v));
        }
        Projection::validate_operation(&envelope.d).map_err(CodecError::InvalidOperation)?;
        Ok(envelope.d)
    }
}

#[derive(Debug, Error)]
pub enum CodecError {
    #[error("serialization failed: {0}")]
    Serialize(#[source] serde_json::Error),
    #[error("deserialization failed: {0}")]
    Deserialize(#[source] serde_json::Error),
    #[error("unsupported envelope version {0}")]
    UnsupportedVersion(u32),
    #[error("serialized operation violates model invariants: {0}")]
    InvalidOperation(#[source] crate::model::ProjectionError),
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::model::{HlcTimestamp, NodeId, OpId, Operation, Payload, Representation};

    const KEY: [u8; 32] = [7; 32];

    fn sample_op() -> StampedOperation {
        let node = NodeId::from_uuid(Uuid::from_u128(1));
        let id = OpId::new(node, 1).unwrap();
        let ts = HlcTimestamp::new(1000, 0);
        let payload = Payload::new(&KEY, vec![Representation::new("text/plain", b"hello")])
            .expect("valid payload");
        let content_id = payload.descriptor().content_id();
        StampedOperation::new(
            id,
            ts,
            Operation::Add {
                content_id,
                payload,
            },
        )
    }

    #[test]
    fn round_trip_preserves_operation() {
        let codec = JsonV1Codec;
        let op = sample_op();
        let bytes = codec.encode_op(&op).unwrap();
        let decoded = codec.decode_op(&bytes).unwrap();
        assert_eq!(decoded, op);
    }

    #[test]
    fn deterministic_encoding() {
        let codec = JsonV1Codec;
        let op = sample_op();
        let first = codec.encode_op(&op).unwrap();
        let second = codec.encode_op(&op).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn rejects_unsupported_version() {
        let codec = JsonV1Codec;
        let op = sample_op();
        let valid_bytes = codec.encode_op(&op).unwrap();
        let mut parsed: serde_json::Value = serde_json::from_slice(&valid_bytes).unwrap();
        parsed["v"] = serde_json::json!(99);
        let tampered = serde_json::to_vec(&parsed).unwrap();

        let err = codec.decode_op(&tampered).unwrap_err();
        assert!(matches!(err, CodecError::UnsupportedVersion(99)));
    }

    #[test]
    fn rejects_garbage_bytes() {
        let codec = JsonV1Codec;
        assert!(codec.decode_op(b"not json").is_err());
    }

    #[test]
    fn rejects_operation_with_zero_counter() {
        let codec = JsonV1Codec;
        let bytes = codec.encode_op(&sample_op()).unwrap();
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["d"]["id"]["counter"] = serde_json::json!(0);
        assert!(
            codec
                .decode_op(&serde_json::to_vec(&value).unwrap())
                .is_err()
        );
    }

    #[test]
    fn rejects_structurally_malformed_payload() {
        let codec = JsonV1Codec;
        let bytes = codec.encode_op(&sample_op()).unwrap();
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["d"]["operation"]["payload"]["representations"][0]["mime"] = serde_json::json!("");
        assert!(matches!(
            codec.decode_op(&serde_json::to_vec(&value).unwrap()),
            Err(CodecError::InvalidOperation(_))
        ));
    }
}
