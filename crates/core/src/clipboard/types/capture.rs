//! Capture generations and aggregate byte-budget policy.

/// Maximum aggregate payload we are willing to capture, in bytes (20 MiB).
pub const MAX_CAPTURE_BYTES: u64 = 20 * 1024 * 1024;

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
