use eframe::egui::{self, CornerRadius, Frame, Margin, RichText, Stroke};

use super::ClipSyncApp;
use crate::{
    ipc::protocol::SharedSettingKind,
    ui::{
        ipc_types::{PendingScope, PendingSetting, UiCommand},
        style::{
            BORDER, CYAN, ERROR, MUTED, SUCCESS, SURFACE, config_pointer, config_pointer_u64,
            config_seconds, format_bytes, message_panel, peer_row, setting_row,
        },
    },
};

impl ClipSyncApp {
    #[allow(
        clippy::too_many_lines,
        reason = "transfer rendering keeps progress and confirmed cancellation together"
    )]
    pub(super) fn transfers_tab(&mut self, ui: &mut egui::Ui) {
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
                            .desired_width(ui.available_width()),
                    );
                    if !matches!(transfer.state.as_str(), "complete" | "cancelled" | "failed") {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .add_enabled(
                                    self.mutation_block_reason(PendingScope::TransferCancel)
                                        .is_none(),
                                    egui::Button::new("Cancel transfer"),
                                )
                                .on_disabled_hover_text(
                                    self.mutation_block_reason(PendingScope::TransferCancel)
                                        .unwrap_or_default(),
                                )
                                .clicked()
                            {
                                requested_cancel = Some(transfer.transfer_id.clone());
                            }
                        });
                    }
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
                        let disabled_reason =
                            self.mutation_block_reason(PendingScope::TransferCancel);
                        if ui
                            .add_enabled(
                                disabled_reason.is_none(),
                                egui::Button::new("Confirm cancel"),
                            )
                            .on_disabled_hover_text(disabled_reason.unwrap_or_default())
                            .clicked()
                        {
                            self.notice = None;
                            self.send(UiCommand::TransferCancel { transfer_id });
                        }
                        if ui
                            .add_enabled(
                                self.mutation_block_reason(PendingScope::TransferCancel).is_none(),
                                egui::Button::new("Keep transfer"),
                            )
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
    pub(super) fn peers_tab(&mut self, ui: &mut egui::Ui) {
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
                        .add_enabled(
                            self.mutation_block_reason(PendingScope::ForgetDevice)
                                .is_none(),
                            egui::Button::new("Forget"),
                        )
                        .on_disabled_hover_text(
                            self.mutation_block_reason(PendingScope::ForgetDevice)
                                .unwrap_or_default(),
                        )
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
                    !self.forget_device_id.trim().is_empty()
                        && self
                            .mutation_block_reason(PendingScope::ForgetDevice)
                            .is_none(),
                    egui::Button::new("Review forget"),
                )
                .on_disabled_hover_text(
                    self.mutation_block_reason(PendingScope::ForgetDevice)
                        .unwrap_or_default(),
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
                        let disabled_reason =
                            self.mutation_block_reason(PendingScope::ForgetDevice);
                        if ui
                            .add_enabled(
                                disabled_reason.is_none(),
                                egui::Button::new("Confirm forget"),
                            )
                            .on_disabled_hover_text(disabled_reason.unwrap_or_default())
                            .clicked()
                        {
                            self.notice = None;
                            self.send(UiCommand::ForgetDevice { device_id });
                        }
                        if ui
                            .add_enabled(
                                self.mutation_block_reason(PendingScope::ForgetDevice).is_none(),
                                egui::Button::new("Keep device"),
                            )
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
    pub(super) fn settings_tab(&mut self, ui: &mut egui::Ui) {
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
                    self.mutation_block_reason(PendingScope::Setting).is_none()
                        && self
                            .mesh_quota_input
                            .parse::<u64>()
                            .is_ok_and(|value| value > 0),
                    egui::Button::new("Review quota"),
                )
                .on_disabled_hover_text(
                    self.mutation_block_reason(PendingScope::Setting)
                        .unwrap_or_default(),
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
                    self.mutation_block_reason(PendingScope::Setting).is_none()
                        && self
                            .capture_threshold_input
                            .parse::<u64>()
                            .is_ok_and(|value| value > 0),
                    egui::Button::new("Review threshold"),
                )
                .on_disabled_hover_text(
                    self.mutation_block_reason(PendingScope::Setting)
                        .unwrap_or_default(),
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
                        let disabled_reason = self.mutation_block_reason(PendingScope::Setting);
                        if ui
                            .add_enabled(
                                disabled_reason.is_none(),
                                egui::Button::new("Apply setting"),
                            )
                            .on_disabled_hover_text(disabled_reason.unwrap_or_default())
                            .clicked()
                        {
                            self.notice = None;
                            self.send(UiCommand::UpdateSharedSetting {
                                setting: pending.kind,
                                value: pending.value,
                            });
                        }
                        if ui
                            .add_enabled(
                                self.mutation_block_reason(PendingScope::Setting).is_none(),
                                egui::Button::new("Cancel"),
                            )
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

    pub(super) fn diagnostics_tab(&self, ui: &mut egui::Ui) {
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
