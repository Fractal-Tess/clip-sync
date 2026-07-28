//! Backend-neutral clipboard monitoring trait.
//!
//! This trait defines the interface that a display-server-specific backend
//! must implement. The daemon owns a `Box<dyn ClipboardBackend>` and drives
//! it without knowing whether the underlying protocol is ext-data-control,
//! wlr-data-control, or something else entirely.

use async_trait::async_trait;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use super::types::{
    ClipboardContent, CurrentClipboardInspection, FeedbackMarker, Generation, OfferMimeList,
    ProbeResult, RejectReason, SelectionKind,
};

/// A clipboard event emitted by the backend to the daemon.
#[derive(Clone, Debug)]
pub enum ClipboardEvent {
    /// A new selection was advertised by the compositor.
    NewOffer {
        generation: Generation,
        kind: SelectionKind,
        mime_list: OfferMimeList,
    },
    /// A regular clipboard offer was captured successfully.
    Captured {
        generation: Generation,
        kind: SelectionKind,
        content: ClipboardContent,
    },
    /// A regular clipboard offer was deliberately not captured.
    CaptureRejected {
        generation: Generation,
        kind: SelectionKind,
        reason: RejectReason,
    },
    /// The compositor echoed a daemon-owned source after we set clipboard
    /// content. This is emitted at most once per feedback marker.
    OwnContent {
        generation: Generation,
        kind: SelectionKind,
        marker: FeedbackMarker,
        mime_list: OfferMimeList,
    },
    /// The regular clipboard selection was cleared.
    Cleared {
        generation: Generation,
        kind: SelectionKind,
    },
    /// The compositor destroyed the data-control device (compositor restart,
    /// seat removal, etc.).
    Finished,
}

/// Errors that can occur during clipboard backend operations.
#[derive(Debug, Error)]
pub enum BackendError {
    #[error("no Wayland display connection available")]
    NoDisplay,
    #[error("Wayland connection failed: {0}")]
    Connection(String),
    #[error("compositor does not support any data-control protocol")]
    NoDataControl,
    #[error("no wl_seat global found")]
    NoSeat,
    #[error("Wayland protocol error: {0}")]
    Protocol(String),
    #[error("clipboard watcher is already running")]
    WatchAlreadyRunning,
    #[error("clipboard watcher is not running")]
    WatchNotRunning,
    #[error("clipboard command failed: {0}")]
    ClipboardCommand(String),
    #[error("the current clipboard offer is unavailable")]
    CurrentOfferUnavailable,
    #[error("the current clipboard changed during explicit inspection")]
    CurrentOfferChanged,
}

/// Backend-neutral interface for clipboard monitoring.
///
/// Implementations connect to the display server, probe for capabilities,
/// and run an event loop that emits [`ClipboardEvent`]s.
#[async_trait]
pub trait ClipboardBackend: Send + Sync {
    /// Probes the compositor for data-control protocol support.
    ///
    /// This performs a blocking Wayland round-trip on a dedicated thread to
    /// avoid tying up the async runtime. It is expected to be called once
    /// during daemon startup.
    async fn probe(&self) -> Result<ProbeResult, BackendError>;

    /// Starts monitoring regular clipboard changes.
    ///
    /// The returned future runs until the cancellation token is triggered or
    /// the compositor sends a `finished` event. Each new selection is
    /// delivered via the provided callback.
    async fn watch(
        &self,
        shutdown: CancellationToken,
        on_event: Box<dyn Fn(ClipboardEvent) + Send + Sync>,
    ) -> Result<(), BackendError>;

    /// Sets daemon-owned regular clipboard content through the active watcher.
    ///
    /// The returned marker is also advertised as an internal MIME type so the
    /// watcher can suppress its own compositor echo.
    async fn set_clipboard_content(
        &self,
        content: ClipboardContent,
    ) -> Result<FeedbackMarker, BackendError>;

    /// Streams the current offer without retaining payload bytes to determine
    /// its exact aggregate size before explicit-share confirmation.
    async fn inspect_current_clipboard(
        &self,
        maximum_bytes: u64,
    ) -> Result<CurrentClipboardInspection, BackendError>;

    /// Re-reads the exact inspected generation after confirmation.
    async fn capture_current_clipboard(
        &self,
        inspection: &CurrentClipboardInspection,
    ) -> Result<ClipboardContent, BackendError>;
}
