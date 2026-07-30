use eframe::egui::{self, CornerRadius, Frame, Margin, RichText, Stroke};

use super::super::ClipSyncApp;
use crate::{
    ipc::protocol::SharedSettingKind,
    ui::{
        ipc_types::{PendingScope, PendingSetting, UiCommand},
        style::{
            CYAN, ERROR, MUTED, SURFACE, config_pointer, config_pointer_u64, config_seconds,
            format_bytes, format_bytes_exact, message_panel, parse_byte_size, setting_row,
        },
    },
};

impl ClipSyncApp {
    #[allow(
        clippy::too_many_lines,
        reason = "effective settings and their validated update forms share one route"
    )]
    pub(in crate::ui) fn settings_tab(&mut self, ui: &mut egui::Ui) {
        if let Some(error) = &self.config_error {
            message_panel(ui, &format!("{error}\nRetrying automatically…"), ERROR);
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
        let parsed_mesh_quota = parse_byte_size(&self.mesh_quota_input).filter(|value| *value > 0);
        ui.horizontal(|ui| {
            ui.label("Mesh quota");
            ui.add(
                egui::TextEdit::singleline(&mut self.mesh_quota_input)
                    .desired_width(180.0)
                    .hint_text("e.g. 1 GiB"),
            );
            if ui
                .add_enabled(
                    self.mutation_block_reason(PendingScope::Setting).is_none()
                        && parsed_mesh_quota.is_some(),
                    egui::Button::new("Review quota"),
                )
                .on_disabled_hover_text(
                    self.mutation_block_reason(PendingScope::Setting)
                        .unwrap_or("Use bytes or a unit such as MiB or GiB."),
                )
                .clicked()
                && let Some(value) = parsed_mesh_quota
            {
                self.pending_setting = Some(PendingSetting {
                    kind: SharedSettingKind::MeshQuotaBytes,
                    value,
                });
            }
        });
        ui.label(
            RichText::new("Enter exact bytes or a value such as 512 MiB or 1.5 GiB.")
                .color(MUTED)
                .size(11.0),
        );
        if !self.mesh_quota_input.is_empty() && parsed_mesh_quota.is_none() {
            ui.colored_label(
                ERROR,
                "Invalid quota. Fractions must resolve to whole bytes, for example 1.5 GiB.",
            );
        }
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
                        format_bytes_exact(pending.value)
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
}
