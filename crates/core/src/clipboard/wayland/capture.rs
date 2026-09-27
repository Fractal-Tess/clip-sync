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

use tokio::{sync::mpsc, task};
use tokio_util::sync::CancellationToken;

use super::{protocol::DataControlOffer, state::WaylandState};
use crate::clipboard::{
    backend::ClipboardEvent,
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

/// The live regular-clipboard offer, kept so it is destroyed exactly once.
#[derive(Clone)]
pub(super) struct CurrentOffer {
    pub(super) offer: DataControlOffer,
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
