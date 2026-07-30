use eframe::egui::{self, CornerRadius, Frame, Margin, RichText, Stroke};

use super::super::ClipSyncApp;
use crate::{
    ipc::protocol::DiagnosticCheck,
    ui::style::{BORDER, ERROR, MUTED, SUCCESS, SURFACE, management_grid_columns, message_panel},
};

impl ClipSyncApp {
    pub(in crate::ui) fn diagnostics_tab(&self, ui: &mut egui::Ui) {
        if let Some(error) = &self.diagnostics_error {
            message_panel(ui, &format!("{error}\nRetrying automatically…"), ERROR);
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
        diagnostics_grid(ui, &self.diagnostics);
    }
}

fn diagnostics_grid(ui: &mut egui::Ui, checks: &[DiagnosticCheck]) {
    let columns = management_grid_columns(ui.available_width());
    ui.columns(columns, |uis| {
        for (index, check) in checks.iter().enumerate() {
            let column = &mut uis[index % columns];
            diagnostic_card(column, check);
            column.add_space(8.0);
        }
    });
}

pub(in crate::ui) fn diagnostic_card(ui: &mut egui::Ui, check: &DiagnosticCheck) -> egui::Response {
    let status_color = if check.ok { SUCCESS } else { ERROR };
    Frame::new()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(7))
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.set_min_height(82.0);
            ui.horizontal(|ui| {
                ui.colored_label(status_color, "●");
                ui.strong(&check.name);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(if check.ok { "PASS" } else { "ISSUE" })
                            .color(status_color)
                            .monospace()
                            .size(10.0),
                    );
                });
            });
            ui.add_space(5.0);
            ui.label(RichText::new(&check.detail).color(MUTED).size(11.0));
        })
        .response
}
