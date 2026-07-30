use super::*;

#[test]
fn quick_and_management_close_policies_are_distinct() {
    assert!(Presentation::Quick.activation_closes());
    assert!(!Presentation::Management.activation_closes());
    assert!(signal_closes_presentation(
        Presentation::Quick,
        UiSignal::CloseQuick
    ));
    assert!(!signal_closes_presentation(
        Presentation::Management,
        UiSignal::CloseQuick
    ));
    assert!(!signal_closes_presentation(
        Presentation::Quick,
        UiSignal::OpenManagement
    ));
    assert!(activation_result_closes(
        MutationKind::ActivateQuick,
        Presentation::Quick
    ));
    assert!(
        !activation_result_closes(MutationKind::ActivateQuick, Presentation::Management),
        "a newer management intent wins over an in-flight Quick activation"
    );
    assert!(share_confirmation_visible(Presentation::Management, true));
    assert!(
        !share_confirmation_visible(Presentation::Quick, true),
        "management-only confirmation controls never leak into Quick History"
    );
    assert_eq!(
        presentation_after_navigation(Presentation::Quick, ControlTab::History),
        Presentation::Quick
    );
    assert_eq!(
        presentation_after_navigation(Presentation::Quick, ControlTab::Transfers),
        Presentation::Management
    );
    assert_eq!(
        presentation_after_navigation(Presentation::Management, ControlTab::History),
        Presentation::Management
    );
    assert_eq!(
        presentation_switcher_key(Presentation::Quick, SwitcherKey::Escape),
        SwitcherKey::Escape
    );
    assert_eq!(
        presentation_switcher_key(Presentation::Management, SwitcherKey::Escape),
        SwitcherKey::None
    );
}
