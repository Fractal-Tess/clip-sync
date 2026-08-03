//! Wayland watcher event loop and backend command dispatch.

use std::{
    io::ErrorKind,
    sync::{Arc, atomic::AtomicU64},
};

use tokio::{
    io::unix::AsyncFd,
    sync::{mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;
use wayland_client::{QueueHandle, globals};

use super::{
    connection::connect_wayland,
    protocol::{bind_data_control_manager, bind_seats},
    state::WaylandState,
};
use crate::clipboard::{
    backend::{BackendError, ClipboardEvent},
    types::{ClipboardContent, ClipboardRepresentation, FeedbackMarker, Generation, OfferMimeList},
};

pub(super) enum ClipboardCommand {
    SetContent {
        content: ClipboardContent,
        reply: oneshot::Sender<Result<FeedbackMarker, BackendError>>,
    },
    ReadCurrent {
        expected_generation: Option<Generation>,
        maximum_bytes: u64,
        retain_bytes: bool,
        reply: oneshot::Sender<Result<ExplicitReadResult, BackendError>>,
    },
}

pub(super) struct ExplicitReadResult {
    pub(super) generation: Generation,
    pub(super) mime_list: OfferMimeList,
    pub(super) logical_size: u64,
    pub(super) representations: Vec<ClipboardRepresentation>,
}

pub(super) async fn run_wayland_watch(
    shutdown: CancellationToken,
    mut command_rx: mpsc::UnboundedReceiver<ClipboardCommand>,
    on_event: Box<dyn Fn(ClipboardEvent) + Send + Sync>,
    capture_threshold: Arc<AtomicU64>,
) -> Result<(), BackendError> {
    let conn = connect_wayland().map_err(BackendError::Connection)?;
    let (globals, mut queue) = globals::registry_queue_init::<WaylandState>(&conn)
        .map_err(|e| BackendError::Connection(format!("{e:?}")))?;
    let qh = queue.handle();

    let (protocol, manager) = bind_data_control_manager(&globals, &qh)?;
    tracing::debug!(%protocol, "using Wayland data-control protocol");
    let seats = bind_seats(&globals, &qh);
    if seats.is_empty() {
        return Err(BackendError::NoSeat);
    }

    let (capture_tx, mut capture_rx) = mpsc::unbounded_channel();
    let mut state = WaylandState::new(
        manager,
        capture_tx,
        on_event,
        shutdown.clone(),
        capture_threshold,
    );
    for seat in &seats {
        state.add_device(seat, &qh);
    }

    queue
        .roundtrip(&mut state)
        .map_err(|error| BackendError::Protocol(error.to_string()))?;
    state.emit(ClipboardEvent::Ready);

    loop {
        queue
            .dispatch_pending(&mut state)
            .map_err(|error| BackendError::Protocol(error.to_string()))?;

        while let Ok(command) = command_rx.try_recv() {
            handle_command(&mut state, &qh, command);
        }
        while let Ok(message) = capture_rx.try_recv() {
            state.handle_capture_message(message);
        }

        if shutdown.is_cancelled() || state.finished {
            break;
        }

        queue
            .flush()
            .map_err(|error| BackendError::Protocol(error.to_string()))?;
        let Some(read_guard) = queue.prepare_read() else {
            continue;
        };

        let readiness_fd = read_guard
            .connection_fd()
            .try_clone_to_owned()
            .map_err(|error| BackendError::Protocol(error.to_string()))?;
        let async_fd = AsyncFd::new(readiness_fd)
            .map_err(|error| BackendError::Protocol(error.to_string()))?;

        tokio::select! {
            () = shutdown.cancelled() => {
                drop(read_guard);
                break;
            }
            command = command_rx.recv() => {
                drop(read_guard);
                if let Some(command) = command {
                    handle_command(&mut state, &qh, command);
                }
            }
            message = capture_rx.recv() => {
                drop(read_guard);
                if let Some(message) = message {
                    state.handle_capture_message(message);
                }
            }
            readiness = async_fd.readable() => {
                let _readiness = readiness
                    .map_err(|error| BackendError::Protocol(error.to_string()))?;
                match read_guard.read() {
                    Ok(_) => {}
                    Err(wayland_client::backend::WaylandError::Io(error))
                        if error.kind() == ErrorKind::WouldBlock => {}
                    Err(error) => return Err(BackendError::Protocol(error.to_string())),
                }
            }
        }
    }

    state.cleanup();
    let _ = queue.flush();
    Ok(())
}

fn handle_command(
    state: &mut WaylandState,
    qh: &QueueHandle<WaylandState>,
    command: ClipboardCommand,
) {
    match command {
        ClipboardCommand::SetContent { content, reply } => {
            let _ = reply.send(state.set_owned_content(content, qh));
        }
        ClipboardCommand::ReadCurrent {
            expected_generation,
            maximum_bytes,
            retain_bytes,
            reply,
        } => state.start_explicit_read(expected_generation, maximum_bytes, retain_bytes, reply),
    }
}
