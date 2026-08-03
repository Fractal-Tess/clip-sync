use std::collections::BTreeSet;

use uuid::Uuid;

use clip_sync_core::model::{NodeId, SeenOps};

use super::super::protocol::{IdentityHello, PROTOCOL_VERSION, ProtocolHello};
use super::{
    MeshError,
    handshake::{parse_identity, validate_protocol},
};

fn hello() -> IdentityHello {
    IdentityHello {
        node_id: Uuid::from_u128(7).as_bytes().to_vec(),
        hostname: "node".to_owned(),
        frontier: serde_json::to_vec(&SeenOps::default()).unwrap(),
        known_members: serde_json::to_vec(&BTreeSet::<NodeId>::new()).unwrap(),
    }
}

#[test]
fn rolling_protocol_version_mismatch_is_rejected_before_session_state() {
    assert!(matches!(
        validate_protocol(&ProtocolHello {
            minimum_version: PROTOCOL_VERSION - 1,
            maximum_version: PROTOCOL_VERSION - 1,
        }),
        Err(MeshError::UnsupportedProtocol { minimum, maximum })
            if minimum == PROTOCOL_VERSION - 1 && maximum == PROTOCOL_VERSION - 1
    ));
    assert!(matches!(
        validate_protocol(&ProtocolHello {
            minimum_version: PROTOCOL_VERSION + 1,
            maximum_version: PROTOCOL_VERSION + 1,
        }),
        Err(MeshError::UnsupportedProtocol { minimum, maximum })
            if minimum == PROTOCOL_VERSION + 1 && maximum == PROTOCOL_VERSION + 1
    ));
}

#[test]
fn malformed_membership_advertisement_is_rejected() {
    let mut malformed = hello();
    malformed.known_members = b"not-json".to_vec();
    assert!(matches!(
        parse_identity(malformed),
        Err(MeshError::Membership(_))
    ));
}
