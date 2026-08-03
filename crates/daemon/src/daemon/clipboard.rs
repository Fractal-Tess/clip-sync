use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use clip_sync_core::{
    clipboard::backend::{ClipboardBackend, ClipboardEvent},
    storage::HistoryStore,
    transfer::TransferCoordinator,
};

use crate::{ipc::DaemonState, mesh::MeshHandle};

use super::{
    capture::{AutomaticClipboardCaptureResult, capture_automatic_clipboard},
    runtime::unix_time_millis,
    views::history_items,
};

pub(super) async fn handle_clipboard_event(
    event: ClipboardEvent,
    history: &mut HistoryStore,
    state: &DaemonState,
    content_key: &[u8; 32],
    mesh: &MeshHandle,
    transfers: &mut TransferCoordinator,
) -> anyhow::Result<()> {
    match event {
        ClipboardEvent::Ready => {
            state
                .set_clipboard_status(true, "Wayland clipboard monitoring is active")
                .await;
        }
        ClipboardEvent::Captured { content, .. } => {
            let result = capture_automatic_clipboard(
                &content,
                content_key,
                transfers,
                history,
                mesh,
                unix_time_millis()?,
                &CancellationToken::new(),
            )
            .await?;
            state.set_history(history_items(history.replica())).await;
            match result {
                AutomaticClipboardCaptureResult::Payload { .. } => {
                    tracing::debug!(
                        history_entries = history.projection().visible_items().len(),
                        "captured clipboard history entry"
                    );
                }
                AutomaticClipboardCaptureResult::Files { transfer_id, .. } => {
                    tracing::debug!(
                        %transfer_id,
                        history_entries = history.projection().visible_items().len(),
                        "captured clipboard file snapshot"
                    );
                }
                AutomaticClipboardCaptureResult::RejectedFiles => {
                    tracing::debug!("clipboard file offer was not captured");
                }
            }
        }
        ClipboardEvent::CaptureRejected { reason, .. } => {
            tracing::debug!(?reason, "clipboard offer was not captured");
        }
        ClipboardEvent::Finished => {
            state
                .set_clipboard_status(false, "Wayland clipboard device was removed; reconnecting")
                .await;
            tracing::warn!("Wayland compositor finished the clipboard data-control device");
        }
        ClipboardEvent::NewOffer { .. }
        | ClipboardEvent::OwnContent { .. }
        | ClipboardEvent::Cleared { .. } => {}
    }
    Ok(())
}

pub(super) fn spawn_clipboard_watch<B>(
    clipboard: B,
    state: DaemonState,
    events: tokio::sync::mpsc::Sender<ClipboardEvent>,
    shutdown: CancellationToken,
) -> JoinHandle<()>
where
    B: ClipboardBackend + Clone + 'static,
{
    tokio::spawn(async move {
        let mut retry_delay = Duration::from_secs(1);
        loop {
            state
                .set_clipboard_status(false, "connecting to the Wayland clipboard")
                .await;
            let callback_events = events.clone();
            let ready = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let callback_ready = ready.clone();
            let result = clipboard
                .watch(
                    shutdown.clone(),
                    Box::new(move |event| {
                        if matches!(event, ClipboardEvent::Ready) {
                            callback_ready.store(true, std::sync::atomic::Ordering::SeqCst);
                        }
                        // Keep compositor callbacks non-blocking: this callback
                        // runs inside the dedicated current-thread Tokio runtime,
                        // where `blocking_send` would panic. A bounded queue
                        // prevents suspend/resume storms from growing memory.
                        if callback_events.try_send(event).is_err() {
                            tracing::warn!("dropping Wayland clipboard event because the daemon queue is full or closed");
                        }
                    }),
                )
                .await;
            if shutdown.is_cancelled() {
                break;
            }
            let detail = match result {
                Ok(()) => "Wayland clipboard device stopped; retrying".to_owned(),
                Err(error) => error.to_string(),
            };
            state.set_clipboard_status(false, detail.clone()).await;
            tracing::warn!(error = %detail, "Wayland clipboard monitoring is unavailable");
            if ready.load(std::sync::atomic::Ordering::SeqCst) {
                retry_delay = Duration::from_secs(1);
            } else {
                retry_delay = retry_delay.saturating_mul(2).min(Duration::from_secs(30));
            }
            tokio::select! {
                () = shutdown.cancelled() => break,
                () = tokio::time::sleep(retry_delay) => {}
            }
        }
    })
}
