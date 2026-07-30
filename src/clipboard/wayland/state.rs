//! Wayland selection state and daemon-owned source lifecycle.

use std::{
    collections::HashMap,
    os::fd::OwnedFd,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use wayland_client::QueueHandle;

use super::{
    capture::{CaptureAssembly, CaptureMessage, CurrentOffer},
    io::spawn_source_writer,
    protocol::{
        DataControlDevice, DataControlManager, DataControlOffer, DataControlSource, SeatBinding,
        SeatToken,
    },
};
use crate::clipboard::{
    backend::{BackendError, ClipboardEvent},
    types::{
        BoundedMimeOffer, ClipboardContent, FeedbackDecision, FeedbackMarker, FeedbackState,
        Generation, RejectReason, SelectionKind,
    },
};

const MAX_CONCURRENT_SOURCE_WRITERS: usize = 32;

struct OwnedClipboardSource {
    source: DataControlSource,
    content: Arc<ClipboardContent>,
    marker: FeedbackMarker,
}

pub(super) struct WaylandState {
    manager: DataControlManager,
    devices: HashMap<SeatToken, DataControlDevice>,
    offers: HashMap<DataControlOffer, BoundedMimeOffer>,
    pub(super) generation: Generation,
    pub(super) current_generation: Arc<AtomicU64>,
    pub(super) captures: HashMap<Generation, CaptureAssembly>,
    pub(super) capture_tx: mpsc::UnboundedSender<CaptureMessage>,
    on_event: Box<dyn Fn(ClipboardEvent) + Send + Sync>,
    pub(super) shutdown: CancellationToken,
    pub(super) capture_threshold: Arc<AtomicU64>,
    feedback: FeedbackState,
    owned_source: Option<OwnedClipboardSource>,
    source_writers: Arc<Semaphore>,
    pub(super) current_offer: Option<CurrentOffer>,
    pub(super) finished: bool,
}

impl WaylandState {
    pub(super) fn new(
        manager: DataControlManager,
        capture_tx: mpsc::UnboundedSender<CaptureMessage>,
        on_event: Box<dyn Fn(ClipboardEvent) + Send + Sync>,
        shutdown: CancellationToken,
        capture_threshold: Arc<AtomicU64>,
    ) -> Self {
        Self {
            manager,
            devices: HashMap::new(),
            offers: HashMap::new(),
            generation: Generation::ZERO,
            current_generation: Arc::new(AtomicU64::new(Generation::ZERO.value())),
            captures: HashMap::new(),
            capture_tx,
            on_event,
            shutdown,
            capture_threshold,
            feedback: FeedbackState::default(),
            owned_source: None,
            source_writers: Arc::new(Semaphore::new(MAX_CONCURRENT_SOURCE_WRITERS)),
            current_offer: None,
            finished: false,
        }
    }

    pub(super) fn add_device(&mut self, binding: &SeatBinding, qh: &QueueHandle<Self>) {
        let device = self
            .manager
            .get_data_device(&binding.seat, qh, binding.token);
        self.devices.insert(binding.token, device);
    }

    pub(super) fn emit(&self, event: ClipboardEvent) {
        (self.on_event)(event);
    }

    fn next_generation(&mut self) -> Generation {
        let generation = self.generation.next();
        self.generation = generation;
        self.current_generation
            .store(generation.value(), Ordering::SeqCst);
        self.invalidate_stale_captures(generation);
        generation
    }

    pub(super) fn handle_data_offer(&mut self, offer: DataControlOffer) {
        self.offers.insert(offer, BoundedMimeOffer::default());
    }

    pub(super) fn handle_offer_mime(&mut self, offer: DataControlOffer, mime_type: String) {
        self.offers.entry(offer).or_default().push(mime_type);
    }

    pub(super) fn handle_selection(&mut self, offer: Option<DataControlOffer>) {
        if let Some(previous) = self.current_offer.take() {
            previous.offer.destroy();
        }
        let generation = self.next_generation();
        let kind = SelectionKind::Clipboard;

        let Some(offer) = offer else {
            self.emit(ClipboardEvent::Cleared { generation, kind });
            return;
        };

        let accumulator = self.offers.remove(&offer).unwrap_or_default();
        let invalid_count = accumulator.invalid_count();
        let truncated_count = accumulator.truncated_count();
        let mime_list = match accumulator.finish() {
            Ok(mime_list) => mime_list,
            Err(error) => {
                offer.destroy();
                self.emit(ClipboardEvent::CaptureRejected {
                    generation,
                    kind,
                    reason: RejectReason::ReadFailed {
                        mime_type: "<offer>".to_owned(),
                        message: error.to_string(),
                    },
                });
                return;
            }
        };

        if invalid_count > 0 || truncated_count > 0 {
            tracing::debug!(
                invalid_count,
                truncated_count,
                "bounded Wayland clipboard MIME offer"
            );
        }

        let public_mime_list = mime_list.without_feedback_markers();
        match self.feedback.classify_offer(&mime_list) {
            FeedbackDecision::OwnIntentional(marker) => {
                offer.destroy();
                self.emit(ClipboardEvent::OwnContent {
                    generation,
                    kind,
                    marker,
                    mime_list: public_mime_list,
                });
                return;
            }
            FeedbackDecision::OwnRepeated(_) => {
                offer.destroy();
                return;
            }
            FeedbackDecision::External => {}
        }

        if public_mime_list.is_empty() {
            offer.destroy();
            self.emit(ClipboardEvent::CaptureRejected {
                generation,
                kind,
                reason: if invalid_count > 0 {
                    RejectReason::InvalidOffer
                } else {
                    RejectReason::EmptyOffer
                },
            });
            return;
        }

        self.emit(ClipboardEvent::NewOffer {
            generation,
            kind,
            mime_list: public_mime_list.clone(),
        });

        self.current_offer = Some(CurrentOffer {
            generation,
            offer: offer.clone(),
            mime_list: public_mime_list.clone(),
        });
        self.start_capture(generation, kind, &offer, &public_mime_list);
    }

    pub(super) fn handle_source_send(
        &mut self,
        source: &DataControlSource,
        mime_type: String,
        fd: OwnedFd,
    ) {
        let Some(owned) = self
            .owned_source
            .as_ref()
            .filter(|owned| owned.source.same_proxy(source))
        else {
            source.destroy();
            return;
        };

        let marker_mime = owned.marker.mime_type();
        let payload = if mime_type == marker_mime.as_str() {
            Some(Arc::<[u8]>::from(owned.marker.as_str().as_bytes()))
        } else {
            owned.content.bytes_for_mime(&mime_type)
        };

        let Some(payload) = payload else {
            tracing::debug!(
                mime_type,
                "ignoring request for unowned clipboard MIME type"
            );
            return;
        };

        let Ok(permit) = self.source_writers.clone().try_acquire_owned() else {
            tracing::debug!("clipboard source writer limit reached");
            return;
        };
        spawn_source_writer(mime_type, fd, payload, self.shutdown.clone(), permit);
    }

    pub(super) fn handle_source_cancelled(&mut self, source: &DataControlSource) {
        if self
            .owned_source
            .as_ref()
            .is_some_and(|owned| owned.source.same_proxy(source))
        {
            self.owned_source = None;
            self.feedback.clear();
        }
    }

    pub(super) fn set_owned_content(
        &mut self,
        content: ClipboardContent,
        qh: &QueueHandle<Self>,
    ) -> Result<FeedbackMarker, BackendError> {
        if self.devices.is_empty() {
            return Err(BackendError::NoSeat);
        }

        let marker = FeedbackMarker::generate();
        let source = self.manager.create_data_source(qh);
        for representation in content.representations() {
            source.offer(representation.mime_type().to_string());
        }
        source.offer(marker.mime_type().to_string());

        let owned_source = OwnedClipboardSource {
            source: source.clone(),
            content: Arc::new(content),
            marker: marker.clone(),
        };

        self.feedback.arm(marker.clone());
        for device in self.devices.values() {
            device.set_selection(Some(&source));
        }

        if let Some(previous) = self.owned_source.replace(owned_source) {
            previous.source.destroy();
        }

        Ok(marker)
    }

    pub(super) fn cleanup(&mut self) {
        for capture in self.captures.drain().map(|(_, capture)| capture) {
            if self
                .current_offer
                .as_ref()
                .is_none_or(|current| current.offer != capture.offer)
            {
                capture.offer.destroy();
            }
        }
        if let Some(current) = self.current_offer.take() {
            current.offer.destroy();
        }
        self.offers.clear();
        if let Some(source) = self.owned_source.take() {
            source.source.destroy();
        }
        for device in self.devices.drain().map(|(_, device)| device) {
            device.destroy();
        }
    }
}
