use eframe::egui::{self, CornerRadius, Frame, Margin, RichText, Stroke};

use super::ClipSyncApp;
use crate::{
    ipc::protocol::ShareClipboardResponse,
    ui::{
        ipc_types::PendingScope,
        style::{CYAN, MUTED, SURFACE, format_bytes},
    },
};

impl ClipSyncApp {
    pub(super) fn share_confirmation(
        &mut self,
        ui: &mut egui::Ui,
        inspection: &ShareClipboardResponse,
    ) {
        Frame::new()
            .fill(SURFACE)
            .stroke(Stroke::new(1.0, CYAN))
            .corner_radius(CornerRadius::same(7))
            .inner_margin(Margin::same(10))
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
                    let share_disabled_reason = self.mutation_block_reason(PendingScope::Share);
                    if ui
                        .add_enabled(
                            share_disabled_reason.is_none(),
                            egui::Button::new("Confirm share"),
                        )
                        .on_disabled_hover_text(share_disabled_reason.unwrap_or_default())
                        .clicked()
                    {
                        self.share_clipboard(true);
                    }
                    if ui
                        .add_enabled(share_disabled_reason.is_none(), egui::Button::new("Cancel"))
                        .on_disabled_hover_text(share_disabled_reason.unwrap_or_default())
                        .clicked()
                    {
                        self.share_inspection = None;
                    }
                });
            });
    }
}
