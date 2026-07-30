use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum IpcError {
    #[error("another clip-sync daemon is already listening")]
    AlreadyRunning,
    #[error("IPC socket path has no parent")]
    MissingSocketParent,
    #[error("IPC socket parent is not an owned private directory")]
    UnsafeSocketParent,
    #[error("refusing to replace non-socket IPC path {0:?}")]
    SocketPathNotSocket(PathBuf),
    #[error("daemon closed the IPC connection without responding")]
    ConnectionClosed,
    #[error("daemon did not respond within the local IPC timeout")]
    Timeout,
    #[error("daemon response protocol mismatch: expected {expected}, got {actual}")]
    ResponseProtocol { expected: u32, actual: u32 },
    #[error("daemon response request ID mismatch: expected {expected}, got {actual}")]
    ResponseRequestId { expected: u64, actual: u64 },
    #[error("IPC I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid IPC message: {0}")]
    Protocol(#[from] prost::DecodeError),
    #[error("could not encode IPC message: {0}")]
    Encode(#[from] prost::EncodeError),
}
