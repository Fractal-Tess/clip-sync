use super::*;

#[test]
fn enter_is_routed_to_focused_share_activate_pin_and_delete_controls() {
    for (target, expected_action) in [
        (FocusedHistoryControl::Share, None),
        (
            FocusedHistoryControl::Activate,
            Some(HistoryAction::Activate("content".to_owned())),
        ),
        (
            FocusedHistoryControl::Pin,
            Some(HistoryAction::Pin {
                content_id: "content".to_owned(),
                pinned: true,
            }),
        ),
        (
            FocusedHistoryControl::Delete,
            Some(HistoryAction::Delete("content".to_owned())),
        ),
    ] {
        let context = egui::Context::default();
        configure_style(&context);
        let mut search_text = String::new();
        let item = history_item("content");
        let _ = context.run_ui(egui_input(Vec2::new(720.0, 480.0), None), |ui| {
            let row = history_search_row(ui, &mut search_text, true, None);
            let actions = history_card_actions(ui, &item, None);
            let target_id = match target {
                FocusedHistoryControl::Share => {
                    row.share_id.expect("management row has a share button")
                }
                FocusedHistoryControl::Activate => actions.control_ids[0],
                FocusedHistoryControl::Pin => actions.control_ids[1],
                FocusedHistoryControl::Delete => actions.control_ids[2],
            };
            ui.memory_mut(|memory| memory.request_focus(target_id));
        });

        let mut shortcut_allowed = true;
        let mut share_clicked = false;
        let mut action = None;
        let _ = context.run_ui(
            egui_input(Vec2::new(720.0, 480.0), Some(Key::Enter)),
            |ui| {
                let row = history_search_row(ui, &mut search_text, true, None);
                shortcut_allowed = history_shortcuts_allowed(ui, row.search.id, &HashSet::new());
                share_clicked = row.share_clicked;
                action = history_card_actions(ui, &item, None).action;
            },
        );

        assert!(!shortcut_allowed, "focused controls must own Enter");
        assert_eq!(
            share_clicked,
            matches!(target, FocusedHistoryControl::Share)
        );
        assert_eq!(action, expected_action);
    }
}

#[test]
fn pending_mutation_visibly_disables_share_control() {
    let context = egui::Context::default();
    let mut search_text = String::new();
    let mut share_enabled = None;
    let _ = context.run_ui(egui_input(Vec2::new(720.0, 480.0), None), |ui| {
        let row = history_search_row(
            ui,
            &mut search_text,
            true,
            Some("Wait for the pending change."),
        );
        share_enabled = row.share_enabled;
    });

    assert_eq!(share_enabled, Some(false));
}

#[test]
fn focused_controls_block_grid_arrow_navigation() {
    let context = egui::Context::default();
    configure_style(&context);
    let mut search_text = String::new();
    let _ = context.run_ui(egui_input(Vec2::new(480.0, 300.0), None), |ui| {
        let row = history_search_row(ui, &mut search_text, true, None);
        ui.memory_mut(|memory| {
            memory.request_focus(row.share_id.expect("management share button"));
        });
    });

    let mut selection = 0;
    let mut shortcut_allowed = true;
    let _ = context.run_ui(
        egui_input(Vec2::new(480.0, 300.0), Some(Key::ArrowDown)),
        |ui| {
            let key = ui.input(switcher_key);
            let row = history_search_row(ui, &mut search_text, true, None);
            shortcut_allowed = history_shortcuts_allowed(ui, row.search.id, &HashSet::new());
            if shortcut_allowed {
                let _ = apply_switcher_key(key, &mut selection, 6, 3, 2, false);
            }
        },
    );
    assert!(!shortcut_allowed);
    assert_eq!(selection, 0);
}

#[test]
fn focused_search_owns_autocomplete_navigation_and_dismissal() {
    let context = egui::Context::default();
    configure_style(&context);
    let mut search_text = "p:".to_owned();
    let _ = context.run_ui(egui_input(Vec2::new(480.0, 300.0), None), |ui| {
        let row = history_search_row(ui, &mut search_text, true, None);
        row.search.request_focus();
    });

    let mut selection = 0;
    let mut allowed = false;
    let mut intent = AutocompleteIntent::None;
    let _ = context.run_ui(
        egui_input(Vec2::new(480.0, 300.0), Some(Key::ArrowDown)),
        |ui| {
            let key = ui.input(switcher_key);
            let row = history_search_row(ui, &mut search_text, true, None);
            allowed = history_shortcuts_allowed(ui, row.search.id, &HashSet::new());
            if allowed {
                intent = apply_autocomplete_key(key, &mut selection, 2);
            }
        },
    );
    assert!(allowed);
    assert_eq!(intent, AutocompleteIntent::Consumed);
    assert_eq!(selection, 1);

    let mut dismiss = AutocompleteIntent::None;
    let _ = context.run_ui(
        egui_input(Vec2::new(480.0, 300.0), Some(Key::Escape)),
        |ui| {
            let key = ui.input(switcher_key);
            let row = history_search_row(ui, &mut search_text, true, None);
            if history_shortcuts_allowed(ui, row.search.id, &HashSet::new()) {
                dismiss = apply_autocomplete_key(key, &mut selection, 2);
            }
        },
    );
    assert_eq!(dismiss, AutocompleteIntent::Dismiss);
}

#[test]
fn shell_controls_stay_inside_supported_viewports() {
    for size in [
        Vec2::new(480.0, 300.0),
        Vec2::new(720.0, 480.0),
        Vec2::new(1_040.0, 700.0),
    ] {
        let context = egui::Context::default();
        configure_style(&context);
        let viewport = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
        let mut search_text = String::new();
        let mut control_rects = Vec::new();
        let mut action_layout = None;
        let _ = context.run_ui(egui_input(size, None), |ui| {
            Frame::new().inner_margin(Margin::same(16)).show(ui, |ui| {
                let navigation = navigation_bar(ui, ControlTab::History);
                control_rects.extend(navigation.control_rects);
                let search = history_search_row(ui, &mut search_text, true, None);
                control_rects.extend(search.control_rects);

                let columns = history_column_count(ui.available_width());
                let columns_f32 =
                    f32::from(u16::try_from(columns).expect("history columns fit in u16"));
                let card_width =
                    ((ui.available_width() - HISTORY_GRID_GAP * (columns_f32 - 1.0) - 14.0)
                        / columns_f32)
                        .floor()
                        .max(180.0);
                let bounds = egui::Rect::from_min_size(
                    ui.cursor().min,
                    Vec2::new(card_width - 16.0, HISTORY_ACTIONS_HEIGHT),
                );
                let actions = ui
                    .scope_builder(egui::UiBuilder::new().max_rect(bounds), |ui| {
                        history_card_actions(ui, &history_item("layout"), None)
                    })
                    .inner;
                control_rects.extend(actions.control_rects);
                action_layout = Some((bounds, actions.control_rects));
            });
        });
        let (action_bounds, action_rects) =
            action_layout.expect("management action controls were laid out");
        for rect in action_rects {
            assert!(
                action_bounds.contains_rect(rect),
                "card action {rect:?} escaped {action_bounds:?} at {size:?}"
            );
        }
        assert!(!control_rects.is_empty());
        for rect in control_rects {
            assert!(rect.is_finite());
            assert!(
                viewport.contains_rect(rect),
                "control {rect:?} escaped viewport {viewport:?} at {size:?}"
            );
        }
    }
}

#[test]
fn card_metadata_footer_is_fixed_and_only_selected_management_has_actions() {
    let rect = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), Vec2::new(220.0, 140.0));
    for management in [false, true] {
        for selected in [false, true] {
            let regions = history_card_regions(rect, management && selected, selected);
            assert_approx_eq(regions.metadata.bottom(), rect.bottom());
            assert_approx_eq(regions.metadata.height(), HISTORY_METADATA_HEIGHT);
            assert_approx_eq(regions.metadata.width(), rect.width());
            assert_eq!(regions.actions.is_some(), management && selected);
            assert_eq!(regions.selection.is_some(), selected);
            assert_approx_eq(
                regions.preview.top(),
                regions
                    .selection
                    .map_or(rect.top(), |selection| selection.bottom()),
            );
            assert_approx_eq(
                regions.preview.bottom(),
                regions
                    .actions
                    .map_or(regions.metadata.top(), |actions| actions.top()),
            );
        }
    }
}

#[test]
fn card_accessibility_and_tooltip_include_all_footer_metadata() {
    let mut item = history_item("content");
    item.pinned = true;
    item.logical_size = 99;
    item.mime_types = vec!["text/plain".to_owned(), "text/html".to_owned()];
    let tooltip = history_card_tooltip(&item);
    let accessible = history_card_accessible_label(&item);
    for expected in [
        "text/plain",
        "text/html",
        "99 bytes",
        "Pinned: yes",
        "Source: vd",
        "Time:",
    ] {
        assert!(tooltip.contains(expected));
        assert!(accessible.contains(expected));
    }
    assert_eq!(history_card_widget_info(&item, true).selected, Some(true));
    assert_eq!(history_card_widget_info(&item, false).selected, Some(false));
}
