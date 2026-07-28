//! Deterministic, transport- and storage-independent replication model.
//!
//! Clipboard bytes are never formatted by this module. Payload `Debug` output
//! is descriptor-only, and callers must opt in explicitly to access bytes.

mod clock;
mod content;
mod identity;
mod operation;
mod projection;
mod seen_ops;

pub use clock::{EventKey, HlcError, HlcTimestamp, HybridLogicalClock};
pub use content::{
    ContentError, ContentId, ContentIdParseError, Payload, PayloadDescriptor, Representation,
    RepresentationDescriptor,
};
pub use identity::{NodeId, OpId, OpIdError};
pub use operation::{Operation, SettingValue, StampedOperation};
pub use projection::{ApplyOutcome, ContentView, Projection, ProjectionError};
pub use seen_ops::SeenOps;
