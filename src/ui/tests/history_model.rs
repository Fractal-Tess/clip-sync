use super::*;

#[test]
fn history_text_wraps_to_four_rows() {
    let layout = history_text_layout("a long clipboard preview", 180.0);

    assert!((layout.wrap.max_width - 180.0).abs() < f32::EPSILON);
    assert_eq!(layout.wrap.max_rows, 4);
}

#[test]
fn grid_selection_stays_within_rows_and_results() {
    assert_eq!(move_grid_selection(0, 0, 3, 2, SwitcherKey::Down), 0);
    assert_eq!(move_grid_selection(0, 6, 3, 2, SwitcherKey::Left), 0);
    assert_eq!(move_grid_selection(2, 6, 3, 2, SwitcherKey::Right), 2);
    assert_eq!(move_grid_selection(1, 6, 3, 2, SwitcherKey::Down), 4);
    assert_eq!(move_grid_selection(4, 6, 3, 2, SwitcherKey::Up), 1);
    assert_eq!(move_grid_selection(5, 6, 3, 2, SwitcherKey::Down), 5);
}

#[test]
fn page_navigation_preserves_columns_and_bounds_partial_rows() {
    assert_eq!(move_grid_selection(1, 8, 3, 2, SwitcherKey::PageDown), 7);
    assert_eq!(move_grid_selection(2, 8, 3, 2, SwitcherKey::PageDown), 7);
    assert_eq!(move_grid_selection(7, 8, 3, 2, SwitcherKey::PageDown), 7);
    assert_eq!(move_grid_selection(7, 8, 3, 2, SwitcherKey::PageUp), 1);
    assert_eq!(move_grid_selection(1, 8, 3, 2, SwitcherKey::PageUp), 1);
    assert_eq!(
        history_visible_grid_rows(480.0, QUICK_HISTORY_CARD_HEIGHT),
        3
    );
    assert_eq!(
        history_visible_grid_rows(80.0, QUICK_HISTORY_CARD_HEIGHT),
        1
    );
}

#[test]
fn switcher_keys_navigate_activate_and_close() {
    let mut selection = 0;
    assert_eq!(
        apply_switcher_key(SwitcherKey::Down, &mut selection, 6, 3, 2, false),
        SwitcherIntent::Moved
    );
    assert_eq!(selection, 3);
    assert_eq!(
        apply_switcher_key(SwitcherKey::Right, &mut selection, 6, 3, 2, false),
        SwitcherIntent::Moved
    );
    assert_eq!(selection, 4);
    assert_eq!(
        apply_switcher_key(SwitcherKey::Enter, &mut selection, 6, 3, 2, false),
        SwitcherIntent::Activate
    );
    assert_eq!(
        apply_switcher_key(SwitcherKey::Escape, &mut selection, 6, 3, 2, false),
        SwitcherIntent::Close
    );
}

#[test]
fn switcher_enter_is_blocked_for_loading_or_empty_results() {
    let mut selection = 0;
    assert_eq!(
        apply_switcher_key(SwitcherKey::Enter, &mut selection, 1, 3, 2, true),
        SwitcherIntent::None
    );
    assert_eq!(
        apply_switcher_key(SwitcherKey::Enter, &mut selection, 0, 3, 2, false),
        SwitcherIntent::None
    );
}

#[test]
fn byte_sizes_are_readable() {
    assert_eq!(format_bytes(12), "12 B");
    assert_eq!(format_bytes(1536), "1.5 KiB");
    assert_eq!(format_bytes(20 * 1024 * 1024), "20.0 MiB");
}

#[test]
fn abbreviated_filter_completion_handles_comma_chains() {
    let completion = filter_completion_context("d:vd,t:im").expect("type completion");
    assert_eq!(completion.kind, FilterCompletionKind::Type);
    assert_eq!(completion.prefix, "im");
    let mut search = "d:vd,t:im".to_owned();
    apply_filter_suggestion(&mut search, &completion, "image");
    assert_eq!(search, "d:vd,t:image");

    assert!(should_defer_history_refresh("d:"));
    assert!(should_defer_history_refresh("d:vd,p:t"));
    assert!(!should_defer_history_refresh("d:vd,p:true"));
}

#[test]
fn pinned_completion_offers_true_and_false_values() {
    let completion = filter_completion_context("P:").expect("pin completion");
    let suggestions = filter_suggestions(&completion, &BTreeSet::new(), &BTreeSet::new());
    assert_eq!(
        suggestions
            .iter()
            .map(|suggestion| suggestion.value.as_str())
            .collect::<Vec<_>>(),
        ["true", "false"]
    );
}

#[test]
fn history_grid_uses_two_or_three_columns() {
    assert_eq!(history_column_count(600.0), 2);
    assert_eq!(history_column_count(700.0), 3);
    assert_eq!(history_column_count(1_000.0), 3);
}

#[test]
fn history_search_and_inline_filter_help_use_one_row() {
    assert_eq!(HISTORY_SEARCH_ROW_COUNT, 1);
    assert!(!history_filter_help().contains('\n'));
    assert!(history_filter_help().contains("before:"));
}

#[test]
fn content_id_selection_survives_prepend_reorder_and_delete() {
    let initial = vec![history_item("c"), history_item("b"), history_item("a")];
    let (index, selected) = preserve_history_selection(&initial, Some("b"), 1);
    assert_eq!((index, selected.as_deref()), (1, Some("b")));

    let prepended = vec![
        history_item("d"),
        history_item("b"),
        history_item("c"),
        history_item("a"),
    ];
    let (index, selected) = preserve_history_selection(&prepended, selected.as_deref(), index);
    assert_eq!((index, selected.as_deref()), (1, Some("b")));

    let reordered = vec![history_item("a"), history_item("c"), history_item("b")];
    let (index, selected) = preserve_history_selection(&reordered, selected.as_deref(), index);
    assert_eq!((index, selected.as_deref()), (2, Some("b")));

    let deleted = vec![history_item("a"), history_item("c")];
    let (index, selected) = preserve_history_selection(&deleted, selected.as_deref(), index);
    assert_eq!((index, selected.as_deref()), (1, Some("c")));
}

#[test]
fn live_refresh_has_bounded_cadence_coalescing_and_failure_backoff() {
    assert_eq!(history_poll_cadence(true), Duration::from_secs(1));
    assert_eq!(history_poll_cadence(false), Duration::from_secs(5));
    assert!(history_poll_allowed(ControlTab::History, false, ""));
    assert!(!history_poll_allowed(ControlTab::History, true, ""));
    assert!(!history_poll_allowed(ControlTab::Transfers, false, ""));
    assert!(!history_poll_allowed(ControlTab::History, false, "d:"));
    assert!(history_refresh_on_focus_regain(
        false,
        true,
        ControlTab::History,
        false,
        ""
    ));
    assert!(!history_refresh_on_focus_regain(
        true,
        true,
        ControlTab::History,
        false,
        ""
    ));
    assert!(!history_refresh_on_focus_regain(
        false,
        true,
        ControlTab::Transfers,
        false,
        ""
    ));

    let now = Instant::now();
    let mut focus_refresh = HistoryRefreshState::new(now);
    focus_refresh.next_due = now + HISTORY_UNFOCUSED_POLL;
    assert!(
        focus_refresh.request(),
        "focus regain dispatches immediately"
    );
    assert!(!focus_refresh.request(), "an in-flight regain is coalesced");
    assert!(focus_refresh.coalesced);

    let mut refresh = HistoryRefreshState::new(now);
    assert!(refresh.request());
    assert!(!refresh.request());
    assert!(refresh.coalesced);
    assert!(refresh.finish(now, true, HISTORY_FOCUSED_POLL));
    assert_eq!(refresh.consecutive_failures, 0);
    assert_eq!(refresh.next_due, now + HISTORY_FOCUSED_POLL);
    assert!(should_dispatch_coalesced_history(true, true));
    assert!(
        !should_dispatch_coalesced_history(true, false),
        "route changes and minimization suppress queued background polls"
    );
    let overdue = now.checked_sub(Duration::from_millis(1)).unwrap_or(now);
    assert!(pending_history_refresh_due(overdue, true, now));
    assert!(
        !pending_history_refresh_due(overdue, false, now),
        "debounced searches wait until History is visible and restored"
    );

    assert!(refresh.request());
    assert!(!refresh.request());
    assert!(!refresh.finish(now, false, HISTORY_FOCUSED_POLL));
    assert!(!refresh.coalesced);
    assert_eq!(refresh.consecutive_failures, 1);
    assert_eq!(refresh.next_due, now + HISTORY_FOCUSED_POLL);
    assert_eq!(
        history_refresh_delay(HISTORY_UNFOCUSED_POLL, 3),
        Duration::from_secs(20)
    );
    assert_eq!(
        history_refresh_delay(HISTORY_UNFOCUSED_POLL, u8::MAX),
        HISTORY_MAX_BACKOFF
    );

    let mut stale = vec![history_item("stale")];
    assert!(replace_history_snapshot(&mut stale, Err("offline".to_owned())).is_err());
    assert_eq!(stale[0].content_id, "stale");
    replace_history_snapshot(
        &mut stale,
        Ok(HistoryResponse {
            items: vec![history_item("fresh")],
        }),
    )
    .expect("successful refresh replaces stale snapshot");
    assert_eq!(stale[0].content_id, "fresh");
}

#[test]
fn raster_mime_types_request_image_previews() {
    let mut item = HistoryItem {
        content_id: "content".to_owned(),
        preview: String::new(),
        mime_types: vec!["image/png".to_owned()],
        logical_size: 4,
        source_node: "node".to_owned(),
        pinned: false,
        source_device: "vd".to_owned(),
        physical_millis: 0,
        origin_millis: Some(0),
    };
    assert!(history_item_has_image(&item));
    item.mime_types = vec!["image/svg+xml".to_owned()];
    assert!(!history_item_has_image(&item));
}
