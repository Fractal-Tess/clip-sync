use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::PathBuf,
    sync::mpsc as std_mpsc,
    time::{Duration, Instant},
};

use eframe::egui;

use crate::{
    config::AppPaths,
    ipc::protocol::{
        DiagnosticCheck, HistoryItem, PeersResponse, ShareClipboardResponse, StatusResponse,
        TransferItem,
    },
    ui::{
        Presentation, UiMode,
        global_shortcut::GlobalShortcutListener,
        history::{
            ControlTab, HistoryRefreshState, history_poll_allowed, history_poll_cadence,
            history_refresh_delay, pending_history_refresh_due, presentation_after_navigation,
            should_defer_history_refresh,
        },
        ipc_types::{
            ImagePreviewState, PendingScope, PendingSetting, ShareGenerationState, UiCommand,
            UiEvent, mutation_block_reason,
        },
        ipc_worker::{IpcWorker, spawn_ipc_worker},
        signal_closes_presentation,
        singleton::{UiInstance, UiSignal},
        style::{Notice, SEARCH_DEBOUNCE, load_brand_texture},
        window::WindowGeometry,
    },
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum WindowState {
    NeedsFocus,
    Ready,
    Close,
}

#[allow(
    clippy::struct_excessive_bools,
    reason = "independent view, refresh, and transfer flags have separate lifecycles"
)]
pub(super) struct ClipSyncApp {
    presentation: Presentation,
    paths: AppPaths,
    context: egui::Context,
    app_id: &'static str,
    window_state_path: PathBuf,
    window_geometry: Option<WindowGeometry>,
    brand_icon: egui::TextureHandle,
    _instance: UiInstance,
    _global_shortcut: GlobalShortcutListener,
    ipc_worker: IpcWorker,
    event_rx: std_mpsc::Receiver<UiEvent>,
    signal_rx: std_mpsc::Receiver<UiSignal>,
    search: String,
    autocomplete_selected: usize,
    autocomplete_dismissed: bool,
    filter_help_open: bool,
    known_devices: BTreeSet<String>,
    known_types: BTreeSet<String>,
    selected_history: usize,
    selected_content_id: Option<String>,
    scroll_selected_history: bool,
    history_card_focus_ids: HashSet<egui::Id>,
    selected_tab: ControlTab,
    window_state: WindowState,
    history_generation: u64,
    history_loading: bool,
    pending_history_refresh: Option<Instant>,
    history_refresh: HistoryRefreshState,
    viewport_focused: bool,
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
    pending_scopes: HashSet<PendingScope>,
    transfer_refresh_pending: bool,
    peers_refresh_pending: bool,
    config_refresh_pending: bool,
    diagnostics_refresh_pending: bool,
    last_transfer_refresh: Instant,
    last_management_refresh: Instant,
    share_inspection: Option<ShareClipboardResponse>,
    share_generation: ShareGenerationState,
    pending_transfer_cancel: Option<String>,
    pending_history_delete: Option<String>,
    forget_device_id: String,
    pending_forget_device: Option<String>,
    mesh_quota_input: String,
    capture_threshold_input: String,
    pending_setting: Option<PendingSetting>,
}

impl ClipSyncApp {
    pub(super) fn new(
        mode: UiMode,
        paths: AppPaths,
        context: egui::Context,
        mut instance: UiInstance,
        app_id: &'static str,
        window_state_path: PathBuf,
        window_geometry: Option<WindowGeometry>,
    ) -> Result<Self, String> {
        let (event_tx, event_rx) = std_mpsc::channel();
        let (signal_tx, signal_rx) = std_mpsc::channel();
        instance.start_signal_listener(context.clone(), signal_tx.clone())?;
        let global_shortcut = GlobalShortcutListener::start(context.clone(), signal_tx);
        let ipc_worker = spawn_ipc_worker(paths.socket.clone(), event_tx, context.clone());
        let brand_icon = load_brand_texture(&context)?;

        let presentation = Presentation::from_mode(mode);
        let now = Instant::now();
        let mut app = Self {
            presentation,
            paths,
            context,
            app_id,
            window_state_path,
            window_geometry,
            brand_icon,
            _instance: instance,
            _global_shortcut: global_shortcut,
            ipc_worker,
            event_rx,
            signal_rx,
            search: String::new(),
            autocomplete_selected: 0,
            autocomplete_dismissed: false,
            filter_help_open: false,
            known_devices: BTreeSet::new(),
            known_types: BTreeSet::new(),
            selected_history: 0,
            selected_content_id: None,
            scroll_selected_history: false,
            history_card_focus_ids: HashSet::new(),
            selected_tab: ControlTab::History,
            window_state: if presentation == Presentation::Quick {
                WindowState::NeedsFocus
            } else {
                WindowState::Ready
            },
            history_generation: 1,
            history_loading: true,
            pending_history_refresh: None,
            history_refresh: HistoryRefreshState::new(now),
            viewport_focused: true,
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
            pending_scopes: HashSet::new(),
            transfer_refresh_pending: false,
            peers_refresh_pending: false,
            config_refresh_pending: false,
            diagnostics_refresh_pending: false,
            last_transfer_refresh: Instant::now()
                .checked_sub(Duration::from_secs(1))
                .unwrap_or_else(Instant::now),
            last_management_refresh: Instant::now(),
            share_inspection: None,
            share_generation: ShareGenerationState::default(),
            pending_transfer_cancel: None,
            pending_history_delete: None,
            forget_device_id: String::new(),
            pending_forget_device: None,
            mesh_quota_input: String::new(),
            capture_threshold_input: String::new(),
            pending_setting: None,
        };
        app.refresh_status();
        app.refresh_history();
        if presentation == Presentation::Management {
            app.refresh_control_data();
        }
        Ok(app)
    }

    pub(super) fn send(&mut self, command: UiCommand) {
        let pending_scope = command.pending_scope();
        if let Some(scope) = pending_scope {
            if mutation_block_reason(&self.pending_scopes, scope).is_some() {
                return;
            }
            self.pending_scopes.insert(scope);
        }
        let share_generation = command.share_generation();
        let transfers = matches!(&command, UiCommand::Transfers);
        let peers = matches!(&command, UiCommand::Peers);
        let config = matches!(&command, UiCommand::Config);
        let diagnostics = matches!(&command, UiCommand::Diagnostics);
        if let Err(error) = self.ipc_worker.send(command) {
            if let Some(scope) = pending_scope {
                self.finish_pending(scope);
            }
            if let Some(generation) = share_generation {
                self.share_generation.cancel_active(generation);
            }
            if transfers {
                self.transfer_refresh_pending = false;
            }
            if peers {
                self.peers_refresh_pending = false;
            }
            if config {
                self.config_refresh_pending = false;
            }
            if diagnostics {
                self.diagnostics_refresh_pending = false;
            }
            self.daemon_error = Some(error);
        }
    }

    pub(super) fn mutation_block_reason(&self, requested: PendingScope) -> Option<&'static str> {
        mutation_block_reason(&self.pending_scopes, requested)
    }

    pub(super) fn finish_pending(&mut self, scope: PendingScope) {
        self.pending_scopes.remove(&scope);
    }

    pub(super) fn share_clipboard(&mut self, confirmed: bool) {
        if self.mutation_block_reason(PendingScope::Share).is_some() {
            return;
        }
        let generation = self.share_generation.start_request();
        self.send(UiCommand::ShareClipboard {
            confirmed,
            generation,
        });
    }

    pub(super) fn invalidate_share_confirmation(&mut self) {
        self.share_generation.invalidate();
        self.share_inspection = None;
    }

    pub(super) fn refresh_status(&mut self) {
        self.send(UiCommand::Status);
    }

    pub(super) fn refresh_history(&mut self) {
        self.pending_history_refresh = None;
        self.history_loading = self.history.is_empty();
        self.dispatch_history_request();
    }

    pub(super) fn dispatch_history_request(&mut self) {
        let query = self.history_query();
        if should_defer_history_refresh(&query) {
            self.history_loading = false;
            return;
        }
        let cadence = history_poll_cadence(self.viewport_focused);
        let now = Instant::now();
        let dispatch = self.history_refresh.request();
        self.history_refresh.next_due = now + cadence;
        if !dispatch {
            return;
        }
        if let Err(error) = self.ipc_worker.send(UiCommand::History {
            query,
            generation: self.history_generation,
        }) {
            self.history_refresh.in_flight = false;
            self.history_refresh.consecutive_failures =
                self.history_refresh.consecutive_failures.saturating_add(1);
            self.history_refresh.next_due = Instant::now()
                + history_refresh_delay(cadence, self.history_refresh.consecutive_failures);
            self.history_error = Some(error.clone());
            self.daemon_error = Some(error);
        }
    }

    pub(super) fn history_query(&self) -> String {
        self.search.clone()
    }

    pub(super) fn schedule_history_refresh(&mut self) {
        self.history_loading = self.history.is_empty();
        self.history_error = None;
        self.history_generation = self.history_generation.saturating_add(1);
        self.pending_history_refresh = Some(Instant::now() + SEARCH_DEBOUNCE);
        self.context.request_repaint_after(SEARCH_DEBOUNCE);
    }

    pub(super) fn dispatch_pending_history_refresh(&mut self, history_poll_eligible: bool) {
        let Some(deadline) = self.pending_history_refresh else {
            return;
        };
        if !pending_history_refresh_due(deadline, history_poll_eligible, Instant::now()) {
            return;
        }
        self.pending_history_refresh = None;
        self.dispatch_history_request();
    }

    pub(super) fn dispatch_live_history_refresh(&mut self, minimized: bool) {
        if !history_poll_allowed(self.selected_tab, minimized, &self.search) {
            return;
        }
        let now = Instant::now();
        if self.pending_history_refresh.is_none() && now >= self.history_refresh.next_due {
            self.dispatch_history_request();
        }
        let delay = self
            .history_refresh
            .next_due
            .saturating_duration_since(now)
            .max(Duration::from_millis(50));
        self.context.request_repaint_after(delay);
    }

    pub(super) fn handle_signal(&mut self, signal: UiSignal) {
        match signal {
            UiSignal::OpenQuick => {
                self.presentation = Presentation::Quick;
                self.selected_tab = ControlTab::History;
                self.invalidate_share_confirmation();
                self.pending_history_delete = None;
                self.window_state = WindowState::NeedsFocus;
                self.refresh_status();
                self.refresh_history();
            }
            UiSignal::OpenManagement => {
                self.presentation = Presentation::Management;
                self.selected_tab = ControlTab::History;
                self.invalidate_share_confirmation();
                self.pending_history_delete = None;
                self.window_state = WindowState::Ready;
                self.refresh_status();
                self.refresh_history();
                self.refresh_control_data();
            }
            UiSignal::CloseQuick if signal_closes_presentation(self.presentation, signal) => {
                self.window_state = WindowState::Close;
            }
            UiSignal::CloseQuick => {}
        }
    }

    pub(super) fn poll_signals(&mut self) {
        while let Ok(signal) = self.signal_rx.try_recv() {
            self.handle_signal(signal);
        }
    }

    pub(super) fn navigate_to(&mut self, tab: ControlTab) {
        let presentation = presentation_after_navigation(self.presentation, tab);
        if presentation != self.presentation {
            self.invalidate_share_confirmation();
        }
        self.presentation = presentation;
        self.selected_tab = tab;
        if tab != ControlTab::History {
            self.pending_history_delete = None;
        }
        self.history_card_focus_ids.clear();
        match tab {
            ControlTab::History => self.refresh_history(),
            ControlTab::Transfers => self.refresh_transfers(),
            ControlTab::Peers => self.refresh_peers(),
            ControlTab::Settings => self.refresh_config(),
            ControlTab::Diagnostics => self.refresh_diagnostics(),
        }
    }

    pub(super) fn refresh_control_data(&mut self) {
        self.refresh_peers();
        self.refresh_config();
        self.refresh_diagnostics();
        self.refresh_transfers();
    }

    pub(super) fn refresh_peers(&mut self) {
        if self.peers_refresh_pending {
            return;
        }
        self.peers_refresh_pending = true;
        self.last_management_refresh = Instant::now();
        self.send(UiCommand::Peers);
    }

    pub(super) fn refresh_config(&mut self) {
        if self.config_refresh_pending {
            return;
        }
        self.config_refresh_pending = true;
        self.last_management_refresh = Instant::now();
        self.send(UiCommand::Config);
    }

    pub(super) fn refresh_diagnostics(&mut self) {
        if self.diagnostics_refresh_pending {
            return;
        }
        self.diagnostics_refresh_pending = true;
        self.last_management_refresh = Instant::now();
        self.send(UiCommand::Diagnostics);
    }

    pub(super) fn refresh_transfers(&mut self) {
        if self.transfer_refresh_pending {
            return;
        }
        self.transfer_refresh_pending = true;
        self.last_transfer_refresh = Instant::now();
        self.send(UiCommand::Transfers);
    }

    pub(super) fn dispatch_management_refresh(&mut self, minimized: bool) {
        if minimized || self.presentation != Presentation::Management {
            return;
        }
        if matches!(
            self.selected_tab,
            ControlTab::History | ControlTab::Transfers
        ) {
            return;
        }
        let pending = match self.selected_tab {
            ControlTab::Peers => self.peers_refresh_pending,
            ControlTab::Settings => self.config_refresh_pending,
            ControlTab::Diagnostics => self.diagnostics_refresh_pending,
            ControlTab::History | ControlTab::Transfers => false,
        };
        if pending {
            self.context.request_repaint_after(Duration::from_secs(1));
            return;
        }
        let cadence = Duration::from_secs(5);
        let remaining = cadence.saturating_sub(self.last_management_refresh.elapsed());
        if !remaining.is_zero() {
            self.context.request_repaint_after(remaining);
            return;
        }
        match self.selected_tab {
            ControlTab::Peers => self.refresh_peers(),
            ControlTab::Settings => self.refresh_config(),
            ControlTab::Diagnostics => self.refresh_diagnostics(),
            ControlTab::History | ControlTab::Transfers => unreachable!("filtered above"),
        }
        self.refresh_status();
        self.context.request_repaint_after(cadence);
    }

    pub(super) fn dispatch_transfer_refresh(&mut self) {
        if self.presentation == Presentation::Management
            && self.selected_tab == ControlTab::Transfers
            && !self.transfer_refresh_pending
            && self.last_transfer_refresh.elapsed() >= Duration::from_millis(500)
        {
            self.refresh_transfers();
        }
    }
    pub(super) fn retry_connection(&mut self) {
        self.status = None;
        self.daemon_error = None;
        self.history_error = None;
        self.peers_error = None;
        self.config_error = None;
        self.diagnostics_error = None;
        self.transfers_error = None;
        self.send(UiCommand::RetryStatus);
        self.refresh_history();
        if self.presentation == Presentation::Management {
            self.refresh_control_data();
        }
    }
}

#[cfg(test)]
pub(in crate::ui) use management::{diagnostic_card, peer_card, peer_card_header};

mod delete;
mod events;
mod history;
mod management;
mod share;
mod shell;
