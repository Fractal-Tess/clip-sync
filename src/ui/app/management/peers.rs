use eframe::egui::{self, CornerRadius, Frame, Margin, RichText, Stroke};

use super::super::ClipSyncApp;
use crate::{
    ipc::protocol::PeerItem,
    ui::{
        history::relative_history_time,
        ipc_types::{PendingScope, UiCommand},
        style::{
            BORDER, ERROR, MUTED, SUCCESS, SURFACE, format_bytes, management_grid_columns,
            message_panel,
        },
    },
};

impl ClipSyncApp {
    #[allow(
        clippy::too_many_lines,
        reason = "peer categories and remembered-device controls form one cohesive route"
    )]
    pub(in crate::ui) fn peers_tab(&mut self, ui: &mut egui::Ui) {
        if let Some(error) = &self.peers_error {
            message_panel(ui, &format!("{error}\nRetrying automatically…"), ERROR);
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
        ui.add_space(10.0);

        let online = peers
            .peers
            .iter()
            .filter(|peer| peer.connected)
            .collect::<Vec<_>>();
        let offline = peers
            .peers
            .iter()
            .filter(|peer| !peer.connected)
            .collect::<Vec<_>>();
        peer_grid(ui, "Online", &online);
        ui.add_space(10.0);
        peer_grid(ui, "Offline", &offline);

        ui.add_space(12.0);
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
}

fn peer_grid(ui: &mut egui::Ui, title: &str, peers: &[&PeerItem]) {
    ui.horizontal(|ui| {
        ui.strong(title);
        ui.label(
            RichText::new(peers.len().to_string())
                .color(MUTED)
                .monospace()
                .size(11.0),
        );
    });
    ui.add_space(4.0);
    if peers.is_empty() {
        ui.label(RichText::new(format!("No {} peers", title.to_ascii_lowercase())).color(MUTED));
        return;
    }
    let columns = management_grid_columns(ui.available_width());
    ui.columns(columns, |uis| {
        for (index, peer) in peers.iter().enumerate() {
            let column = &mut uis[index % columns];
            peer_card(column, peer);
            column.add_space(8.0);
        }
    });
}

pub(in crate::ui) fn peer_card(ui: &mut egui::Ui, peer: &PeerItem) -> egui::Response {
    let status_color = if peer.connected { SUCCESS } else { MUTED };
    Frame::new()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(7))
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.set_min_height(116.0);
            peer_card_header(ui, peer, status_color);
            ui.label(
                RichText::new(&peer.address)
                    .color(MUTED)
                    .monospace()
                    .size(11.0),
            );
            ui.add_space(6.0);
            if let Some(stats) = peer.stats {
                ui.columns(3, |columns| {
                    stat(&mut columns[0], stats.shared_items, "SHARED");
                    stat(&mut columns[1], stats.shared_bytes, "BYTES");
                    stat(&mut columns[2], stats.pinned_items, "PINNED");
                });
                let latest = stats.last_shared_millis.map_or_else(
                    || "No retained items".to_owned(),
                    |millis| format!("Latest share {}", relative_history_time(millis)),
                );
                ui.label(RichText::new(latest).color(MUTED).size(10.0));
            } else {
                ui.label(
                    RichText::new("History stats unavailable until identity is authenticated")
                        .color(MUTED)
                        .size(10.0),
                );
            }
        })
        .response
}

pub(in crate::ui) fn peer_card_header(
    ui: &mut egui::Ui,
    peer: &PeerItem,
    status_color: egui::Color32,
) -> (egui::Response, egui::Response) {
    ui.horizontal(|ui| {
        ui.colored_label(status_color, "●");
        let status_width = 50.0;
        let name_width =
            (ui.available_width() - status_width - ui.spacing().item_spacing.x).max(40.0);
        let name = ui.add_sized(
            [name_width, 18.0],
            egui::Label::new(RichText::new(&peer.hostname).strong()).truncate(),
        );
        let status = ui
            .allocate_ui_with_layout(
                egui::Vec2::new(status_width, 18.0),
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| {
                    ui.label(
                        RichText::new(if peer.connected { "ONLINE" } else { "OFFLINE" })
                            .color(status_color)
                            .monospace()
                            .size(10.0),
                    )
                },
            )
            .inner;
        (name, status)
    })
    .inner
}

fn stat(ui: &mut egui::Ui, value: u64, label: &str) {
    let value = if label == "BYTES" {
        format_bytes(value)
    } else {
        value.to_string()
    };
    ui.strong(RichText::new(value).monospace().size(11.0));
    ui.label(RichText::new(label).color(MUTED).monospace().size(9.0));
}
