use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fs::{File, OpenOptions},
    io::Write as _,
    os::unix::{
        fs::OpenOptionsExt as _,
        net::{UnixListener as StdUnixListener, UnixStream as StdUnixStream},
    },
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc as std_mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use eframe::egui::{
    self, Color32, CornerRadius, FontId, Frame, Key, Margin, RichText, ScrollArea, Stroke, Vec2,
};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{
    config::AppPaths,
    ipc::{
        self,
        protocol::{
            ActivateRequest, ConfigRequest, ConfigResponse, DiagnosticCheck, DiagnosticsRequest,
            DiagnosticsResponse, ForgetDeviceRequest, HistoryItem, HistoryRequest, HistoryResponse,
            HistoryUpdateAction, HistoryUpdateRequest, IPC_PROTOCOL_VERSION, ImagePreviewRequest,
            ImagePreviewResponse, MutationResponse, PeerItem, PeersRequest, PeersResponse, Request,
            Response, ShareClipboardRequest, ShareClipboardResponse, SharedSettingKind,
            SharedSettingUpdateRequest, StatusRequest, StatusResponse, TransferCancelRequest,
            TransferItem, TransfersRequest, TransfersResponse, request, response,
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
const HISTORY_GRID_GAP: f32 = 8.0;
const SWITCHER_HISTORY_CARD_HEIGHT: f32 = 94.0;
const CONTROL_HISTORY_CARD_HEIGHT: f32 = 122.0;
const HISTORY_PREVIEW_HEIGHT: f32 = 46.0;
const SWITCHER_FOOTER_HEIGHT: f32 = 44.0;
const MAX_IMAGE_PREVIEW_WIDTH: u32 = 320;
const MAX_IMAGE_PREVIEW_HEIGHT: u32 = 180;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiMode {
    Switcher,
    Control,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct WindowGeometry {
    x: Option<i32>,
    y: Option<i32>,
    width: u32,
    height: u32,
}

impl WindowGeometry {
    fn is_valid(self) -> bool {
        let valid_position = match (self.x, self.y) {
            (Some(x), Some(y)) => x.abs() <= 100_000 && y.abs() <= 100_000,
            (None, None) => true,
            _ => false,
        };
        valid_position
            && (480..=16_384).contains(&self.width)
            && (300..=16_384).contains(&self.height)
    }
}

#[derive(Deserialize)]
struct HyprlandClient {
    address: String,
    class: String,
    at: [i32; 2],
    size: [i32; 2],
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
    prepare_window_state_directory(&paths.state_dir)?;
    let window_state_path = window_state_path(&paths.state_dir, mode);
    let saved_geometry = load_window_geometry(&window_state_path);
    let restored_size = saved_geometry.map_or(size, |geometry| {
        Vec2::new(
            geometry_coordinate_to_f32(geometry.width),
            geometry_coordinate_to_f32(geometry.height),
        )
    });
    let mut viewport = egui::ViewportBuilder::default()
        .with_title(title)
        .with_app_id(app_id)
        .with_inner_size(restored_size)
        .with_min_inner_size(Vec2::new(480.0, 300.0))
        .with_decorations(decorations);
    if let Some(geometry) = saved_geometry
        && let (Some(x), Some(y)) = (geometry.x, geometry.y)
    {
        viewport =
            viewport.with_position([geometry_position_to_f32(x), geometry_position_to_f32(y)]);
        restore_hyprland_geometry(app_id, geometry);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        title,
        options,
        Box::new(move |context| {
            configure_style(&context.egui_ctx);
            let app = ClipSyncApp::new(
                mode,
                paths,
                context.egui_ctx.clone(),
                instance,
                app_id,
                window_state_path,
                saved_geometry,
            )
            .map_err(std::io::Error::other)?;
            Ok(Box::new(app))
        }),
    )
    .map_err(|error| error.to_string())
}

fn window_state_path(state_dir: &Path, mode: UiMode) -> PathBuf {
    let name = match mode {
        UiMode::Switcher => "switcher-window.json",
        UiMode::Control => "control-window.json",
    };
    state_dir.join(name)
}

fn prepare_window_state_directory(state_dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(state_dir).map_err(|error| {
        format!(
            "could not create UI state directory {}: {error}",
            state_dir.display()
        )
    })?;
    let metadata = std::fs::symlink_metadata(state_dir)
        .map_err(|error| format!("could not inspect {}: {error}", state_dir.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!(
            "refusing unsafe UI state directory {}",
            state_dir.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        if metadata.uid() != rustix::process::getuid().as_raw() {
            return Err(format!(
                "refusing UI state directory not owned by this user: {}",
                state_dir.display()
            ));
        }
    }
    set_private_mode(state_dir, 0o700)
}

fn load_window_geometry(path: &Path) -> Option<WindowGeometry> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return None;
    }
    let geometry = serde_json::from_slice::<WindowGeometry>(&std::fs::read(path).ok()?).ok()?;
    geometry.is_valid().then_some(geometry)
}

fn save_window_geometry(path: &Path, geometry: WindowGeometry) -> Result<(), String> {
    if !geometry.is_valid() {
        return Err("refusing to persist invalid window geometry".to_owned());
    }
    let parent = path
        .parent()
        .ok_or_else(|| format!("window state path has no parent: {}", path.display()))?;
    prepare_window_state_directory(parent)?;
    let temporary = parent.join(format!(
        ".window-state-{}-{}.tmp",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|error| format!("could not create {}: {error}", temporary.display()))?;
        let encoded = serde_json::to_vec(&geometry)
            .map_err(|error| format!("could not encode window geometry: {error}"))?;
        file.write_all(&encoded)
            .map_err(|error| format!("could not write {}: {error}", temporary.display()))?;
        file.sync_all()
            .map_err(|error| format!("could not sync {}: {error}", temporary.display()))?;
        std::fs::rename(&temporary, path).map_err(|error| {
            format!("could not replace window state {}: {error}", path.display())
        })?;
        set_private_mode(path, 0o600)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn query_hyprland_geometry(app_id: &str) -> Option<WindowGeometry> {
    let output = Command::new("hyprctl")
        .args(["-j", "clients"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let clients = serde_json::from_slice::<Vec<HyprlandClient>>(&output.stdout).ok()?;
    let client = clients.into_iter().find(|client| client.class == app_id)?;
    let width = u32::try_from(client.size[0]).ok()?;
    let height = u32::try_from(client.size[1]).ok()?;
    let geometry = WindowGeometry {
        x: Some(client.at[0]),
        y: Some(client.at[1]),
        width,
        height,
    };
    geometry.is_valid().then_some(geometry)
}

fn restore_hyprland_geometry(app_id: &'static str, geometry: WindowGeometry) {
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
        return;
    }
    thread::spawn(move || {
        for _ in 0..20 {
            let Some(client) = query_hyprland_client(app_id) else {
                thread::sleep(Duration::from_millis(50));
                continue;
            };
            let selector = format!("address:{}", client.address);
            let resize = format!("exact {} {},{selector}", geometry.width, geometry.height);
            let _ = Command::new("hyprctl")
                .args(["dispatch", "resizewindowpixel", &resize])
                .output();
            if let (Some(x), Some(y)) = (geometry.x, geometry.y) {
                let movement = format!("exact {x} {y},{selector}");
                let _ = Command::new("hyprctl")
                    .args(["dispatch", "movewindowpixel", &movement])
                    .output();
            }
            return;
        }
    });
}

fn query_hyprland_client(app_id: &str) -> Option<HyprlandClient> {
    let output = Command::new("hyprctl")
        .args(["-j", "clients"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice::<Vec<HyprlandClient>>(&output.stdout)
        .ok()?
        .into_iter()
        .find(|client| client.class == app_id)
}

fn context_window_geometry(
    context: &egui::Context,
    previous: Option<WindowGeometry>,
) -> Option<WindowGeometry> {
    if let Some(rect) = context.input(|input| input.viewport().outer_rect)
        && rect.is_finite()
        && let (Some(x), Some(y), Some(width), Some(height)) = (
            rounded_geometry_position(rect.min.x),
            rounded_geometry_position(rect.min.y),
            rounded_geometry_coordinate(rect.width()),
            rounded_geometry_coordinate(rect.height()),
        )
    {
        let geometry = WindowGeometry {
            x: Some(x),
            y: Some(y),
            width,
            height,
        };
        if geometry.is_valid() {
            return Some(geometry);
        }
    }

    let size = context.content_rect().size();
    let geometry = WindowGeometry {
        x: previous.and_then(|geometry| geometry.x),
        y: previous.and_then(|geometry| geometry.y),
        width: rounded_geometry_coordinate(size.x)?,
        height: rounded_geometry_coordinate(size.y)?,
    };
    geometry.is_valid().then_some(geometry)
}

#[allow(
    clippy::cast_precision_loss,
    reason = "validated geometry coordinates are far below f32's exact integer limit"
)]
fn geometry_coordinate_to_f32(value: u32) -> f32 {
    value as f32
}

#[allow(
    clippy::cast_precision_loss,
    reason = "validated window positions are far below f32's exact integer limit"
)]
fn geometry_position_to_f32(value: i32) -> f32 {
    value as f32
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "the finite value is range-checked before rounding"
)]
fn rounded_geometry_position(value: f32) -> Option<i32> {
    (value.is_finite() && value.abs() <= 100_000.0).then(|| value.round() as i32)
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the finite positive value is range-checked before rounding"
)]
fn rounded_geometry_coordinate(value: f32) -> Option<u32> {
    (value.is_finite() && (0.0..=16_384.0).contains(&value)).then(|| value.round() as u32)
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
    app_id: &'static str,
    window_state_path: PathBuf,
    window_geometry: Option<WindowGeometry>,
    _instance: UiInstance,
    ipc_worker: IpcWorker,
    event_rx: std_mpsc::Receiver<UiEvent>,
    search: String,
    autocomplete_selected: usize,
    autocomplete_dismissed: bool,
    known_devices: BTreeSet<String>,
    known_types: BTreeSet<String>,
    selected_history: usize,
    scroll_selected_history: bool,
    selected_tab: ControlTab,
    switcher_state: SwitcherState,
    history_generation: u64,
    history_loading: bool,
    pending_history_refresh: Option<Instant>,
    history: Vec<HistoryItem>,
    image_previews: HashMap<String, ImagePreviewState>,
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
        app_id: &'static str,
        window_state_path: PathBuf,
        window_geometry: Option<WindowGeometry>,
    ) -> Result<Self, String> {
        let (event_tx, event_rx) = std_mpsc::channel();
        instance.start_focus_listener(context.clone())?;
        let ipc_worker = spawn_ipc_worker(paths.socket.clone(), event_tx, context.clone());

        let mut app = Self {
            mode,
            paths,
            context,
            app_id,
            window_state_path,
            window_geometry,
            _instance: instance,
            ipc_worker,
            event_rx,
            search: String::new(),
            autocomplete_selected: 0,
            autocomplete_dismissed: false,
            known_devices: BTreeSet::new(),
            known_types: BTreeSet::new(),
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
            image_previews: HashMap::new(),
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
            query: self.history_query(),
            generation: self.history_generation,
        });
    }

    fn history_query(&self) -> String {
        self.search.clone()
    }

    fn schedule_history_refresh(&mut self) {
        self.history_loading = true;
        self.history_error = None;
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
        let query = self.history_query();
        if should_defer_history_refresh(&query) {
            self.history_loading = false;
            return;
        }
        self.send(UiCommand::History {
            query,
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
                            for item in &self.history {
                                if !item.source_device.is_empty() {
                                    self.known_devices.insert(item.source_device.clone());
                                }
                                for mime in &item.mime_types {
                                    let normalized = mime.to_ascii_lowercase();
                                    self.known_types.insert(normalized.clone());
                                    if normalized.starts_with("image/") {
                                        self.known_types.insert("image".to_owned());
                                    }
                                    if normalized.starts_with("text/")
                                        || matches!(
                                            normalized.as_str(),
                                            "string" | "text" | "utf8_string"
                                        )
                                    {
                                        self.known_types.insert("text".to_owned());
                                    }
                                    if normalized == "text/uri-list" {
                                        self.known_types.insert("files".to_owned());
                                    }
                                }
                            }
                            let visible_content_ids = self
                                .history
                                .iter()
                                .map(|item| item.content_id.as_str())
                                .collect::<HashSet<_>>();
                            self.image_previews.retain(|content_id, _| {
                                visible_content_ids.contains(content_id.as_str())
                            });
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
                UiEvent::ImagePreview { content_id, result } => {
                    if !self
                        .history
                        .iter()
                        .any(|item| item.content_id == content_id)
                    {
                        self.image_previews.remove(&content_id);
                        continue;
                    }
                    let preview = result
                        .and_then(|preview| preview_texture(&self.context, &content_id, &preview));
                    self.image_previews.insert(
                        content_id,
                        preview.map_or(ImagePreviewState::Unavailable, ImagePreviewState::Ready),
                    );
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

    #[allow(
        clippy::too_many_lines,
        reason = "keyboard, autocomplete, and card actions share one switcher event pass"
    )]
    fn switcher(&mut self, ui: &mut egui::Ui) {
        let mut pressed_key = ui.input(switcher_key);
        let autocomplete_tab = !self.autocomplete_dismissed
            && filter_completion_context(&self.search).is_some()
            && ui
                .ctx()
                .input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Tab));
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
                        .hint_text("Search · d:device, t:type, p:true")
                        .font(FontId::proportional(16.0)),
                );
                if self.switcher_state == SwitcherState::NeedsFocus {
                    search.request_focus();
                    self.switcher_state = SwitcherState::Ready;
                }
                if search.changed() {
                    self.autocomplete_selected = 0;
                    self.autocomplete_dismissed = false;
                    self.selected_history = 0;
                    self.schedule_history_refresh();
                }

                let completion = filter_completion_context(&self.search);
                let suggestions = completion.as_ref().map_or_else(Vec::new, |completion| {
                    filter_suggestions(completion, &self.known_devices, &self.known_types)
                });
                let autocomplete_open = !self.autocomplete_dismissed && !suggestions.is_empty();
                let mut accepted_suggestion = None;
                if autocomplete_open {
                    self.autocomplete_selected = self
                        .autocomplete_selected
                        .min(suggestions.len().saturating_sub(1));
                    match pressed_key {
                        SwitcherKey::Up => {
                            self.autocomplete_selected =
                                self.autocomplete_selected.saturating_sub(1);
                            pressed_key = SwitcherKey::None;
                        }
                        SwitcherKey::Down => {
                            self.autocomplete_selected = (self.autocomplete_selected + 1)
                                .min(suggestions.len().saturating_sub(1));
                            pressed_key = SwitcherKey::None;
                        }
                        SwitcherKey::Enter => {
                            accepted_suggestion = Some(self.autocomplete_selected);
                            pressed_key = SwitcherKey::None;
                        }
                        SwitcherKey::Escape => {
                            self.autocomplete_dismissed = true;
                            pressed_key = SwitcherKey::None;
                        }
                        SwitcherKey::None | SwitcherKey::Left | SwitcherKey::Right => {}
                    }
                    if autocomplete_tab {
                        accepted_suggestion = Some(self.autocomplete_selected);
                    }
                    if let Some(clicked) = autocomplete_popup(
                        ui,
                        search.rect,
                        completion.as_ref().expect("open autocomplete has context"),
                        &suggestions,
                        self.autocomplete_selected,
                    ) {
                        accepted_suggestion = Some(clicked);
                    }
                }
                if let Some(index) = accepted_suggestion
                    && let Some(completion) = completion.as_ref()
                    && let Some(suggestion) = suggestions.get(index)
                {
                    apply_filter_suggestion(&mut self.search, completion, &suggestion.value);
                    if let Some(mut state) =
                        egui::text_edit::TextEditState::load(ui.ctx(), search.id)
                    {
                        let cursor = egui::text::CCursor::new(self.search.chars().count());
                        state
                            .cursor
                            .set_char_range(Some(egui::text::CCursorRange::one(cursor)));
                        state.store(ui.ctx(), search.id);
                    }
                    search.request_focus();
                    self.autocomplete_selected = 0;
                    self.autocomplete_dismissed = true;
                    self.selected_history = 0;
                    self.schedule_history_refresh();
                }

                if ui.input(|input| input.modifiers.ctrl && input.key_pressed(Key::P))
                    && !self.mutation_pending
                    && let Some(item) = self.history.get(self.selected_history)
                {
                    let content_id = item.content_id.clone();
                    let action = if item.pinned {
                        HistoryUpdateAction::Unpin
                    } else {
                        HistoryUpdateAction::Pin
                    };
                    self.mutation_pending = true;
                    self.notice = None;
                    self.send(UiCommand::HistoryUpdate {
                        content_id,
                        action,
                        kind: MutationKind::Pin,
                    });
                }

                let columns = history_column_count(ui.available_width(), self.mode);
                match apply_switcher_key(
                    pressed_key,
                    &mut self.selected_history,
                    self.history.len(),
                    columns,
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
                let history_height = (ui.available_height() - SWITCHER_FOOTER_HEIGHT).max(80.0);
                ui.allocate_ui(Vec2::new(ui.available_width(), history_height), |ui| {
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
                        self.history_grid(ui, false);
                    }
                });

                ui.add_space(10.0);
                if let Some(notice) = &self.notice {
                    notice.show(ui);
                }
                ui.label(
                    RichText::new(
                        "←→↑↓ navigate    Enter activate    Ctrl+P pin/unpin    Esc close",
                    )
                    .monospace()
                    .color(MUTED)
                    .size(11.0),
                );
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
                [(ui.available_width() - 210.0).max(140.0), 36.0],
                egui::TextEdit::singleline(&mut self.search)
                    .hint_text("Search · d:device, t:type, p:true"),
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
                "Filters: d:, t:, p:, before:, min-size:, max-size:. Chain with commas; quote phrases. Items with local payloads can be activated.",
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
            self.history_grid(ui, true);
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the grid keeps card rendering and its keyboard/mutation actions together"
    )]
    fn history_grid(&mut self, ui: &mut egui::Ui, allow_delete: bool) {
        let columns = history_column_count(ui.available_width(), self.mode);
        let columns_f32 = f32::from(u16::try_from(columns).expect("history columns fit in u16"));
        let total_gap = HISTORY_GRID_GAP * (columns_f32 - 1.0);
        let card_width = ((ui.available_width() - total_gap - 14.0) / columns_f32)
            .floor()
            .max(180.0);
        let card_height = if allow_delete {
            CONTROL_HISTORY_CARD_HEIGHT
        } else {
            SWITCHER_HISTORY_CARD_HEIGHT
        };
        let mut action = None;
        let mut requested_previews = Vec::new();
        let grid_id = match self.mode {
            UiMode::Switcher => "switcher-history-grid",
            UiMode::Control => "control-history-grid",
        };

        ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Grid::new(grid_id)
                    .num_columns(columns)
                    .spacing(Vec2::splat(HISTORY_GRID_GAP))
                    .show(ui, |ui| {
                        for index in 0..self.history.len() {
                            let item = self.history[index].clone();
                            let selected = index == self.selected_history;
                            let has_image = history_item_has_image(&item);
                            let preview = self.image_previews.get(&item.content_id);
                            let card = Frame::new()
                                .fill(if selected {
                                    Color32::from_rgb(24, 48, 54)
                                } else {
                                    SURFACE
                                })
                                .stroke(Stroke::new(1.0, if selected { CYAN } else { BORDER }))
                                .corner_radius(CornerRadius::same(8))
                                .inner_margin(Margin::same(8))
                                .show(ui, |ui| {
                                    ui.vertical(|ui| {
                                        ui.spacing_mut().item_spacing.y = 3.0;
                                        ui.set_width((card_width - 16.0).max(160.0));
                                        ui.set_min_height(card_height - 16.0);
                                        history_card_preview(ui, &item, preview);

                                        let mime = item
                                            .mime_types
                                            .first()
                                            .map_or("unknown", String::as_str);
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(format!(
                                                    "{mime} · {}{}",
                                                    format_bytes(item.logical_size),
                                                    if item.pinned { " · PIN" } else { "" }
                                                ))
                                                .color(MUTED)
                                                .monospace()
                                                .size(10.0),
                                            )
                                            .truncate(),
                                        );
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(format!(
                                                    "source {}",
                                                    history_source_label(&item)
                                                ))
                                                .color(MUTED)
                                                .monospace()
                                                .size(10.0),
                                            )
                                            .truncate(),
                                        );
                                        if allow_delete {
                                            ui.horizontal(|ui| {
                                                if ui
                                                    .add_enabled(
                                                        !self.mutation_pending,
                                                        egui::Button::new("Activate").small(),
                                                    )
                                                    .clicked()
                                                {
                                                    action = Some(HistoryAction::Activate(
                                                        item.content_id.clone(),
                                                    ));
                                                }
                                                if ui
                                                    .add_enabled(
                                                        !self.mutation_pending,
                                                        egui::Button::new(if item.pinned {
                                                            "Unpin"
                                                        } else {
                                                            "Pin"
                                                        })
                                                        .small(),
                                                    )
                                                    .clicked()
                                                {
                                                    action = Some(HistoryAction::Pin {
                                                        content_id: item.content_id.clone(),
                                                        pinned: !item.pinned,
                                                    });
                                                }
                                                if ui
                                                    .add_enabled(
                                                        !self.mutation_pending,
                                                        egui::Button::new("Delete").small(),
                                                    )
                                                    .clicked()
                                                {
                                                    action = Some(HistoryAction::Delete(
                                                        item.content_id.clone(),
                                                    ));
                                                }
                                            });
                                        }
                                    });
                                });
                            let response = card.response.interact(egui::Sense::click());
                            if response.clicked() {
                                self.selected_history = index;
                            }
                            if !allow_delete && response.double_clicked() {
                                action = Some(HistoryAction::Activate(item.content_id.clone()));
                            }
                            if selected && self.scroll_selected_history {
                                response.scroll_to_me(Some(egui::Align::Center));
                            }
                            if has_image && preview.is_none() && ui.is_rect_visible(response.rect) {
                                requested_previews.push(item.content_id);
                            }
                            if (index + 1) % columns == 0 {
                                ui.end_row();
                            }
                        }
                    });
                ui.add_space(18.0);
            });
        self.scroll_selected_history = false;

        for content_id in requested_previews {
            if self
                .image_previews
                .insert(content_id.clone(), ImagePreviewState::Loading)
                .is_none()
            {
                self.send(UiCommand::ImagePreview { content_id });
            }
        }

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
        self.window_geometry = context_window_geometry(ui.ctx(), self.window_geometry);
        ui.painter()
            .rect_filled(ui.max_rect(), CornerRadius::ZERO, BACKGROUND);
        self.poll_events();
        self.dispatch_pending_history_refresh();
        self.dispatch_transfer_refresh();
        match self.mode {
            UiMode::Switcher => self.switcher(ui),
            UiMode::Control => self.control(ui),
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let geometry = query_hyprland_geometry(self.app_id).or(self.window_geometry);
        if let Some(geometry) = geometry
            && let Err(error) = save_window_geometry(&self.window_state_path, geometry)
        {
            tracing::warn!(%error, "could not persist UI window geometry");
        }
    }

    fn persist_egui_memory(&self) -> bool {
        false
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

enum ImagePreviewState {
    Loading,
    Ready(egui::TextureHandle),
    Unavailable,
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
    ImagePreview {
        content_id: String,
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
    ImagePreview {
        content_id: String,
        result: Result<ImagePreviewResponse, String>,
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
            Self::ImagePreview { content_id } => (
                request::Body::ImagePreview(ImagePreviewRequest {
                    content_id: content_id.clone(),
                }),
                EventTarget::ImagePreview(content_id),
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
    ImagePreview(String),
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
            Self::ImagePreview(content_id) => UiEvent::ImagePreview {
                content_id,
                result: expect_image_preview(result),
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

fn expect_image_preview(result: Result<Response, String>) -> Result<ImagePreviewResponse, String> {
    match response_error(result?)? {
        response::Body::ImagePreview(value) => Ok(value),
        _ => Err("daemon returned an unexpected image-preview response".to_owned()),
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

fn preview_texture(
    context: &egui::Context,
    requested_content_id: &str,
    preview: &ImagePreviewResponse,
) -> Result<egui::TextureHandle, String> {
    if preview.content_id != requested_content_id {
        return Err("image preview content ID did not match its request".to_owned());
    }
    if preview.width == 0
        || preview.height == 0
        || preview.width > MAX_IMAGE_PREVIEW_WIDTH
        || preview.height > MAX_IMAGE_PREVIEW_HEIGHT
    {
        return Err("image preview dimensions are invalid".to_owned());
    }
    let width = usize::try_from(preview.width)
        .map_err(|_| "image preview width does not fit in memory".to_owned())?;
    let height = usize::try_from(preview.height)
        .map_err(|_| "image preview height does not fit in memory".to_owned())?;
    let expected = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| "image preview dimensions overflow".to_owned())?;
    if preview.rgba.len() != expected {
        return Err("image preview pixel data has the wrong length".to_owned());
    }
    let image = egui::ColorImage::from_rgba_unmultiplied([width, height], &preview.rgba);
    Ok(context.load_texture(
        format!("clip-sync-preview-{requested_content_id}"),
        image,
        egui::TextureOptions::LINEAR,
    ))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FilterCompletionKind {
    Device,
    Type,
    Pinned,
}

struct FilterCompletion {
    value_start: usize,
    kind: FilterCompletionKind,
    prefix: String,
}

struct FilterSuggestion {
    value: String,
    label: String,
    detail: &'static str,
}

fn filter_completion_context(search: &str) -> Option<FilterCompletion> {
    let mut token_start = 0;
    let mut quote = None;
    for (offset, character) in search.char_indices() {
        match character {
            '"' | '\'' if quote.is_none() => quote = Some(character),
            character if quote == Some(character) => quote = None,
            ',' if quote.is_none() => token_start = offset + character.len_utf8(),
            character if quote.is_none() && character.is_whitespace() => {
                token_start = offset + character.len_utf8();
            }
            _ => {}
        }
    }
    if quote.is_some() {
        return None;
    }
    let token = search.get(token_start..)?;
    let (name, value) = token.split_once(':')?;
    let kind = match name.to_ascii_lowercase().as_str() {
        "d" | "device" => FilterCompletionKind::Device,
        "t" | "type" => FilterCompletionKind::Type,
        "p" | "pinned" => FilterCompletionKind::Pinned,
        _ => return None,
    };
    Some(FilterCompletion {
        value_start: token_start + name.len() + 1,
        kind,
        prefix: value.to_ascii_lowercase(),
    })
}

fn should_defer_history_refresh(search: &str) -> bool {
    search
        .split(|character: char| character.is_whitespace() || character == ',')
        .filter_map(|token| token.split_once(':'))
        .any(|(name, value)| match name.to_ascii_lowercase().as_str() {
            "d" | "device" | "t" | "type" => value.is_empty(),
            "p" | "pinned" => !matches!(value.to_ascii_lowercase().as_str(), "true" | "false"),
            _ => false,
        })
}

fn filter_suggestions(
    completion: &FilterCompletion,
    known_devices: &BTreeSet<String>,
    known_types: &BTreeSet<String>,
) -> Vec<FilterSuggestion> {
    match completion.kind {
        FilterCompletionKind::Device => known_devices
            .iter()
            .filter(|device| device.to_ascii_lowercase().starts_with(&completion.prefix))
            .take(6)
            .map(|device| FilterSuggestion {
                value: device.clone(),
                label: device.clone(),
                detail: "device",
            })
            .collect(),
        FilterCompletionKind::Type => {
            let mut candidates = ["image", "text", "files"]
                .into_iter()
                .filter(|kind| kind.starts_with(&completion.prefix))
                .map(str::to_owned)
                .collect::<Vec<_>>();
            candidates.extend(
                known_types
                    .iter()
                    .filter(|kind| {
                        kind.starts_with(&completion.prefix)
                            && !matches!(kind.as_str(), "image" | "text" | "files")
                    })
                    .take(6_usize.saturating_sub(candidates.len()))
                    .cloned(),
            );
            candidates
                .into_iter()
                .map(|kind| FilterSuggestion {
                    detail: if matches!(kind.as_str(), "image" | "text" | "files") {
                        "type group"
                    } else {
                        "exact MIME"
                    },
                    value: kind.clone(),
                    label: kind,
                })
                .collect()
        }
        FilterCompletionKind::Pinned => [
            FilterSuggestion {
                value: "true".to_owned(),
                label: "pinned".to_owned(),
                detail: "p:true",
            },
            FilterSuggestion {
                value: "false".to_owned(),
                label: "unpinned".to_owned(),
                detail: "p:false",
            },
        ]
        .into_iter()
        .filter(|suggestion| suggestion.value.starts_with(&completion.prefix))
        .collect(),
    }
}

fn autocomplete_popup(
    ui: &egui::Ui,
    anchor: egui::Rect,
    completion: &FilterCompletion,
    suggestions: &[FilterSuggestion],
    selected: usize,
) -> Option<usize> {
    let mut clicked = None;
    egui::Area::new(egui::Id::new("history-filter-autocomplete"))
        .order(egui::Order::Foreground)
        .fixed_pos(anchor.left_bottom() + Vec2::new(0.0, 4.0))
        .show(ui.ctx(), |ui| {
            Frame::popup(ui.style())
                .fill(SURFACE)
                .stroke(Stroke::new(1.0, BORDER))
                .inner_margin(Margin::same(8))
                .show(ui, |ui| {
                    ui.set_width(anchor.width());
                    let heading = match completion.kind {
                        FilterCompletionKind::Device => "DEVICES",
                        FilterCompletionKind::Type => "TYPES",
                        FilterCompletionKind::Pinned => "PIN STATE",
                    };
                    ui.label(RichText::new(heading).monospace().color(MUTED).size(10.0));
                    for (index, suggestion) in suggestions.iter().enumerate() {
                        let text = format!("{}    {}", suggestion.label, suggestion.detail);
                        if ui.selectable_label(index == selected, text).clicked() {
                            clicked = Some(index);
                        }
                    }
                    ui.label(
                        RichText::new("↑↓ choose · Tab/Enter complete · Esc dismiss")
                            .monospace()
                            .color(MUTED)
                            .size(9.0),
                    );
                });
        });
    clicked
}

fn apply_filter_suggestion(search: &mut String, completion: &FilterCompletion, value: &str) {
    search.replace_range(completion.value_start.., value);
}

fn history_source_label(item: &HistoryItem) -> String {
    if item.source_device.is_empty() {
        short_identifier(&item.source_node)
    } else {
        item.source_device.clone()
    }
}

fn history_column_count(available_width: f32, mode: UiMode) -> usize {
    let three_column_threshold = match mode {
        UiMode::Switcher => 640.0,
        UiMode::Control => 860.0,
    };
    if available_width >= three_column_threshold {
        3
    } else {
        2
    }
}

fn history_item_has_image(item: &HistoryItem) -> bool {
    item.mime_types.iter().any(|mime| {
        matches!(
            mime.split(';')
                .next()
                .unwrap_or(mime)
                .trim()
                .to_ascii_lowercase()
                .as_str(),
            "image/png"
                | "image/jpeg"
                | "image/jpg"
                | "image/gif"
                | "image/webp"
                | "image/bmp"
                | "image/x-ms-bmp"
                | "image/tiff"
        )
    })
}

fn history_card_preview(
    ui: &mut egui::Ui,
    item: &HistoryItem,
    preview: Option<&ImagePreviewState>,
) {
    let size = Vec2::new(ui.available_width(), HISTORY_PREVIEW_HEIGHT);
    if history_item_has_image(item) {
        ui.allocate_ui_with_layout(
            size,
            egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
            |ui| match preview {
                Some(ImagePreviewState::Ready(texture)) => {
                    ui.add(egui::Image::new(texture).max_size(size));
                }
                Some(ImagePreviewState::Loading) => {
                    ui.spinner();
                }
                Some(ImagePreviewState::Unavailable) => {
                    ui.label(RichText::new("Preview unavailable").color(MUTED).size(12.0));
                }
                None => {
                    ui.label(RichText::new("Loading preview…").color(MUTED).size(12.0));
                }
            },
        );
    } else {
        let title = if item.preview.trim().is_empty() {
            "Binary clipboard content"
        } else {
            item.preview.trim()
        };
        ui.allocate_ui(size, |ui| {
            ui.add(
                egui::Label::new(RichText::new(title).color(Color32::WHITE).size(14.0)).truncate(),
            );
        });
    }
}

fn short_identifier(identifier: &str) -> String {
    const VISIBLE_CHARS: usize = 12;
    let mut short = identifier.chars().take(VISIBLE_CHARS).collect::<String>();
    if identifier.chars().count() > VISIBLE_CHARS {
        short.push('…');
    }
    short
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
    Left,
    Right,
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
    } else if input.key_pressed(Key::ArrowLeft) {
        SwitcherKey::Left
    } else if input.key_pressed(Key::ArrowRight) {
        SwitcherKey::Right
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
    columns: usize,
    activation_blocked: bool,
) -> SwitcherIntent {
    match key {
        SwitcherKey::Escape => SwitcherIntent::Close,
        SwitcherKey::Left | SwitcherKey::Right | SwitcherKey::Up | SwitcherKey::Down => {
            *selection = move_grid_selection(*selection, item_count, columns, key);
            SwitcherIntent::Moved
        }
        SwitcherKey::Enter if item_count > 0 && !activation_blocked => {
            *selection = (*selection).min(item_count - 1);
            SwitcherIntent::Activate
        }
        SwitcherKey::None | SwitcherKey::Enter => SwitcherIntent::None,
    }
}

fn move_grid_selection(
    current: usize,
    item_count: usize,
    columns: usize,
    key: SwitcherKey,
) -> usize {
    if item_count == 0 || columns == 0 {
        return 0;
    }
    let current = current.min(item_count - 1);
    match key {
        SwitcherKey::Left if !current.is_multiple_of(columns) => current - 1,
        SwitcherKey::Right
            if !(current + 1).is_multiple_of(columns) && current + 1 < item_count =>
        {
            current + 1
        }
        SwitcherKey::Up if current >= columns => current - columns,
        SwitcherKey::Down if current + columns < item_count => current + columns,
        SwitcherKey::None
        | SwitcherKey::Left
        | SwitcherKey::Right
        | SwitcherKey::Up
        | SwitcherKey::Down
        | SwitcherKey::Enter
        | SwitcherKey::Escape => current,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_geometry_is_private_and_round_trips() {
        use std::os::unix::fs::PermissionsExt as _;

        let temporary = tempfile::tempdir().expect("temporary directory");
        let path = window_state_path(temporary.path(), UiMode::Switcher);
        let geometry = WindowGeometry {
            x: Some(-1200),
            y: Some(80),
            width: 860,
            height: 510,
        };

        save_window_geometry(&path, geometry).expect("save geometry");

        assert_eq!(load_window_geometry(&path), Some(geometry));
        assert_eq!(
            std::fs::metadata(&path)
                .expect("window state metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_ne!(
            window_state_path(temporary.path(), UiMode::Control),
            window_state_path(temporary.path(), UiMode::Switcher)
        );
    }

    #[test]
    fn invalid_window_geometry_is_ignored() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let path = window_state_path(temporary.path(), UiMode::Control);
        std::fs::write(&path, br#"{"x":10,"y":null,"width":1040,"height":700}"#)
            .expect("write invalid geometry");

        assert_eq!(load_window_geometry(&path), None);
        assert!(
            save_window_geometry(
                &path,
                WindowGeometry {
                    x: None,
                    y: None,
                    width: 120,
                    height: 100,
                }
            )
            .is_err()
        );
    }

    #[test]
    fn hyprland_client_geometry_deserializes() {
        let client = serde_json::from_str::<HyprlandClient>(
            r#"{"address":"0xabc","class":"clip-sync-switcher","at":[100,200],"size":[720,420]}"#,
        )
        .expect("Hyprland client");

        assert_eq!(client.address, "0xabc");
        assert_eq!(client.at, [100, 200]);
        assert_eq!(client.size, [720, 420]);
    }

    #[test]
    fn grid_selection_stays_within_rows_and_results() {
        assert_eq!(move_grid_selection(0, 0, 3, SwitcherKey::Down), 0);
        assert_eq!(move_grid_selection(0, 6, 3, SwitcherKey::Left), 0);
        assert_eq!(move_grid_selection(2, 6, 3, SwitcherKey::Right), 2);
        assert_eq!(move_grid_selection(1, 6, 3, SwitcherKey::Down), 4);
        assert_eq!(move_grid_selection(4, 6, 3, SwitcherKey::Up), 1);
        assert_eq!(move_grid_selection(5, 6, 3, SwitcherKey::Down), 5);
    }

    #[test]
    fn switcher_keys_navigate_activate_and_close() {
        let mut selection = 0;
        assert_eq!(
            apply_switcher_key(SwitcherKey::Down, &mut selection, 6, 3, false),
            SwitcherIntent::Moved
        );
        assert_eq!(selection, 3);
        assert_eq!(
            apply_switcher_key(SwitcherKey::Right, &mut selection, 6, 3, false),
            SwitcherIntent::Moved
        );
        assert_eq!(selection, 4);
        assert_eq!(
            apply_switcher_key(SwitcherKey::Enter, &mut selection, 6, 3, false),
            SwitcherIntent::Activate
        );
        assert_eq!(
            apply_switcher_key(SwitcherKey::Escape, &mut selection, 6, 3, false),
            SwitcherIntent::Close
        );
    }

    #[test]
    fn switcher_enter_is_blocked_for_loading_or_empty_results() {
        let mut selection = 0;
        assert_eq!(
            apply_switcher_key(SwitcherKey::Enter, &mut selection, 1, 3, true),
            SwitcherIntent::None
        );
        assert_eq!(
            apply_switcher_key(SwitcherKey::Enter, &mut selection, 0, 3, false),
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

    #[test]
    fn abbreviated_filter_completion_handles_comma_chains() {
        let completion = filter_completion_context("d:vd,t:im").expect("type completion");
        assert_eq!(completion.kind, FilterCompletionKind::Type);
        assert_eq!(completion.prefix, "im");
        let mut search = "d:vd,t:im".to_owned();
        apply_filter_suggestion(&mut search, &completion, "image");
        assert_eq!(search, "d:vd,t:image");

        assert!(should_defer_history_refresh("d:"));
        assert!(should_defer_history_refresh("d:vd,p:t"));
        assert!(!should_defer_history_refresh("d:vd,p:true"));
    }

    #[test]
    fn pinned_completion_offers_true_and_false_values() {
        let completion = filter_completion_context("P:").expect("pin completion");
        let suggestions = filter_suggestions(&completion, &BTreeSet::new(), &BTreeSet::new());
        assert_eq!(
            suggestions
                .iter()
                .map(|suggestion| suggestion.value.as_str())
                .collect::<Vec<_>>(),
            ["true", "false"]
        );
    }

    #[test]
    fn history_grid_uses_two_or_three_columns() {
        assert_eq!(history_column_count(600.0, UiMode::Switcher), 2);
        assert_eq!(history_column_count(700.0, UiMode::Switcher), 3);
        assert_eq!(history_column_count(1_000.0, UiMode::Switcher), 3);
        assert_eq!(history_column_count(800.0, UiMode::Control), 2);
        assert_eq!(history_column_count(900.0, UiMode::Control), 3);
    }

    #[test]
    fn raster_mime_types_request_image_previews() {
        let mut item = HistoryItem {
            content_id: "content".to_owned(),
            preview: String::new(),
            mime_types: vec!["image/png".to_owned()],
            logical_size: 4,
            source_node: "node".to_owned(),
            pinned: false,
            source_device: "vd".to_owned(),
            physical_millis: 0,
        };
        assert!(history_item_has_image(&item));
        item.mime_types = vec!["image/svg+xml".to_owned()];
        assert!(!history_item_has_image(&item));
    }
}
