//! Wayland data-control capability probe, capture, and ownership.
//!
//! Connects to the Wayland display, prefers `ext-data-control-v1`, falls back
//! to `zwlr-data-control-v1`, monitors only the regular clipboard selection,
//! and owns daemon-provided sources while the watcher is running.

use std::sync::{
    Arc, Mutex as StdMutex,
    atomic::{AtomicU64, Ordering},
};

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::backend::{BackendError, ClipboardBackend, ClipboardEvent};
use super::types::{
    ClipboardContent, CurrentClipboardInspection, FeedbackMarker, MAX_CAPTURE_BYTES, ProbeResult,
};

mod capture;
mod connection;
mod dispatch;
mod io;
mod protocol;
mod runtime;
mod state;

use protocol::probe_wayland_globals;
use runtime::{ClipboardCommand, run_wayland_watch};

/// Wayland-native clipboard backend.
///
/// Probes for data-control support by connecting to the compositor's Wayland
/// display. Capture and ownership use the protocol directly and never shell out
/// to external clipboard tools.
#[derive(Clone)]
pub struct WaylandBackend {
    commands: Arc<StdMutex<Option<mpsc::UnboundedSender<ClipboardCommand>>>>,
    capture_threshold: Arc<AtomicU64>,
}

impl WaylandBackend {
    #[must_use]
    pub fn new() -> Self {
        Self {
            commands: Arc::new(StdMutex::new(None)),
            capture_threshold: Arc::new(AtomicU64::new(MAX_CAPTURE_BYTES)),
        }
    }

    /// Applies the current replicated automatic-capture threshold. Captures
    /// already in flight retain the limit they started with.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero threshold.
    pub fn set_capture_threshold(&self, max_bytes: u64) -> Result<(), BackendError> {
        if max_bytes == 0 {
            return Err(BackendError::InvalidCaptureThreshold);
        }
        self.capture_threshold.store(max_bytes, Ordering::SeqCst);
        Ok(())
    }

    #[must_use]
    pub fn capture_threshold(&self) -> u64 {
        self.capture_threshold.load(Ordering::SeqCst)
    }

    fn active_sender(&self) -> Result<mpsc::UnboundedSender<ClipboardCommand>, BackendError> {
        self.commands
            .lock()
            .map_err(|_| BackendError::ClipboardCommand("watch command lock poisoned".into()))?
            .clone()
            .ok_or(BackendError::WatchNotRunning)
    }
}

#[async_trait]
impl ClipboardBackend for WaylandBackend {
    async fn probe(&self) -> Result<ProbeResult, BackendError> {
        // Wayland connection + registry round-trip is blocking, so run it on
        // a dedicated thread to avoid starving the Tokio runtime.
        tokio::task::spawn_blocking(probe_wayland_globals)
            .await
            .map_err(|join_err| BackendError::Connection(join_err.to_string()))?
    }

    async fn watch(
        &self,
        shutdown: CancellationToken,
        on_event: Box<dyn Fn(ClipboardEvent) + Send + Sync>,
    ) -> Result<(), BackendError> {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        {
            let mut active = self.commands.lock().map_err(|_| {
                BackendError::ClipboardCommand("watch command lock poisoned".into())
            })?;
            if active.is_some() {
                return Err(BackendError::WatchAlreadyRunning);
            }
            *active = Some(command_tx);
        }

        let (done_tx, done_rx) = oneshot::channel();
        let capture_threshold = self.capture_threshold.clone();
        let spawned = std::thread::Builder::new()
            .name("clip-sync-wayland".to_owned())
            .spawn(move || {
                let result = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| BackendError::Connection(error.to_string()))
                    .and_then(|runtime| {
                        runtime.block_on(run_wayland_watch(
                            shutdown,
                            command_rx,
                            on_event,
                            capture_threshold,
                        ))
                    });
                let _ = done_tx.send(result);
            });
        if let Err(error) = spawned {
            if let Ok(mut active) = self.commands.lock() {
                *active = None;
            }
            return Err(BackendError::Connection(error.to_string()));
        }

        let result = match done_rx.await {
            Ok(result) => result,
            Err(_) => Err(BackendError::Connection(
                "Wayland event thread stopped unexpectedly".to_owned(),
            )),
        };

        if let Ok(mut active) = self.commands.lock() {
            *active = None;
        }

        result
    }

    async fn set_clipboard_content(
        &self,
        content: ClipboardContent,
    ) -> Result<FeedbackMarker, BackendError> {
        let sender = self.active_sender()?;
        let (reply_tx, reply_rx) = oneshot::channel();
        sender
            .send(ClipboardCommand::SetContent {
                content,
                reply: reply_tx,
            })
            .map_err(|_| BackendError::WatchNotRunning)?;

        reply_rx.await.map_err(|_| BackendError::WatchNotRunning)?
    }

    async fn inspect_current_clipboard(
        &self,
        maximum_bytes: u64,
    ) -> Result<CurrentClipboardInspection, BackendError> {
        let sender = self.active_sender()?;
        let (reply_tx, reply_rx) = oneshot::channel();
        sender
            .send(ClipboardCommand::ReadCurrent {
                expected_generation: None,
                maximum_bytes,
                retain_bytes: false,
                reply: reply_tx,
            })
            .map_err(|_| BackendError::WatchNotRunning)?;
        let result = reply_rx
            .await
            .map_err(|_| BackendError::WatchNotRunning)??;
        Ok(CurrentClipboardInspection::new(
            result.generation,
            result.mime_list,
            result.logical_size,
        ))
    }

    async fn capture_current_clipboard(
        &self,
        inspection: &CurrentClipboardInspection,
    ) -> Result<ClipboardContent, BackendError> {
        let sender = self.active_sender()?;
        let (reply_tx, reply_rx) = oneshot::channel();
        sender
            .send(ClipboardCommand::ReadCurrent {
                expected_generation: Some(inspection.generation()),
                maximum_bytes: inspection.logical_size(),
                retain_bytes: true,
                reply: reply_tx,
            })
            .map_err(|_| BackendError::WatchNotRunning)?;
        let result = reply_rx
            .await
            .map_err(|_| BackendError::WatchNotRunning)??;
        let content =
            ClipboardContent::new_with_limit(result.representations, inspection.logical_size())
                .map_err(|error| BackendError::ClipboardCommand(error.to_string()))?;
        if content.total_bytes() != inspection.logical_size() {
            return Err(BackendError::CurrentOfferChanged);
        }
        Ok(content)
    }
}

impl Default for WaylandBackend {
    fn default() -> Self {
        Self::new()
    }
}
