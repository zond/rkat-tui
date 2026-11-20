//! What tests read of the [`App`] and its [`StatusInfo`] that the client
//! runner takes another way ([`App::take_local`], the renderer).

use super::{App, CompactionPolicy, SessionCost, StatusInfo};

impl StatusInfo {
    /// Token totals per model, for the cost readout.
    #[must_use]
    pub fn cost(&self) -> &SessionCost {
        &self.cost
    }

    /// Prices for the cost readout.
    #[must_use]
    pub fn prices(&self) -> &crate::pricing::PriceTable {
        &self.prices
    }

    /// Number of messages typed mid-turn that have not been delivered yet.
    #[must_use]
    pub fn queued(&self) -> usize {
        self.queued
    }
}

impl App {
    /// Whether a bell is due; clears it.
    pub fn take_bell(&mut self) -> bool {
        std::mem::take(&mut self.ring)
    }

    /// The context-compaction policy in effect.
    pub fn compaction_policy(&self) -> CompactionPolicy {
        self.gauge.policy()
    }

    /// Lines submitted with Enter (prompts and slash commands, as typed)
    /// since the last call, oldest first; blank submissions are never
    /// recorded.
    pub fn take_submitted(&mut self) -> Vec<String> {
        std::mem::take(&mut self.submitted)
    }

    /// Return and reset the redraw flag.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }
}
