//! Decoder for operations stored as JSON by 0.3, used once to convert them.
//!
//! Every device converts the same operation to the same new operation, so
//! converted copies still compare equal byte for byte across the mesh.

use serde::Deserialize;

use crate::model::{ContentId, HlcTimestamp, NodeId, OpId, Operation, Payload, StampedOperation};

#[derive(Deserialize)]
struct LegacyStamped {
    id: OpId,
    timestamp: HlcTimestamp,
    operation: LegacyOperation,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum LegacyOperation {
    Add {
        content_id: ContentId,
        payload: Payload,
    },
    /// An explicit share larger than the quota. Quota exemption is gone, but
    /// the item is kept as an ordinary copy.
    AddQuotaExempt {
        content_id: ContentId,
        payload: Payload,
    },
    Touch {
        content_id: ContentId,
    },
    Delete {
        content_id: ContentId,
    },
    SetPin {
        content_id: ContentId,
        pinned: bool,
    },
    ForgetDevice {
        node_id: NodeId,
    },
    /// Replicated settings and manifest shares (begin, complete, cancel).
    #[serde(other)]
    Retired,
}

/// Converts one 0.3 JSON operation into its current form.
///
/// # Errors
///
/// Returns the JSON error for bytes that are not a 0.3 operation.
pub(super) fn convert(encoded: &[u8]) -> serde_json::Result<StampedOperation> {
    let legacy: LegacyStamped = serde_json::from_slice(encoded)?;
    let operation = match legacy.operation {
        LegacyOperation::Add {
            content_id,
            payload,
        }
        | LegacyOperation::AddQuotaExempt {
            content_id,
            payload,
        } => Operation::Add {
            content_id,
            payload,
        },
        LegacyOperation::Touch { content_id } => Operation::Touch { content_id },
        LegacyOperation::Delete { content_id } => Operation::Delete { content_id },
        LegacyOperation::SetPin { content_id, pinned } => Operation::SetPin { content_id, pinned },
        LegacyOperation::ForgetDevice { node_id } => Operation::ForgetDevice { node_id },
        LegacyOperation::Retired => Operation::Retired,
    };
    Ok(StampedOperation::new(
        legacy.id,
        legacy.timestamp,
        operation,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NODE: &str = "00000000-0000-0000-0000-000000000007";

    fn stamped(operation: &str) -> Vec<u8> {
        format!(
            r#"{{"id":{{"node":"{NODE}","counter":4}},"timestamp":{{"physical_millis":10,"logical":1}},"operation":{operation}}}"#
        )
        .into_bytes()
    }

    #[test]
    fn settings_and_shares_become_retired() {
        for operation in [
            r#"{"type":"set_setting","key":"mesh_quota_bytes","value":{"type":"unsigned","value":5}}"#,
            r#"{"type":"cancel_share","transfer_id":"x","content_id":"y","manifest_id":"z"}"#,
        ] {
            let converted = convert(&stamped(operation)).unwrap();
            assert_eq!(converted.operation(), &Operation::Retired);
            assert_eq!(converted.id().counter(), 4);
            assert_eq!(converted.timestamp(), HlcTimestamp::new(10, 1));
        }
    }

    #[test]
    fn content_operations_keep_their_meaning() {
        let content_id = "ab".repeat(32);
        let converted = convert(&stamped(&format!(
            r#"{{"type":"set_pin","content_id":"{content_id}","pinned":true}}"#
        )))
        .unwrap();
        assert_eq!(
            converted.operation(),
            &Operation::SetPin {
                content_id: content_id.parse().unwrap(),
                pinned: true
            }
        );
    }
}
