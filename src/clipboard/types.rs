//! Backend-neutral clipboard types.
//!
//! These types model clipboard offers, generation tracking, MIME validation,
//! and capture-size policy without coupling to any specific display server
//! protocol.

use std::{collections::HashSet, fmt, sync::Arc};

use thiserror::Error;

/// Maximum number of MIME types we accept in a single offer.
///
/// Compositors can advertise an arbitrary number of types; we cap at a
/// reasonable bound to avoid unbounded allocations from a misbehaving source.
pub const MAX_MIME_TYPES_PER_OFFER: usize = 128;

/// Maximum byte length of a single MIME type string.
///
/// RFC 6838 limits type/subtype to 127 octets each plus the slash, and
/// parameters can extend further, but real-world clipboard types stay well
/// under 256 bytes.
pub const MAX_MIME_NAME_BYTES: usize = 256;

/// Maximum aggregate payload we are willing to capture, in bytes (20 MiB).
pub const MAX_CAPTURE_BYTES: u64 = 20 * 1024 * 1024;

/// MIME prefix used only for daemon feedback-loop detection.
///
/// This representation is advertised with daemon-owned clipboard sources so
/// the watcher can identify its own Wayland echo without shelling out or
/// reading user payloads back into the history.
pub const FEEDBACK_MARKER_MIME_PREFIX: &str = "application/x-clip-sync-owner;marker=";

/// Monotonically increasing generation counter for clipboard contents.
///
/// Each time the compositor advertises a new selection, the watcher bumps
/// the generation so that stale in-flight reads can be detected and cancelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Generation(u64);

impl Generation {
    /// The initial generation before any selection has been observed.
    pub const ZERO: Self = Self(0);

    /// Advances to the next generation.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }

    /// Returns the raw counter value.
    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }

    /// Rebuilds a generation from a stored counter value.
    #[must_use]
    pub const fn from_value(value: u64) -> Self {
        Self(value)
    }
}

/// A validated MIME type name from a clipboard offer.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MimeType(String);

impl MimeType {
    /// Validates and wraps a MIME type string.
    ///
    /// # Errors
    ///
    /// Returns an error if the name is empty, exceeds [`MAX_MIME_NAME_BYTES`],
    /// or contains a NUL byte (which would be truncated by the Wayland wire
    /// format).
    pub fn new(name: impl Into<String>) -> Result<Self, MimeTypeError> {
        let name = name.into();
        if name.is_empty() {
            return Err(MimeTypeError::Empty);
        }
        if name.len() > MAX_MIME_NAME_BYTES {
            return Err(MimeTypeError::TooLong {
                len: name.len(),
                max: MAX_MIME_NAME_BYTES,
            });
        }
        if name.bytes().any(|b| b == 0) {
            return Err(MimeTypeError::ContainsNul);
        }
        Ok(Self(name))
    }

    /// Returns the validated name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for MimeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("MimeType").field(&self.0).finish()
    }
}

impl fmt::Display for MimeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Errors when constructing a [`MimeType`].
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum MimeTypeError {
    #[error("MIME type name must not be empty")]
    Empty,
    #[error("MIME type name is {len} bytes, exceeding the {max}-byte limit")]
    TooLong { len: usize, max: usize },
    #[error("MIME type name must not contain NUL bytes")]
    ContainsNul,
}

/// A snapshot of the MIME types advertised by a single clipboard offer.
#[derive(Clone, Debug)]
pub struct OfferMimeList {
    types: Vec<MimeType>,
}

impl OfferMimeList {
    /// Builds an offer from accumulated MIME type events.
    ///
    /// # Errors
    ///
    /// Returns an error if the count exceeds [`MAX_MIME_TYPES_PER_OFFER`].
    pub fn new(types: Vec<MimeType>) -> Result<Self, OfferError> {
        let mut seen = HashSet::with_capacity(types.len());
        let types = types
            .into_iter()
            .filter(|mime_type| seen.insert(mime_type.clone()))
            .collect::<Vec<_>>();
        if types.len() > MAX_MIME_TYPES_PER_OFFER {
            return Err(OfferError::TooManyMimeTypes {
                count: types.len(),
                max: MAX_MIME_TYPES_PER_OFFER,
            });
        }
        Ok(Self { types })
    }

    /// Returns the MIME types in the order they were advertised.
    #[must_use]
    pub fn types(&self) -> &[MimeType] {
        &self.types
    }

    /// Number of MIME types in this offer.
    #[must_use]
    pub fn len(&self) -> usize {
        self.types.len()
    }

    /// Whether this offer advertises zero MIME types.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.types.is_empty()
    }

    /// Returns this list without internal feedback marker MIME types.
    ///
    /// The marker is an implementation detail for loop prevention and should
    /// not become part of captured user clipboard content.
    #[must_use]
    pub fn without_feedback_markers(&self) -> Self {
        let types = self
            .types
            .iter()
            .filter(|mime_type| FeedbackMarker::from_mime_type(mime_type).is_none())
            .cloned()
            .collect();
        Self { types }
    }

    /// Finds the first internal feedback marker advertised by this offer.
    #[must_use]
    pub fn feedback_marker(&self) -> Option<FeedbackMarker> {
        self.types.iter().find_map(FeedbackMarker::from_mime_type)
    }
}

/// Errors when constructing an [`OfferMimeList`].
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum OfferError {
    #[error("offer has {count} MIME types, exceeding the {max} limit")]
    TooManyMimeTypes { count: usize, max: usize },
}

/// Decision about whether to capture a clipboard offer's data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureDecision {
    /// Proceed: the aggregate size is within budget.
    Accept,
    /// Reject: at least one reason to skip this offer.
    Reject(RejectReason),
}

/// Why we decided not to capture an offer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RejectReason {
    /// The offer advertises no MIME types at all.
    EmptyOffer,
    /// Aggregate payload would exceed [`MAX_CAPTURE_BYTES`].
    TooLarge { total_bytes: u64 },
    /// The offer did not contain any valid, non-internal MIME types.
    InvalidOffer,
    /// The generation has already been superseded.
    StaleGeneration {
        offer_generation: Generation,
        current_generation: Generation,
    },
    /// A pipe read failed before all MIME representations were captured.
    ReadFailed { mime_type: String, message: String },
    /// Capture was cancelled by shutdown.
    Cancelled,
}

/// Evaluates whether an offer should be captured based on aggregate size.
///
/// `mime_sizes` contains the expected byte count for each MIME representation
/// we would fetch. Returns [`CaptureDecision::Accept`] only when the total
/// is within [`MAX_CAPTURE_BYTES`].
pub fn should_capture(mime_sizes: &[u64]) -> CaptureDecision {
    if mime_sizes.is_empty() {
        return CaptureDecision::Reject(RejectReason::EmptyOffer);
    }

    let total: u64 = mime_sizes.iter().copied().fold(0_u64, u64::saturating_add);

    if total > MAX_CAPTURE_BYTES {
        CaptureDecision::Reject(RejectReason::TooLarge { total_bytes: total })
    } else {
        CaptureDecision::Accept
    }
}

/// Checks whether an in-flight capture is still valid given the current
/// generation, or whether it has been superseded and should be cancelled.
#[must_use]
pub fn is_stale(offer_generation: Generation, current_generation: Generation) -> bool {
    offer_generation < current_generation
}

/// Which data-control protocol the compositor supports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataControlProtocol {
    /// The standardised `ext-data-control-v1` (preferred).
    Ext,
    /// The wlroots-specific `zwlr-data-control-v1` (legacy fallback).
    Wlr,
}

impl fmt::Display for DataControlProtocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ext => f.write_str("ext-data-control-v1"),
            Self::Wlr => f.write_str("zwlr-data-control-v1"),
        }
    }
}

/// Result of probing the compositor for data-control support.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeResult {
    /// Which protocol was found, if any.
    pub protocol: Option<DataControlProtocol>,
    /// Whether a `wl_seat` global is present (required for `get_data_device`).
    pub has_seat: bool,
}

impl ProbeResult {
    /// Whether the compositor is usable for clipboard monitoring.
    #[must_use]
    pub fn is_usable(&self) -> bool {
        self.protocol.is_some() && self.has_seat
    }
}

/// Selection source kind. We only care about the regular clipboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionKind {
    /// The regular clipboard (ctrl+c / ctrl+v).
    Clipboard,
    /// Primary selection (middle-click paste). We intentionally ignore this.
    Primary,
}

/// One lazily served or captured MIME representation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardRepresentation {
    mime_type: MimeType,
    bytes: Arc<[u8]>,
}

impl ClipboardRepresentation {
    /// Builds a representation from a validated MIME type and owned bytes.
    #[must_use]
    pub fn new(mime_type: MimeType, bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            mime_type,
            bytes: Arc::from(bytes.into().into_boxed_slice()),
        }
    }

    /// Returns the representation MIME type.
    #[must_use]
    pub fn mime_type(&self) -> &MimeType {
        &self.mime_type
    }

    /// Returns the representation bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns a cheap shared clone of the representation bytes.
    #[must_use]
    pub fn bytes_arc(&self) -> Arc<[u8]> {
        self.bytes.clone()
    }

    /// Builds a representation from already shared bytes.
    #[must_use]
    pub fn from_shared_bytes(mime_type: MimeType, bytes: Arc<[u8]>) -> Self {
        Self { mime_type, bytes }
    }
}

/// A bounded set of clipboard MIME byte representations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardContent {
    representations: Vec<ClipboardRepresentation>,
}

/// Size-only snapshot of the current live offer used for explicit sharing.
#[derive(Clone, Debug)]
pub struct CurrentClipboardInspection {
    generation: Generation,
    mime_list: OfferMimeList,
    logical_size: u64,
}

impl CurrentClipboardInspection {
    #[must_use]
    pub const fn new(generation: Generation, mime_list: OfferMimeList, logical_size: u64) -> Self {
        Self {
            generation,
            mime_list,
            logical_size,
        }
    }

    #[must_use]
    pub const fn generation(&self) -> Generation {
        self.generation
    }

    #[must_use]
    pub const fn mime_list(&self) -> &OfferMimeList {
        &self.mime_list
    }

    #[must_use]
    pub const fn logical_size(&self) -> u64 {
        self.logical_size
    }
}

impl ClipboardContent {
    /// Builds clipboard content after enforcing count, uniqueness, and size
    /// bounds.
    ///
    /// # Errors
    ///
    /// Returns an error when there are no usable representations, too many MIME
    /// types, duplicate MIME names, internal marker MIME names, or more than
    /// [`MAX_CAPTURE_BYTES`] aggregate bytes.
    pub fn new(
        representations: Vec<ClipboardRepresentation>,
    ) -> Result<Self, ClipboardContentError> {
        Self::new_with_max(representations, MAX_CAPTURE_BYTES)
    }

    /// Builds clipboard content with a caller-provided aggregate byte limit.
    ///
    /// This is used by the daemon's replicated automatic-capture threshold,
    /// confirmed explicit shares, and activation.
    ///
    /// # Errors
    ///
    /// Returns the same structural errors as [`Self::new`], or
    /// [`ClipboardContentError::TooLarge`] when `max_bytes` is zero or
    /// exceeded.
    pub fn new_with_max(
        representations: Vec<ClipboardRepresentation>,
        max_bytes: u64,
    ) -> Result<Self, ClipboardContentError> {
        if max_bytes == 0 {
            return Err(ClipboardContentError::TooLarge { total_bytes: 0 });
        }
        if representations.is_empty() {
            return Err(ClipboardContentError::Empty);
        }
        if representations.len() > MAX_MIME_TYPES_PER_OFFER {
            return Err(ClipboardContentError::TooManyMimeTypes {
                count: representations.len(),
                max: MAX_MIME_TYPES_PER_OFFER,
            });
        }

        let mut names = HashSet::with_capacity(representations.len());
        let mut total_bytes = 0_u64;
        for representation in &representations {
            if FeedbackMarker::from_mime_type(representation.mime_type()).is_some() {
                return Err(ClipboardContentError::InternalMimeType {
                    mime_type: representation.mime_type().to_string(),
                });
            }
            if !names.insert(representation.mime_type().to_string()) {
                return Err(ClipboardContentError::DuplicateMimeType {
                    mime_type: representation.mime_type().to_string(),
                });
            }
            total_bytes = total_bytes
                .saturating_add(u64::try_from(representation.bytes().len()).unwrap_or(u64::MAX));
        }

        if total_bytes > max_bytes {
            return Err(ClipboardContentError::TooLarge { total_bytes });
        }

        Ok(Self { representations })
    }

    /// Compatibility alias for explicit-share callers.
    ///
    /// # Errors
    ///
    /// Returns the same validation errors as [`Self::new_with_max`].
    pub fn new_with_limit(
        representations: Vec<ClipboardRepresentation>,
        maximum_bytes: u64,
    ) -> Result<Self, ClipboardContentError> {
        Self::new_with_max(representations, maximum_bytes)
    }

    /// Returns the MIME byte representations in offer order.
    #[must_use]
    pub fn representations(&self) -> &[ClipboardRepresentation] {
        &self.representations
    }

    /// Returns the aggregate byte size across all representations.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.representations
            .iter()
            .map(|representation| u64::try_from(representation.bytes().len()).unwrap_or(u64::MAX))
            .fold(0_u64, u64::saturating_add)
    }

    /// Returns the bytes for a MIME type if this content can serve it.
    #[must_use]
    pub fn bytes_for_mime(&self, mime_type: &str) -> Option<Arc<[u8]>> {
        self.representations
            .iter()
            .find(|representation| representation.mime_type().as_str() == mime_type)
            .map(ClipboardRepresentation::bytes_arc)
    }

    /// Returns a MIME-only view of the user-visible representations.
    ///
    /// # Errors
    ///
    /// Propagates the bounded offer error if the content somehow exceeds MIME
    /// count limits.
    pub fn mime_list(&self) -> Result<OfferMimeList, OfferError> {
        OfferMimeList::new(
            self.representations
                .iter()
                .map(|representation| representation.mime_type().clone())
                .collect(),
        )
    }
}

/// Errors when constructing [`ClipboardContent`].
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ClipboardContentError {
    #[error("clipboard content must contain at least one MIME representation")]
    Empty,
    #[error("clipboard content has {count} MIME types, exceeding the {max} limit")]
    TooManyMimeTypes { count: usize, max: usize },
    #[error("clipboard content contains duplicate MIME type {mime_type}")]
    DuplicateMimeType { mime_type: String },
    #[error("clipboard content uses internal feedback MIME type {mime_type}")]
    InternalMimeType { mime_type: String },
    #[error("clipboard content is {total_bytes} bytes, exceeding the capture limit")]
    TooLarge { total_bytes: u64 },
}

/// Per-source marker used to recognize daemon-owned clipboard echoes.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FeedbackMarker(String);

impl FeedbackMarker {
    /// Generates a fresh marker token.
    #[must_use]
    pub fn generate() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// Builds a marker from an existing token.
    ///
    /// # Errors
    ///
    /// Returns an error if the token is empty or contains characters that are
    /// unsuitable for a MIME parameter value.
    pub fn new(token: impl Into<String>) -> Result<Self, FeedbackMarkerError> {
        let token = token.into();
        if token.is_empty() {
            return Err(FeedbackMarkerError::Empty);
        }
        if !token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(FeedbackMarkerError::InvalidCharacter);
        }
        Ok(Self(token))
    }

    /// Returns the marker token.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the private MIME type used to advertise this marker.
    #[must_use]
    pub fn mime_type(&self) -> MimeType {
        MimeType(format!("{FEEDBACK_MARKER_MIME_PREFIX}{}", self.0))
    }

    /// Extracts a feedback marker from an internal MIME type.
    #[must_use]
    pub fn from_mime_type(mime_type: &MimeType) -> Option<Self> {
        let token = mime_type
            .as_str()
            .strip_prefix(FEEDBACK_MARKER_MIME_PREFIX)?;
        Self::new(token.to_owned()).ok()
    }
}

/// Errors when constructing a [`FeedbackMarker`].
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum FeedbackMarkerError {
    #[error("feedback marker must not be empty")]
    Empty,
    #[error("feedback marker contains an invalid MIME parameter character")]
    InvalidCharacter,
}

/// Classification for a newly advertised regular clipboard offer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FeedbackDecision {
    /// The offer should be treated as external clipboard content.
    External,
    /// This is the first compositor echo for a daemon-owned source.
    OwnIntentional(FeedbackMarker),
    /// This is another echo for the current daemon-owned source and should not
    /// produce another event.
    OwnRepeated(FeedbackMarker),
}

/// State machine that prevents daemon-owned clipboard sources from becoming
/// capture feedback loops.
#[derive(Clone, Debug, Default)]
pub struct FeedbackState {
    pending: Option<FeedbackMarker>,
    active: Option<FeedbackMarker>,
}

impl FeedbackState {
    /// Arms feedback suppression for a daemon-owned source.
    pub fn arm(&mut self, marker: FeedbackMarker) {
        self.pending = Some(marker.clone());
        self.active = Some(marker);
    }

    /// Classifies an offer based on its internal feedback marker, if any.
    ///
    /// The first matching pending marker becomes a single intentional event.
    /// Repeated echoes for the same active marker are suppressed.
    #[must_use]
    pub fn classify_offer(&mut self, mime_list: &OfferMimeList) -> FeedbackDecision {
        let Some(marker) = mime_list.feedback_marker() else {
            return FeedbackDecision::External;
        };

        if self.pending.as_ref() == Some(&marker) {
            self.pending = None;
            self.active = Some(marker.clone());
            return FeedbackDecision::OwnIntentional(marker);
        }

        if self.active.as_ref() == Some(&marker) {
            return FeedbackDecision::OwnRepeated(marker);
        }

        FeedbackDecision::External
    }

    /// Clears ownership tracking.
    pub fn clear(&mut self) {
        self.pending = None;
        self.active = None;
    }
}

/// Bounded MIME accumulator for a Wayland offer.
#[derive(Clone, Debug, Default)]
pub struct BoundedMimeOffer {
    types: Vec<MimeType>,
    invalid_count: usize,
    truncated_count: usize,
}

impl BoundedMimeOffer {
    /// Adds one MIME name, keeping only the bounded, valid prefix.
    pub fn push(&mut self, mime_type: String) {
        let Ok(mime_type) = MimeType::new(mime_type) else {
            self.invalid_count += 1;
            return;
        };

        if self.types.contains(&mime_type) {
            return;
        }

        if self.types.len() == MAX_MIME_TYPES_PER_OFFER {
            self.truncated_count += 1;
            return;
        }

        self.types.push(mime_type);
    }

    /// Finishes the offer into a bounded MIME list.
    ///
    /// # Errors
    ///
    /// Propagates [`OfferError`] if the internal bound is violated.
    pub fn finish(self) -> Result<OfferMimeList, OfferError> {
        OfferMimeList::new(self.types)
    }

    /// Number of invalid MIME names ignored while collecting this offer.
    #[must_use]
    pub fn invalid_count(&self) -> usize {
        self.invalid_count
    }

    /// Number of valid MIME names ignored after the offer reached the limit.
    #[must_use]
    pub fn truncated_count(&self) -> usize {
        self.truncated_count
    }
}

/// Aggregate byte budget used while streaming MIME data.
#[derive(Clone, Debug)]
pub struct CaptureBudget {
    max_bytes: u64,
    total_bytes: u64,
    exceeded: bool,
}

impl CaptureBudget {
    /// Creates a budget with the default 20 MiB maximum.
    #[must_use]
    pub const fn new() -> Self {
        Self::with_max(MAX_CAPTURE_BYTES)
    }

    /// Creates a budget with a caller-provided maximum.
    #[must_use]
    pub const fn with_max(max_bytes: u64) -> Self {
        Self {
            max_bytes,
            total_bytes: 0,
            exceeded: false,
        }
    }

    /// Reserves bytes from the aggregate capture budget.
    ///
    /// # Errors
    ///
    /// Returns [`RejectReason::TooLarge`] when the reservation would exceed the
    /// configured aggregate maximum.
    pub fn reserve(&mut self, bytes: usize) -> Result<(), RejectReason> {
        if self.exceeded {
            return Err(RejectReason::TooLarge {
                total_bytes: self.total_bytes,
            });
        }

        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        let next_total = self.total_bytes.saturating_add(bytes);
        if next_total > self.max_bytes {
            self.total_bytes = next_total;
            self.exceeded = true;
            return Err(RejectReason::TooLarge {
                total_bytes: next_total,
            });
        }

        self.total_bytes = next_total;
        Ok(())
    }

    /// Returns the observed aggregate byte count, including the rejected
    /// over-limit reservation when the budget has been exceeded.
    #[must_use]
    pub const fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// Returns whether the budget has been exceeded.
    #[must_use]
    pub const fn exceeded(&self) -> bool {
        self.exceeded
    }
}

impl Default for CaptureBudget {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
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
}
