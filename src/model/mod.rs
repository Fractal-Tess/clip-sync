//! Deterministic, transport- and storage-independent replication model.
//!
//! Clipboard bytes are never formatted by this module. Payload `Debug` output
//! is descriptor-only, and callers must opt in explicitly to access bytes.

mod clock;
mod content;
mod identity;
mod operation;
mod projection;
mod retention;
mod seen_ops;

pub use clock::{EventKey, HlcError, HlcTimestamp, HybridLogicalClock};
pub use content::{
    ContentError, ContentId, ContentIdParseError, Payload, PayloadDescriptor, Representation,
    RepresentationDescriptor,
};
pub use identity::{NodeId, OpId, OpIdError};
pub use operation::{
    DEFAULT_CAPTURE_THRESHOLD_BYTES, DEFAULT_MESH_QUOTA_BYTES, EffectiveSharedSettings, Operation,
    SettingValue, SharedSetting, StampedOperation,
};
pub use projection::{
    ApplyOutcome, ContentView, Projection, ProjectionError, QuotaPlan, TombstoneView,
};
pub use retention::Acknowledgements;
pub use seen_ops::SeenOps;
