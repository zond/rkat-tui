//! Denial counters and the auto-mode state machine.
//!
//! Only classifier blocks count. Three consecutive blocks or twenty in total
//! pause auto mode; while paused, every Tier-3 action is escalated to the human.

use super::mode::PermissionMode;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Consecutive classifier blocks that pause auto mode.
pub const MAX_CONSECUTIVE_DENIALS: u32 = 3;
/// Total classifier blocks (per process) that pause auto mode.
pub const MAX_TOTAL_DENIALS: u32 = 20;

/// Classifier denial counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DenialCounters {
    /// Classifier blocks since the last reviewed allow.
    pub consecutive: u32,
    /// Classifier blocks since start (or the last `/auto resume`).
    pub total: u32,
}

/// Why auto mode paused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PauseCause {
    /// [`MAX_CONSECUTIVE_DENIALS`] reached.
    ConsecutiveDenials,
    /// [`MAX_TOTAL_DENIALS`] reached; only an explicit resume clears it.
    TotalDenials,
}

impl fmt::Display for PauseCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConsecutiveDenials => {
                write!(f, "{MAX_CONSECUTIVE_DENIALS} consecutive classifier blocks")
            }
            Self::TotalDenials => write!(f, "{MAX_TOTAL_DENIALS} classifier blocks in total"),
        }
    }
}

/// The permission mode, the denial counters and the pause, mutated together
/// under one lock. The pause is auto mode's: it decides nothing in another
/// mode, and entering auto mode clears it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoMode {
    /// The permission mode.
    pub mode: PermissionMode,
    /// Denial counters.
    pub counters: DenialCounters,
    /// Why auto mode is paused (Tier-3 actions go to the human), if it is.
    pub paused: Option<PauseCause>,
}

impl AutoMode {
    /// Fresh state in `mode`, not paused.
    pub fn new(mode: PermissionMode) -> Self {
        Self {
            mode,
            counters: DenialCounters::default(),
            paused: None,
        }
    }

    /// A Tier-3 action was allowed after review: reset the consecutive count.
    pub fn record_allow(&mut self) {
        self.counters.consecutive = 0;
    }

    /// The classifier blocked an action. Returns the pause cause when this
    /// block paused auto mode.
    pub fn record_classifier_deny(&mut self) -> Option<PauseCause> {
        self.counters.consecutive = self.counters.consecutive.saturating_add(1);
        self.counters.total = self.counters.total.saturating_add(1);
        if self.paused.is_some() {
            return None;
        }
        let cause = if self.counters.total >= MAX_TOTAL_DENIALS {
            PauseCause::TotalDenials
        } else if self.counters.consecutive >= MAX_CONSECUTIVE_DENIALS {
            PauseCause::ConsecutiveDenials
        } else {
            return None;
        };
        self.paused = Some(cause);
        Some(cause)
    }

    /// Try to resume. A [`PauseCause::TotalDenials`] pause only clears with
    /// `reset_total` (an explicit `/auto resume`, or entering auto mode),
    /// which also resets both counters. Returns `true` when a pause ended.
    pub fn resume(&mut self, reset_total: bool) -> bool {
        if reset_total {
            self.counters = DenialCounters::default();
        }
        match self.paused {
            None => false,
            Some(PauseCause::TotalDenials) if !reset_total => false,
            Some(_) => {
                self.counters.consecutive = 0;
                self.paused = None;
                true
            }
        }
    }

    /// Whether Tier-3 actions are escalated to the human.
    pub fn escalating(&self) -> bool {
        self.paused.is_some()
    }
}

impl Default for AutoMode {
    fn default() -> Self {
        Self::new(PermissionMode::Auto)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_consecutive_pause() {
        let mut m = AutoMode::default();
        assert_eq!(m.record_classifier_deny(), None);
        assert_eq!(m.record_classifier_deny(), None);
        assert_eq!(
            m.record_classifier_deny(),
            Some(PauseCause::ConsecutiveDenials)
        );
        assert!(m.escalating());
        assert_eq!(m.record_classifier_deny(), None, "already paused");
        assert!(m.resume(false));
        assert_eq!(m.counters.consecutive, 0);
        assert_eq!(m.counters.total, 4);
    }

    #[test]
    fn allows_reset_consecutive_only() {
        let mut m = AutoMode::default();
        m.record_classifier_deny();
        m.record_classifier_deny();
        m.record_allow();
        assert_eq!(m.record_classifier_deny(), None);
        assert_eq!(m.counters.total, 3);
        assert_eq!(m.paused, None);
    }

    #[test]
    fn twenty_total_pause_needs_reset() {
        let mut m = AutoMode::default();
        let mut cause = None;
        for _ in 0..20 {
            m.record_allow();
            if let Some(c) = m.record_classifier_deny() {
                cause = Some(c);
            }
        }
        assert_eq!(cause, Some(PauseCause::TotalDenials));
        assert!(!m.resume(false), "allow-once does not clear a total pause");
        assert!(m.escalating());
        assert!(m.resume(true));
        assert_eq!(m.counters, DenialCounters::default());
    }
}
