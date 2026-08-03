use super::*;

// ── MimeType validation ─────────────────────────────────────────

#[test]
fn valid_mime_type_accepted() {
    assert!(MimeType::new("text/plain").is_ok());
    assert!(MimeType::new("image/png").is_ok());
    assert!(MimeType::new("application/x-special/gnome-copied-files").is_ok());
}

#[test]
fn empty_mime_type_rejected() {
    assert_eq!(MimeType::new("").unwrap_err(), MimeTypeError::Empty);
}

#[test]
fn mime_type_with_nul_rejected() {
    assert_eq!(
        MimeType::new("text/\0plain").unwrap_err(),
        MimeTypeError::ContainsNul
    );
}

#[test]
fn mime_type_at_exact_limit_accepted() {
    let name = "x".repeat(MAX_MIME_NAME_BYTES);
    assert!(MimeType::new(name).is_ok());
}

#[test]
fn mime_type_over_limit_rejected() {
    let name = "x".repeat(MAX_MIME_NAME_BYTES + 1);
    assert!(matches!(
        MimeType::new(name).unwrap_err(),
        MimeTypeError::TooLong { .. }
    ));
}

// ── OfferMimeList count bounds ──────────────────────────────────

#[test]
fn offer_at_exact_limit_accepted() {
    let types: Vec<MimeType> = (0..MAX_MIME_TYPES_PER_OFFER)
        .map(|i| MimeType::new(format!("type/{i}")).unwrap())
        .collect();
    assert!(OfferMimeList::new(types).is_ok());
}

#[test]
fn offer_over_limit_rejected() {
    let types: Vec<MimeType> = (0..=MAX_MIME_TYPES_PER_OFFER)
        .map(|i| MimeType::new(format!("type/{i}")).unwrap())
        .collect();
    assert!(matches!(
        OfferMimeList::new(types).unwrap_err(),
        OfferError::TooManyMimeTypes { .. }
    ));
}

#[test]
fn duplicate_offer_mime_types_keep_the_first_occurrence() {
    let plain = MimeType::new("text/plain").unwrap();
    let html = MimeType::new("text/html").unwrap();
    let list = OfferMimeList::new(vec![plain.clone(), plain, html]).unwrap();

    assert_eq!(list.len(), 2);
    assert_eq!(list.types()[0].as_str(), "text/plain");
    assert_eq!(list.types()[1].as_str(), "text/html");
}

#[test]
fn empty_offer_has_zero_len() {
    let list = OfferMimeList::new(vec![]).unwrap();
    assert!(list.is_empty());
    assert_eq!(list.len(), 0);
}

// ── Aggregate threshold decisions ───────────────────────────────

#[test]
fn empty_sizes_rejected_as_empty_offer() {
    assert_eq!(
        should_capture(&[]),
        CaptureDecision::Reject(RejectReason::EmptyOffer)
    );
}

#[test]
fn single_small_payload_accepted() {
    assert_eq!(should_capture(&[1024]), CaptureDecision::Accept);
}

#[test]
fn exactly_at_limit_accepted() {
    assert_eq!(
        should_capture(&[MAX_CAPTURE_BYTES]),
        CaptureDecision::Accept
    );
}

#[test]
fn one_byte_over_limit_rejected() {
    assert_eq!(
        should_capture(&[MAX_CAPTURE_BYTES + 1]),
        CaptureDecision::Reject(RejectReason::TooLarge {
            total_bytes: MAX_CAPTURE_BYTES + 1
        })
    );
}

#[test]
fn multiple_representations_summed() {
    let half = MAX_CAPTURE_BYTES / 2;
    // Two halves fit
    assert_eq!(should_capture(&[half, half]), CaptureDecision::Accept);
    // Two halves + 1 exceeds
    assert_eq!(
        should_capture(&[half, half + 1]),
        CaptureDecision::Reject(RejectReason::TooLarge {
            total_bytes: half + half + 1
        })
    );
}

#[test]
fn u64_overflow_saturates_to_rejection() {
    // Two huge values that would overflow u64 if added naively.
    assert!(matches!(
        should_capture(&[u64::MAX, 1]),
        CaptureDecision::Reject(RejectReason::TooLarge { .. })
    ));
}

// ── Stale generation cancellation ───────────────────────────────

#[test]
fn same_generation_is_not_stale() {
    let g = Generation::ZERO.next();
    assert!(!is_stale(g, g));
}

#[test]
fn older_generation_is_stale() {
    let old = Generation::ZERO.next();
    let new = old.next();
    assert!(is_stale(old, new));
}

#[test]
fn newer_generation_is_not_stale() {
    let old = Generation::ZERO.next();
    let new = old.next();
    assert!(!is_stale(new, old));
}

#[test]
fn generation_advances_deterministically() {
    let g0 = Generation::ZERO;
    let g1 = g0.next();
    let g2 = g1.next();
    assert_eq!(g0.value(), 0);
    assert_eq!(g1.value(), 1);
    assert_eq!(g2.value(), 2);
    assert!(g0 < g1);
    assert!(g1 < g2);
}

// ── Primary selection is a distinct kind ────────────────────────

#[test]
fn selection_kinds_are_distinct() {
    assert_ne!(SelectionKind::Clipboard, SelectionKind::Primary);
}

// ── ProbeResult usability ───────────────────────────────────────

#[test]
fn probe_usable_requires_protocol_and_seat() {
    let usable = ProbeResult {
        protocol: Some(DataControlProtocol::Ext),
        has_seat: true,
    };
    assert!(usable.is_usable());

    let no_protocol = ProbeResult {
        protocol: None,
        has_seat: true,
    };
    assert!(!no_protocol.is_usable());

    let no_seat = ProbeResult {
        protocol: Some(DataControlProtocol::Wlr),
        has_seat: false,
    };
    assert!(!no_seat.is_usable());
}

#[test]
fn data_control_protocol_display() {
    assert_eq!(DataControlProtocol::Ext.to_string(), "ext-data-control-v1");
    assert_eq!(DataControlProtocol::Wlr.to_string(), "zwlr-data-control-v1");
}

#[test]
fn bounded_offer_keeps_only_limit() {
    let mut offer = BoundedMimeOffer::default();
    for index in 0..(MAX_MIME_TYPES_PER_OFFER + 3) {
        offer.push(format!("application/x-test-{index}"));
    }

    assert_eq!(offer.truncated_count(), 3);
    assert_eq!(offer.invalid_count(), 0);
    assert_eq!(offer.finish().unwrap().len(), MAX_MIME_TYPES_PER_OFFER);
}

#[test]
fn bounded_offer_counts_invalid_names() {
    let mut offer = BoundedMimeOffer::default();
    offer.push(String::new());
    offer.push("text/plain".to_owned());

    let list = offer.finish().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list.types()[0].as_str(), "text/plain");
}

#[test]
fn clipboard_content_rejects_duplicates() {
    let content = ClipboardContent::new(vec![
        ClipboardRepresentation::new(MimeType::new("text/plain").unwrap(), b"a".to_vec()),
        ClipboardRepresentation::new(MimeType::new("text/plain").unwrap(), b"b".to_vec()),
    ]);

    assert!(matches!(
        content.unwrap_err(),
        ClipboardContentError::DuplicateMimeType { .. }
    ));
}

#[test]
fn clipboard_content_rejects_internal_marker_mime() {
    let marker = FeedbackMarker::new("owned-1").unwrap();
    let content = ClipboardContent::new(vec![ClipboardRepresentation::new(
        marker.mime_type(),
        b"owned-1".to_vec(),
    )]);

    assert!(matches!(
        content.unwrap_err(),
        ClipboardContentError::InternalMimeType { .. }
    ));
}

#[test]
fn clipboard_content_tracks_total_and_lookup() {
    let text = MimeType::new("text/plain").unwrap();
    let html = MimeType::new("text/html").unwrap();
    let content = ClipboardContent::new(vec![
        ClipboardRepresentation::new(text.clone(), b"hello".to_vec()),
        ClipboardRepresentation::new(html.clone(), b"<b>hello</b>".to_vec()),
    ])
    .unwrap();

    assert_eq!(content.total_bytes(), 17);
    assert_eq!(
        content.bytes_for_mime(text.as_str()).unwrap().as_ref(),
        b"hello"
    );
    assert_eq!(
        content.bytes_for_mime(html.as_str()).unwrap().as_ref(),
        b"<b>hello</b>"
    );
}

#[test]
fn feedback_marker_round_trips_through_mime() {
    let marker = FeedbackMarker::new("abc-123").unwrap();
    let mime_type = marker.mime_type();

    assert_eq!(
        FeedbackMarker::from_mime_type(&mime_type),
        Some(marker.clone())
    );
    assert_eq!(marker.as_str(), "abc-123");
}

#[test]
fn feedback_state_emits_intentional_once() {
    let marker = FeedbackMarker::new("abc-123").unwrap();
    let marker_mime = marker.mime_type();
    let public_mime = MimeType::new("text/plain").unwrap();
    let list = OfferMimeList::new(vec![public_mime, marker_mime]).unwrap();

    let mut feedback = FeedbackState::default();
    feedback.arm(marker.clone());

    assert_eq!(
        feedback.classify_offer(&list),
        FeedbackDecision::OwnIntentional(marker.clone())
    );
    assert_eq!(
        feedback.classify_offer(&list),
        FeedbackDecision::OwnRepeated(marker)
    );
}

#[test]
fn offer_mime_list_strips_feedback_marker() {
    let marker = FeedbackMarker::new("abc-123").unwrap();
    let list = OfferMimeList::new(vec![
        MimeType::new("text/plain").unwrap(),
        marker.mime_type(),
        MimeType::new("text/html").unwrap(),
    ])
    .unwrap();

    let public = list.without_feedback_markers();
    assert_eq!(public.len(), 2);
    assert_eq!(public.types()[0].as_str(), "text/plain");
    assert_eq!(public.types()[1].as_str(), "text/html");
}

#[test]
fn capture_budget_rejects_aggregate_overflow() {
    let mut budget = CaptureBudget::with_max(10);
    budget.reserve(4).unwrap();
    budget.reserve(6).unwrap();

    assert_eq!(budget.total_bytes(), 10);
    assert_eq!(
        budget.reserve(1).unwrap_err(),
        RejectReason::TooLarge { total_bytes: 11 }
    );
    assert!(budget.exceeded());
}
