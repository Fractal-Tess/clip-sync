use std::{
    fs::{File, OpenOptions},
    os::unix::net::{UnixListener as StdUnixListener, UnixStream as StdUnixStream},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc as std_mpsc,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use eframe::egui::{
    self, Color32, CornerRadius, FontId, Frame, Key, Margin, RichText, ScrollArea, Stroke, Vec2,
};
use fs2::FileExt;
use tokio_util::sync::CancellationToken;

use crate::{
    config::AppPaths,
    ipc::{
        self,
        protocol::{
            ActivateRequest, ConfigRequest, ConfigResponse, DiagnosticCheck, DiagnosticsRequest,
            DiagnosticsResponse, ForgetDeviceRequest, HistoryItem, HistoryRequest, HistoryResponse,
            HistoryUpdateAction, HistoryUpdateRequest, IPC_PROTOCOL_VERSION, MutationResponse,
            PeerItem, PeersRequest, PeersResponse, Request, Response, ShareClipboardRequest,
            ShareClipboardResponse, SharedSettingKind, SharedSettingUpdateRequest, StatusRequest,
            StatusResponse, TransferCancelRequest, TransferItem, TransfersRequest,
            TransfersResponse, request, response,
        },
    },
};

const BACKGROUND: Color32 = Color32::from_rgb(12, 17, 20);
const SURFACE: Color32 = Color32::from_rgb(20, 28, 32);
const BORDER: Color32 = Color32::from_rgb(44, 63, 70);
const CYAN: Color32 = Color32::from_rgb(35, 200, 226);
const MUTED: Color32 = Color32::from_rgb(137, 154, 160);
const ERROR: Color32 = Color32::from_rgb(242, 119, 119);
const SUCCESS: Color32 = Color32::from_rgb(105, 219, 160);
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiMode {
    Switcher,
    Control,
}

/// Starts the optional native egui process using the caller's resolved XDG paths.
///
/// # Errors
///
/// Returns an error when the native event loop or graphics context cannot start.
pub fn run(mode: UiMode, paths: AppPaths) -> Result<(), String> {
    let Some(instance) = UiInstance::acquire(&paths.runtime_dir, mode)? else {
        return Ok(());
    };
    let (title, app_id, size, decorations) = match mode {
        UiMode::Switcher => (
            "clip-sync switcher",
            "clip-sync-switcher",
            Vec2::new(720.0, 480.0),
            false,
        ),
        UiMode::Control => (
            "clip-sync control center",
            "clip-sync-control",
            Vec2::new(1040.0, 700.0),
            true,
        ),
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(title)
            .with_app_id(app_id)
            .with_inner_size(size)
            .with_min_inner_size(Vec2::new(480.0, 300.0))
            .with_decorations(decorations),
        ..Default::default()
    };

    eframe::run_native(
        title,
        options,
        Box::new(move |context| {
            configure_style(&context.egui_ctx);
            let app = ClipSyncApp::new(mode, paths, context.egui_ctx.clone(), instance)
                .map_err(std::io::Error::other)?;
            Ok(Box::new(app))
        }),
    )
    .map_err(|error| error.to_string())
}

fn configure_style(context: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = BACKGROUND;
    visuals.window_fill = BACKGROUND;
    visuals.extreme_bg_color = SURFACE;
    visuals.faint_bg_color = SURFACE;
    visuals.selection.bg_fill = CYAN;
    visuals.selection.stroke = Stroke::new(1.0, Color32::BLACK);
    visuals.widgets.inactive.bg_fill = SURFACE;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, CYAN);
    visuals.widgets.active.bg_fill = CYAN;
    visuals.window_corner_radius = CornerRadius::same(10);
    context.set_visuals(visuals);

    let mut style = (*context.style_of(egui::Theme::Dark)).clone();
    style.spacing.item_spacing = Vec2::new(10.0, 10.0);
    style.spacing.button_padding = Vec2::new(12.0, 8.0);
    context.set_style_of(egui::Theme::Dark, style);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SwitcherState {
    NeedsFocus,
    Ready,
    Close,
}

struct UiInstance {
    _lock: File,
    focus_socket: PathBuf,
    listener: Option<StdUnixListener>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl UiInstance {
    fn acquire(runtime_dir: &Path, mode: UiMode) -> Result<Option<Self>, String> {
        std::fs::create_dir_all(runtime_dir).map_err(|error| {
            format!(
                "could not create UI runtime directory {}: {error}",
                runtime_dir.display()
            )
        })?;
        set_private_mode(runtime_dir, 0o700)?;

        let name = match mode {
            UiMode::Switcher => "switcher",
            UiMode::Control => "control",
        };
        let lock_path = runtime_dir.join(format!("{name}.lock"));
        let focus_socket = runtime_dir.join(format!("{name}.sock"));
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|error| format!("could not open {}: {error}", lock_path.display()))?;
        set_private_mode(&lock_path, 0o600)?;

        match lock.try_lock_exclusive() {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                request_existing_focus(&focus_socket)?;
                return Ok(None);
            }
            Err(error) => {
                return Err(format!(
                    "could not lock UI instance file {}: {error}",
                    lock_path.display()
                ));
            }
        }

        remove_stale_ui_socket(&focus_socket)?;
        let listener = StdUnixListener::bind(&focus_socket).map_err(|error| {
            format!(
                "could not bind UI focus socket {}: {error}",
                focus_socket.display()
            )
        })?;
        set_private_mode(&focus_socket, 0o600)?;
        Ok(Some(Self {
            _lock: lock,
            focus_socket,
            listener: Some(listener),
            shutdown: Arc::new(AtomicBool::new(false)),
            thread: None,
        }))
    }

    fn start_focus_listener(&mut self, context: egui::Context) -> Result<(), String> {
        let Some(listener) = self.listener.take() else {
            return Ok(());
        };
        let shutdown = Arc::clone(&self.shutdown);
        let thread = std::thread::Builder::new()
            .name("clip-sync-ui-focus".to_owned())
            .spawn(move || {
                for stream in listener.incoming() {
                    match stream {
                        Ok(_) if shutdown.load(Ordering::Acquire) => break,
                        Ok(_) => {
                            context.send_viewport_cmd(egui::ViewportCommand::Focus);
                            context.request_repaint();
                        }
                        Err(error) => {
                            tracing::debug!(%error, "UI focus listener stopped");
                            break;
                        }
                    }
                }
            })
            .map_err(|error| format!("could not start UI focus listener: {error}"))?;
        self.thread = Some(thread);
        Ok(())
    }
}

impl Drop for UiInstance {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = StdUnixStream::connect(&self.focus_socket);
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            tracing::debug!("UI focus listener panicked during shutdown");
        }
        match std::fs::remove_file(&self.focus_socket) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::debug!(%error, "could not remove UI focus socket"),
        }
    }
}

fn request_existing_focus(socket: &Path) -> Result<(), String> {
    let mut last_error = None;
    for _ in 0..10 {
        match StdUnixStream::connect(socket) {
            Ok(_stream) => return Ok(()),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                last_error = Some(error);
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                last_error = Some(error);
                break;
            }
        }
    }
    Err(format!(
        "another clip-sync UI is running, but its focus socket {} is unavailable: {}",
        socket.display(),
        last_error.expect("at least one focus connection was attempted")
    ))
}

fn remove_stale_ui_socket(socket: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(socket) {
        Ok(metadata) => {
            use std::os::unix::fs::FileTypeExt;

            if !metadata.file_type().is_socket() {
                return Err(format!(
                    "refusing to replace non-socket UI focus path {}",
                    socket.display()
                ));
            }
            std::fs::remove_file(socket).map_err(|error| {
                format!(
                    "could not remove stale UI focus socket {}: {error}",
                    socket.display()
                )
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "could not inspect UI focus socket {}: {error}",
            socket.display()
        )),
    }
}

#[cfg(unix)]
fn set_private_mode(path: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|error| format!("could not secure {}: {error}", path.display()))
}

#[allow(
    clippy::struct_excessive_bools,
    reason = "independent UI request states do not form one shared state machine"
)]
struct ClipSyncApp {
    mode: UiMode,
    paths: AppPaths,
    context: egui::Context,
    _instance: UiInstance,
    ipc_worker: IpcWorker,
    event_rx: std_mpsc::Receiver<UiEvent>,
    search: String,
    selected_history: usize,
    scroll_selected_history: bool,
    selected_tab: ControlTab,
    switcher_state: SwitcherState,
    history_generation: u64,
    history_loading: bool,
    pending_history_refresh: Option<Instant>,
    history: Vec<HistoryItem>,
    status: Option<StatusResponse>,
    peers: Option<PeersResponse>,
    config: Option<serde_json::Value>,
    diagnostics: Vec<DiagnosticCheck>,
    transfers: Vec<TransferItem>,
    daemon_error: Option<String>,
    history_error: Option<String>,
    peers_error: Option<String>,
    config_error: Option<String>,
    diagnostics_error: Option<String>,
    transfers_error: Option<String>,
    notice: Option<Notice>,
    mutation_pending: bool,
    transfer_refresh_pending: bool,
    last_transfer_refresh: Instant,
    share_inspection: Option<ShareClipboardResponse>,
    pending_transfer_cancel: Option<String>,
    forget_device_id: String,
    pending_forget_device: Option<String>,
    mesh_quota_input: String,
    capture_threshold_input: String,
    pending_setting: Option<PendingSetting>,
}

impl ClipSyncApp {
    fn new(
        mode: UiMode,
        paths: AppPaths,
        context: egui::Context,
        mut instance: UiInstance,
    ) -> Result<Self, String> {
        let (event_tx, event_rx) = std_mpsc::channel();
        instance.start_focus_listener(context.clone())?;
        let ipc_worker = spawn_ipc_worker(paths.socket.clone(), event_tx, context.clone());

        let mut app = Self {
            mode,
            paths,
            context,
            _instance: instance,
            ipc_worker,
            event_rx,
            search: String::new(),
            selected_history: 0,
            scroll_selected_history: false,
            selected_tab: ControlTab::History,
            switcher_state: if mode == UiMode::Switcher {
                SwitcherState::NeedsFocus
            } else {
                SwitcherState::Ready
            },
            history_generation: 1,
            history_loading: true,
            pending_history_refresh: None,
            history: Vec::new(),
            status: None,
            peers: None,
            config: None,
            diagnostics: Vec::new(),
            transfers: Vec::new(),
            daemon_error: None,
            history_error: None,
            peers_error: None,
            config_error: None,
            diagnostics_error: None,
            transfers_error: None,
            notice: None,
            mutation_pending: false,
            transfer_refresh_pending: false,
            last_transfer_refresh: Instant::now()
                .checked_sub(Duration::from_secs(1))
                .unwrap_or_else(Instant::now),
            share_inspection: None,
            pending_transfer_cancel: None,
            forget_device_id: String::new(),
            pending_forget_device: None,
            mesh_quota_input: String::new(),
            capture_threshold_input: String::new(),
            pending_setting: None,
        };
        app.refresh_status();
        app.refresh_history();
        if mode == UiMode::Control {
            app.refresh_control_data();
        }
        Ok(app)
    }

    fn send(&mut self, command: UiCommand) {
        if self.ipc_worker.send(command).is_err() {
            self.daemon_error = Some("the local IPC worker stopped unexpectedly".to_owned());
        }
    }

    fn refresh_status(&mut self) {
        self.send(UiCommand::Status);
    }

    fn refresh_history(&mut self) {
        self.history_loading = true;
        self.history_error = None;
        self.pending_history_refresh = None;
        self.history_generation = self.history_generation.saturating_add(1);
        self.send(UiCommand::History {
            query: self.search.clone(),
            generation: self.history_generation,
        });
    }

    fn schedule_history_refresh(&mut self) {
        self.history_loading = true;
        self.history_error = None;
        self.history.clear();
        self.history_generation = self.history_generation.saturating_add(1);
        self.pending_history_refresh = Some(Instant::now() + SEARCH_DEBOUNCE);
        self.context.request_repaint_after(SEARCH_DEBOUNCE);
    }

    fn dispatch_pending_history_refresh(&mut self) {
        let Some(deadline) = self.pending_history_refresh else {
            return;
        };
        if Instant::now() < deadline {
            return;
        }
        self.pending_history_refresh = None;
        self.send(UiCommand::History {
            query: self.search.clone(),
            generation: self.history_generation,
        });
    }

    fn refresh_control_data(&mut self) {
        self.send(UiCommand::Peers);
        self.send(UiCommand::Config);
        self.send(UiCommand::Diagnostics);
        self.refresh_transfers();
    }

    fn refresh_transfers(&mut self) {
        if self.transfer_refresh_pending {
            return;
        }
        self.transfer_refresh_pending = true;
        self.last_transfer_refresh = Instant::now();
        self.send(UiCommand::Transfers);
    }

    fn dispatch_transfer_refresh(&mut self) {
        if self.mode == UiMode::Control
            && self.selected_tab == ControlTab::Transfers
            && !self.transfer_refresh_pending
            && self.last_transfer_refresh.elapsed() >= Duration::from_millis(500)
        {
            self.refresh_transfers();
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "event handling keeps each typed IPC result and its UI state transition together"
    )]
    fn poll_events(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                UiEvent::Status(result) => match result {
                    Ok(status) => {
                        self.status = Some(status);
                        self.daemon_error = None;
                    }
                    Err(error) => {
                        self.status = None;
                        self.daemon_error = Some(error);
                    }
                },
                UiEvent::History { generation, result } => {
                    if generation != self.history_generation {
                        continue;
                    }
                    self.history_loading = false;
                    match result {
                        Ok(history) => {
                            self.history = history.items;
                            self.selected_history = self
                                .selected_history
                                .min(self.history.len().saturating_sub(1));
                            self.history_error = None;
                        }
                        Err(error) => {
                            self.history.clear();
                            self.history_error = Some(error);
                            self.refresh_status();
                        }
                    }
                }
                UiEvent::Peers(result) => match result {
                    Ok(peers) => {
                        self.peers = Some(peers);
                        self.peers_error = None;
                    }
                    Err(error) => self.peers_error = Some(error),
                },
                UiEvent::Config(result) => match result {
                    Ok(config) => match serde_json::from_slice(&config.redacted_json) {
                        Ok(config) => {
                            if self.mesh_quota_input.is_empty() {
                                self.mesh_quota_input =
                                    config_pointer_u64(&config, "/shared/mesh_quota_bytes")
                                        .map_or_else(String::new, |value| value.to_string());
                            }
                            if self.capture_threshold_input.is_empty() {
                                self.capture_threshold_input =
                                    config_pointer_u64(&config, "/shared/capture_threshold_bytes")
                                        .map_or_else(String::new, |value| value.to_string());
                            }
                            self.config = Some(config);
                            self.config_error = None;
                        }
                        Err(error) => {
                            self.config_error = Some(format!("invalid config response: {error}"));
                        }
                    },
                    Err(error) => self.config_error = Some(error),
                },
                UiEvent::Diagnostics(result) => match result {
                    Ok(diagnostics) => {
                        self.diagnostics = diagnostics.checks;
                        self.diagnostics_error = None;
                    }
                    Err(error) => self.diagnostics_error = Some(error),
                },
                UiEvent::Transfers(result) => {
                    self.transfer_refresh_pending = false;
                    self.last_transfer_refresh = Instant::now();
                    match result {
                        Ok(transfers) => {
                            self.transfers = transfers.transfers;
                            self.transfers_error = None;
                        }
                        Err(error) => self.transfers_error = Some(error),
                    }
                }
                UiEvent::Share(result) => {
                    self.mutation_pending = false;
                    match result {
                        Ok(result) if result.shared => {
                            self.share_inspection = None;
                            self.notice = Some(Notice::success(format!(
                                "{} · transfer {} · content {}",
                                result.message,
                                result.transfer_id.as_deref().unwrap_or("unavailable"),
                                result.content_id.as_deref().unwrap_or("unavailable"),
                            )));
                            self.refresh_history();
                            self.refresh_transfers();
                        }
                        Ok(result) if result.confirmation_required => {
                            self.notice = None;
                            self.share_inspection = Some(result);
                        }
                        Ok(result) => {
                            self.notice = Some(Notice::error(result.message));
                        }
                        Err(error) => self.notice = Some(Notice::error(error)),
                    }
                }
                UiEvent::Mutation { kind, result } => {
                    self.mutation_pending = false;
                    match result {
                        Ok(result) if result.ok => {
                            self.notice = Some(Notice::success(result.message));
                            if kind == MutationKind::ActivateSwitcher {
                                self.switcher_state = SwitcherState::Close;
                            } else if kind.refreshes_history() {
                                self.refresh_history();
                            }
                            if kind == MutationKind::TransferCancel {
                                self.pending_transfer_cancel = None;
                                self.refresh_transfers();
                                self.refresh_history();
                            }
                            if kind == MutationKind::ForgetDevice {
                                self.pending_forget_device = None;
                                self.forget_device_id.clear();
                                self.send(UiCommand::Peers);
                            }
                            if kind == MutationKind::Setting {
                                self.pending_setting = None;
                                self.mesh_quota_input.clear();
                                self.capture_threshold_input.clear();
                                self.send(UiCommand::Config);
                            }
                        }
                        Ok(_) => {
                            self.notice = Some(Notice::error(
                                "the daemon rejected the operation without a reason",
                            ));
                        }
                        Err(error) => self.notice = Some(Notice::error(error)),
                    }
                }
            }
        }
    }

    fn retry_connection(&mut self) {
        self.status = None;
        self.daemon_error = None;
        self.history_error = None;
        self.peers_error = None;
        self.config_error = None;
        self.diagnostics_error = None;
        self.transfers_error = None;
        self.send(UiCommand::RetryStatus);
        self.refresh_history();
        if self.mode == UiMode::Control {
            self.refresh_control_data();
        }
    }

    fn switcher(&mut self, ui: &mut egui::Ui) {
        let pressed_key = ui.input(switcher_key);
        if pressed_key == SwitcherKey::Escape {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if self.switcher_state == SwitcherState::Close {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        Frame::new()
            .fill(BACKGROUND)
            .inner_margin(Margin::same(22))
            .show(ui, |ui| {
                brand_header(ui, "history switcher");
                ui.add_space(8.0);

                let search = ui.add_sized(
                    [ui.available_width(), 42.0],
                    egui::TextEdit::singleline(&mut self.search)
                        .hint_text("Search history")
                        .font(FontId::proportional(16.0)),
                );
                if self.switcher_state == SwitcherState::NeedsFocus {
                    search.request_focus();
                    self.switcher_state = SwitcherState::Ready;
                }
                if search.changed() {
                    self.selected_history = 0;
                    self.schedule_history_refresh();
                }

                match apply_switcher_key(
                    pressed_key,
                    &mut self.selected_history,
                    self.history.len(),
                    self.history_loading || self.mutation_pending,
                ) {
                    SwitcherIntent::None => {}
                    SwitcherIntent::Moved => self.scroll_selected_history = true,
                    SwitcherIntent::Close => {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                        return;
                    }
                    SwitcherIntent::Activate => {
                        let content_id = self.history[self.selected_history].content_id.clone();
                        self.mutation_pending = true;
                        self.notice = None;
                        self.send(UiCommand::Activate {
                            content_id,
                            kind: MutationKind::ActivateSwitcher,
                        });
                    }
                }

                ui.add_space(6.0);
                if let Some(error) = self.daemon_error.clone() {
                    let socket = self.paths.socket.clone();
                    unavailable_panel(ui, &socket, &error, || self.retry_connection());
                } else if let Some(error) = &self.history_error {
                    message_panel(ui, error, ERROR);
                } else if self.history_loading && self.history.is_empty() {
                    message_panel(ui, "Loading history…", MUTED);
                } else if self.history.is_empty() {
                    message_panel(ui, "No matching clipboard history", MUTED);
                } else {
                    self.history_list(ui, false);
                }

                if let Some(notice) = &self.notice {
                    notice.show(ui);
                }
                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    ui.label(
                        RichText::new("↑↓ navigate    Enter activate    Esc close")
                            .monospace()
                            .color(MUTED)
                            .size(11.0),
                    );
                });
            });
    }

    fn control(&mut self, ui: &mut egui::Ui) {
        Frame::new()
            .fill(BACKGROUND)
            .inner_margin(Margin::same(20))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading(RichText::new("clip-sync").color(Color32::WHITE));
                    ui.label(
                        RichText::new("CONTROL CENTER")
                            .color(CYAN)
                            .monospace()
                            .size(11.0),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let (color, text) = if self.daemon_error.is_some() {
                            (ERROR, "daemon unavailable".to_owned())
                        } else if let Some(status) = &self.status {
                            (
                                SUCCESS,
                                format!("{} · {} peers", status.hostname, status.discovered_peers),
                            )
                        } else {
                            (MUTED, "connecting to daemon…".to_owned())
                        };
                        ui.label(RichText::new(text).color(MUTED).size(12.0));
                        ui.colored_label(color, "●");
                    });
                });
                ui.separator();

                if let Some(error) = self.daemon_error.clone() {
                    let socket = self.paths.socket.clone();
                    unavailable_banner(ui, &socket, &error, || self.retry_connection());
                }
                if let Some(notice) = &self.notice {
                    notice.show(ui);
                }

                ui.horizontal(|ui| {
                    for tab in ControlTab::ALL {
                        let selected = self.selected_tab == tab;
                        if ui.selectable_label(selected, tab.label()).clicked() {
                            self.selected_tab = tab;
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Refresh").clicked() {
                            self.retry_connection();
                        }
                    });
                });
                ui.add_space(12.0);

                match self.selected_tab {
                    ControlTab::History => self.history_tab(ui),
                    ControlTab::Transfers => {
                        ScrollArea::vertical().show(ui, |ui| self.transfers_tab(ui));
                    }
                    ControlTab::Peers => {
                        ScrollArea::vertical().show(ui, |ui| self.peers_tab(ui));
                    }
                    ControlTab::Settings => {
                        ScrollArea::vertical().show(ui, |ui| self.settings_tab(ui));
                    }
                    ControlTab::Diagnostics => {
                        ScrollArea::vertical().show(ui, |ui| self.diagnostics_tab(ui));
                    }
                }
            });
    }

    fn history_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let search = ui.add_sized(
                [ui.available_width() - 210.0, 36.0],
                egui::TextEdit::singleline(&mut self.search).hint_text("Search merged history"),
            );
            if search.changed() {
                self.selected_history = 0;
                self.schedule_history_refresh();
            }
            if ui
                .add_enabled(
                    !self.mutation_pending,
                    egui::Button::new("Share current clipboard"),
                )
                .clicked()
            {
                self.mutation_pending = true;
                self.notice = None;
                self.share_inspection = None;
                self.send(UiCommand::ShareClipboard { confirmed: false });
            }
        });
        if let Some(inspection) = self.share_inspection.clone() {
            Frame::new()
                .fill(SURFACE)
                .stroke(Stroke::new(1.0, CYAN))
                .corner_radius(CornerRadius::same(7))
                .inner_margin(Margin::same(12))
                .show(ui, |ui| {
                    ui.strong("Confirm explicit share");
                    ui.label(&inspection.message);
                    ui.label(
                        RichText::new(format!(
                            "{} · {}{}",
                            format_bytes(inspection.logical_size),
                            inspection.mime_types.join(", "),
                            if inspection.quota_exempt {
                                " · quota-exempt"
                            } else {
                                ""
                            }
                        ))
                        .color(MUTED),
                    );
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(!self.mutation_pending, egui::Button::new("Confirm share"))
                            .clicked()
                        {
                            self.mutation_pending = true;
                            self.send(UiCommand::ShareClipboard { confirmed: true });
                        }
                        if ui
                            .add_enabled(!self.mutation_pending, egui::Button::new("Cancel"))
                            .clicked()
                        {
                            self.share_inspection = None;
                        }
                    });
                });
        }
        ui.label(
            RichText::new(
                "Items with locally available payloads can be activated. Pins and deletes replicate.",
            )
            .color(MUTED)
            .size(12.0),
        );
        ui.add_space(6.0);
        if let Some(error) = &self.history_error {
            message_panel(ui, error, ERROR);
        } else if self.history.is_empty() {
            message_panel(
                ui,
                if self.history_loading {
                    "Loading history…"
                } else {
                    "No matching clipboard history"
                },
                MUTED,
            );
        } else {
            self.history_list(ui, true);
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the list keeps row rendering and its keyboard/mutation actions together"
    )]
    fn history_list(&mut self, ui: &mut egui::Ui, show_actions: bool) {
        let mut action = None;
        ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (index, item) in self.history.iter().enumerate() {
                    let selected = index == self.selected_history;
                    let row = Frame::new()
                        .fill(if selected {
                            Color32::from_rgb(24, 48, 54)
                        } else {
                            SURFACE
                        })
                        .stroke(Stroke::new(1.0, if selected { CYAN } else { BORDER }))
                        .corner_radius(CornerRadius::same(7))
                        .inner_margin(Margin::same(12))
                        .show(ui, |ui| {
                            let response = ui
                                .horizontal(|ui| {
                                    ui.vertical(|ui| {
                                        ui.set_min_width(
                                            (ui.available_width()
                                                - if show_actions { 220.0 } else { 20.0 })
                                            .max(120.0),
                                        );
                                        let title = if item.preview.trim().is_empty() {
                                            "Binary clipboard content"
                                        } else {
                                            item.preview.trim()
                                        };
                                        ui.label(
                                            RichText::new(title).color(Color32::WHITE).size(14.0),
                                        );
                                        let mime = item
                                            .mime_types
                                            .first()
                                            .map_or("unknown", String::as_str);
                                        ui.label(
                                            RichText::new(format!(
                                                "{} · {} · {}{}",
                                                item.source_node,
                                                mime,
                                                format_bytes(item.logical_size),
                                                if item.pinned { " · pinned" } else { "" }
                                            ))
                                            .color(MUTED)
                                            .monospace()
                                            .size(11.0),
                                        );
                                    });
                                    if show_actions {
                                        if ui.button("Activate").clicked() {
                                            action = Some(HistoryAction::Activate(
                                                item.content_id.clone(),
                                            ));
                                        }
                                        if ui
                                            .button(if item.pinned { "Unpin" } else { "Pin" })
                                            .clicked()
                                        {
                                            action = Some(HistoryAction::Pin {
                                                content_id: item.content_id.clone(),
                                                pinned: !item.pinned,
                                            });
                                        }
                                        if ui.button("Delete").clicked() {
                                            action = Some(HistoryAction::Delete(
                                                item.content_id.clone(),
                                            ));
                                        }
                                    }
                                })
                                .response;
                            if response.clicked() {
                                self.selected_history = index;
                            }
                            if !show_actions && response.double_clicked() {
                                action = Some(HistoryAction::Activate(item.content_id.clone()));
                            }
                        });
                    if selected && self.scroll_selected_history {
                        row.response.scroll_to_me(Some(egui::Align::Center));
                    }
                }
            });
        self.scroll_selected_history = false;

        if self.mutation_pending {
            return;
        }
        match action {
            Some(HistoryAction::Activate(content_id)) => {
                self.mutation_pending = true;
                self.notice = None;
                self.send(UiCommand::Activate {
                    content_id,
                    kind: if self.mode == UiMode::Switcher {
                        MutationKind::ActivateSwitcher
                    } else {
                        MutationKind::Activate
                    },
                });
            }
            Some(HistoryAction::Pin { content_id, pinned }) => {
                self.mutation_pending = true;
                self.notice = None;
                self.send(UiCommand::HistoryUpdate {
                    content_id,
                    action: if pinned {
                        HistoryUpdateAction::Pin
                    } else {
                        HistoryUpdateAction::Unpin
                    },
                    kind: MutationKind::Pin,
                });
            }
            Some(HistoryAction::Delete(content_id)) => {
                self.mutation_pending = true;
                self.notice = None;
                self.send(UiCommand::HistoryUpdate {
                    content_id,
                    action: HistoryUpdateAction::Delete,
                    kind: MutationKind::Delete,
                });
            }
            None => {}
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "transfer rendering keeps progress and confirmed cancellation together"
    )]
    fn transfers_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Transfers");
            if ui
                .add_enabled(
                    !self.transfer_refresh_pending,
                    egui::Button::new("Refresh progress"),
                )
                .clicked()
            {
                self.refresh_transfers();
            }
        });
        if let Some(error) = &self.transfers_error {
            message_panel(ui, error, ERROR);
            return;
        }
        if self.transfers.is_empty() {
            message_panel(ui, "No transfers", MUTED);
            return;
        }

        let mut requested_cancel = None;
        for transfer in &self.transfers {
            Frame::new()
                .fill(SURFACE)
                .stroke(Stroke::new(1.0, BORDER))
                .corner_radius(CornerRadius::same(7))
                .inner_margin(Margin::same(12))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.monospace(&transfer.transfer_id);
                            ui.label(
                                RichText::new(format!(
                                    "{} · content {} · peer {}",
                                    transfer.state,
                                    if transfer.content_id.is_empty() {
                                        "pending"
                                    } else {
                                        &transfer.content_id
                                    },
                                    if transfer.peer.is_empty() {
                                        "unknown"
                                    } else {
                                        &transfer.peer
                                    },
                                ))
                                .color(MUTED)
                                .size(11.0),
                            );
                            let per_mille = u16::try_from(
                                u128::from(transfer.completed_bytes)
                                    .saturating_mul(1000)
                                    .checked_div(u128::from(transfer.total_bytes))
                                    .unwrap_or(0)
                                    .min(1000),
                            )
                            .unwrap_or(1000);
                            let fraction = f32::from(per_mille) / 1000.0;
                            ui.add(
                                egui::ProgressBar::new(fraction)
                                    .text(format!(
                                        "{} / {}",
                                        format_bytes(transfer.completed_bytes),
                                        format_bytes(transfer.total_bytes)
                                    ))
                                    .desired_width(ui.available_width().max(160.0)),
                            );
                        });
                        if !matches!(transfer.state.as_str(), "complete" | "cancelled" | "failed")
                            && ui
                                .add_enabled(
                                    !self.mutation_pending,
                                    egui::Button::new("Cancel transfer"),
                                )
                                .clicked()
                        {
                            requested_cancel = Some(transfer.transfer_id.clone());
                        }
                    });
                });
        }
        if let Some(transfer_id) = requested_cancel {
            self.pending_transfer_cancel = Some(transfer_id);
        }
        if let Some(transfer_id) = self.pending_transfer_cancel.clone() {
            Frame::new()
                .fill(SURFACE)
                .stroke(Stroke::new(1.0, ERROR))
                .corner_radius(CornerRadius::same(7))
                .inner_margin(Margin::same(12))
                .show(ui, |ui| {
                    ui.strong("Confirm transfer cancellation");
                    ui.label(format!(
                        "Cancel {transfer_id}? Partial local staging will be cleaned and cancellation will replicate."
                    ));
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(
                                !self.mutation_pending,
                                egui::Button::new("Confirm cancel"),
                            )
                            .clicked()
                        {
                            self.mutation_pending = true;
                            self.notice = None;
                            self.send(UiCommand::TransferCancel { transfer_id });
                        }
                        if ui
                            .add_enabled(!self.mutation_pending, egui::Button::new("Keep transfer"))
                            .clicked()
                        {
                            self.pending_transfer_cancel = None;
                        }
                    });
                });
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "peer discovery and remembered-device controls share one compact tab"
    )]
    fn peers_tab(&mut self, ui: &mut egui::Ui) {
        if let Some(error) = &self.peers_error {
            message_panel(ui, error, ERROR);
            return;
        }
        let Some(peers) = &self.peers else {
            message_panel(ui, "Loading peer discovery…", MUTED);
            return;
        };
        ui.heading("Peers");
        ui.label(
            RichText::new(format!(
                "Local: {} ({})",
                peers.local_hostname,
                peers
                    .local_address
                    .as_deref()
                    .unwrap_or("address unavailable")
            ))
            .color(MUTED),
        );
        if let Some(error) = &peers.discovery_error {
            ui.colored_label(ERROR, format!("NetBird discovery: {error}"));
        }
        ui.add_space(8.0);
        if peers.peers.is_empty() {
            message_panel(ui, "No peers discovered", MUTED);
        }
        for peer in &peers.peers {
            peer_row(ui, peer);
        }
        ui.add_space(8.0);
        ui.heading("Remembered mesh devices");
        ui.label(
            RichText::new(
                "Forgetting removes a replication identity from history maintenance; it does not revoke the shared mesh secret.",
            )
            .color(MUTED)
            .size(12.0),
        );
        let mut requested_forget = None;
        for device in &peers.devices {
            ui.horizontal(|ui| {
                let state = if device.local {
                    "local"
                } else if device.forgotten {
                    "forgotten"
                } else {
                    "remembered"
                };
                ui.monospace(&device.device_id);
                ui.label(RichText::new(state).color(MUTED));
                if !device.local
                    && !device.forgotten
                    && ui
                        .add_enabled(!self.mutation_pending, egui::Button::new("Forget"))
                        .clicked()
                {
                    requested_forget = Some(device.device_id.clone());
                }
            });
        }
        if let Some(device_id) = requested_forget {
            self.pending_forget_device = Some(device_id);
        }
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.forget_device_id)
                    .hint_text("Stable device UUID"),
            );
            if ui
                .add_enabled(
                    !self.forget_device_id.trim().is_empty() && !self.mutation_pending,
                    egui::Button::new("Review forget"),
                )
                .clicked()
            {
                self.pending_forget_device = Some(self.forget_device_id.trim().to_owned());
            }
        });
        if let Some(device_id) = self.pending_forget_device.clone() {
            Frame::new()
                .fill(SURFACE)
                .stroke(Stroke::new(1.0, ERROR))
                .corner_radius(CornerRadius::same(7))
                .inner_margin(Margin::same(12))
                .show(ui, |ui| {
                    ui.strong("Confirm device forget");
                    ui.label(format!(
                        "Forget {device_id}? A machine holding the mesh secret can rejoin with a new identity."
                    ));
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(
                                !self.mutation_pending,
                                egui::Button::new("Confirm forget"),
                            )
                            .clicked()
                        {
                            self.mutation_pending = true;
                            self.notice = None;
                            self.send(UiCommand::ForgetDevice { device_id });
                        }
                        if ui
                            .add_enabled(!self.mutation_pending, egui::Button::new("Keep device"))
                            .clicked()
                        {
                            self.pending_forget_device = None;
                        }
                    });
                });
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "effective settings and their validated update forms are presented together"
    )]
    fn settings_tab(&mut self, ui: &mut egui::Ui) {
        if let Some(error) = &self.config_error {
            message_panel(ui, error, ERROR);
            return;
        }
        let Some(config) = self.config.clone() else {
            message_panel(ui, "Loading effective configuration…", MUTED);
            return;
        };
        ui.heading("Effective settings");
        ui.label(
            RichText::new("Shared values replicate immediately; local secret paths stay redacted.")
                .color(MUTED),
        );
        ui.add_space(8.0);
        setting_row(
            ui,
            "Mesh quota",
            config_pointer_u64(&config, "/shared/mesh_quota_bytes")
                .map_or_else(|| "unavailable".to_owned(), format_bytes),
        );
        setting_row(
            ui,
            "Capture threshold",
            config_pointer_u64(&config, "/shared/capture_threshold_bytes")
                .map_or_else(|| "unavailable".to_owned(), format_bytes),
        );
        ui.horizontal(|ui| {
            ui.label("Mesh quota bytes");
            ui.add(egui::TextEdit::singleline(&mut self.mesh_quota_input).desired_width(180.0));
            if ui
                .add_enabled(
                    !self.mutation_pending
                        && self
                            .mesh_quota_input
                            .parse::<u64>()
                            .is_ok_and(|value| value > 0),
                    egui::Button::new("Review quota"),
                )
                .clicked()
                && let Ok(value) = self.mesh_quota_input.parse()
            {
                self.pending_setting = Some(PendingSetting {
                    kind: SharedSettingKind::MeshQuotaBytes,
                    value,
                });
            }
        });
        ui.horizontal(|ui| {
            ui.label("Capture threshold bytes");
            ui.add(
                egui::TextEdit::singleline(&mut self.capture_threshold_input).desired_width(180.0),
            );
            if ui
                .add_enabled(
                    !self.mutation_pending
                        && self
                            .capture_threshold_input
                            .parse::<u64>()
                            .is_ok_and(|value| value > 0),
                    egui::Button::new("Review threshold"),
                )
                .clicked()
                && let Ok(value) = self.capture_threshold_input.parse()
            {
                self.pending_setting = Some(PendingSetting {
                    kind: SharedSettingKind::CaptureThresholdBytes,
                    value,
                });
            }
        });
        if let Some(pending) = self.pending_setting {
            Frame::new()
                .fill(SURFACE)
                .stroke(Stroke::new(
                    1.0,
                    if pending.kind == SharedSettingKind::MeshQuotaBytes {
                        ERROR
                    } else {
                        CYAN
                    },
                ))
                .corner_radius(CornerRadius::same(7))
                .inner_margin(Margin::same(12))
                .show(ui, |ui| {
                    let label = pending.label();
                    ui.strong(format!("Confirm {label} update"));
                    ui.label(format!(
                        "Set {label} to {}? This is a replicated mesh-wide setting. Lowering quota may delete unpinned history deterministically.",
                        format_bytes(pending.value)
                    ));
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(
                                !self.mutation_pending,
                                egui::Button::new("Apply setting"),
                            )
                            .clicked()
                        {
                            self.mutation_pending = true;
                            self.notice = None;
                            self.send(UiCommand::UpdateSharedSetting {
                                setting: pending.kind,
                                value: pending.value,
                            });
                        }
                        if ui
                            .add_enabled(!self.mutation_pending, egui::Button::new("Cancel"))
                            .clicked()
                        {
                            self.pending_setting = None;
                        }
                    });
                });
        }
        ui.separator();
        setting_row(
            ui,
            "Listen port",
            config_pointer(&config, "/local/listen_port"),
        );
        setting_row(
            ui,
            "Discovery interval",
            config_seconds(&config, "/local/discovery_interval_seconds"),
        );
        setting_row(
            ui,
            "Reconciliation interval",
            config_seconds(&config, "/local/reconcile_interval_seconds"),
        );
        setting_row(
            ui,
            "Reconnect delay",
            match (
                config_pointer_u64(&config, "/local/reconnect_min_seconds"),
                config_pointer_u64(&config, "/local/reconnect_max_seconds"),
            ) {
                (Some(minimum), Some(maximum)) => format!("{minimum}–{maximum} seconds"),
                _ => "unavailable".to_owned(),
            },
        );
        setting_row(
            ui,
            "NetBird command",
            config_pointer(&config, "/local/netbird_command"),
        );
        setting_row(
            ui,
            "Mesh key file",
            match config
                .pointer("/local/mesh_key_file_configured")
                .and_then(serde_json::Value::as_bool)
            {
                Some(true) => "configured (path redacted)".to_owned(),
                Some(false) => "not configured".to_owned(),
                None => "unavailable".to_owned(),
            },
        );
        setting_row(ui, "Config", config_pointer(&config, "/local/config_path"));
    }

    fn diagnostics_tab(&self, ui: &mut egui::Ui) {
        if let Some(error) = &self.diagnostics_error {
            message_panel(ui, error, ERROR);
            return;
        }
        if self.diagnostics.is_empty() {
            message_panel(ui, "Loading live diagnostics…", MUTED);
            return;
        }
        ui.heading("Diagnostics");
        ui.label(
            RichText::new("These checks reflect the running daemon, not a second local probe.")
                .color(MUTED),
        );
        ui.add_space(8.0);
        for check in &self.diagnostics {
            Frame::new()
                .fill(SURFACE)
                .stroke(Stroke::new(1.0, BORDER))
                .corner_radius(CornerRadius::same(7))
                .inner_margin(Margin::same(12))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.colored_label(if check.ok { SUCCESS } else { ERROR }, "●");
                        ui.strong(&check.name);
                        ui.label(RichText::new(&check.detail).color(MUTED));
                    });
                });
        }
    }
}

impl eframe::App for ClipSyncApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_events();
        self.dispatch_pending_history_refresh();
        self.dispatch_transfer_refresh();
        match self.mode {
            UiMode::Switcher => self.switcher(ui),
            UiMode::Control => self.control(ui),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ControlTab {
    History,
    Transfers,
    Peers,
    Settings,
    Diagnostics,
}

impl ControlTab {
    const ALL: [Self; 5] = [
        Self::History,
        Self::Transfers,
        Self::Peers,
        Self::Settings,
        Self::Diagnostics,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::History => "History",
            Self::Transfers => "Transfers",
            Self::Peers => "Peers",
            Self::Settings => "Settings",
            Self::Diagnostics => "Diagnostics",
        }
    }
}

enum HistoryAction {
    Activate(String),
    Pin { content_id: String, pinned: bool },
    Delete(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MutationKind {
    Activate,
    ActivateSwitcher,
    Pin,
    Delete,
    TransferCancel,
    ForgetDevice,
    Setting,
}

impl MutationKind {
    const fn refreshes_history(self) -> bool {
        matches!(self, Self::Activate | Self::Pin | Self::Delete)
    }
}

#[derive(Clone, Copy)]
struct PendingSetting {
    kind: SharedSettingKind,
    value: u64,
}

impl PendingSetting {
    const fn label(self) -> &'static str {
        match self.kind {
            SharedSettingKind::MeshQuotaBytes => "mesh quota",
            SharedSettingKind::CaptureThresholdBytes => "capture threshold",
            SharedSettingKind::Unspecified => "shared setting",
        }
    }
}

enum UiCommand {
    Status,
    RetryStatus,
    History {
        query: String,
        generation: u64,
    },
    Peers,
    Config,
    Diagnostics,
    Transfers,
    ShareClipboard {
        confirmed: bool,
    },
    TransferCancel {
        transfer_id: String,
    },
    ForgetDevice {
        device_id: String,
    },
    UpdateSharedSetting {
        setting: SharedSettingKind,
        value: u64,
    },
    Activate {
        content_id: String,
        kind: MutationKind,
    },
    HistoryUpdate {
        content_id: String,
        action: HistoryUpdateAction,
        kind: MutationKind,
    },
}

enum UiEvent {
    Status(Result<StatusResponse, String>),
    History {
        generation: u64,
        result: Result<HistoryResponse, String>,
    },
    Peers(Result<PeersResponse, String>),
    Config(Result<ConfigResponse, String>),
    Diagnostics(Result<DiagnosticsResponse, String>),
    Transfers(Result<TransfersResponse, String>),
    Share(Result<ShareClipboardResponse, String>),
    Mutation {
        kind: MutationKind,
        result: Result<MutationResponse, String>,
    },
}

struct IpcWorker {
    command_tx: Option<std_mpsc::Sender<UiCommand>>,
    shutdown: CancellationToken,
    thread: Option<JoinHandle<()>>,
}

impl IpcWorker {
    fn send(&self, command: UiCommand) -> Result<(), std_mpsc::SendError<UiCommand>> {
        self.command_tx
            .as_ref()
            .expect("IPC sender exists until worker drop")
            .send(command)
    }
}

impl Drop for IpcWorker {
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.command_tx.take();
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            tracing::debug!("local UI IPC worker panicked during shutdown");
        }
    }
}

fn spawn_ipc_worker(
    socket: PathBuf,
    event_tx: std_mpsc::Sender<UiEvent>,
    context: egui::Context,
) -> IpcWorker {
    let (command_tx, command_rx) = std_mpsc::channel();
    let shutdown = CancellationToken::new();
    let worker_shutdown = shutdown.clone();
    let thread = std::thread::Builder::new()
        .name("clip-sync-ui-ipc".to_owned())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = event_tx.send(UiEvent::Status(Err(format!(
                        "could not start the local IPC runtime: {error}"
                    ))));
                    context.request_repaint();
                    return;
                }
            };
            let mut request_id = 0_u64;
            let mut start_attempted = false;
            while let Ok(command) = command_rx.recv() {
                request_id = request_id.saturating_add(1);
                if matches!(&command, UiCommand::RetryStatus) {
                    start_attempted = false;
                }
                let (body, target) = command.request_body();
                let response = runtime.block_on(async {
                    tokio::select! {
                        biased;
                        () = worker_shutdown.cancelled() => None,
                        response = request_with_daemon_start(
                            &socket,
                            Request {
                                protocol_version: IPC_PROTOCOL_VERSION,
                                request_id,
                                body: Some(body),
                            },
                            &mut start_attempted,
                        ) => Some(response),
                    }
                });
                let Some(response) = response else {
                    break;
                };
                let event = target.into_event(response);
                if event_tx.send(event).is_err() {
                    break;
                }
                context.request_repaint();
            }
        })
        .ok();
    IpcWorker {
        command_tx: Some(command_tx),
        shutdown,
        thread,
    }
}

impl UiCommand {
    fn request_body(self) -> (request::Body, EventTarget) {
        match self {
            Self::Status | Self::RetryStatus => {
                (request::Body::Status(StatusRequest {}), EventTarget::Status)
            }
            Self::History { query, generation } => (
                request::Body::History(HistoryRequest { query, limit: 200 }),
                EventTarget::History(generation),
            ),
            Self::Peers => (request::Body::Peers(PeersRequest {}), EventTarget::Peers),
            Self::Config => (request::Body::Config(ConfigRequest {}), EventTarget::Config),
            Self::Diagnostics => (
                request::Body::Diagnostics(DiagnosticsRequest {}),
                EventTarget::Diagnostics,
            ),
            Self::Transfers => (
                request::Body::Transfers(TransfersRequest {}),
                EventTarget::Transfers,
            ),
            Self::ShareClipboard { confirmed } => (
                request::Body::ShareClipboard(ShareClipboardRequest { confirmed }),
                EventTarget::Share,
            ),
            Self::TransferCancel { transfer_id } => (
                request::Body::TransferCancel(TransferCancelRequest { transfer_id }),
                EventTarget::Mutation(MutationKind::TransferCancel),
            ),
            Self::ForgetDevice { device_id } => (
                request::Body::ForgetDevice(ForgetDeviceRequest { device_id }),
                EventTarget::Mutation(MutationKind::ForgetDevice),
            ),
            Self::UpdateSharedSetting { setting, value } => (
                request::Body::SharedSettingUpdate(SharedSettingUpdateRequest {
                    setting: setting as i32,
                    value,
                }),
                EventTarget::Mutation(MutationKind::Setting),
            ),
            Self::Activate { content_id, kind } => (
                request::Body::Activate(ActivateRequest { content_id }),
                EventTarget::Mutation(kind),
            ),
            Self::HistoryUpdate {
                content_id,
                action,
                kind,
            } => (
                request::Body::HistoryUpdate(HistoryUpdateRequest {
                    content_id,
                    action: action as i32,
                }),
                EventTarget::Mutation(kind),
            ),
        }
    }
}

enum EventTarget {
    Status,
    History(u64),
    Peers,
    Config,
    Diagnostics,
    Transfers,
    Share,
    Mutation(MutationKind),
}

impl EventTarget {
    fn into_event(self, result: Result<Response, String>) -> UiEvent {
        match self {
            Self::Status => UiEvent::Status(expect_status(result)),
            Self::History(generation) => UiEvent::History {
                generation,
                result: expect_history(result),
            },
            Self::Peers => UiEvent::Peers(expect_peers(result)),
            Self::Config => UiEvent::Config(expect_config(result)),
            Self::Diagnostics => UiEvent::Diagnostics(expect_diagnostics(result)),
            Self::Transfers => UiEvent::Transfers(expect_transfers(result)),
            Self::Share => UiEvent::Share(expect_share(result)),
            Self::Mutation(kind) => UiEvent::Mutation {
                kind,
                result: expect_mutation(result),
            },
        }
    }
}

async fn request_with_daemon_start(
    socket: &Path,
    request: Request,
    start_attempted: &mut bool,
) -> Result<Response, String> {
    match ipc::request(socket, request.clone()).await {
        Ok(response) => {
            *start_attempted = false;
            return Ok(response);
        }
        Err(error) if !is_daemon_absent(&error) => return Err(error.to_string()),
        Err(_) if *start_attempted => {}
        Err(_) => {
            *start_attempted = true;
            let start_detail = start_user_service().await;
            for _ in 0..20 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                match ipc::request(socket, request.clone()).await {
                    Ok(response) => {
                        *start_attempted = false;
                        return Ok(response);
                    }
                    Err(error) if is_daemon_absent(&error) => {}
                    Err(error) => return Err(error.to_string()),
                }
            }
            return Err(format!(
                "daemon did not become ready at {} ({start_detail})",
                socket.display()
            ));
        }
    }

    Err(format!(
        "daemon is unavailable at {}; start clip-sync.service or run `clip-sync daemon`",
        socket.display()
    ))
}

fn is_daemon_absent(error: &ipc::IpcError) -> bool {
    matches!(
        error,
        ipc::IpcError::Io(io_error)
            if matches!(
                io_error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            )
    )
}

async fn start_user_service() -> String {
    let mut command = tokio::process::Command::new("systemctl");
    command.args(["--user", "start", "clip-sync.service"]);
    command.kill_on_drop(true);
    match tokio::time::timeout(Duration::from_secs(3), command.output()).await {
        Ok(Ok(output)) if output.status.success() => {
            "requested systemd user service start".to_owned()
        }
        Ok(Ok(output)) => {
            let detail = String::from_utf8_lossy(&output.stderr);
            let detail = detail.trim();
            if detail.is_empty() {
                format!("systemctl exited with {}", output.status)
            } else {
                format!("systemctl: {detail}")
            }
        }
        Ok(Err(error)) => format!("could not run systemctl: {error}"),
        Err(_) => "systemctl start timed out".to_owned(),
    }
}

fn response_error(response: Response) -> Result<response::Body, String> {
    match response.body {
        Some(response::Body::Error(error)) => Err(format!("{}: {}", error.code, error.message)),
        Some(body) => Ok(body),
        None => Err("daemon returned an empty response".to_owned()),
    }
}

fn expect_status(result: Result<Response, String>) -> Result<StatusResponse, String> {
    match response_error(result?)? {
        response::Body::Status(value) => Ok(value),
        _ => Err("daemon returned an unexpected status response".to_owned()),
    }
}

fn expect_history(result: Result<Response, String>) -> Result<HistoryResponse, String> {
    match response_error(result?)? {
        response::Body::History(value) => Ok(value),
        _ => Err("daemon returned an unexpected history response".to_owned()),
    }
}

fn expect_peers(result: Result<Response, String>) -> Result<PeersResponse, String> {
    match response_error(result?)? {
        response::Body::Peers(value) => Ok(value),
        _ => Err("daemon returned an unexpected peers response".to_owned()),
    }
}

fn expect_config(result: Result<Response, String>) -> Result<ConfigResponse, String> {
    match response_error(result?)? {
        response::Body::Config(value) => Ok(value),
        _ => Err("daemon returned an unexpected config response".to_owned()),
    }
}

fn expect_diagnostics(result: Result<Response, String>) -> Result<DiagnosticsResponse, String> {
    match response_error(result?)? {
        response::Body::Diagnostics(value) => Ok(value),
        _ => Err("daemon returned an unexpected diagnostics response".to_owned()),
    }
}

fn expect_transfers(result: Result<Response, String>) -> Result<TransfersResponse, String> {
    match response_error(result?)? {
        response::Body::Transfers(value) => Ok(value),
        _ => Err("daemon returned an unexpected transfers response".to_owned()),
    }
}

fn expect_share(result: Result<Response, String>) -> Result<ShareClipboardResponse, String> {
    match response_error(result?)? {
        response::Body::ShareClipboard(value) => Ok(value),
        _ => Err("daemon returned an unexpected clipboard-share response".to_owned()),
    }
}

fn expect_mutation(result: Result<Response, String>) -> Result<MutationResponse, String> {
    match response_error(result?)? {
        response::Body::Mutation(value) => Ok(value),
        _ => Err("daemon returned an unexpected mutation response".to_owned()),
    }
}

fn brand_header(ui: &mut egui::Ui, subtitle: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("CLIP").strong().color(CYAN).size(13.0));
        ui.label(
            RichText::new("SYNC")
                .strong()
                .color(Color32::WHITE)
                .size(13.0),
        );
        ui.add_space(6.0);
        ui.label(RichText::new(subtitle).color(MUTED).size(12.0));
    });
}

fn unavailable_panel(ui: &mut egui::Ui, socket: &Path, error: &str, retry: impl FnOnce()) {
    Frame::new()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0, ERROR))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::same(18))
        .show(ui, |ui| {
            ui.heading(RichText::new("Daemon unavailable").color(ERROR));
            ui.label(error);
            ui.label(
                RichText::new(format!("IPC socket: {}", socket.display()))
                    .monospace()
                    .color(MUTED),
            );
            ui.label(
                RichText::new(
                    "When the socket is absent, the UI requests the systemd user service. You can also run `clip-sync daemon`.",
                )
                .color(MUTED),
            );
            if ui.button("Retry").clicked() {
                retry();
            }
        });
}

fn unavailable_banner(ui: &mut egui::Ui, socket: &Path, error: &str, retry: impl FnOnce()) {
    ui.horizontal_wrapped(|ui| {
        ui.colored_label(ERROR, "Daemon unavailable");
        ui.label(error);
        ui.label(RichText::new(socket.display().to_string()).color(MUTED));
        if ui.small_button("Retry").clicked() {
            retry();
        }
    });
    ui.separator();
}

fn message_panel(ui: &mut egui::Ui, message: &str, color: Color32) {
    Frame::new()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::same(18))
        .show(ui, |ui| {
            ui.set_min_height(90.0);
            ui.vertical_centered(|ui| {
                ui.add_space(22.0);
                ui.label(RichText::new(message).color(color).size(14.0));
            });
        });
}

fn peer_row(ui: &mut egui::Ui, peer: &PeerItem) {
    Frame::new()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(7))
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.colored_label(if peer.connected { SUCCESS } else { MUTED }, "●");
                ui.strong(&peer.hostname);
                ui.label(RichText::new(&peer.address).color(MUTED).monospace());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(if peer.connected {
                        "connected"
                    } else {
                        "offline"
                    });
                });
            });
        });
}

fn setting_row(ui: &mut egui::Ui, name: &str, value: String) {
    Frame::new()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(7))
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.strong(name);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(value).color(MUTED).monospace());
                });
            });
        });
}

struct Notice {
    message: String,
    error: bool,
}

impl Notice {
    fn success(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            error: false,
        }
    }

    fn error(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            error: true,
        }
    }

    fn show(&self, ui: &mut egui::Ui) {
        ui.colored_label(if self.error { ERROR } else { SUCCESS }, &self.message);
    }
}

fn config_pointer(config: &serde_json::Value, pointer: &str) -> String {
    config.pointer(pointer).map_or_else(
        || "unavailable".to_owned(),
        |value| match value {
            serde_json::Value::String(value) => value.clone(),
            other => other.to_string(),
        },
    )
}

fn config_pointer_u64(config: &serde_json::Value, pointer: &str) -> Option<u64> {
    config.pointer(pointer).and_then(serde_json::Value::as_u64)
}

fn config_seconds(config: &serde_json::Value, pointer: &str) -> String {
    config_pointer_u64(config, pointer).map_or_else(
        || "unavailable".to_owned(),
        |value| format!("{value} seconds"),
    )
}

fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    if bytes >= GIB {
        format_unit(bytes, GIB, "GiB")
    } else if bytes >= MIB {
        format_unit(bytes, MIB, "MiB")
    } else if bytes >= KIB {
        format_unit(bytes, KIB, "KiB")
    } else {
        format!("{bytes} B")
    }
}

fn format_unit(bytes: u64, unit: u64, suffix: &str) -> String {
    let whole = bytes / unit;
    let decimal = (bytes % unit) * 10 / unit;
    format!("{whole}.{decimal} {suffix}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SwitcherKey {
    None,
    Up,
    Down,
    Enter,
    Escape,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SwitcherIntent {
    None,
    Moved,
    Activate,
    Close,
}

fn switcher_key(input: &egui::InputState) -> SwitcherKey {
    if input.key_pressed(Key::Escape) {
        SwitcherKey::Escape
    } else if input.key_pressed(Key::ArrowDown) {
        SwitcherKey::Down
    } else if input.key_pressed(Key::ArrowUp) {
        SwitcherKey::Up
    } else if input.key_pressed(Key::Enter) {
        SwitcherKey::Enter
    } else {
        SwitcherKey::None
    }
}

fn apply_switcher_key(
    key: SwitcherKey,
    selection: &mut usize,
    item_count: usize,
    activation_blocked: bool,
) -> SwitcherIntent {
    match key {
        SwitcherKey::Escape => SwitcherIntent::Close,
        SwitcherKey::Up => {
            *selection = move_selection(*selection, item_count, -1);
            SwitcherIntent::Moved
        }
        SwitcherKey::Down => {
            *selection = move_selection(*selection, item_count, 1);
            SwitcherIntent::Moved
        }
        SwitcherKey::Enter if item_count > 0 && !activation_blocked => {
            *selection = (*selection).min(item_count - 1);
            SwitcherIntent::Activate
        }
        SwitcherKey::None | SwitcherKey::Enter => SwitcherIntent::None,
    }
}

fn move_selection(current: usize, item_count: usize, direction: i32) -> usize {
    if item_count == 0 {
        return 0;
    }
    match direction.cmp(&0) {
        std::cmp::Ordering::Greater => (current + 1).min(item_count - 1),
        std::cmp::Ordering::Less => current.saturating_sub(1),
        std::cmp::Ordering::Equal => current.min(item_count - 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_selection_stays_within_results() {
        assert_eq!(move_selection(0, 0, 1), 0);
        assert_eq!(move_selection(0, 3, -1), 0);
        assert_eq!(move_selection(0, 3, 1), 1);
        assert_eq!(move_selection(2, 3, 1), 2);
    }

    #[test]
    fn switcher_keys_navigate_activate_and_close() {
        let mut selection = 0;
        assert_eq!(
            apply_switcher_key(SwitcherKey::Down, &mut selection, 3, false),
            SwitcherIntent::Moved
        );
        assert_eq!(selection, 1);
        assert_eq!(
            apply_switcher_key(SwitcherKey::Enter, &mut selection, 3, false),
            SwitcherIntent::Activate
        );
        assert_eq!(
            apply_switcher_key(SwitcherKey::Escape, &mut selection, 3, false),
            SwitcherIntent::Close
        );
    }

    #[test]
    fn switcher_enter_is_blocked_for_loading_or_empty_results() {
        let mut selection = 0;
        assert_eq!(
            apply_switcher_key(SwitcherKey::Enter, &mut selection, 1, true),
            SwitcherIntent::None
        );
        assert_eq!(
            apply_switcher_key(SwitcherKey::Enter, &mut selection, 0, false),
            SwitcherIntent::None
        );
    }

    #[tokio::test]
    async fn unavailable_daemon_after_start_attempt_returns_actionable_error() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let socket = temporary.path().join("missing.sock");
        let mut start_attempted = true;
        let error = request_with_daemon_start(
            &socket,
            Request {
                protocol_version: IPC_PROTOCOL_VERSION,
                request_id: 1,
                body: Some(request::Body::Status(StatusRequest {})),
            },
            &mut start_attempted,
        )
        .await
        .expect_err("missing daemon must fail");

        assert!(error.contains(&socket.display().to_string()));
        assert!(error.contains("clip-sync daemon"));
    }

    #[test]
    fn second_ui_instance_signals_existing_instance() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let first = UiInstance::acquire(temporary.path(), UiMode::Switcher)
            .expect("acquire first instance")
            .expect("first instance owns lock");

        let second =
            UiInstance::acquire(temporary.path(), UiMode::Switcher).expect("signal first instance");

        assert!(second.is_none());
        drop(first);
        assert!(!temporary.path().join("switcher.sock").exists());
    }

    #[test]
    fn ui_instance_does_not_replace_regular_focus_path() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let focus_socket = temporary.path().join("control.sock");
        std::fs::write(&focus_socket, b"sentinel").expect("write sentinel");

        let Err(error) = UiInstance::acquire(temporary.path(), UiMode::Control) else {
            panic!("regular focus path must be rejected");
        };

        assert!(error.contains("refusing to replace non-socket"));
        assert_eq!(
            std::fs::read(&focus_socket).expect("sentinel remains"),
            b"sentinel"
        );
    }

    #[test]
    fn byte_sizes_are_readable() {
        assert_eq!(format_bytes(12), "12 B");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(20 * 1024 * 1024), "20.0 MiB");
    }
}
