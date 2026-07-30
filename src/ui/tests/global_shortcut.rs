use super::*;

#[test]
fn hyprland_shortcut_identity_and_anonymous_event_gate_are_stable() {
    assert_eq!(GLOBAL_SHORTCUT_APP_ID, "clip-sync");
    assert_eq!(CLOSE_QUICK_SHORTCUT_ID, "close-quick");
    assert_eq!(
        signal_for_shortcut_event(ShortcutEventKind::Pressed),
        Some(UiSignal::CloseQuick)
    );
    assert_eq!(signal_for_shortcut_event(ShortcutEventKind::Released), None);
}

#[test]
fn late_share_inspection_cannot_return_after_quick_management_switch() {
    let mut share = ShareGenerationState::default();
    let inspection_generation = share.start_request();
    let mut presentation = Presentation::Management;
    assert_eq!(presentation, Presentation::Management);

    presentation = Presentation::Quick;
    assert_eq!(presentation, Presentation::Quick);
    share.invalidate();
    assert_eq!(
        share.complete(inspection_generation),
        ShareCompletion::Discard
    );

    presentation = Presentation::Management;
    share.invalidate();
    assert_eq!(presentation, Presentation::Management);
    assert!(!share_confirmation_visible(presentation, false));
}

#[test]
fn late_share_response_cannot_clobber_a_newer_request() {
    let mut share = ShareGenerationState::default();
    let stale = share.start_request();
    let current = share.start_request();

    assert_eq!(share.complete(stale), ShareCompletion::Ignore);
    assert_eq!(share.complete(current), ShareCompletion::Apply);
}
