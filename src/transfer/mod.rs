//! Cancellable and resumable large-payload transfer primitives.

mod protocol;
mod state;

pub use protocol::{
    BeginTransfer, CancelTransfer, ChunkAck, ChunkRequest, CompleteTransfer, TransferControl,
    TransferProtocolError,
};
pub use state::{
    PeerProgress, TransferError, TransferId, TransferPhase, TransferRecord, TransferStateLimits,
};
