use eframe::egui::{self, CornerRadius, Frame, Margin, Stroke};

use super::ClipSyncApp;
use crate::{
    ipc::protocol::HistoryUpdateAction,
    ui::{
        ipc_types::{MutationKind, PendingScope, UiCommand},
        style::{ERROR, SURFACE},
    },
};

impl ClipSyncApp {
    pub(super) fn history_delete_confirmation(&mut self, ui: &mut egui::Ui) {
        let Some(content_id) = self.pending_history_delete.clone() else {
            return;
        };
        Frame::new()
            .fill(SURFACE)
            .stroke(Stroke::new(1.0, ERROR))
            .corner_radius(CornerRadius::same(7))
            .inner_margin(Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.strong("Delete this item from every mesh device?");
                ui.horizontal(|ui| {
                    let disabled_reason = self.mutation_block_reason(PendingScope::HistoryMutation);
                    if ui
                        .add_enabled(
                            disabled_reason.is_none(),
                            egui::Button::new("Confirm delete"),
                        )
                        .on_disabled_hover_text(disabled_reason.unwrap_or_default())
                        .clicked()
                    {
                        self.notice = None;
                        self.pending_history_delete = None;
                        self.send(UiCommand::HistoryUpdate {
                            content_id,
                            action: HistoryUpdateAction::Delete,
                            kind: MutationKind::Delete,
                        });
                    }
                    if ui.button("Keep item").clicked() {
                        self.pending_history_delete = None;
                    }
                });
            });
    }
}
