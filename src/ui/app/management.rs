mod diagnostics;
mod peers;
mod settings;

#[cfg(test)]
pub(in crate::ui) use diagnostics::diagnostic_card;
#[cfg(test)]
pub(in crate::ui) use peers::{peer_card, peer_card_header};

use eframe::egui::{self, CornerRadius, Frame, Margin, RichText, Stroke};

use super::ClipSyncApp;
use crate::ui::{
    ipc_types::{PendingScope, UiCommand},
    style::{BORDER, ERROR, MUTED, SURFACE, format_bytes, message_panel},
};

impl ClipSyncApp {
    #[allow(
        clippy::too_many_lines,
        reason = "transfer progress and its cancellation confirmation form one cohesive route"
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
}
