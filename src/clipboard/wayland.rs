//! Wayland data-control capability probe, capture, and ownership.
//!
//! Connects to the Wayland display, prefers `ext-data-control-v1`, falls back
//! to `zwlr-data-control-v1`, monitors only the regular clipboard selection,
//! and owns daemon-provided sources while the watcher is running.

use std::{
    collections::HashMap,
    fs::File,
    io::{self, ErrorKind, Read, Write},
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
    sync::{mpsc, oneshot},
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
    DataControlProtocol, FeedbackDecision, FeedbackMarker, FeedbackState, Generation, MimeType,
    OfferMimeList, ProbeResult, RejectReason, SelectionKind,
};

// Interface names as they appear in the Wayland global registry.
const EXT_DATA_CONTROL_MANAGER: &str = "ext_data_control_manager_v1";
const WLR_DATA_CONTROL_MANAGER: &str = "zwlr_data_control_manager_v1";
const WL_SEAT: &str = "wl_seat";
const PIPE_READ_TIMEOUT: Duration = Duration::from_millis(100);
const PIPE_CHUNK_BYTES: usize = 16 * 1024;

/// Wayland-native clipboard backend.
///
/// Probes for data-control support by connecting to the compositor's Wayland
/// display. Capture and ownership use the protocol directly and never shell out
/// to external clipboard tools.
#[derive(Clone, Default)]
pub struct WaylandBackend {
    commands: Arc<StdMutex<Option<mpsc::UnboundedSender<ClipboardCommand>>>>,
}

impl WaylandBackend {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn active_sender(&self) -> Result<mpsc::UnboundedSender<ClipboardCommand>, BackendError> {
        self.commands
            .lock()
            .map_err(|_| BackendError::ClipboardCommand("watch command lock poisoned".into()))?
            .clone()
            .ok_or(BackendError::WatchNotRunning)
    }
}

#[async_trait(?Send)]
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

        let result = run_wayland_watch(shutdown, command_rx, on_event).await;

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
}

enum ClipboardCommand {
    SetContent {
        content: ClipboardContent,
        reply: oneshot::Sender<Result<FeedbackMarker, BackendError>>,
    },
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
}

struct CaptureMessage {
    generation: Generation,
    index: usize,
    mime_type: MimeType,
    result: Result<Arc<[u8]>, RejectReason>,
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
    feedback: FeedbackState,
    owned_source: Option<OwnedClipboardSource>,
    finished: bool,
}

impl WaylandState {
    fn new(
        manager: DataControlManager,
        capture_tx: mpsc::UnboundedSender<CaptureMessage>,
        on_event: Box<dyn Fn(ClipboardEvent) + Send + Sync>,
        shutdown: CancellationToken,
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
            feedback: FeedbackState::default(),
            owned_source: None,
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
        let budget = Arc::new(StdMutex::new(CaptureBudget::new()));

        self.captures.insert(
            generation,
            CaptureAssembly {
                kind,
                offer: offer.clone(),
                slots: vec![None; expected],
                remaining: expected,
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
        assembly.offer.destroy();

        let representations: Option<Vec<_>> = assembly.slots.into_iter().collect();
        let Some(representations) = representations else {
            return;
        };

        match ClipboardContent::new(representations) {
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
        assembly.offer.destroy();
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

        spawn_source_writer(mime_type, fd, payload, self.shutdown.clone());
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

    fn cleanup(&mut self) {
        for capture in self.captures.drain().map(|(_, capture)| capture) {
            capture.offer.destroy();
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
    let mut state = WaylandState::new(manager, capture_tx, on_event, shutdown.clone());
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
) {
    task::spawn_blocking(move || {
        if let Err(error) = write_payload(fd, &payload, &shutdown) {
            tracing::debug!(
                mime_type,
                error = %error,
                "failed to serve daemon-owned clipboard MIME data"
            );
        }
    });
}

fn write_payload(
    fd: OwnedFd,
    payload: &[u8],
    shutdown: &CancellationToken,
) -> Result<(), io::Error> {
    let mut file = File::from(fd);
    for chunk in payload.chunks(PIPE_CHUNK_BYTES) {
        if shutdown.is_cancelled() {
            return Ok(());
        }
        file.write_all(chunk)?;
    }
    file.flush()
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
