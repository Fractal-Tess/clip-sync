use std::time::{Duration, Instant};

use super::history_refresh_delay;

#[derive(Debug)]
pub(in crate::ui) struct HistoryRefreshState {
    pub(in crate::ui) in_flight: bool,
    pub(in crate::ui) coalesced: bool,
    pub(in crate::ui) consecutive_failures: u8,
    pub(in crate::ui) next_due: Instant,
}

impl HistoryRefreshState {
    pub(in crate::ui) fn new(now: Instant) -> Self {
        Self {
            in_flight: false,
            coalesced: false,
            consecutive_failures: 0,
            next_due: now,
        }
    }

    pub(in crate::ui) fn request(&mut self) -> bool {
        if self.in_flight {
            self.coalesced = true;
            false
        } else {
            self.in_flight = true;
            true
        }
    }

    pub(in crate::ui) fn finish(
        &mut self,
        now: Instant,
        succeeded: bool,
        cadence: Duration,
    ) -> bool {
        self.in_flight = false;
        let dispatch_coalesced = if succeeded {
            self.consecutive_failures = 0;
            std::mem::take(&mut self.coalesced)
        } else {
            self.consecutive_failures = self.consecutive_failures.saturating_add(1);
            self.coalesced = false;
            false
        };
        self.next_due = now + history_refresh_delay(cadence, self.consecutive_failures);
        dispatch_coalesced
    }
}
