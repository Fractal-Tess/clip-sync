use std::collections::HashSet;

use eframe::egui::{
    self, Color32, CornerRadius, Frame, Key, Margin, RichText, ScrollArea, Stroke, Vec2,
};

use super::{ClipSyncApp, WindowState};
use crate::{
    ipc::protocol::HistoryUpdateAction,
    ui::{
        Presentation,
        history::{
            AutocompleteIntent, SwitcherIntent, SwitcherKey, apply_autocomplete_key,
            apply_filter_suggestion, apply_switcher_key, autocomplete_popup,
            filter_completion_context, filter_suggestions, history_card_actions,
            history_card_metadata, history_card_preview, history_card_regions,
            history_card_tooltip, history_card_widget_info, history_column_count,
            history_filter_help, history_item_has_image, history_search_row,
            history_shortcuts_allowed, history_visible_grid_rows, presentation_switcher_key,
            switcher_key,
        },
        ipc_types::{
            HistoryAction, ImagePreviewState, MutationKind, PendingScope, UiCommand,
            share_confirmation_visible,
        },
        style::{
            BORDER, CYAN, ERROR, HISTORY_GRID_GAP, HISTORY_SEARCH_ROW_COUNT,
            MANAGEMENT_HISTORY_CARD_HEIGHT, MUTED, QUICK_HISTORY_CARD_HEIGHT,
            QUICK_HISTORY_FOOTER_HEIGHT, SURFACE, message_panel,
        },
    },
};

impl ClipSyncApp {
    #[allow(
        clippy::too_many_lines,
        reason = "keyboard, autocomplete, and compact History actions share one event pass"
    )]
    pub(super) fn history_route(&mut self, ui: &mut egui::Ui) {
        let input_key = ui.input(switcher_key);
        let quick_escape =
            self.presentation == Presentation::Quick && input_key == SwitcherKey::Escape;

        debug_assert_eq!(HISTORY_SEARCH_ROW_COUNT, 1);
        let management = self.presentation == Presentation::Management;
        let share_disabled_reason = self.mutation_block_reason(PendingScope::Share);
        let search_row =
            history_search_row(ui, &mut self.search, management, share_disabled_reason);
        let search = search_row.search;
        if search_row.share_clicked {
            self.notice = None;
            self.share_inspection = None;
            self.share_clipboard(false);
        }
        if search_row.help_clicked {
            self.filter_help_open = !self.filter_help_open;
        }
        if self.filter_help_open {
            Frame::new()
                .fill(SURFACE)
                .stroke(Stroke::new(1.0, BORDER))
                .corner_radius(CornerRadius::same(6))
                .inner_margin(Margin::symmetric(10, 6))
                .show(ui, |ui| {
                    ui.label(RichText::new(history_filter_help()).color(MUTED).size(11.0));
                });
        }
        if self.window_state == WindowState::NeedsFocus {
            search.request_focus();
            self.window_state = WindowState::Ready;
        }
        if search.changed() {
            self.autocomplete_selected = 0;
            self.autocomplete_dismissed = false;
            self.selected_history = 0;
            self.selected_content_id = None;
            self.schedule_history_refresh();
        }

        let shortcut_scope = history_shortcuts_allowed(ui, search.id, &self.history_card_focus_ids);
        let focused_card_owns_enter = input_key == SwitcherKey::Enter
            && ui.memory(|memory| {
                memory
                    .focused()
                    .is_some_and(|id| self.history_card_focus_ids.contains(&id))
            });
        let mut pressed_key = if shortcut_scope && !focused_card_owns_enter {
            input_key
        } else {
            SwitcherKey::None
        };
        let completion = filter_completion_context(&self.search);
        let suggestions = completion.as_ref().map_or_else(Vec::new, |completion| {
            filter_suggestions(completion, &self.known_devices, &self.known_types)
        });
        let autocomplete_open = !self.autocomplete_dismissed && !suggestions.is_empty();
        let autocomplete_tab = autocomplete_open
            && search.has_focus()
            && ui
                .ctx()
                .input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Tab));
        let mut accepted_suggestion = None;
        if autocomplete_open {
            self.autocomplete_selected = self
                .autocomplete_selected
                .min(suggestions.len().saturating_sub(1));
            if search.has_focus() {
                match apply_autocomplete_key(
                    pressed_key,
                    &mut self.autocomplete_selected,
                    suggestions.len(),
                ) {
                    AutocompleteIntent::None => {}
                    AutocompleteIntent::Consumed => pressed_key = SwitcherKey::None,
                    AutocompleteIntent::Accept(index) => {
                        accepted_suggestion = Some(index);
                        pressed_key = SwitcherKey::None;
                    }
                    AutocompleteIntent::Dismiss => {
                        self.autocomplete_dismissed = true;
                        pressed_key = SwitcherKey::None;
                    }
                }
            }
            if autocomplete_tab {
                accepted_suggestion = Some(self.autocomplete_selected);
            }
            if let Some(clicked) = autocomplete_popup(
                ui,
                search.rect,
                completion.as_ref().expect("open autocomplete has context"),
                &suggestions,
                self.autocomplete_selected,
            ) {
                accepted_suggestion = Some(clicked);
            }
        }
        if let Some(index) = accepted_suggestion
            && let Some(completion) = completion.as_ref()
            && let Some(suggestion) = suggestions.get(index)
        {
            apply_filter_suggestion(&mut self.search, completion, &suggestion.value);
            if let Some(mut state) = egui::text_edit::TextEditState::load(ui.ctx(), search.id) {
                let cursor = egui::text::CCursor::new(self.search.chars().count());
                state
                    .cursor
                    .set_char_range(Some(egui::text::CCursorRange::one(cursor)));
                state.store(ui.ctx(), search.id);
            }
            search.request_focus();
            self.autocomplete_selected = 0;
            self.autocomplete_dismissed = true;
            self.selected_history = 0;
            self.selected_content_id = None;
            self.schedule_history_refresh();
        }

        if shortcut_scope
            && ui.input(|input| input.modifiers.ctrl && input.key_pressed(Key::P))
            && self
                .mutation_block_reason(PendingScope::HistoryMutation)
                .is_none()
            && let Some(item) = self.history.get(self.selected_history)
        {
            let content_id = item.content_id.clone();
            let action = if item.pinned {
                HistoryUpdateAction::Unpin
            } else {
                HistoryUpdateAction::Pin
            };
            self.notice = None;
            self.send(UiCommand::HistoryUpdate {
                content_id,
                action,
                kind: MutationKind::Pin,
            });
        }

        if share_confirmation_visible(self.presentation, self.share_inspection.is_some())
            && let Some(inspection) = self.share_inspection.clone()
        {
            self.share_confirmation(ui, &inspection);
        }
        self.history_delete_confirmation(ui);
        ui.add_space(5.0);
        if let Some(error) = &self.history_error {
            if self.history.is_empty() {
                message_panel(ui, error, ERROR);
            } else {
                ui.label(
                    RichText::new(format!("Live refresh delayed: {error}"))
                        .color(ERROR)
                        .size(11.0),
                )
                .on_hover_text(
                    "Showing the last successful history snapshot; cards remain usable.",
                );
            }
        }

        let footer = if self.presentation == Presentation::Quick {
            QUICK_HISTORY_FOOTER_HEIGHT
        } else {
            0.0
        };
        let grid_height = (ui.available_height() - footer).max(80.0);
        let columns = history_column_count(ui.available_width());
        let card_height = if self.presentation == Presentation::Management {
            MANAGEMENT_HISTORY_CARD_HEIGHT
        } else {
            QUICK_HISTORY_CARD_HEIGHT
        };
        let page_rows = history_visible_grid_rows(grid_height, card_height);
        if quick_escape {
            pressed_key = SwitcherKey::Escape;
        } else {
            pressed_key = presentation_switcher_key(self.presentation, pressed_key);
        }
        let activation_blocked = self.history_loading
            || self
                .mutation_block_reason(PendingScope::HistoryMutation)
                .is_some();
        match apply_switcher_key(
            pressed_key,
            &mut self.selected_history,
            self.history.len(),
            columns,
            page_rows,
            activation_blocked,
        ) {
            SwitcherIntent::None => {}
            SwitcherIntent::Moved => {
                self.selected_content_id = self
                    .history
                    .get(self.selected_history)
                    .map(|item| item.content_id.clone());
                self.scroll_selected_history = true;
            }
            SwitcherIntent::Close => {
                self.window_state = WindowState::Close;
                return;
            }
            SwitcherIntent::Activate => {
                let content_id = self.history[self.selected_history].content_id.clone();
                self.notice = None;
                self.send(UiCommand::Activate {
                    content_id,
                    kind: if self.presentation.activation_closes() {
                        MutationKind::ActivateQuick
                    } else {
                        MutationKind::Activate
                    },
                });
            }
        }

        if self.history.is_empty() {
            if self.history_error.is_none() {
                message_panel(
                    ui,
                    if self.history_loading {
                        "Loading history…"
                    } else {
                        "No matching clipboard history"
                    },
                    MUTED,
                );
            }
        } else {
            ui.allocate_ui(Vec2::new(ui.available_width(), grid_height), |ui| {
                self.history_grid(ui, self.presentation == Presentation::Management);
            });
        }
        if self.presentation == Presentation::Quick {
            ui.label(
                RichText::new(
                    "←→↑↓ navigate    PgUp/PgDn page    Enter activate    Ctrl+P pin/unpin    Esc close",
                )
                .monospace()
                .color(MUTED)
                .size(10.0),
            );
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the grid keeps card rendering and its keyboard/mutation actions together"
    )]
    pub(super) fn history_grid(&mut self, ui: &mut egui::Ui, allow_delete: bool) {
        let columns = history_column_count(ui.available_width());
        let columns_f32 = f32::from(u16::try_from(columns).expect("history columns fit in u16"));
        let total_gap = HISTORY_GRID_GAP * (columns_f32 - 1.0);
        let card_width = ((ui.available_width() - total_gap - 14.0) / columns_f32)
            .floor()
            .max(180.0);
        let card_height = if allow_delete {
            MANAGEMENT_HISTORY_CARD_HEIGHT
        } else {
            QUICK_HISTORY_CARD_HEIGHT
        };
        let mut action = None;
        let mut requested_previews = Vec::new();
        let mut current_card_focus_ids = HashSet::new();
        let grid_id = if allow_delete {
            "management-history-grid"
        } else {
            "quick-history-grid"
        };

        ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Grid::new(grid_id)
                    .num_columns(columns)
                    .spacing(Vec2::splat(HISTORY_GRID_GAP))
                    .show(ui, |ui| {
                        for index in 0..self.history.len() {
                            let item = self.history[index].clone();
                            let selected = index == self.selected_history;
                            let has_image = history_item_has_image(&item);
                            let preview = self.image_previews.get(&item.content_id);
                            let card = Frame::new()
                                .fill(if selected {
                                    Color32::from_rgb(24, 48, 54)
                                } else {
                                    SURFACE
                                })
                                .stroke(Stroke::new(1.0, if selected { CYAN } else { BORDER }))
                                .corner_radius(CornerRadius::same(8))
                                .inner_margin(Margin::same(8))
                                .show(ui, |ui| {
                                    let inner_size = Vec2::new(
                                        (card_width - 16.0).max(160.0),
                                        card_height - 16.0,
                                    );
                                    let (inner_rect, _) =
                                        ui.allocate_exact_size(inner_size, egui::Sense::hover());
                                    let regions = history_card_regions(
                                        inner_rect,
                                        allow_delete && selected,
                                        selected,
                                    );
                                    if let Some(selection) = regions.selection {
                                        ui.scope_builder(
                                            egui::UiBuilder::new().max_rect(selection),
                                            |ui| {
                                                ui.label(
                                                    RichText::new("✓ Selected")
                                                        .strong()
                                                        .color(Color32::WHITE)
                                                        .size(11.0),
                                                );
                                            },
                                        );
                                    }
                                    ui.scope_builder(
                                        egui::UiBuilder::new().max_rect(regions.preview),
                                        |ui| {
                                            history_card_preview(
                                                ui,
                                                &item,
                                                preview,
                                                regions.preview.size(),
                                            );
                                        },
                                    );
                                    if let Some(actions) = regions.actions {
                                        ui.scope_builder(
                                            egui::UiBuilder::new().max_rect(actions),
                                            |ui| {
                                                if let Some(requested) = history_card_actions(
                                                    ui,
                                                    &item,
                                                    self.mutation_block_reason(
                                                        PendingScope::HistoryMutation,
                                                    ),
                                                )
                                                .action
                                                {
                                                    action = Some(requested);
                                                }
                                            },
                                        );
                                    }
                                    ui.scope_builder(
                                        egui::UiBuilder::new().max_rect(regions.metadata),
                                        |ui| history_card_metadata(ui, &item),
                                    );
                                });
                            let response = card.response.interact(egui::Sense::click());
                            current_card_focus_ids.insert(response.id);
                            let keyboard_activated = response.has_focus()
                                && ui.input(|input| input.key_pressed(Key::Enter));
                            if response.clicked() || response.has_focus() {
                                self.selected_history = index;
                                self.selected_content_id = Some(item.content_id.clone());
                            }
                            if keyboard_activated {
                                action = Some(HistoryAction::Activate(item.content_id.clone()));
                            }
                            response.clone().on_hover_text(history_card_tooltip(&item));
                            response.widget_info(|| history_card_widget_info(&item, selected));
                            if !allow_delete && response.double_clicked() {
                                action = Some(HistoryAction::Activate(item.content_id.clone()));
                            }
                            if selected && self.scroll_selected_history {
                                response.scroll_to_me(Some(egui::Align::Center));
                            }
                            if has_image && preview.is_none() && ui.is_rect_visible(response.rect) {
                                requested_previews.push(item.content_id);
                            }
                            if (index + 1) % columns == 0 {
                                ui.end_row();
                            }
                        }
                    });
                ui.add_space(18.0);
            });
        self.scroll_selected_history = false;
        self.history_card_focus_ids = current_card_focus_ids;

        for content_id in requested_previews {
            if self
                .image_previews
                .insert(content_id.clone(), ImagePreviewState::Loading)
                .is_none()
            {
                self.send(UiCommand::ImagePreview { content_id });
            }
        }

        if self
            .mutation_block_reason(PendingScope::HistoryMutation)
            .is_some()
        {
            return;
        }
        match action {
            Some(HistoryAction::Activate(content_id)) => {
                self.notice = None;
                self.send(UiCommand::Activate {
                    content_id,
                    kind: if self.presentation.activation_closes() {
                        MutationKind::ActivateQuick
                    } else {
                        MutationKind::Activate
                    },
                });
            }
            Some(HistoryAction::Pin { content_id, pinned }) => {
                self.notice = None;
                self.send(UiCommand::HistoryUpdate {
                    content_id,
                    action: if pinned {
                        HistoryUpdateAction::Pin
                    } else {
                        HistoryUpdateAction::Unpin
                    },
                    kind: MutationKind::Pin,
                });
            }
            Some(HistoryAction::Delete(content_id)) => {
                self.notice = None;
                self.pending_history_delete = Some(content_id);
            }
            None => {}
        }
    }
}
