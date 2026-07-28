use std::{
    path::{Path, PathBuf},
    sync::mpsc as std_mpsc,
    time::Duration,
};

use eframe::egui::{
    self, Color32, CornerRadius, FontId, Frame, Key, Margin, RichText, ScrollArea, Stroke, Vec2,
};

use crate::{
    config::AppPaths,
    ipc::{
        self,
        protocol::{
            ActivateRequest, ConfigRequest, ConfigResponse, DiagnosticCheck, DiagnosticsRequest,
            DiagnosticsResponse, HistoryItem, HistoryRequest, HistoryResponse, HistoryUpdateAction,
            HistoryUpdateRequest, IPC_PROTOCOL_VERSION, MutationResponse, PeerItem, PeersRequest,
            PeersResponse, Request, Response, ShareClipboardRequest, StatusRequest, StatusResponse,
            TransferCancelRequest, TransferItem, TransfersRequest, TransfersResponse, request,
            response,
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
            Ok(Box::new(ClipSyncApp::new(
                mode,
                paths,
                context.egui_ctx.clone(),
            )))
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

struct ClipSyncApp {
    mode: UiMode,
    paths: AppPaths,
    command_tx: std_mpsc::Sender<UiCommand>,
    event_rx: std_mpsc::Receiver<UiEvent>,
    search: String,
    selected_history: usize,
    selected_tab: ControlTab,
    switcher_state: SwitcherState,
    history_generation: u64,
    history_loading: bool,
    history: Vec<HistoryItem>,
    status: Option<StatusResponse>,
    peers: Option<PeersResponse>,
    config: Option<serde_json::Value>,
    diagnostics: Vec<DiagnosticCheck>,
    transfers: Vec<TransferItem>,
    daemon_error: Option<String>,
    peers_error: Option<String>,
    config_error: Option<String>,
    diagnostics_error: Option<String>,
    transfers_error: Option<String>,
    notice: Option<Notice>,
    mutation_pending: bool,
}

impl ClipSyncApp {
    fn new(mode: UiMode, paths: AppPaths, context: egui::Context) -> Self {
        let (command_tx, command_rx) = std_mpsc::channel();
        let (event_tx, event_rx) = std_mpsc::channel();
        spawn_ipc_worker(paths.socket.clone(), command_rx, event_tx, context);

        let mut app = Self {
            mode,
            paths,
            command_tx,
            event_rx,
            search: String::new(),
            selected_history: 0,
            selected_tab: ControlTab::History,
            switcher_state: if mode == UiMode::Switcher {
                SwitcherState::NeedsFocus
            } else {
                SwitcherState::Ready
            },
            history_generation: 1,
            history_loading: true,
            history: Vec::new(),
            status: None,
            peers: None,
            config: None,
            diagnostics: Vec::new(),
            transfers: Vec::new(),
            daemon_error: None,
            peers_error: None,
            config_error: None,
            diagnostics_error: None,
            transfers_error: None,
            notice: None,
            mutation_pending: false,
        };
        app.refresh_status();
        app.refresh_history();
        if mode == UiMode::Control {
            app.refresh_control_data();
        }
        app
    }

    fn send(&mut self, command: UiCommand) {
        if self.command_tx.send(command).is_err() {
            self.daemon_error = Some("the local IPC worker stopped unexpectedly".to_owned());
        }
    }

    fn refresh_status(&mut self) {
        self.send(UiCommand::Status);
    }

    fn refresh_history(&mut self) {
        self.history_loading = true;
        self.history_generation = self.history_generation.saturating_add(1);
        self.send(UiCommand::History {
            query: self.search.clone(),
            generation: self.history_generation,
        });
    }

    fn refresh_control_data(&mut self) {
        self.send(UiCommand::Peers);
        self.send(UiCommand::Config);
        self.send(UiCommand::Diagnostics);
        self.send(UiCommand::Transfers);
    }

    fn poll_events(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                UiEvent::Status(result) => match result {
                    Ok(status) => {
                        self.status = Some(status);
                        self.daemon_error = None;
                    }
                    Err(error) => self.daemon_error = Some(error),
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
                            self.daemon_error = None;
                        }
                        Err(error) => self.daemon_error = Some(error),
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
                UiEvent::Transfers(result) => match result {
                    Ok(transfers) => {
                        self.transfers = transfers.transfers;
                        self.transfers_error = None;
                    }
                    Err(error) => self.transfers_error = Some(error),
                },
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
        self.daemon_error = None;
        self.send(UiCommand::RetryStatus);
        self.refresh_history();
        if self.mode == UiMode::Control {
            self.refresh_control_data();
        }
    }

    fn switcher(&mut self, ui: &mut egui::Ui) {
        if ui.input(|input| input.key_pressed(Key::Escape)) {
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
                    self.refresh_history();
                }

                if ui.input(|input| input.key_pressed(Key::ArrowDown)) {
                    self.selected_history =
                        move_selection(self.selected_history, self.history.len(), 1);
                }
                if ui.input(|input| input.key_pressed(Key::ArrowUp)) {
                    self.selected_history =
                        move_selection(self.selected_history, self.history.len(), -1);
                }
                if ui.input(|input| input.key_pressed(Key::Enter))
                    && !self.mutation_pending
                    && let Some(item) = self.history.get(self.selected_history)
                {
                    self.mutation_pending = true;
                    self.send(UiCommand::Activate {
                        content_id: item.content_id.clone(),
                        kind: MutationKind::ActivateSwitcher,
                    });
                }

                ui.add_space(6.0);
                if let Some(error) = self.daemon_error.clone() {
                    let socket = self.paths.socket.clone();
                    unavailable_panel(ui, &socket, &error, || self.retry_connection());
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
                        let (color, text) = if let Some(status) = &self.status {
                            (
                                SUCCESS,
                                format!("{} · {} peers", status.hostname, status.discovered_peers),
                            )
                        } else {
                            (ERROR, "daemon unavailable".to_owned())
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
                    ControlTab::Peers => self.peers_tab(ui),
                    ControlTab::Settings => self.settings_tab(ui),
                    ControlTab::Diagnostics => self.diagnostics_tab(ui),
                    ControlTab::Transfers => self.transfers_tab(ui),
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
                self.refresh_history();
            }
            if ui
                .add_enabled(!self.mutation_pending, egui::Button::new("Share clipboard"))
                .clicked()
            {
                self.mutation_pending = true;
                self.send(UiCommand::ShareClipboard);
            }
        });
        ui.label(
            RichText::new(
                "Remote items remain history-only until you activate them. Pins and deletes replicate.",
            )
            .color(MUTED)
            .size(12.0),
        );
        ui.add_space(6.0);
        if self.history.is_empty() {
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
                    Frame::new()
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
                }
            });

        if self.mutation_pending {
            return;
        }
        match action {
            Some(HistoryAction::Activate(content_id)) => {
                self.mutation_pending = true;
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
                self.send(UiCommand::HistoryUpdate {
                    content_id,
                    action: HistoryUpdateAction::Delete,
                    kind: MutationKind::Delete,
                });
            }
            None => {}
        }
    }

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
        ui.label(
            RichText::new(
                "Device forgetting needs the stable mesh node ID; NetBird hostnames are not identities.",
            )
            .color(MUTED)
            .size(12.0),
        );
    }

    fn settings_tab(&self, ui: &mut egui::Ui) {
        if let Some(error) = &self.config_error {
            message_panel(ui, error, ERROR);
            return;
        }
        let Some(config) = &self.config else {
            message_panel(ui, "Loading effective configuration…", MUTED);
            return;
        };
        ui.heading("Effective settings");
        ui.label(
            RichText::new("Secret paths are redacted. Edit the TOML file and restart the daemon.")
                .color(MUTED),
        );
        ui.add_space(8.0);
        setting_row(
            ui,
            "Mesh quota",
            config_pointer_u64(config, "/shared/mesh_quota_bytes")
                .map_or_else(|| "unavailable".to_owned(), format_bytes),
        );
        setting_row(
            ui,
            "Capture threshold",
            config_pointer_u64(config, "/shared/capture_threshold_bytes")
                .map_or_else(|| "unavailable".to_owned(), format_bytes),
        );
        setting_row(
            ui,
            "Listen port",
            config_pointer(config, "/local/listen_port"),
        );
        setting_row(
            ui,
            "Discovery interval",
            format!(
                "{} seconds",
                config_pointer(config, "/local/discovery_interval_seconds")
            ),
        );
        setting_row(
            ui,
            "NetBird command",
            config_pointer(config, "/local/netbird_command"),
        );
        setting_row(ui, "Config", config_pointer(config, "/local/config_path"));
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

    fn transfers_tab(&mut self, ui: &mut egui::Ui) {
        if let Some(error) = &self.transfers_error {
            message_panel(ui, error, ERROR);
            return;
        }
        if self.transfers.is_empty() {
            message_panel(ui, "No active or interrupted transfers", MUTED);
            return;
        }
        let mut cancel = None;
        for transfer in &self.transfers {
            Frame::new()
                .fill(SURFACE)
                .stroke(Stroke::new(1.0, BORDER))
                .corner_radius(CornerRadius::same(7))
                .inner_margin(Margin::same(12))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.strong(format!("{} · {}", transfer.state, transfer.peer));
                            ui.label(
                                RichText::new(format!(
                                    "{} / {}",
                                    format_bytes(transfer.completed_bytes),
                                    format_bytes(transfer.total_bytes)
                                ))
                                .color(MUTED),
                            );
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("Cancel").clicked() {
                                cancel = Some(transfer.transfer_id.clone());
                            }
                        });
                    });
                });
        }
        if let Some(transfer_id) = cancel {
            self.mutation_pending = true;
            self.send(UiCommand::TransferCancel { transfer_id });
        }
    }
}

impl eframe::App for ClipSyncApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_events();
        match self.mode {
            UiMode::Switcher => self.switcher(ui),
            UiMode::Control => self.control(ui),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ControlTab {
    History,
    Peers,
    Settings,
    Diagnostics,
    Transfers,
}

impl ControlTab {
    const ALL: [Self; 5] = [
        Self::History,
        Self::Peers,
        Self::Settings,
        Self::Diagnostics,
        Self::Transfers,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::History => "History",
            Self::Peers => "Peers",
            Self::Settings => "Settings",
            Self::Diagnostics => "Diagnostics",
            Self::Transfers => "Transfers",
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
    Share,
    TransferCancel,
}

impl MutationKind {
    const fn refreshes_history(self) -> bool {
        matches!(
            self,
            Self::Activate | Self::Pin | Self::Delete | Self::Share
        )
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
    Activate {
        content_id: String,
        kind: MutationKind,
    },
    HistoryUpdate {
        content_id: String,
        action: HistoryUpdateAction,
        kind: MutationKind,
    },
    ShareClipboard,
    TransferCancel {
        transfer_id: String,
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
    Mutation {
        kind: MutationKind,
        result: Result<MutationResponse, String>,
    },
}

fn spawn_ipc_worker(
    socket: PathBuf,
    command_rx: std_mpsc::Receiver<UiCommand>,
    event_tx: std_mpsc::Sender<UiEvent>,
    context: egui::Context,
) {
    std::thread::Builder::new()
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
                let response = runtime.block_on(request_with_daemon_start(
                    &socket,
                    Request {
                        protocol_version: IPC_PROTOCOL_VERSION,
                        request_id,
                        body: Some(body),
                    },
                    &mut start_attempted,
                ));
                let event = target.into_event(response);
                if event_tx.send(event).is_err() {
                    break;
                }
                context.request_repaint();
            }
        })
        .ok();
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
            Self::ShareClipboard => (
                request::Body::ShareClipboard(ShareClipboardRequest {}),
                EventTarget::Mutation(MutationKind::Share),
            ),
            Self::TransferCancel { transfer_id } => (
                request::Body::TransferCancel(TransferCancelRequest { transfer_id }),
                EventTarget::Mutation(MutationKind::TransferCancel),
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
        Ok(response) => return Ok(response),
        Err(error) if !is_daemon_absent(&error) => return Err(error.to_string()),
        Err(_) if *start_attempted => {}
        Err(_) => {
            *start_attempted = true;
            let start_detail = start_user_service().await;
            for _ in 0..20 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                match ipc::request(socket, request.clone()).await {
                    Ok(response) => return Ok(response),
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
                    "A systemd user-service start was attempted. You can also run `clip-sync daemon`.",
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
    fn byte_sizes_are_readable() {
        assert_eq!(format_bytes(12), "12 B");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(20 * 1024 * 1024), "20.0 MiB");
    }
}
