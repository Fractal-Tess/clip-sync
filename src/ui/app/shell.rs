use eframe::egui::{self, CornerRadius, Frame, Margin, RichText, ScrollArea};

use super::{ClipSyncApp, WindowState};
use crate::ui::{
    history::{ControlTab, history_poll_allowed, history_refresh_on_focus_regain, navigation_bar},
    style::{BACKGROUND, ERROR, MUTED, SUCCESS, brand_header, unavailable_banner},
    window::{context_window_geometry, query_hyprland_geometry, save_window_geometry},
};

impl ClipSyncApp {
    pub(super) fn shell(&mut self, ui: &mut egui::Ui) {
        if self.window_state == WindowState::Close {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        Frame::new()
            .fill(BACKGROUND)
            .inner_margin(Margin::same(16))
            .show(ui, |ui| {
                self.shell_header(ui);
                ui.add_space(8.0);
                if let Some(error) = self.daemon_error.clone() {
                    let socket = self.paths.socket.clone();
                    unavailable_banner(ui, &socket, &error, || self.retry_connection());
                }
                if let Some(notice) = &self.notice {
                    notice.show(ui);
                }

                match self.selected_tab {
                    ControlTab::History => self.history_route(ui),
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

    pub(super) fn shell_header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            brand_header(ui, &self.brand_icon);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (color, text) = if self.daemon_error.is_some() {
                    (ERROR, "daemon unavailable".to_owned())
                } else if let Some(status) = &self.status {
                    (
                        SUCCESS,
                        format!("{} · {} peers", status.hostname, status.discovered_peers),
                    )
                } else {
                    (MUTED, "connecting…".to_owned())
                };
                ui.label(RichText::new(text).color(MUTED).size(11.0));
                ui.colored_label(color, "●");
            });
        });
        ui.add_space(6.0);
        let navigation = navigation_bar(ui, self.selected_tab);
        if let Some(tab) = navigation.destination {
            self.navigate_to(tab);
        }
        ui.separator();
    }
}

impl eframe::App for ClipSyncApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.window_geometry = context_window_geometry(ui.ctx(), self.window_geometry);
        let (focused, minimized) = ui.ctx().input(|input| {
            (
                input.viewport().focused.unwrap_or(input.focused),
                input.viewport().minimized.unwrap_or(false),
            )
        });
        let was_focused = self.viewport_focused;
        self.viewport_focused = focused;
        ui.painter()
            .rect_filled(ui.max_rect(), CornerRadius::ZERO, BACKGROUND);
        self.poll_signals();
        let history_poll_eligible =
            history_poll_allowed(self.selected_tab, minimized, &self.search);
        self.poll_events(history_poll_eligible);
        if history_refresh_on_focus_regain(
            was_focused,
            focused,
            self.selected_tab,
            minimized,
            &self.search,
        ) {
            self.dispatch_history_request();
        }
        self.dispatch_pending_history_refresh(history_poll_eligible);
        self.dispatch_live_history_refresh(minimized);
        self.dispatch_management_refresh(minimized);
        self.dispatch_transfer_refresh();
        self.shell(ui);
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
