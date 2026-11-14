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

/// Default `compact_prepare_at`: 70% of the context window, below the
/// engine's own threshold (80%) and `compact_auto_percent` (85%).
pub const DEFAULT_PREPARE_AT: PrepareAt = PrepareAt::Percent(70);

/// The window a percentage `compact_prepare_at` is taken of when the
/// model's is unknown.
pub const FALLBACK_PREPARE_WINDOW: u64 = 200_000;

/// The engine's own auto-compaction threshold (native mode) as a share of
/// the window: four fifths (`meerkat-core` `compact.rs`, "the model-aware
/// token default").
pub const NATIVE_THRESHOLD_PERCENT: u8 = 80;

/// The engine's own threshold when the window is unknown.
pub const NATIVE_FALLBACK_TOKENS: u64 = 100_000;

/// Where `[auto] compact_prepare` runs: a number of tokens, or a share of
/// the model's context window (`"70%"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrepareAt {
    /// This many tokens of context.
    Tokens(u64),
    /// This percentage (1..=100) of the window.
    Percent(u8),
}

impl PrepareAt {
    /// The threshold in tokens for a model of `window` tokens
    /// ([`FALLBACK_PREPARE_WINDOW`] when unknown).
    #[must_use]
    pub fn tokens(self, window: Option<u64>) -> u64 {
        match self {
            Self::Tokens(tokens) => tokens,
            Self::Percent(percent) => {
                window
                    .filter(|w| *w > 0)
                    .unwrap_or(FALLBACK_PREPARE_WINDOW)
                    .saturating_mul(u64::from(percent))
                    / 100
            }
        }
    }

    /// `300000`, `300k`, `1.2m` or `"70%"` (a string of digits reads as
    /// tokens; `_` separates digits).
    ///
    /// # Errors
    ///
    /// What is wrong with `text`, for a notice.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let bad = || {
            format!(
                "`{text}` is no threshold: give tokens (300000, 300k or 1.2m) or a share of the window (65%)"
            )
        };
        if let Some(percent) = text.strip_suffix('%') {
            return match percent.trim().parse::<u8>() {
                Ok(percent @ 1..=100) => Ok(Self::Percent(percent)),
                _ => Err(bad()),
            };
        }
        let number = text.replace('_', "").to_ascii_lowercase();
        let (number, scale) = if let Some(number) = number.strip_suffix('k') {
            (number, 1_000)
        } else if let Some(number) = number.strip_suffix('m') {
            (number, 1_000_000)
        } else {
            (number.as_str(), 1)
        };
        scaled(number, scale)
            .filter(|tokens| *tokens > 0)
            .map(Self::Tokens)
            .ok_or_else(bad)
    }
}

/// `number` (digits, a decimal fraction allowed) times `scale`, when it is
/// a whole number of tokens (`1.2` of a million is, `1.5` tokens is not).
fn scaled(number: &str, scale: u64) -> Option<u64> {
    let (whole, fraction) = match number.split_once('.') {
        Some((_, "")) => return None,
        Some((whole, fraction)) => (whole, fraction),
        None => (number, ""),
    };
    let digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
    if (whole.is_empty() && fraction.is_empty()) || !digits(whole) || !digits(fraction) {
        return None;
    }
    let mut value = if whole.is_empty() {
        0
    } else {
        whole.parse::<u64>().ok()?.checked_mul(scale)?
    };
    let mut unit = scale;
    for digit in fraction.bytes() {
        if !unit.is_multiple_of(10) {
            return None;
        }
        unit /= 10;
        value = value.checked_add(u64::from(digit - b'0') * unit)?;
    }
    Some(value)
}

impl std::fmt::Display for PrepareAt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tokens(tokens) => write!(f, "{tokens}"),
            Self::Percent(percent) => write!(f, "{percent}%"),
        }
    }
}

impl Serialize for PrepareAt {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Tokens(tokens) => serializer.serialize_u64(*tokens),
            Self::Percent(_) => serializer.serialize_str(&self.to_string()),
        }
    }
}

impl<'de> Deserialize<'de> for PrepareAt {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Tokens(u64),
            Text(String),
        }
        match Raw::deserialize(deserializer)? {
            Raw::Tokens(0) => Err(serde::de::Error::custom(
                "compact_prepare_at must be above 0",
            )),
            Raw::Tokens(tokens) => Ok(Self::Tokens(tokens)),
            Raw::Text(text) => Self::parse(&text).map_err(serde::de::Error::custom),
        }
    }
}

/// Which `tui.toml` layer the prepare instruction or its threshold in
/// force comes from (`/compact prepare` and `/compact status` say it).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PrepareSource {
    /// No layer sets it.
    #[default]
    Default,
    /// The user's own `tui.toml`.
    User,
    /// The realm's `tui.toml`, which `/compact prepare` writes.
    Workspace,
    /// `--set` on the command line.
    CommandLine,
    /// The project's `.rkat/tui.toml`.
    Project,
}

impl PrepareSource {
    /// The source of a value a `tui.toml` layer of `origin` set.
    #[must_use]
    pub fn of(origin: crate::approval::rules::RuleOrigin) -> Self {
        use crate::approval::rules::RuleOrigin;
        match origin {
            RuleOrigin::Global => Self::User,
            RuleOrigin::Realm | RuleOrigin::Session => Self::Workspace,
            RuleOrigin::Cli => Self::CommandLine,
            RuleOrigin::Project => Self::Project,
        }
    }

    /// Where the value comes from, as the notices end with it.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::User => "from your tui.toml",
            Self::Workspace => "from this workspace (/compact prepare)",
            Self::CommandLine => "from the command line (applies until the daemon restarts)",
            Self::Project => "from the project's .rkat/tui.toml",
        }
    }

    /// The layer by name (`your tui.toml has one`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Default => "the default",
            Self::User => "your tui.toml",
            Self::Workspace => "this workspace",
            Self::CommandLine => "the command line",
            Self::Project => "the project's .rkat/tui.toml",
        }
    }

    /// A layer over the workspace's (it wins again at the next start).
    #[must_use]
    pub fn over_workspace(self) -> bool {
        matches!(self, Self::CommandLine | Self::Project)
    }
}

/// A setting's value and the layer it comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sourced<T> {
    /// The value.
    pub value: T,
    /// Where it comes from.
    pub source: PrepareSource,
}

impl<T> Sourced<T> {
    /// `value` from `source`.
    pub fn new(value: T, source: PrepareSource) -> Self {
        Self { value, source }
    }
}

/// The prepare instruction and its threshold as the daemon starts with
/// them, and what the lower layers give (`/compact prepare default`
/// goes back to those, live).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrepareSetting {
    /// The instruction in force (`None` from a layer: off there; from
    /// [`PrepareSource::Default`]: none is set).
    pub instruction: Sourced<Option<String>>,
    /// Its threshold in force.
    pub at: Sourced<PrepareAt>,
    /// The instruction without the realm file's `compact_prepare`.
    pub default_instruction: Sourced<Option<String>>,
    /// The threshold without the realm file's `compact_prepare_at`.
    pub default_at: Sourced<PrepareAt>,
}

impl Default for PrepareSetting {
    fn default() -> Self {
        Self {
            instruction: Sourced::new(None, PrepareSource::Default),
            at: Sourced::new(DEFAULT_PREPARE_AT, PrepareSource::Default),
            default_instruction: Sourced::new(None, PrepareSource::Default),
            default_at: Sourced::new(DEFAULT_PREPARE_AT, PrepareSource::Default),
        }
    }
}

/// An instruction as a layer wrote it: trimmed, an empty one is off.
#[must_use]
pub fn prepare_instruction(text: Option<&str>) -> Option<String> {
    text.map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// What `/compact prepare at …` does to the threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThresholdChange {
    /// Set it for the workspace (written to the realm's `tui.toml`).
    To(PrepareAt),
    /// Remove the workspace's (`/compact prepare at default`): the lower
    /// layers' applies.
    Default,
}

/// What `/compact prepare <text>|off|default` does to the instruction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstructionChange {
    /// Set (or replace) it for the workspace.
    To(String),
    /// None in this workspace, whatever the lower layers say (written as
    /// an empty `compact_prepare`).
    Off,
    /// Remove the workspace's: the lower layers' applies.
    Default,
}

/// One `/compact prepare` change: the threshold first, then the
/// instruction; `None` leaves that part as it is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrepareChange {
    /// The threshold's change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<ThresholdChange>,
    /// The instruction's change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<InstructionChange>,
}

impl CompactionPolicy {
    /// The context at which the engine compacts on its own: its own
    /// threshold in native mode, `compact_auto_percent` in auto mode,
    /// none in warn mode.
    #[must_use]
    pub fn backstop_tokens(&self, window: Option<u64>) -> Option<u64> {
        match self.mode {
            CompactMode::Native => Some(
                window
                    .filter(|w| *w > 0)
                    .map_or(NATIVE_FALLBACK_TOKENS, |window| {
                        window.saturating_mul(u64::from(NATIVE_THRESHOLD_PERCENT)) / 100
                    }),
            ),
            CompactMode::Auto => Some(self.auto_tokens(window)),
            CompactMode::Warn => None,
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
