use std::{
    collections::{BTreeSet, HashSet},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use eframe::egui::{self, Color32, FontId, Key};

use super::widgets::ControlTab;
use crate::{
    ipc::protocol::{HistoryItem, HistoryResponse},
    ui::{
        Presentation,
        style::{
            HISTORY_FOCUSED_POLL, HISTORY_GRID_GAP, HISTORY_MAX_BACKOFF, HISTORY_UNFOCUSED_POLL,
        },
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ui) enum FilterCompletionKind {
    Device,
    Type,
    Pinned,
}

pub(in crate::ui) struct FilterCompletion {
    pub(in crate::ui) value_start: usize,
    pub(in crate::ui) kind: FilterCompletionKind,
    pub(in crate::ui) prefix: String,
}

pub(in crate::ui) struct FilterSuggestion {
    pub(in crate::ui) value: String,
    pub(in crate::ui) label: String,
    pub(in crate::ui) detail: &'static str,
}

pub(in crate::ui) fn filter_completion_context(search: &str) -> Option<FilterCompletion> {
    let mut token_start = 0;
    let mut quote = None;
    for (offset, character) in search.char_indices() {
        match character {
            '"' | '\'' if quote.is_none() => quote = Some(character),
            character if quote == Some(character) => quote = None,
            ',' if quote.is_none() => token_start = offset + character.len_utf8(),
            character if quote.is_none() && character.is_whitespace() => {
                token_start = offset + character.len_utf8();
            }
            _ => {}
        }
    }
    if quote.is_some() {
        return None;
    }
    let token = search.get(token_start..)?;
    let (name, value) = token.split_once(':')?;
    let kind = match name.to_ascii_lowercase().as_str() {
        "d" | "device" => FilterCompletionKind::Device,
        "t" | "type" => FilterCompletionKind::Type,
        "p" | "pinned" => FilterCompletionKind::Pinned,
        _ => return None,
    };
    Some(FilterCompletion {
        value_start: token_start + name.len() + 1,
        kind,
        prefix: value.to_ascii_lowercase(),
    })
}

pub(in crate::ui) fn should_defer_history_refresh(search: &str) -> bool {
    search
        .split(|character: char| character.is_whitespace() || character == ',')
        .filter_map(|token| token.split_once(':'))
        .any(|(name, value)| match name.to_ascii_lowercase().as_str() {
            "d" | "device" | "t" | "type" => value.is_empty(),
            "p" | "pinned" => !matches!(value.to_ascii_lowercase().as_str(), "true" | "false"),
            _ => false,
        })
}

pub(in crate::ui) fn filter_suggestions(
    completion: &FilterCompletion,
    known_devices: &BTreeSet<String>,
    known_types: &BTreeSet<String>,
) -> Vec<FilterSuggestion> {
    match completion.kind {
        FilterCompletionKind::Device => known_devices
            .iter()
            .filter(|device| device.to_ascii_lowercase().starts_with(&completion.prefix))
            .take(6)
            .map(|device| FilterSuggestion {
                value: device.clone(),
                label: device.clone(),
                detail: "device",
            })
            .collect(),
        FilterCompletionKind::Type => {
            let mut candidates = ["image", "text", "files"]
                .into_iter()
                .filter(|kind| kind.starts_with(&completion.prefix))
                .map(str::to_owned)
                .collect::<Vec<_>>();
            candidates.extend(
                known_types
                    .iter()
                    .filter(|kind| {
                        kind.starts_with(&completion.prefix)
                            && !matches!(kind.as_str(), "image" | "text" | "files")
                    })
                    .take(6_usize.saturating_sub(candidates.len()))
                    .cloned(),
            );
            candidates
                .into_iter()
                .map(|kind| FilterSuggestion {
                    detail: if matches!(kind.as_str(), "image" | "text" | "files") {
                        "type group"
                    } else {
                        "exact MIME"
                    },
                    value: kind.clone(),
                    label: kind,
                })
                .collect()
        }
        FilterCompletionKind::Pinned => [
            FilterSuggestion {
                value: "true".to_owned(),
                label: "pinned".to_owned(),
                detail: "p:true",
            },
            FilterSuggestion {
                value: "false".to_owned(),
                label: "unpinned".to_owned(),
                detail: "p:false",
            },
        ]
        .into_iter()
        .filter(|suggestion| suggestion.value.starts_with(&completion.prefix))
        .collect(),
    }
}
pub(in crate::ui) fn history_source_label(item: &HistoryItem) -> String {
    if item.source_device.is_empty() {
        short_identifier(&item.source_node)
    } else {
        item.source_device.clone()
    }
}

pub(in crate::ui) fn history_column_count(available_width: f32) -> usize {
    if available_width >= 640.0 { 3 } else { 2 }
}

pub(in crate::ui) fn history_filter_help() -> &'static str {
    "Filters: d:/device:, t:/type:, p:/pinned:, before:, min-size:, max-size:. Chain filters with commas or spaces; quote phrases."
}

pub(in crate::ui) const fn history_poll_cadence(focused: bool) -> Duration {
    if focused {
        HISTORY_FOCUSED_POLL
    } else {
        HISTORY_UNFOCUSED_POLL
    }
}

pub(in crate::ui) fn history_poll_allowed(tab: ControlTab, minimized: bool, search: &str) -> bool {
    tab == ControlTab::History && !minimized && !should_defer_history_refresh(search)
}

pub(in crate::ui) const fn should_dispatch_coalesced_history(
    dispatch_coalesced: bool,
    history_poll_eligible: bool,
) -> bool {
    dispatch_coalesced && history_poll_eligible
}

pub(in crate::ui) fn pending_history_refresh_due(
    deadline: Instant,
    history_poll_eligible: bool,
    now: Instant,
) -> bool {
    history_poll_eligible && now >= deadline
}

pub(in crate::ui) fn history_refresh_on_focus_regain(
    was_focused: bool,
    focused: bool,
    tab: ControlTab,
    minimized: bool,
    search: &str,
) -> bool {
    !was_focused && focused && history_poll_allowed(tab, minimized, search)
}

pub(in crate::ui) fn history_refresh_delay(
    cadence: Duration,
    consecutive_failures: u8,
) -> Duration {
    let exponent = u32::from(consecutive_failures.saturating_sub(1).min(5));
    cadence
        .saturating_mul(2_u32.saturating_pow(exponent))
        .min(HISTORY_MAX_BACKOFF)
}

pub(in crate::ui) fn replace_history_snapshot(
    current: &mut Vec<HistoryItem>,
    result: Result<HistoryResponse, String>,
) -> Result<(), String> {
    match result {
        Ok(response) => {
            *current = response.items;
            Ok(())
        }
        Err(error) => Err(error),
    }
}

pub(in crate::ui) fn preserve_history_selection(
    history: &[HistoryItem],
    selected_content_id: Option<&str>,
    previous_index: usize,
) -> (usize, Option<String>) {
    if history.is_empty() {
        return (0, None);
    }
    let index = selected_content_id
        .and_then(|selected| history.iter().position(|item| item.content_id == selected))
        .unwrap_or_else(|| previous_index.min(history.len() - 1));
    (index, Some(history[index].content_id.clone()))
}
pub(in crate::ui) fn history_shortcuts_allowed(
    ui: &egui::Ui,
    search_id: egui::Id,
    card_ids: &HashSet<egui::Id>,
) -> bool {
    ui.memory(|memory| {
        memory
            .focused()
            .is_none_or(|focused| focused == search_id || card_ids.contains(&focused))
    })
}

pub(in crate::ui) fn history_visible_grid_rows(available_height: f32, card_height: f32) -> usize {
    let mut rows = 1;
    let mut occupied = card_height;
    while rows < 1_024 && occupied + HISTORY_GRID_GAP + card_height <= available_height {
        rows += 1;
        occupied += HISTORY_GRID_GAP + card_height;
    }
    rows
}
pub(in crate::ui) fn history_card_tooltip(item: &HistoryItem) -> String {
    format!(
        "MIME: {}\nSize: {} bytes\nPinned: {}\nSource: {}\nTime: {}",
        if item.mime_types.is_empty() {
            "unknown".to_owned()
        } else {
            item.mime_types.join(", ")
        },
        item.logical_size,
        if item.pinned { "yes" } else { "no" },
        history_source_label(item),
        item.physical_millis,
    )
}

pub(in crate::ui) fn history_card_accessible_label(item: &HistoryItem) -> String {
    format!(
        "Clipboard item: {}; {}",
        if item.preview.trim().is_empty() {
            "binary content"
        } else {
            item.preview.trim()
        },
        history_card_tooltip(item).replace('\n', "; ")
    )
}

pub(in crate::ui) fn relative_history_time(physical_millis: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(physical_millis, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        });
    let seconds = now.saturating_sub(physical_millis) / 1_000;
    match seconds {
        0..=59 => format!("{seconds}s ago"),
        60..=3_599 => format!("{}m ago", seconds / 60),
        3_600..=86_399 => format!("{}h ago", seconds / 3_600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

pub(in crate::ui) fn history_item_has_image(item: &HistoryItem) -> bool {
    item.mime_types.iter().any(|mime| {
        matches!(
            mime.split(';')
                .next()
                .unwrap_or(mime)
                .trim()
                .to_ascii_lowercase()
                .as_str(),
            "image/png"
                | "image/jpeg"
                | "image/jpg"
                | "image/gif"
                | "image/webp"
                | "image/bmp"
                | "image/x-ms-bmp"
                | "image/tiff"
        )
    })
}
pub(in crate::ui) fn history_text_layout(text: &str, width: f32) -> egui::text::LayoutJob {
    let mut layout = egui::text::LayoutJob::simple(
        text.to_owned(),
        FontId::proportional(12.0),
        Color32::WHITE,
        width,
    );
    layout.wrap.max_rows = 4;
    layout
}

pub(in crate::ui) fn short_identifier(identifier: &str) -> String {
    const VISIBLE_CHARS: usize = 12;
    let mut short = identifier.chars().take(VISIBLE_CHARS).collect::<String>();
    if identifier.chars().count() > VISIBLE_CHARS {
        short.push('…');
    }
    short
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::ui) enum SwitcherKey {
    None,
    Left,
    Right,
    Up,
    Down,
    PageUp,
    PageDown,
    Enter,
    Escape,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::ui) enum SwitcherIntent {
    None,
    Moved,
    Activate,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::ui) enum AutocompleteIntent {
    None,
    Consumed,
    Accept(usize),
    Dismiss,
}

pub(in crate::ui) fn apply_autocomplete_key(
    key: SwitcherKey,
    selection: &mut usize,
    suggestion_count: usize,
) -> AutocompleteIntent {
    if suggestion_count == 0 {
        return AutocompleteIntent::None;
    }
    *selection = (*selection).min(suggestion_count - 1);
    match key {
        SwitcherKey::Up => {
            *selection = selection.saturating_sub(1);
            AutocompleteIntent::Consumed
        }
        SwitcherKey::Down => {
            *selection = selection.saturating_add(1).min(suggestion_count - 1);
            AutocompleteIntent::Consumed
        }
        SwitcherKey::Enter => AutocompleteIntent::Accept(*selection),
        SwitcherKey::Escape => AutocompleteIntent::Dismiss,
        SwitcherKey::None
        | SwitcherKey::Left
        | SwitcherKey::Right
        | SwitcherKey::PageUp
        | SwitcherKey::PageDown => AutocompleteIntent::None,
    }
}

pub(in crate::ui) fn switcher_key(input: &egui::InputState) -> SwitcherKey {
    if input.key_pressed(Key::Escape) {
        SwitcherKey::Escape
    } else if input.key_pressed(Key::ArrowLeft) {
        SwitcherKey::Left
    } else if input.key_pressed(Key::ArrowRight) {
        SwitcherKey::Right
    } else if input.key_pressed(Key::ArrowDown) {
        SwitcherKey::Down
    } else if input.key_pressed(Key::ArrowUp) {
        SwitcherKey::Up
    } else if input.key_pressed(Key::PageUp) {
        SwitcherKey::PageUp
    } else if input.key_pressed(Key::PageDown) {
        SwitcherKey::PageDown
    } else if input.key_pressed(Key::Enter) {
        SwitcherKey::Enter
    } else {
        SwitcherKey::None
    }
}

pub(in crate::ui) const fn presentation_switcher_key(
    presentation: Presentation,
    key: SwitcherKey,
) -> SwitcherKey {
    if matches!(
        (presentation, key),
        (Presentation::Management, SwitcherKey::Escape)
    ) {
        SwitcherKey::None
    } else {
        key
    }
}

pub(in crate::ui) fn apply_switcher_key(
    key: SwitcherKey,
    selection: &mut usize,
    item_count: usize,
    columns: usize,
    page_rows: usize,
    activation_blocked: bool,
) -> SwitcherIntent {
    match key {
        SwitcherKey::Escape => SwitcherIntent::Close,
        SwitcherKey::Left
        | SwitcherKey::Right
        | SwitcherKey::Up
        | SwitcherKey::Down
        | SwitcherKey::PageUp
        | SwitcherKey::PageDown => {
            *selection = move_grid_selection(*selection, item_count, columns, page_rows, key);
            SwitcherIntent::Moved
        }
        SwitcherKey::Enter if item_count > 0 && !activation_blocked => {
            *selection = (*selection).min(item_count - 1);
            SwitcherIntent::Activate
        }
        SwitcherKey::None | SwitcherKey::Enter => SwitcherIntent::None,
    }
}

pub(in crate::ui) fn move_grid_selection(
    current: usize,
    item_count: usize,
    columns: usize,
    page_rows: usize,
    key: SwitcherKey,
) -> usize {
    if item_count == 0 || columns == 0 {
        return 0;
    }
    let current = current.min(item_count - 1);
    let current_row = current / columns;
    let current_column = current % columns;
    let last_row = (item_count - 1) / columns;
    match key {
        SwitcherKey::Left if !current.is_multiple_of(columns) => current - 1,
        SwitcherKey::Right
            if !(current + 1).is_multiple_of(columns) && current + 1 < item_count =>
        {
            current + 1
        }
        SwitcherKey::Up if current >= columns => current - columns,
        SwitcherKey::Down if current + columns < item_count => current + columns,
        SwitcherKey::PageUp => {
            let target_row = current_row.saturating_sub(page_rows.max(1));
            (target_row * columns + current_column).min(item_count - 1)
        }
        SwitcherKey::PageDown => {
            let target_row = current_row.saturating_add(page_rows.max(1)).min(last_row);
            (target_row * columns + current_column).min(item_count - 1)
        }
        SwitcherKey::None
        | SwitcherKey::Left
        | SwitcherKey::Right
        | SwitcherKey::Up
        | SwitcherKey::Down
        | SwitcherKey::Enter
        | SwitcherKey::Escape => current,
    }
}
