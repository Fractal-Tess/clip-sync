use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use clip_sync_core::clipboard::{
    backend::{BackendError, ClipboardBackend, ClipboardEvent},
    types::{
        BoundedMimeOffer, CaptureBudget, ClipboardContent, ClipboardRepresentation,
        FeedbackDecision, FeedbackMarker, FeedbackState, MAX_CAPTURE_BYTES,
        MAX_MIME_TYPES_PER_OFFER, MimeType, OfferMimeList, RejectReason,
    },
    wayland::WaylandBackend,
};
use tokio_util::sync::CancellationToken;

fn representation(mime_type: &str, bytes: &[u8]) -> ClipboardRepresentation {
    ClipboardRepresentation::new(MimeType::new(mime_type).unwrap(), bytes.to_vec())
}

#[test]
fn bounded_wayland_offer_keeps_a_finite_mime_prefix() {
    let mut offer = BoundedMimeOffer::default();
    for index in 0..(MAX_MIME_TYPES_PER_OFFER + 2) {
        offer.push(format!("application/x-wayland-{index}"));
    }
    offer.push(String::new());

    assert_eq!(offer.truncated_count(), 2);
    assert_eq!(offer.invalid_count(), 1);
    assert_eq!(offer.finish().unwrap().len(), MAX_MIME_TYPES_PER_OFFER);
}

#[test]
fn feedback_marker_is_stripped_from_public_offer_view() {
    let marker = FeedbackMarker::new("test-marker-1").unwrap();
    let offer = OfferMimeList::new(vec![
        MimeType::new("text/plain").unwrap(),
        marker.mime_type(),
        MimeType::new("text/html").unwrap(),
    ])
    .unwrap();

    let public = offer.without_feedback_markers();
    assert_eq!(public.len(), 2);
    assert_eq!(public.types()[0].as_str(), "text/plain");
    assert_eq!(public.types()[1].as_str(), "text/html");
}

#[test]
fn feedback_state_allows_only_one_intentional_owned_event() {
    let marker = FeedbackMarker::new("owned-echo").unwrap();
    let offer = OfferMimeList::new(vec![
        MimeType::new("text/plain").unwrap(),
        marker.mime_type(),
    ])
    .unwrap();

    let mut feedback = FeedbackState::default();
    feedback.arm(marker.clone());

    assert_eq!(
        feedback.classify_offer(&offer),
        FeedbackDecision::OwnIntentional(marker.clone())
    );
    assert_eq!(
        feedback.classify_offer(&offer),
        FeedbackDecision::OwnRepeated(marker)
    );
}

#[test]
fn clipboard_content_serves_multiple_representations_lazily() {
    let content = ClipboardContent::new(vec![
        representation("text/plain", b"hello"),
        representation("text/html", b"<b>hello</b>"),
    ])
    .unwrap();

    assert_eq!(content.total_bytes(), 17);
    assert_eq!(
        content.bytes_for_mime("text/plain").unwrap().as_ref(),
        b"hello"
    );
    assert_eq!(
        content.bytes_for_mime("text/html").unwrap().as_ref(),
        b"<b>hello</b>"
    );
    assert!(content.bytes_for_mime("image/png").is_none());
}

#[test]
fn clipboard_content_rejects_more_than_twenty_mib_aggregate() {
    let over_limit = vec![0_u8; usize::try_from(MAX_CAPTURE_BYTES + 1).unwrap()];
    let result = ClipboardContent::new(vec![representation(
        "application/octet-stream",
        &over_limit,
    )]);

    assert!(matches!(
        result.unwrap_err(),
        clip_sync_core::clipboard::types::ClipboardContentError::TooLarge { .. }
    ));
}

#[test]
fn capture_budget_is_aggregate_across_mime_streams() {
    let mut budget = CaptureBudget::with_max(8);
    budget.reserve(3).unwrap();
    budget.reserve(5).unwrap();

    assert_eq!(
        budget.reserve(1).unwrap_err(),
        RejectReason::TooLarge { total_bytes: 9 }
    );
}

#[tokio::test]
async fn setting_clipboard_without_watch_reports_not_running() {
    let backend = WaylandBackend::new();
    let content = ClipboardContent::new(vec![representation("text/plain", b"owned")]).unwrap();

    let error = backend.set_clipboard_content(content).await.unwrap_err();
    assert!(matches!(error, BackendError::WatchNotRunning));
}

#[test]
fn replicated_capture_threshold_applies_without_restarting_backend() {
    let backend = WaylandBackend::new();
    assert_eq!(backend.capture_threshold(), MAX_CAPTURE_BYTES);
    backend.set_capture_threshold(1_234).unwrap();
    assert_eq!(backend.capture_threshold(), 1_234);
    assert!(backend.set_capture_threshold(0).is_err());
    assert_eq!(backend.capture_threshold(), 1_234);
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "manual: requires a live Wayland compositor with data-control permission and mutates the clipboard"]
async fn live_wayland_owned_set_emits_at_most_one_own_event() {
    let backend = WaylandBackend::new();
    let probe = backend.probe().await.expect("Wayland probe should run");
    assert!(
        probe.is_usable(),
        "live compositor must advertise data-control and wl_seat: {probe:?}"
    );

    let setter = backend.clone();
    let shutdown = CancellationToken::new();
    let events = Arc::new(Mutex::new(Vec::<ClipboardEvent>::new()));
    let event_sink = {
        let events = events.clone();
        Box::new(move |event| {
            events.lock().unwrap().push(event);
        })
    };

    let content = ClipboardContent::new(vec![
        representation("text/plain;charset=utf-8", b"clip-sync owned"),
        representation("text/plain", b"clip-sync owned"),
    ])
    .unwrap();

    let watch = backend.watch(shutdown.clone(), event_sink);
    tokio::pin!(watch);

    let scenario = async {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let marker = setter.set_clipboard_content(content).await?;
        tokio::time::sleep(Duration::from_millis(750)).await;
        shutdown.cancel();
        Ok::<FeedbackMarker, BackendError>(marker)
    };
    tokio::pin!(scenario);

    let marker = tokio::select! {
        result = &mut scenario => result.expect("set content should succeed"),
        result = &mut watch => {
            result.expect("watch ended before ownership scenario completed");
            panic!("watch ended before ownership scenario completed");
        }
    };

    watch.await.expect("watch should cancel cleanly");

    let own_event_count = events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| {
            matches!(
                event,
                ClipboardEvent::OwnContent {
                    marker: event_marker,
                    ..
                } if event_marker == &marker
            )
        })
        .count();

    assert!(own_event_count <= 1);
}
