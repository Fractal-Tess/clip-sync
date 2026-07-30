//! Backend-neutral data-control protocol and selection models.

use std::fmt;

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
