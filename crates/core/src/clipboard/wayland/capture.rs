//! Bounded automatic capture and generation-checked explicit reads.

use std::{
    io::{ErrorKind, Read},
    os::{fd::AsFd, unix::net::UnixStream},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use tokio::{
    sync::{mpsc, oneshot},
    task,
};
use tokio_util::sync::CancellationToken;

use super::{protocol::DataControlOffer, runtime::ExplicitReadResult, state::WaylandState};
use crate::clipboard::{
    backend::{BackendError, ClipboardEvent},
    types::{
        CaptureBudget, ClipboardContent, ClipboardRepresentation, Generation, MimeType,
        OfferMimeList, RejectReason, SelectionKind,
    },
};

const PIPE_READ_TIMEOUT: Duration = Duration::from_millis(100);
const PIPE_CHUNK_BYTES: usize = 16 * 1024;

pub(super) struct CaptureAssembly {
    pub(super) kind: SelectionKind,
    pub(super) offer: DataControlOffer,
    pub(super) slots: Vec<Option<ClipboardRepresentation>>,
    pub(super) remaining: usize,
    pub(super) max_bytes: u64,
}

pub(super) struct CaptureMessage {
    pub(super) generation: Generation,
    pub(super) index: usize,
    pub(super) mime_type: MimeType,
    pub(super) result: Result<Arc<[u8]>, RejectReason>,
}

#[derive(Clone)]
pub(super) struct CurrentOffer {
    pub(super) generation: Generation,
    pub(super) offer: DataControlOffer,
    pub(super) mime_list: OfferMimeList,
}

impl WaylandState {
    pub(super) fn start_capture(
        &mut self,
        generation: Generation,
        kind: SelectionKind,
        offer: &DataControlOffer,
        mime_list: &OfferMimeList,
    ) {
        let mime_types = mime_list.types().to_vec();
        let expected = mime_types.len();
        let max_bytes = self.capture_threshold.load(Ordering::SeqCst);
        let budget = Arc::new(StdMutex::new(CaptureBudget::with_max(max_bytes)));

        self.captures.insert(
            generation,
            CaptureAssembly {
                kind,
                offer: offer.clone(),
                slots: vec![None; expected],
                remaining: expected,
                max_bytes,
            },
        );

        for (index, mime_type) in mime_types.into_iter().enumerate() {
            let (read_end, write_end) = match UnixStream::pair() {
                Ok(pair) => pair,
                Err(error) => {
                    self.reject_capture(
                        generation,
                        RejectReason::ReadFailed {
                            mime_type: mime_type.to_string(),
                            message: error.to_string(),
                        },
                    );
                    return;
                }
            };

            offer.receive(mime_type.to_string(), write_end.as_fd());
            drop(write_end);

            CaptureReaderJob {
                sender: self.capture_tx.clone(),
                generation,
                index,
                mime_type,
                read_end,
                budget: budget.clone(),
                current_generation: self.current_generation.clone(),
                shutdown: self.shutdown.clone(),
            }
            .spawn();
        }
    }

    pub(super) fn handle_capture_message(&mut self, message: CaptureMessage) {
        if message.generation != self.generation {
            self.reject_capture(
                message.generation,
                RejectReason::StaleGeneration {
                    offer_generation: message.generation,
                    current_generation: self.generation,
                },
            );
            return;
        }

        match message.result {
            Ok(bytes) => self.accept_mime_bytes(
                message.generation,
                message.index,
                ClipboardRepresentation::from_shared_bytes(message.mime_type, bytes),
            ),
            Err(reason) => self.reject_capture(message.generation, reason),
        }
    }

    fn accept_mime_bytes(
        &mut self,
        generation: Generation,
        index: usize,
        representation: ClipboardRepresentation,
    ) {
        let Some(assembly) = self.captures.get_mut(&generation) else {
            return;
        };

        if index >= assembly.slots.len() || assembly.slots[index].is_some() {
            return;
        }

        assembly.slots[index] = Some(representation);
        assembly.remaining = assembly.remaining.saturating_sub(1);
        if assembly.remaining != 0 {
            return;
        }

        let Some(assembly) = self.captures.remove(&generation) else {
            return;
        };
        let representations: Option<Vec<_>> = assembly.slots.into_iter().collect();
        let Some(representations) = representations else {
            return;
        };

        match ClipboardContent::new_with_max(representations, assembly.max_bytes) {
            Ok(content) => self.emit(ClipboardEvent::Captured {
                generation,
                kind: assembly.kind,
                content,
            }),
            Err(error) => self.emit(ClipboardEvent::CaptureRejected {
                generation,
                kind: assembly.kind,
                reason: RejectReason::ReadFailed {
                    mime_type: "<content>".to_owned(),
                    message: error.to_string(),
                },
            }),
        }
    }

    fn reject_capture(&mut self, generation: Generation, reason: RejectReason) {
        let Some(assembly) = self.captures.remove(&generation) else {
            return;
        };
        self.emit(ClipboardEvent::CaptureRejected {
            generation,
            kind: assembly.kind,
            reason,
        });
    }

    pub(super) fn invalidate_stale_captures(&mut self, current_generation: Generation) {
        let stale_generations = self
            .captures
            .keys()
            .copied()
            .filter(|generation| *generation < current_generation)
            .collect::<Vec<_>>();

        for generation in stale_generations {
            self.reject_capture(
                generation,
                RejectReason::StaleGeneration {
                    offer_generation: generation,
                    current_generation,
                },
            );
        }
    }
    #[allow(clippy::too_many_lines)]
    pub(super) fn start_explicit_read(
        &self,
        expected_generation: Option<Generation>,
        maximum_bytes: u64,
        retain_bytes: bool,
        reply: oneshot::Sender<Result<ExplicitReadResult, BackendError>>,
    ) {
        if maximum_bytes == 0 {
            let _ = reply.send(Err(BackendError::ClipboardCommand(
                "explicit clipboard limit must be nonzero".to_owned(),
            )));
            return;
        }
        let Some(current) = self.current_offer.clone() else {
            let _ = reply.send(Err(BackendError::CurrentOfferUnavailable));
            return;
        };
        if expected_generation.is_some_and(|expected| expected != current.generation) {
            let _ = reply.send(Err(BackendError::CurrentOfferChanged));
            return;
        }

        let budget = Arc::new(StdMutex::new(CaptureBudget::with_max(maximum_bytes)));
        let (result_tx, mut result_rx) = mpsc::unbounded_channel();
        let expected = current.mime_list.len();
        for (index, mime_type) in current.mime_list.types().iter().cloned().enumerate() {
            let (read_end, write_end) = match UnixStream::pair() {
                Ok(pair) => pair,
                Err(error) => {
                    let _ = reply.send(Err(BackendError::ClipboardCommand(error.to_string())));
                    return;
                }
            };
            current
                .offer
                .receive(mime_type.to_string(), write_end.as_fd());
            drop(write_end);
            let sender = result_tx.clone();
            let budget = budget.clone();
            let generation = current.generation;
            let current_generation = self.current_generation.clone();
            let shutdown = self.shutdown.clone();
            task::spawn_blocking(move || {
                let result = read_explicit_pipe(
                    read_end,
                    &mime_type,
                    &budget,
                    generation,
                    &current_generation,
                    &shutdown,
                    retain_bytes,
                );
                let _ = sender.send((index, mime_type, result));
            });
        }
        drop(result_tx);
        let mime_list = current.mime_list;
        let generation = current.generation;
        task::spawn(async move {
            let mut slots = vec![None; expected];
            for _ in 0..expected {
                let Some((index, mime_type, result)) = result_rx.recv().await else {
                    let _ = reply.send(Err(BackendError::ClipboardCommand(
                        "explicit clipboard read stopped unexpectedly".to_owned(),
                    )));
                    return;
                };
                match result {
                    Ok(bytes) => {
                        if retain_bytes {
                            slots[index] =
                                Some(ClipboardRepresentation::from_shared_bytes(mime_type, bytes));
                        }
                    }
                    Err(reason) => {
                        let error = match reason {
                            RejectReason::StaleGeneration { .. } => {
                                BackendError::CurrentOfferChanged
                            }
                            other => BackendError::ClipboardCommand(format!("{other:?}")),
                        };
                        let _ = reply.send(Err(error));
                        return;
                    }
                }
            }
            let Ok(budget) = budget.lock() else {
                let _ = reply.send(Err(BackendError::ClipboardCommand(
                    "explicit clipboard budget lock poisoned".to_owned(),
                )));
                return;
            };
            let logical_size = budget.total_bytes();
            drop(budget);
            let representations = if retain_bytes {
                let Some(representations) = slots.into_iter().collect() else {
                    let _ = reply.send(Err(BackendError::ClipboardCommand(
                        "explicit clipboard representation was lost".to_owned(),
                    )));
                    return;
                };
                representations
            } else {
                Vec::new()
            };
            let _ = reply.send(Ok(ExplicitReadResult {
                generation,
                mime_list,
                logical_size,
                representations,
            }));
        });
    }
}

fn read_explicit_pipe(
    mut read_end: UnixStream,
    mime_type: &MimeType,
    budget: &Arc<StdMutex<CaptureBudget>>,
    generation: Generation,
    current_generation: &Arc<AtomicU64>,
    shutdown: &CancellationToken,
    retain_bytes: bool,
) -> Result<Arc<[u8]>, RejectReason> {
    read_end
        .set_read_timeout(Some(PIPE_READ_TIMEOUT))
        .map_err(|error| RejectReason::ReadFailed {
            mime_type: mime_type.to_string(),
            message: error.to_string(),
        })?;
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; PIPE_CHUNK_BYTES];
    loop {
        if shutdown.is_cancelled() {
            return Err(RejectReason::Cancelled);
        }
        let current_value = current_generation.load(Ordering::SeqCst);
        if current_value != generation.value() {
            return Err(RejectReason::StaleGeneration {
                offer_generation: generation,
                current_generation: Generation::from_value(current_value),
            });
        }
        match read_end.read(&mut chunk) {
            Ok(0) => return Ok(Arc::from(bytes.into_boxed_slice())),
            Ok(count) => {
                budget
                    .lock()
                    .map_err(|_| RejectReason::ReadFailed {
                        mime_type: mime_type.to_string(),
                        message: "capture budget lock poisoned".to_owned(),
                    })?
                    .reserve(count)?;
                if retain_bytes {
                    bytes.extend_from_slice(&chunk[..count]);
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) => {}
            Err(error) => {
                return Err(RejectReason::ReadFailed {
                    mime_type: mime_type.to_string(),
                    message: error.to_string(),
                });
            }
        }
    }
}

struct CaptureReaderJob {
    sender: mpsc::UnboundedSender<CaptureMessage>,
    generation: Generation,
    index: usize,
    mime_type: MimeType,
    read_end: UnixStream,
    budget: Arc<StdMutex<CaptureBudget>>,
    current_generation: Arc<AtomicU64>,
    shutdown: CancellationToken,
}

impl CaptureReaderJob {
    fn spawn(self) {
        task::spawn_blocking(move || {
            let sender = self.sender.clone();
            let generation = self.generation;
            let index = self.index;
            let mime_type = self.mime_type.clone();
            let result = self.read_bounded_payload();
            let _ = sender.send(CaptureMessage {
                generation,
                index,
                mime_type,
                result,
            });
        });
    }

    fn read_bounded_payload(mut self) -> Result<Arc<[u8]>, RejectReason> {
        self.read_end
            .set_read_timeout(Some(PIPE_READ_TIMEOUT))
            .map_err(|error| RejectReason::ReadFailed {
                mime_type: self.mime_type.to_string(),
                message: error.to_string(),
            })?;

        let mut bytes = Vec::new();
        let mut chunk = [0_u8; PIPE_CHUNK_BYTES];

        loop {
            if self.shutdown.is_cancelled() {
                return Err(RejectReason::Cancelled);
            }

            let current_value = self.current_generation.load(Ordering::SeqCst);
            if current_value != self.generation.value() {
                return Err(RejectReason::StaleGeneration {
                    offer_generation: self.generation,
                    current_generation: Generation::from_value(current_value),
                });
            }

            match self.read_end.read(&mut chunk) {
                Ok(0) => return Ok(Arc::from(bytes.into_boxed_slice())),
                Ok(count) => {
                    {
                        let mut budget =
                            self.budget.lock().map_err(|_| RejectReason::ReadFailed {
                                mime_type: self.mime_type.to_string(),
                                message: "capture budget lock poisoned".to_owned(),
                            })?;
                        budget.reserve(count)?;
                    }
                    bytes.extend_from_slice(&chunk[..count]);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                    ) => {}
                Err(error) => {
                    return Err(RejectReason::ReadFailed {
                        mime_type: self.mime_type.to_string(),
                        message: error.to_string(),
                    });
                }
            }
        }
    }
}
