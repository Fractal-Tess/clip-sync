//! Integration-level tests for the backend-neutral clipboard model.

use clip_sync_core::clipboard::types::*;

// ── MIME count bounds ───────────────────────────────────────────────────

#[test]
fn mime_count_at_boundary() {
    let at_limit: Vec<MimeType> = (0..MAX_MIME_TYPES_PER_OFFER)
        .map(|i| MimeType::new(format!("x/t{i}")).unwrap())
        .collect();
    assert!(OfferMimeList::new(at_limit).is_ok());

    let over_limit: Vec<MimeType> = (0..=MAX_MIME_TYPES_PER_OFFER)
        .map(|i| MimeType::new(format!("x/t{i}")).unwrap())
        .collect();
    assert!(OfferMimeList::new(over_limit).is_err());
}

// ── MIME name bounds ────────────────────────────────────────────────────

#[test]
fn mime_name_length_boundaries() {
    // Exactly at limit: ok
    let ok_name = "m".repeat(MAX_MIME_NAME_BYTES);
    assert!(MimeType::new(ok_name).is_ok());

    // One byte over: rejected
    let bad_name = "m".repeat(MAX_MIME_NAME_BYTES + 1);
    assert!(matches!(
        MimeType::new(bad_name),
        Err(MimeTypeError::TooLong { .. })
    ));
}

#[test]
fn mime_name_rejects_nul_and_empty() {
    assert!(matches!(MimeType::new(""), Err(MimeTypeError::Empty)));
    assert!(matches!(
        MimeType::new("a\0b"),
        Err(MimeTypeError::ContainsNul)
    ));
}

// ── Aggregate threshold decisions ───────────────────────────────────────

#[test]
fn threshold_empty_is_rejected() {
    assert_eq!(
        should_capture(&[]),
        CaptureDecision::Reject(RejectReason::EmptyOffer)
    );
}

#[test]
fn threshold_within_budget() {
    assert_eq!(should_capture(&[1, 2, 3]), CaptureDecision::Accept);
    assert_eq!(
        should_capture(&[MAX_CAPTURE_BYTES]),
        CaptureDecision::Accept
    );
}

#[test]
fn threshold_over_budget() {
    let over = MAX_CAPTURE_BYTES + 1;
    assert_eq!(
        should_capture(&[over]),
        CaptureDecision::Reject(RejectReason::TooLarge { total_bytes: over })
    );
}

#[test]
fn threshold_multi_representation_sum() {
    let third = MAX_CAPTURE_BYTES / 3;
    // Three thirds fit
    assert_eq!(
        should_capture(&[third, third, third]),
        CaptureDecision::Accept
    );
    // Adding one more byte tips over (depends on rounding)
    let remainder = MAX_CAPTURE_BYTES - 3 * third;
    assert_eq!(
        should_capture(&[third, third, third, remainder + 1]),
        CaptureDecision::Reject(RejectReason::TooLarge {
            total_bytes: 3 * third + remainder + 1
        })
    );
}

#[test]
fn threshold_saturating_addition_on_overflow() {
    // u64::MAX + 1 should saturate, not panic or wrap
    assert!(matches!(
        should_capture(&[u64::MAX, u64::MAX]),
        CaptureDecision::Reject(RejectReason::TooLarge { .. })
    ));
}

// ── Stale generation cancellation ───────────────────────────────────────

#[test]
fn generation_ordering_for_staleness() {
    let g0 = Generation::ZERO;
    let g1 = g0.next();
    let g2 = g1.next();
    let g3 = g2.next();

    // Same generation: not stale
    assert!(!is_stale(g1, g1));

    // Older offer vs newer current: stale
    assert!(is_stale(g1, g2));
    assert!(is_stale(g1, g3));

    // Newer offer vs older current: not stale (shouldn't happen, but safe)
    assert!(!is_stale(g3, g1));
}

#[test]
fn many_generations_stay_ordered() {
    let mut current = Generation::ZERO;
    for i in 0..1000 {
        assert_eq!(current.value(), i);
        let next = current.next();
        assert!(current < next);
        current = next;
    }
}

// ── No primary selection ────────────────────────────────────────────────

#[test]
fn primary_selection_is_intentionally_distinct() {
    // The system should never treat primary selection events as clipboard
    // events. This test ensures the type system keeps them separate.
    assert_ne!(SelectionKind::Clipboard, SelectionKind::Primary);

    // Pattern matching must be exhaustive — a compile-time guarantee that
    // we handle both variants.
    let kind = SelectionKind::Primary;
    let is_clipboard = match kind {
        SelectionKind::Clipboard => true,
        SelectionKind::Primary => false,
    };
    assert!(!is_clipboard);
}

// ── ProbeResult logic ───────────────────────────────────────────────────

#[test]
fn probe_result_usability_matrix() {
    // Both present: usable
    assert!(
        ProbeResult {
            protocol: Some(DataControlProtocol::Ext),
            has_seat: true,
        }
        .is_usable()
    );

    assert!(
        ProbeResult {
            protocol: Some(DataControlProtocol::Wlr),
            has_seat: true,
        }
        .is_usable()
    );

    // Missing protocol: not usable
    assert!(
        !ProbeResult {
            protocol: None,
            has_seat: true,
        }
        .is_usable()
    );

    // Missing seat: not usable
    assert!(
        !ProbeResult {
            protocol: Some(DataControlProtocol::Ext),
            has_seat: false,
        }
        .is_usable()
    );

    // Both missing: not usable
    assert!(
        !ProbeResult {
            protocol: None,
            has_seat: false,
        }
        .is_usable()
    );
}
