//! The `tui.toml` settings the UI renders and the daemon applies: the
//! context-compaction policy (`[auto] compact*`, see
//! `crate::ui::compaction` for how the gauge reads it), the checklist
//! panel's view (`[ui] checklist`) and how helper headers start
//! (`[ui] helpers`).

use std::num::NonZeroU64;

use serde::{Deserialize, Serialize};

/// Default `compact_warn_percent`.
pub const DEFAULT_WARN_PERCENT: u8 = 75;

/// Default `compact_auto_percent`.
pub const DEFAULT_AUTO_PERCENT: u8 = 85;

/// Tokens at which to warn when the context window is unknown.
pub const FALLBACK_WARN_TOKENS: u64 = 120_000;

/// Tokens at which to compact (auto) or warn every turn (warn) when the
/// context window is unknown.
pub const FALLBACK_AUTO_TOKENS: u64 = 150_000;

/// What happens when the context fills up.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompactMode {
    /// The engine's own threshold policy; the gauge only warns.
    #[default]
    Native,
    /// Warn; compaction only on `/compact now`.
    Warn,
    /// Let the engine compact at `compact_auto_percent`.
    Auto,
}

/// Thresholds and mode for the context gauge (`crate::ui::CompactionGauge`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionPolicy {
    /// Warn or compact automatically.
    pub mode: CompactMode,
    /// Usage (percent of the window) at which to warn once.
    pub warn_percent: u8,
    /// Usage (percent of the window) at which the engine compacts (auto
    /// mode) or the gauge warns after every turn (warn mode). Never below
    /// `warn_percent`.
    pub auto_percent: u8,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            mode: CompactMode::Native,
            warn_percent: DEFAULT_WARN_PERCENT,
            auto_percent: DEFAULT_AUTO_PERCENT,
        }
    }
}

impl CompactionPolicy {
    /// Clamp `warn_percent` to `1..=100` and `auto_percent` to
    /// `warn_percent..=100`. Returns the clamped policy and whether anything
    /// changed.
    #[must_use]
    pub fn normalized(self) -> (Self, bool) {
        let warn_percent = self.warn_percent.clamp(1, 100);
        let auto_percent = self.auto_percent.clamp(warn_percent, 100);
        let fixed = Self {
            mode: self.mode,
            warn_percent,
            auto_percent,
        };
        (fixed, fixed != self)
    }
    /// Tokens at `auto_percent` of `window` (or [`FALLBACK_AUTO_TOKENS`]
    /// when the window is unknown or zero).
    #[must_use]
    pub fn auto_tokens(&self, window: Option<u64>) -> u64 {
        match window.filter(|w| *w > 0) {
            Some(window) => window.saturating_mul(u64::from(self.auto_percent)) / 100,
            None => FALLBACK_AUTO_TOKENS,
        }
    }
    /// The native auto-compaction trigger for a session whose model has
    /// `window` tokens of context: [`CompactionTrigger::Inherit`] in native
    /// mode, [`CompactionTrigger::Disable`] in warn mode, [`Self::auto_tokens`]
    /// in auto mode.
    #[must_use]
    pub fn trigger(&self, window: Option<u64>) -> CompactionTrigger {
        match self.mode {
            CompactMode::Native => CompactionTrigger::Inherit,
            CompactMode::Warn => CompactionTrigger::Disable,
            CompactMode::Auto => CompactionTrigger::At(
                NonZeroU64::new(self.auto_tokens(window)).unwrap_or(NonZeroU64::MIN),
            ),
        }
    }
}

/// The native auto-compaction trigger a session is built with
/// (`SessionBuildOptions::auto_compact_threshold_override`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompactionTrigger {
    /// Upstream's model-aware default threshold.
    #[default]
    Inherit,
    /// No compaction by size. Upstream has no typed "off", so this lowers to
    /// the largest threshold ([`NonZeroU64::MAX`], never reached); upstream
    /// still compacts to recover a request the provider refused as too
    /// large. Recorded as an upstream ask in `docs/evidence.md`.
    Disable,
    /// Compact before the first model call whose context reaches this many
    /// tokens.
    At(NonZeroU64),
}

impl CompactionTrigger {
    /// The build's `auto_compact_threshold_override`.
    #[must_use]
    pub fn threshold_override(self) -> Option<NonZeroU64> {
        match self {
            Self::Inherit => None,
            Self::Disable => Some(NonZeroU64::MAX),
            Self::At(tokens) => Some(tokens),
        }
    }

    /// The trigger a build's override stands for.
    #[must_use]
    #[cfg(test)]
    pub fn from_override(threshold: Option<NonZeroU64>) -> Self {
        match threshold {
            None => Self::Inherit,
            Some(NonZeroU64::MAX) => Self::Disable,
            Some(tokens) => Self::At(tokens),
        }
    }
}

/// Whether the checklist panel shows (`tui.toml` `[ui] checklist`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChecklistView {
    /// Hidden until Ctrl+T (the default).
    #[default]
    Hidden,
    /// Shown under the input whenever the session has tasks.
    Shown,
}

impl ChecklistView {
    /// The other view.
    #[must_use]
    pub fn toggled(self) -> Self {
        match self {
            Self::Hidden => Self::Shown,
            Self::Shown => Self::Hidden,
        }
    }

    /// Whether the panel shows.
    #[must_use]
    pub fn is_shown(self) -> bool {
        self == Self::Shown
    }
}

/// How helper headers start: folded to one line (the default) or showing the
/// helper's nested blocks (`tui.toml` `[ui] helpers`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HelperView {
    /// One header line per helper; open one to watch it.
    #[default]
    Folded,
    /// Every helper shows its nested blocks.
    Expanded,
}
