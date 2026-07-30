use eframe::egui::{self, FontId, Frame, Margin, RichText, Stroke, Vec2};

use super::model::{
    FilterCompletion, FilterCompletionKind, FilterSuggestion, history_card_accessible_label,
    history_card_tooltip, history_filter_help, history_item_has_image, history_source_label,
    history_text_layout, relative_history_time,
};
use crate::{
    ipc::protocol::HistoryItem,
    ui::{
        Presentation,
        ipc_types::{HistoryAction, ImagePreviewState},
        style::{
            BORDER, HISTORY_ACTIONS_HEIGHT, HISTORY_METADATA_HEIGHT, HISTORY_SELECTION_HEIGHT,
            MUTED, NARROW_NAVIGATION_THRESHOLD, SURFACE, format_bytes,
        },
    },
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::ui) enum ControlTab {
    History,
    Transfers,
    Peers,
    Settings,
    Diagnostics,
}

pub(in crate::ui) const fn presentation_after_navigation(
    current: Presentation,
    destination: ControlTab,
) -> Presentation {
    if matches!(destination, ControlTab::History) {
        current
    } else {
        Presentation::Management
    }
}

impl ControlTab {
    const ALL: [Self; 5] = [
        Self::History,
        Self::Transfers,
        Self::Peers,
        Self::Settings,
        Self::Diagnostics,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::History => "History",
            Self::Transfers => "Transfers",
            Self::Peers => "Peers",
            Self::Settings => "Settings",
            Self::Diagnostics => "Diagnostics",
        }
    }
}

pub(in crate::ui) struct NavigationBarResponse {
    pub(in crate::ui) destination: Option<ControlTab>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub(in crate::ui) control_rects: Vec<egui::Rect>,
}

pub(in crate::ui) fn navigation_bar(
    ui: &mut egui::Ui,
    selected: ControlTab,
) -> NavigationBarResponse {
    let compact = ui.available_width() < NARROW_NAVIGATION_THRESHOLD;
    let mut destination = None;
    let mut control_rects = Vec::new();
    ui.horizontal(|ui| {
        if compact {
            let menu = ui.menu_button(format!("{} ▾", selected.label()), |ui| {
                for tab in ControlTab::ALL {
                    if ui.selectable_label(selected == tab, tab.label()).clicked() {
                        destination = Some(tab);
                        ui.close();
                    }
                }
            });
            control_rects.push(menu.response.rect);
        } else {
            ui.scope(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.spacing_mut().button_padding = Vec2::new(8.0, 6.0);
                for tab in ControlTab::ALL {
                    let response = ui.selectable_label(selected == tab, tab.label());
                    control_rects.push(response.rect);
                    if response.clicked() {
                        destination = Some(tab);
                    }
                }
            });
        }
    });
    NavigationBarResponse {
        destination,
        control_rects,
    }
}
pub(in crate::ui) fn autocomplete_popup(
    ui: &egui::Ui,
    anchor: egui::Rect,
    completion: &FilterCompletion,
    suggestions: &[FilterSuggestion],
    selected: usize,
) -> Option<usize> {
    let mut clicked = None;
    egui::Area::new(egui::Id::new("history-filter-autocomplete"))
        .order(egui::Order::Foreground)
        .fixed_pos(anchor.left_bottom() + Vec2::new(0.0, 4.0))
        .show(ui.ctx(), |ui| {
            Frame::popup(ui.style())
                .fill(SURFACE)
                .stroke(Stroke::new(1.0, BORDER))
                .inner_margin(Margin::same(8))
                .show(ui, |ui| {
                    ui.set_width(anchor.width());
                    let heading = match completion.kind {
                        FilterCompletionKind::Device => "DEVICES",
                        FilterCompletionKind::Type => "TYPES",
                        FilterCompletionKind::Pinned => "PIN STATE",
                    };
                    ui.label(RichText::new(heading).monospace().color(MUTED).size(10.0));
                    for (index, suggestion) in suggestions.iter().enumerate() {
                        let text = format!("{}    {}", suggestion.label, suggestion.detail);
                        if ui.selectable_label(index == selected, text).clicked() {
                            clicked = Some(index);
                        }
                    }
                    ui.label(
                        RichText::new("↑↓ choose · Tab/Enter complete")
                            .monospace()
                            .color(MUTED)
                            .size(9.0),
                    );
                });
        });
    clicked
}

pub(in crate::ui) fn apply_filter_suggestion(
    search: &mut String,
    completion: &FilterCompletion,
    value: &str,
) {
    search.replace_range(completion.value_start.., value);
}

pub(in crate::ui) struct HistorySearchRowResponse {
    pub(in crate::ui) search: egui::Response,
    pub(in crate::ui) share_clicked: bool,
    pub(in crate::ui) help_clicked: bool,
    #[cfg_attr(not(test), allow(dead_code))]
    pub(in crate::ui) share_id: Option<egui::Id>,
    #[cfg(test)]
    pub(in crate::ui) share_enabled: Option<bool>,
    _help_id: egui::Id,
    #[cfg_attr(not(test), allow(dead_code))]
    pub(in crate::ui) control_rects: Vec<egui::Rect>,
}

pub(in crate::ui) fn history_search_row(
    ui: &mut egui::Ui,
    search_text: &mut String,
    management: bool,
    share_disabled_reason: Option<&str>,
) -> HistorySearchRowResponse {
    let available_width = ui.available_width();
    let spacing = ui.spacing().item_spacing.x;
    let help_width = 84.0;
    let share_width = 132.0;
    let reserved_width = help_width
        + spacing
        + if management {
            share_width + spacing
        } else {
            0.0
        };
    let search_width = (available_width - reserved_width).max(120.0);
    let mut search_response = None;
    let mut share_response = None;
    let mut help_response = None;
    ui.horizontal(|ui| {
        search_response = Some(
            ui.add_sized(
                [search_width, 30.0],
                egui::TextEdit::singleline(search_text)
                    .hint_text("Search · d:device, t:type, p:true")
                    .font(FontId::proportional(15.0)),
            ),
        );
        if management {
            share_response = Some(
                ui.add_enabled_ui(share_disabled_reason.is_none(), |ui| {
                    ui.add_sized([share_width, 30.0], egui::Button::new("Share clipboard"))
                })
                .inner
                .on_disabled_hover_text(share_disabled_reason.unwrap_or_default()),
            );
        }
        help_response = Some(
            ui.add_sized([help_width, 30.0], egui::Button::new("Filter help"))
                .on_hover_text(history_filter_help()),
        );
    });
    let search = search_response.expect("History search row always creates its input");
    let share = share_response;
    let help = help_response.expect("History search row always creates filter help");
    let mut control_rects = vec![search.rect, help.rect];
    if let Some(share) = &share {
        control_rects.push(share.rect);
    }
    HistorySearchRowResponse {
        search,
        share_clicked: share.as_ref().is_some_and(egui::Response::clicked),
        help_clicked: help.clicked(),
        share_id: share.as_ref().map(|response| response.id),
        #[cfg(test)]
        share_enabled: share.as_ref().map(egui::Response::enabled),
        _help_id: help.id,
        control_rects,
    }
}
pub(in crate::ui) struct HistoryCardActionsResponse {
    pub(in crate::ui) action: Option<HistoryAction>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub(in crate::ui) control_ids: [egui::Id; 3],
    #[cfg_attr(not(test), allow(dead_code))]
    pub(in crate::ui) control_rects: [egui::Rect; 3],
}

pub(in crate::ui) fn history_card_actions(
    ui: &mut egui::Ui,
    item: &HistoryItem,
    disabled_reason: Option<&str>,
) -> HistoryCardActionsResponse {
    let mut action = None;
    ui.spacing_mut().item_spacing.x = 4.0;
    ui.spacing_mut().button_padding.x = 6.0;
    let responses = ui.horizontal(|ui| {
        let activate = ui
            .add_enabled(
                disabled_reason.is_none(),
                egui::Button::new("Activate").small(),
            )
            .on_disabled_hover_text(disabled_reason.unwrap_or_default());
        if activate.clicked() {
            action = Some(HistoryAction::Activate(item.content_id.clone()));
        }
        let pin = ui
            .add_enabled(
                disabled_reason.is_none(),
                egui::Button::new(if item.pinned { "Unpin" } else { "Pin" }).small(),
            )
            .on_disabled_hover_text(disabled_reason.unwrap_or_default());
        if pin.clicked() {
            action = Some(HistoryAction::Pin {
                content_id: item.content_id.clone(),
                pinned: !item.pinned,
            });
        }
        let delete = ui
            .add_enabled(
                disabled_reason.is_none(),
                egui::Button::new("Delete").small(),
            )
            .on_disabled_hover_text(disabled_reason.unwrap_or_default());
        if delete.clicked() {
            action = Some(HistoryAction::Delete(item.content_id.clone()));
        }
        (
            [activate.id, pin.id, delete.id],
            [activate.rect, pin.rect, delete.rect],
        )
    });
    HistoryCardActionsResponse {
        action,
        control_ids: responses.inner.0,
        control_rects: responses.inner.1,
    }
}

pub(in crate::ui) fn history_card_widget_info(
    item: &HistoryItem,
    selected: bool,
) -> egui::WidgetInfo {
    egui::WidgetInfo::selected(
        egui::WidgetType::Button,
        true,
        selected,
        history_card_accessible_label(item),
    )
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(in crate::ui) struct HistoryCardRegions {
    pub(in crate::ui) selection: Option<egui::Rect>,
    pub(in crate::ui) preview: egui::Rect,
    pub(in crate::ui) actions: Option<egui::Rect>,
    pub(in crate::ui) metadata: egui::Rect,
}

pub(in crate::ui) fn history_card_regions(
    rect: egui::Rect,
    actions_visible: bool,
    selected: bool,
) -> HistoryCardRegions {
    let metadata_top = rect.bottom() - HISTORY_METADATA_HEIGHT;
    let metadata =
        egui::Rect::from_min_max(egui::pos2(rect.left(), metadata_top), rect.right_bottom());
    let actions = actions_visible.then(|| {
        egui::Rect::from_min_max(
            egui::pos2(rect.left(), metadata_top - HISTORY_ACTIONS_HEIGHT),
            egui::pos2(rect.right(), metadata_top),
        )
    });
    let selection = selected.then(|| {
        egui::Rect::from_min_max(
            rect.left_top(),
            egui::pos2(rect.right(), rect.top() + HISTORY_SELECTION_HEIGHT),
        )
    });
    let preview_top = selection.map_or(rect.top(), |selection| selection.bottom());
    let preview_bottom = actions.map_or(metadata_top, |actions| actions.top());
    let preview = egui::Rect::from_min_max(
        egui::pos2(rect.left(), preview_top),
        egui::pos2(rect.right(), preview_bottom),
    );
    HistoryCardRegions {
        selection,
        preview,
        actions,
        metadata,
    }
}

pub(in crate::ui) fn history_card_metadata(ui: &mut egui::Ui, item: &HistoryItem) {
    let mime = item.mime_types.first().map_or("unknown", String::as_str);
    ui.spacing_mut().item_spacing.y = 1.0;
    ui.add(
        egui::Label::new(
            RichText::new(format!(
                "{mime} · {}{}",
                format_bytes(item.logical_size),
                if item.pinned { " · PIN" } else { "" }
            ))
            .color(MUTED)
            .monospace()
            .size(11.0),
        )
        .truncate(),
    )
    .on_hover_text(history_card_tooltip(item));
    ui.add(
        egui::Label::new(
            RichText::new(format!(
                "{} · {}",
                history_source_label(item),
                relative_history_time(item.physical_millis)
            ))
            .color(MUTED)
            .monospace()
            .size(11.0),
        )
        .truncate(),
    )
    .on_hover_text(history_card_tooltip(item));
}
pub(in crate::ui) fn history_card_preview(
    ui: &mut egui::Ui,
    item: &HistoryItem,
    preview: Option<&ImagePreviewState>,
    size: Vec2,
) {
    if history_item_has_image(item) {
        ui.allocate_ui_with_layout(
            size,
            egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
            |ui| match preview {
                Some(ImagePreviewState::Ready(texture)) => {
                    ui.add(egui::Image::new(texture).max_size(size));
                }
                Some(ImagePreviewState::Loading) => {
                    ui.spinner();
                }
                Some(ImagePreviewState::Unavailable) => {
                    ui.label(RichText::new("Preview unavailable").color(MUTED).size(12.0));
                }
                None => {
                    ui.label(RichText::new("Loading preview…").color(MUTED).size(12.0));
                }
            },
        );
    } else {
        let title = if item.preview.trim().is_empty() {
            "Binary clipboard content"
        } else {
            item.preview.trim()
        };
        ui.allocate_ui(size, |ui| {
            let layout = history_text_layout(title, size.x);
            let galley = ui.fonts_mut(|fonts| fonts.layout_job(layout));
            ui.add(egui::Label::new(galley));
        });
    }
}
