//! The control centre: what the daemon is doing and how it is configured.
//!
//! Unlike the picker this view is mouse-driven and edits state, so it collects
//! [`Action`]s during the UI pass and hands them back to the caller, which
//! applies them once the frame is over and egui no longer borrows anything.

use clip_sync_ipc::protocol::{
    DiagnosticCheck, PeersResponse, SharedSettingKind, StatusResponse, TransferItem,
};

use crate::{
    daemon::{Daemon, Settings},
    theme,
};

/// A daemon mutation requested by a widget during the UI pass.
pub enum Action {
    Refresh,
    CancelTransfer(String),
    ForgetDevice(String),
    SetShared(SharedSettingKind, u64),
    SetInterfaces(Vec<String>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Status,
    Peers,
    Transfers,
    Diagnostics,
    Settings,
}

impl Tab {
    const ALL: [Self; 5] = [
        Self::Status,
        Self::Peers,
        Self::Transfers,
        Self::Diagnostics,
        Self::Settings,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Status => "Status",
            Self::Peers => "Peers",
            Self::Transfers => "Transfers",
            Self::Diagnostics => "Diagnostics",
            Self::Settings => "Settings",
        }
    }
}

#[derive(Default)]
pub struct Control {
    tab_index: usize,
    status: Option<StatusResponse>,
    peers: Option<PeersResponse>,
    transfers: Vec<TransferItem>,
    diagnostics: Vec<DiagnosticCheck>,
    settings: Option<Settings>,
    /// Mesh quota in mebibytes, which is the unit people actually think in.
    quota_draft: u64,
    /// Capture threshold in kibibytes.
    threshold_draft: u64,
    interfaces_draft: String,
    error: Option<String>,
    notice: Option<String>,
}

impl Control {
    fn tab(&self) -> Tab {
        Tab::ALL[self.tab_index]
    }

    /// Refetches everything the view can show.
    ///
    /// Each call is a handful of local socket round trips costing about a
    /// millisecond, so the view reloads wholesale rather than tracking which
    /// tab invalidated what.
    pub fn refresh(&mut self, daemon: &Daemon) {
        let mut failure = None;
        let mut record = |result: Result<(), anyhow::Error>| {
            if let Err(error) = result
                && failure.is_none()
            {
                failure = Some(format!("{error:#}"));
            }
        };

        record(daemon.status().map(|value| self.status = Some(value)));
        record(daemon.peers().map(|value| self.peers = Some(value)));
        record(daemon.transfers().map(|value| self.transfers = value));
        record(daemon.diagnostics().map(|value| self.diagnostics = value));
        record(daemon.settings().map(|value| {
            self.quota_draft = value.shared.mesh_quota_bytes / (1024 * 1024);
            self.threshold_draft = value.shared.capture_threshold_bytes / 1024;
            self.interfaces_draft = value.local.peer_interfaces.join(", ");
            self.settings = Some(value);
        }));

        self.error = failure;
    }

    /// Applies one mutation and reloads, so the view never shows a stale value
    /// the daemon has already rejected or clamped.
    pub fn apply(&mut self, daemon: &Daemon, action: Action) {
        let outcome = match action {
            Action::Refresh => Ok("Reloaded"),
            Action::CancelTransfer(id) => daemon.cancel_transfer(&id).map(|()| "Transfer cancelled"),
            Action::ForgetDevice(id) => daemon.forget_device(&id).map(|()| "Device forgotten"),
            Action::SetShared(setting, value) => {
                daemon.set_shared_setting(setting, value).map(|()| "Saved")
            }
            Action::SetInterfaces(interfaces) => {
                daemon.set_peer_interfaces(interfaces).map(|()| "Saved")
            }
        };
        match outcome {
            Ok(message) => {
                self.refresh(daemon);
                self.notice = Some(message.to_owned());
            }
            Err(error) => {
                self.error = Some(format!("{error:#}"));
                self.notice = None;
            }
        }
    }

    /// Draws the navigation rail down the left edge of the window.
    pub fn nav(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.label(
            egui::RichText::new("ClipSync")
                .size(16.0)
                .strong()
                .color(theme::TEXT_SELECTED),
        );
        ui.label(
            egui::RichText::new(self.status.as_ref().map_or_else(
                || "not connected".to_owned(),
                |status| format!("v{}", status.version),
            ))
            .monospace()
            .size(9.0)
            .weak(),
        );
        ui.add_space(14.0);

        ui.with_layout(
            egui::Layout::top_down_justified(egui::Align::LEFT),
            |ui| {
                for (index, tab) in Tab::ALL.into_iter().enumerate() {
                    let selected = index == self.tab_index;
                    let text = egui::RichText::new(tab.label()).size(12.0).color(if selected {
                        theme::TEXT_SELECTED
                    } else {
                        theme::TEXT
                    });
                    if ui.selectable_label(selected, text).clicked() {
                        self.tab_index = index;
                        self.notice = None;
                    }
                }
            },
        );

        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            if ui
                .add_sized([ui.available_width(), 22.0], egui::Button::new("Reload"))
                .clicked()
            {
                actions.push(Action::Refresh);
            }
        });
    }

    /// Draws the selected tab in the remaining space.
    pub fn body(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.spacing_mut().item_spacing = egui::vec2(8.0, 6.0);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // Settings rows and diagnostic detail read badly when stretched
                // across a maximised window, so the column stops growing.
                ui.set_max_width(ui.available_width().min(720.0));

                if let Some(error) = self.error.clone() {
                    banner(ui, theme::DANGER, &error);
                } else if let Some(notice) = self.notice.clone() {
                    banner(ui, theme::ACCENT, &notice);
                }

                match self.tab() {
                    Tab::Status => self.status_tab(ui),
                    Tab::Peers => self.peers_tab(ui, actions),
                    Tab::Transfers => self.transfers_tab(ui, actions),
                    Tab::Diagnostics => self.diagnostics_tab(ui),
                    Tab::Settings => self.settings_tab(ui, actions),
                }
            });
    }

    /// Moves to the next or previous tab, wrapping at both ends.
    pub fn cycle_tab(&mut self, forward: bool) {
        let count = Tab::ALL.len();
        self.tab_index = if forward {
            (self.tab_index + 1) % count
        } else {
            (self.tab_index + count - 1) % count
        };
        self.notice = None;
    }

    fn status_tab(&mut self, ui: &mut egui::Ui) {
        let Some(status) = self.status.as_ref() else {
            ui.label(egui::RichText::new("The daemon did not report a status.").weak());
            return;
        };

        ui.horizontal_wrapped(|ui| {
            tile(ui, "Uptime", &uptime(status.uptime_seconds));
            tile(ui, "Discovered", &status.discovered_peers.to_string());
            tile(ui, "Connected", &status.connected_peers.to_string());
        });
        ui.add_space(4.0);

        section(ui, "This node", |ui| {
            field(ui, "Hostname", &status.hostname);
            field(ui, "Version", &status.version);
            field(ui, "Config", &status.config_path);
        });

        section(ui, "Listening on", |ui| {
            if status.local_addresses.is_empty() {
                ui.label(egui::RichText::new("No usable interfaces yet.").weak());
            }
            for address in &status.local_addresses {
                ui.label(egui::RichText::new(address).monospace().size(11.0));
            }
        });
    }

    fn peers_tab(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let Some(peers) = self.peers.as_ref() else {
            ui.label(egui::RichText::new("The daemon did not report any peers.").weak());
            return;
        };

        if let Some(error) = peers.discovery_error.as_ref() {
            banner(ui, theme::DANGER, error);
        }

        section(ui, "Connected peers", |ui| {
            if peers.peers.is_empty() {
                ui.label(egui::RichText::new("No peers are connected right now.").weak());
            }
            for peer in &peers.peers {
                card(ui, |ui| {
                    ui.horizontal(|ui| {
                        dot(ui, peer.connected);
                        ui.label(
                            egui::RichText::new(&peer.hostname)
                                .strong()
                                .color(theme::TEXT_SELECTED),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                egui::RichText::new(&peer.address)
                                    .monospace()
                                    .size(10.0)
                                    .weak(),
                            );
                        });
                    });
                    if let Some(stats) = peer.stats.as_ref() {
                        ui.label(
                            egui::RichText::new(format!(
                                "{} items · {} · {} pinned",
                                stats.shared_items,
                                bytes(stats.shared_bytes),
                                stats.pinned_items
                            ))
                            .size(10.0)
                            .weak(),
                        );
                    }
                });
            }
        });

        section(ui, "Known devices", |ui| {
            // Forgotten devices stay in the list so the revocation is visible
            // and obviously not reversible from here.
            for device in &peers.devices {
                ui.horizontal(|ui| {
                    fixed_text(
                        ui,
                        DEVICE_WIDTH,
                        &device.device_id,
                        egui::FontId::monospace(10.0),
                        if device.forgotten {
                            theme::TEXT
                        } else {
                            theme::TEXT_SELECTED
                        },
                    );
                    let (badge, color) = match (device.local, device.forgotten) {
                        (true, _) => ("this node", theme::ACCENT),
                        (_, true) => ("forgotten", theme::TEXT),
                        _ => ("", theme::TEXT),
                    };
                    fixed_text(
                        ui,
                        64.0,
                        badge,
                        egui::FontId::proportional(9.0),
                        color,
                    );
                    if !device.local && !device.forgotten && ui.small_button("Forget").clicked() {
                        actions.push(Action::ForgetDevice(device.device_id.clone()));
                    }
                });
            }
        });
    }

    fn transfers_tab(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        if self.transfers.is_empty() {
            ui.add_space(8.0);
            ui.label(egui::RichText::new("Nothing is transferring right now.").weak());
            return;
        }
        for transfer in &self.transfers {
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(&transfer.peer)
                            .strong()
                            .color(theme::TEXT_SELECTED),
                    );
                    ui.label(egui::RichText::new(&transfer.state).size(10.0).weak());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Cancel").clicked() {
                            actions.push(Action::CancelTransfer(transfer.transfer_id.clone()));
                        }
                    });
                });
                let fraction = if transfer.total_bytes == 0 {
                    0.0
                } else {
                    transfer.completed_bytes as f32 / transfer.total_bytes as f32
                };
                ui.add(
                    egui::ProgressBar::new(fraction)
                        .desired_height(6.0)
                        .fill(theme::ACCENT),
                );
                ui.label(
                    egui::RichText::new(format!(
                        "{} of {}",
                        bytes(transfer.completed_bytes),
                        bytes(transfer.total_bytes)
                    ))
                    .size(10.0)
                    .weak(),
                );
            });
        }
    }

    fn diagnostics_tab(&mut self, ui: &mut egui::Ui) {
        if self.diagnostics.is_empty() {
            ui.label(egui::RichText::new("The daemon reported no checks.").weak());
            return;
        }
        for check in &self.diagnostics {
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    dot(ui, check.ok);
                    ui.label(
                        egui::RichText::new(check.name.replace('_', " "))
                            .strong()
                            .color(theme::TEXT_SELECTED),
                    );
                });
                ui.label(egui::RichText::new(&check.detail).size(10.5).color(theme::TEXT));
            });
        }
    }

    fn settings_tab(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let Some(settings) = self.settings.as_ref() else {
            ui.label(egui::RichText::new("The daemon did not report its configuration.").weak());
            return;
        };

        let saved_quota = settings.shared.mesh_quota_bytes / (1024 * 1024);
        let saved_threshold = settings.shared.capture_threshold_bytes / 1024;
        let saved_interfaces = settings.local.peer_interfaces.join(", ");

        section(ui, "Shared across the mesh", |ui| {
            ui.label(
                egui::RichText::new(format!(
                    // The revision is a content hash; a prefix is enough to tell
                    // two of them apart, and the full string is unreadable.
                    "revision {}",
                    settings.shared.revision.chars().take(12).collect::<String>()
                ))
                .monospace()
                .size(9.0)
                .weak(),
            );
            ui.horizontal(|ui| {
                label_cell(ui, "History quota");
                ui.add_sized(
                    VALUE_SIZE,
                    egui::DragValue::new(&mut self.quota_draft).suffix(" MiB"),
                );
                if ui
                    .add_enabled(self.quota_draft != saved_quota, egui::Button::new("Save"))
                    .clicked()
                {
                    actions.push(Action::SetShared(
                        SharedSettingKind::MeshQuotaBytes,
                        self.quota_draft * 1024 * 1024,
                    ));
                }
            });
            ui.horizontal(|ui| {
                label_cell(ui, "Capture threshold");
                ui.add_sized(
                    VALUE_SIZE,
                    egui::DragValue::new(&mut self.threshold_draft).suffix(" KiB"),
                );
                if ui
                    .add_enabled(
                        self.threshold_draft != saved_threshold,
                        egui::Button::new("Save"),
                    )
                    .clicked()
                {
                    actions.push(Action::SetShared(
                        SharedSettingKind::CaptureThresholdBytes,
                        self.threshold_draft * 1024,
                    ));
                }
            });
            ui.label(
                egui::RichText::new("Changes here replicate to every device in the mesh.")
                    .size(10.0)
                    .weak(),
            );
        });

        section(ui, "This node only", |ui| {
            ui.horizontal(|ui| {
                label_cell(ui, "Peer interfaces");
                ui.add(
                    egui::TextEdit::singleline(&mut self.interfaces_draft)
                        .hint_text("all interfaces")
                        .desired_width(220.0),
                );
                if ui
                    .add_enabled(
                        self.interfaces_draft != saved_interfaces,
                        egui::Button::new("Save"),
                    )
                    .clicked()
                {
                    actions.push(Action::SetInterfaces(parse_interfaces(
                        &self.interfaces_draft,
                    )));
                }
            });
            field(ui, "Listen port", &settings.local.listen_port.to_string());
            field(
                ui,
                "Discovery interval",
                &format!("{}s", settings.local.discovery_interval_seconds),
            );
            field(
                ui,
                "Reconcile interval",
                &format!("{}s", settings.local.reconcile_interval_seconds),
            );
            field(
                ui,
                "Reconnect backoff",
                &format!(
                    "{}s – {}s",
                    settings.local.reconnect_min_seconds, settings.local.reconnect_max_seconds
                ),
            );
            field(
                ui,
                "Mesh key file",
                if settings.local.mesh_key_file_configured {
                    "configured"
                } else {
                    "not configured"
                },
            );
            field(ui, "Config", &settings.local.config_path);
        });
    }
}

/// Splits a comma or whitespace separated interface list, dropping blanks.
fn parse_interfaces(raw: &str) -> Vec<String> {
    raw.split([',', ' ', '\t'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn banner(ui: &mut egui::Ui, color: egui::Color32, message: &str) {
    egui::Frame::new()
        .fill(theme::CARD_BACKGROUND)
        .corner_radius(4)
        .inner_margin(6.0)
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.label(egui::RichText::new(message).color(color).size(11.0));
        });
}

fn section(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(14.0);
    ui.label(
        egui::RichText::new(title.to_uppercase())
            .monospace()
            .size(9.0)
            .color(theme::ACCENT),
    );
    ui.add_space(4.0);
    body(ui);
}

fn card(ui: &mut egui::Ui, body: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(theme::CARD_BACKGROUND)
        .corner_radius(5)
        .inner_margin(8.0)
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 4.0;
            body(ui);
        });
}

/// A headline number, large enough to read without hunting for it.
fn tile(ui: &mut egui::Ui, label: &str, value: &str) {
    egui::Frame::new()
        .fill(theme::CARD_BACKGROUND)
        .corner_radius(5)
        .inner_margin(egui::Margin::symmetric(14, 8))
        .show(ui, |ui| {
            ui.vertical(|ui| {
                // A fixed width keeps a one-digit tile the same size as a
                // six-character one, so the row reads as a set.
                ui.set_min_width(96.0);
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(
                    egui::RichText::new(value)
                        .size(17.0)
                        .strong()
                        .color(theme::TEXT_SELECTED),
                );
                ui.label(
                    egui::RichText::new(label.to_uppercase())
                        .monospace()
                        .size(8.0)
                        .weak(),
                );
            });
        });
}

/// Width every label column in the view shares, so values line up down the page.
const LABEL_WIDTH: f32 = 148.0;
/// Size every editable value shares, so the Save buttons line up too.
const VALUE_SIZE: [f32; 2] = [120.0, 20.0];

/// Width of the device identifier column, sized for a UUID.
const DEVICE_WIDTH: f32 = 300.0;

/// Reserves a fixed-width cell and left-aligns text in it.
///
/// A plain `Label` shrinks to its text and `add_sized` centres it, so neither
/// gives a column that later widgets line up against.
fn fixed_text(ui: &mut egui::Ui, width: f32, text: &str, font: egui::FontId, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 18.0), egui::Sense::hover());
    ui.painter()
        .text(rect.left_center(), egui::Align2::LEFT_CENTER, text, font, color);
}

fn label_cell(ui: &mut egui::Ui, text: &str) {
    let color = ui.visuals().weak_text_color();
    fixed_text(ui, LABEL_WIDTH, text, egui::FontId::proportional(11.0), color);
}

fn field(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        label_cell(ui, label);
        ui.label(
            egui::RichText::new(value)
                .monospace()
                .size(11.0)
                .color(theme::TEXT_SELECTED),
        );
    });
}

fn dot(ui: &mut egui::Ui, ok: bool) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
    let color = if ok { theme::ACCENT } else { theme::DANGER };
    ui.painter()
        .circle_filled(rect.center(), 3.5, color);
}

fn uptime(seconds: u64) -> String {
    let (days, hours, minutes) = (seconds / 86400, seconds % 86400 / 3600, seconds % 3600 / 60);
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

fn bytes(count: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = count as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{count} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
