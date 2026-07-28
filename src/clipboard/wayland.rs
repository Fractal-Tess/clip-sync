//! Wayland data-control capability probe, capture, and ownership.
//!
//! Connects to the Wayland display, prefers `ext-data-control-v1`, falls back
//! to `zwlr-data-control-v1`, monitors only the regular clipboard selection,
//! and owns daemon-provided sources while the watcher is running.

use std::{
    collections::HashMap,
    io::{self, ErrorKind, Read},
    os::{
        fd::{AsFd, BorrowedFd, OwnedFd},
        unix::net::UnixStream,
    },
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use tokio::{
    io::unix::AsyncFd,
    sync::{Semaphore, mpsc, oneshot},
    task,
};
use tokio_util::sync::CancellationToken;
use wayland_client::globals::{self, GlobalList, GlobalListContents};
use wayland_client::protocol::{wl_registry, wl_seat::WlSeat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, event_created_child};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::{self, ExtDataControlManagerV1},
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::{self, ZwlrDataControlManagerV1},
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

use super::backend::{BackendError, ClipboardBackend, ClipboardEvent};
use super::types::{
    BoundedMimeOffer, CaptureBudget, ClipboardContent, ClipboardRepresentation,
    CurrentClipboardInspection, DataControlProtocol, FeedbackDecision, FeedbackMarker,
    FeedbackState, Generation, MAX_CAPTURE_BYTES, MimeType, OfferMimeList, ProbeResult,
    RejectReason, SelectionKind,
};

// Interface names as they appear in the Wayland global registry.
const EXT_DATA_CONTROL_MANAGER: &str = "ext_data_control_manager_v1";
const WLR_DATA_CONTROL_MANAGER: &str = "zwlr_data_control_manager_v1";
const WL_SEAT: &str = "wl_seat";
const PIPE_READ_TIMEOUT: Duration = Duration::from_millis(100);
const PIPE_CHUNK_BYTES: usize = 16 * 1024;
const MAX_CONCURRENT_SOURCE_WRITERS: usize = 32;

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
        std::thread::Builder::new()
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
            })
            .map_err(|error| BackendError::Connection(error.to_string()))?;

        let result = done_rx.await.map_err(|_| {
            BackendError::Connection("Wayland event thread stopped unexpectedly".to_owned())
        })?;

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

enum ClipboardCommand {
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

struct ExplicitReadResult {
    generation: Generation,
    mime_list: OfferMimeList,
    logical_size: u64,
    representations: Vec<ClipboardRepresentation>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct SeatToken(u32);

#[derive(Clone, Debug)]
enum DataControlManager {
    Ext(ExtDataControlManagerV1),
    Wlr(ZwlrDataControlManagerV1),
}

#[derive(Clone, Debug)]
enum DataControlDevice {
    Ext(ExtDataControlDeviceV1),
    Wlr(ZwlrDataControlDeviceV1),
}

#[derive(Clone, Debug)]
enum DataControlSource {
    Ext(ExtDataControlSourceV1),
    Wlr(ZwlrDataControlSourceV1),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum DataControlOffer {
    Ext(ExtDataControlOfferV1),
    Wlr(ZwlrDataControlOfferV1),
}

impl DataControlManager {
    fn get_data_device(
        &self,
        seat: &WlSeat,
        qh: &QueueHandle<WaylandState>,
        token: SeatToken,
    ) -> DataControlDevice {
        match self {
            Self::Ext(manager) => DataControlDevice::Ext(manager.get_data_device(seat, qh, token)),
            Self::Wlr(manager) => DataControlDevice::Wlr(manager.get_data_device(seat, qh, token)),
        }
    }

    fn create_data_source(&self, qh: &QueueHandle<WaylandState>) -> DataControlSource {
        match self {
            Self::Ext(manager) => DataControlSource::Ext(manager.create_data_source(qh, ())),
            Self::Wlr(manager) => DataControlSource::Wlr(manager.create_data_source(qh, ())),
        }
    }
}

impl DataControlDevice {
    fn set_selection(&self, source: Option<&DataControlSource>) {
        match self {
            Self::Ext(device) => {
                device.set_selection(source.and_then(DataControlSource::as_ext));
            }
            Self::Wlr(device) => {
                device.set_selection(source.and_then(DataControlSource::as_wlr));
            }
        }
    }

    fn destroy(&self) {
        match self {
            Self::Ext(device) => device.destroy(),
            Self::Wlr(device) => device.destroy(),
        }
    }
}

impl DataControlSource {
    fn offer(&self, mime_type: String) {
        match self {
            Self::Ext(source) => source.offer(mime_type),
            Self::Wlr(source) => source.offer(mime_type),
        }
    }

    fn destroy(&self) {
        match self {
            Self::Ext(source) => source.destroy(),
            Self::Wlr(source) => source.destroy(),
        }
    }

    fn as_ext(&self) -> Option<&ExtDataControlSourceV1> {
        match self {
            Self::Ext(source) => Some(source),
            Self::Wlr(_) => None,
        }
    }

    fn as_wlr(&self) -> Option<&ZwlrDataControlSourceV1> {
        match self {
            Self::Wlr(source) => Some(source),
            Self::Ext(_) => None,
        }
    }

    fn same_proxy(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Ext(left), Self::Ext(right)) => left.id() == right.id(),
            (Self::Wlr(left), Self::Wlr(right)) => left.id() == right.id(),
            _ => false,
        }
    }
}

impl DataControlOffer {
    fn receive(&self, mime_type: String, fd: BorrowedFd<'_>) {
        match self {
            Self::Ext(offer) => offer.receive(mime_type, fd),
            Self::Wlr(offer) => offer.receive(mime_type, fd),
        }
    }

    fn destroy(&self) {
        match self {
            Self::Ext(offer) => offer.destroy(),
            Self::Wlr(offer) => offer.destroy(),
        }
    }
}

struct SeatBinding {
    token: SeatToken,
    seat: WlSeat,
}

struct OwnedClipboardSource {
    source: DataControlSource,
    content: Arc<ClipboardContent>,
    marker: FeedbackMarker,
}

struct CaptureAssembly {
    kind: SelectionKind,
    offer: DataControlOffer,
    slots: Vec<Option<ClipboardRepresentation>>,
    remaining: usize,
    max_bytes: u64,
}

struct CaptureMessage {
    generation: Generation,
    index: usize,
    mime_type: MimeType,
    result: Result<Arc<[u8]>, RejectReason>,
}

#[derive(Clone)]
struct CurrentOffer {
    generation: Generation,
    offer: DataControlOffer,
    mime_list: OfferMimeList,
}

struct WaylandState {
    manager: DataControlManager,
    devices: HashMap<SeatToken, DataControlDevice>,
    offers: HashMap<DataControlOffer, BoundedMimeOffer>,
    generation: Generation,
    current_generation: Arc<AtomicU64>,
    captures: HashMap<Generation, CaptureAssembly>,
    capture_tx: mpsc::UnboundedSender<CaptureMessage>,
    on_event: Box<dyn Fn(ClipboardEvent) + Send + Sync>,
    shutdown: CancellationToken,
    capture_threshold: Arc<AtomicU64>,
    feedback: FeedbackState,
    owned_source: Option<OwnedClipboardSource>,
    source_writers: Arc<Semaphore>,
    current_offer: Option<CurrentOffer>,
    finished: bool,
}

impl WaylandState {
    fn new(
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

    fn add_device(&mut self, binding: &SeatBinding, qh: &QueueHandle<Self>) {
        let device = self
            .manager
            .get_data_device(&binding.seat, qh, binding.token);
        self.devices.insert(binding.token, device);
    }

    fn emit(&self, event: ClipboardEvent) {
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

    fn handle_data_offer(&mut self, offer: DataControlOffer) {
        self.offers.insert(offer, BoundedMimeOffer::default());
    }

    fn handle_offer_mime(&mut self, offer: DataControlOffer, mime_type: String) {
        self.offers.entry(offer).or_default().push(mime_type);
    }

    fn handle_selection(&mut self, offer: Option<DataControlOffer>) {
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

    fn start_capture(
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

    fn handle_capture_message(&mut self, message: CaptureMessage) {
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

    fn invalidate_stale_captures(&mut self, current_generation: Generation) {
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

    fn handle_source_send(&mut self, source: &DataControlSource, mime_type: String, fd: OwnedFd) {
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

    fn handle_source_cancelled(&mut self, source: &DataControlSource) {
        if self
            .owned_source
            .as_ref()
            .is_some_and(|owned| owned.source.same_proxy(source))
        {
            self.owned_source = None;
            self.feedback.clear();
        }
    }

    fn set_owned_content(
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

    #[allow(clippy::too_many_lines)]
    fn start_explicit_read(
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

    fn cleanup(&mut self) {
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

async fn run_wayland_watch(
    shutdown: CancellationToken,
    mut command_rx: mpsc::UnboundedReceiver<ClipboardCommand>,
    on_event: Box<dyn Fn(ClipboardEvent) + Send + Sync>,
    capture_threshold: Arc<AtomicU64>,
) -> Result<(), BackendError> {
    let conn = Connection::connect_to_env().map_err(|e| BackendError::Connection(e.to_string()))?;
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

fn bind_data_control_manager(
    globals: &GlobalList,
    qh: &QueueHandle<WaylandState>,
) -> Result<(DataControlProtocol, DataControlManager), BackendError> {
    if let Ok(manager) = globals.bind::<ExtDataControlManagerV1, _, _>(qh, 1..=1, ()) {
        return Ok((DataControlProtocol::Ext, DataControlManager::Ext(manager)));
    }

    if let Ok(manager) = globals.bind::<ZwlrDataControlManagerV1, _, _>(qh, 1..=1, ()) {
        return Ok((DataControlProtocol::Wlr, DataControlManager::Wlr(manager)));
    }

    Err(BackendError::NoDataControl)
}

fn bind_seats(globals: &GlobalList, qh: &QueueHandle<WaylandState>) -> Vec<SeatBinding> {
    let registry = globals.registry();
    globals.contents().with_list(|items| {
        items
            .iter()
            .filter(|global| global.interface == WlSeat::interface().name && global.version >= 1)
            .enumerate()
            .map(|(index, global)| {
                let token = SeatToken(u32::try_from(index).unwrap_or(u32::MAX));
                SeatBinding {
                    token,
                    seat: registry.bind(global.name, 1, qh, ()),
                }
            })
            .collect::<Vec<_>>()
    })
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

fn spawn_source_writer(
    mime_type: String,
    fd: OwnedFd,
    payload: Arc<[u8]>,
    shutdown: CancellationToken,
    permit: tokio::sync::OwnedSemaphorePermit,
) {
    task::spawn(async move {
        let _permit = permit;
        if let Err(error) = write_payload(fd, &payload, &shutdown).await {
            tracing::debug!(
                mime_type,
                error = %error,
                "failed to serve daemon-owned clipboard MIME data"
            );
        }
    });
}

async fn write_payload(
    fd: OwnedFd,
    payload: &[u8],
    shutdown: &CancellationToken,
) -> Result<(), io::Error> {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};

    let flags = fcntl_getfl(&fd).map_err(io::Error::from)?;
    fcntl_setfl(&fd, flags | OFlags::NONBLOCK).map_err(io::Error::from)?;
    let async_fd = AsyncFd::new(fd)?;
    let mut offset = 0;
    while offset < payload.len() {
        let writable = tokio::select! {
            () = shutdown.cancelled() => return Ok(()),
            result = async_fd.writable() => result?,
        };
        let mut writable = writable;
        match writable.try_io(|inner| {
            rustix::io::write(inner.get_ref(), &payload[offset..]).map_err(Into::into)
        }) {
            Ok(Ok(0)) => {
                return Err(io::Error::new(
                    ErrorKind::WriteZero,
                    "clipboard destination accepted zero bytes",
                ));
            }
            Ok(Ok(written)) => offset += written,
            Ok(Err(error)) => return Err(error),
            Err(_) => {}
        }
    }
    Ok(())
}

/// Minimal dispatch state used only for the capability probe.
///
/// We only need the `wl_registry` dispatch to satisfy `registry_queue_init`;
/// the actual protocol objects are never bound during the probe.
struct ProbeState;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for ProbeState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // The GlobalList machinery handles registry events internally.
        // We have nothing extra to do here.
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &WlSeat,
        _event: <WlSeat as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlManagerV1, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &ExtDataControlManagerV1,
        _event: ext_data_control_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrDataControlManagerV1,
        _event: zwlr_data_control_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlDeviceV1, SeatToken> for WaylandState {
    fn event(
        state: &mut Self,
        _proxy: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _data: &SeatToken,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_device_v1::Event::DataOffer { id } => {
                state.handle_data_offer(DataControlOffer::Ext(id));
            }
            ext_data_control_device_v1::Event::Selection { id } => {
                state.handle_selection(id.map(DataControlOffer::Ext));
            }
            ext_data_control_device_v1::Event::Finished => {
                state.finished = true;
                state.emit(ClipboardEvent::Finished);
            }
            ext_data_control_device_v1::Event::PrimarySelection { .. } => {
                tracing::trace!("ignoring ext-data-control primary selection event");
            }
            _ => {}
        }
    }

    event_created_child!(WaylandState, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlDeviceV1, SeatToken> for WaylandState {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _data: &SeatToken,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_device_v1::Event::DataOffer { id } => {
                state.handle_data_offer(DataControlOffer::Wlr(id));
            }
            zwlr_data_control_device_v1::Event::Selection { id } => {
                state.handle_selection(id.map(DataControlOffer::Wlr));
            }
            zwlr_data_control_device_v1::Event::Finished => {
                state.finished = true;
                state.emit(ClipboardEvent::Finished);
            }
            zwlr_data_control_device_v1::Event::PrimarySelection { .. } => {
                tracing::trace!("ignoring wlr-data-control primary selection event");
            }
            _ => {}
        }
    }

    event_created_child!(WaylandState, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        proxy: &ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.handle_offer_mime(DataControlOffer::Ext(proxy.clone()), mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        proxy: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.handle_offer_mime(DataControlOffer::Wlr(proxy.clone()), mime_type);
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        proxy: &ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let source = DataControlSource::Ext(proxy.clone());
        match event {
            ext_data_control_source_v1::Event::Send { mime_type, fd } => {
                state.handle_source_send(&source, mime_type, fd);
            }
            ext_data_control_source_v1::Event::Cancelled => {
                state.handle_source_cancelled(&source);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        proxy: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let source = DataControlSource::Wlr(proxy.clone());
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
                state.handle_source_send(&source, mime_type, fd);
            }
            zwlr_data_control_source_v1::Event::Cancelled => {
                state.handle_source_cancelled(&source);
            }
            _ => {}
        }
    }
}

/// Performs a single Wayland round-trip to discover data-control globals.
///
/// This is intentionally synchronous; it is called from within
/// `spawn_blocking`.
fn probe_wayland_globals() -> Result<ProbeResult, BackendError> {
    let conn = Connection::connect_to_env().map_err(|e| BackendError::Connection(e.to_string()))?;

    let (global_list, _queue) = globals::registry_queue_init::<ProbeState>(&conn)
        .map_err(|e| BackendError::Connection(e.to_string()))?;

    let contents = global_list.contents();

    let mut has_ext = false;
    let mut has_wlr = false;
    let mut has_seat = false;

    contents.with_list(|globals| {
        for global in globals {
            match global.interface.as_str() {
                EXT_DATA_CONTROL_MANAGER => has_ext = true,
                WLR_DATA_CONTROL_MANAGER => has_wlr = true,
                WL_SEAT => has_seat = true,
                _ => {}
            }
        }
    });

    let protocol = if has_ext {
        Some(DataControlProtocol::Ext)
    } else if has_wlr {
        Some(DataControlProtocol::Wlr)
    } else {
        None
    };

    Ok(ProbeResult { protocol, has_seat })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn blocked_source_writer_observes_shutdown() {
        let (_read_end, write_end) = UnixStream::pair().expect("socket pair");
        let fd: OwnedFd = write_end.into();
        let shutdown = CancellationToken::new();
        let writer_shutdown = shutdown.clone();
        let writer = tokio::spawn(async move {
            let payload = vec![0x5a; 8 * 1024 * 1024];
            write_payload(fd, &payload, &writer_shutdown).await
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(1), writer)
            .await
            .expect("writer stopped after cancellation")
            .expect("writer task")
            .expect("cancellation is a clean writer stop");
    }
}
