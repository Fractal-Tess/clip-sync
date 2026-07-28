//! Cancellable and resumable large-payload transfer primitives.

mod coordinator;
mod protocol;
mod state;

pub use coordinator::{
    ActivatedClipboard, TransferChunk, TransferCoordinator, TransferCoordinatorError,
    TransferProgress, operation_transfer_id,
};
pub use protocol::{
    BeginTransfer, CancelTransfer, ChunkAck, ChunkRequest, CompleteTransfer, TransferControl,
    TransferProtocolError,
};
pub use state::{
    PeerProgress, TransferError, TransferId, TransferPhase, TransferRecord, TransferStateLimits,
};
