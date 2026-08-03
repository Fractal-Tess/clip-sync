//! Daemon-owned clipboard feedback-loop markers and suppression state.

use thiserror::Error;

use super::mime::{MimeType, OfferMimeList};

/// MIME prefix used only for daemon feedback-loop detection.
///
/// This representation is advertised with daemon-owned clipboard sources so
/// the watcher can identify its own Wayland echo without shelling out or
/// reading user payloads back into the history.
pub const FEEDBACK_MARKER_MIME_PREFIX: &str = "application/x-clip-sync-owner;marker=";

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
