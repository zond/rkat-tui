//! The `tui.toml` schema: [`RulesFile`] and its sections, and the settings
//! they layer into (see [`super::layers`]).

use super::settings::{ChecklistView, CompactMode, CompactionPolicy, HelperView};
use crate::approval::mode::PermissionMode;
use crate::approval::rules::{Rule, RuleKind, RuleOrigin, RulesError};
use crate::bootstrap::confinement::ConfinementSection;
use crate::endpoints::ApiStyle;
use crate::thinking::ThinkingSetting;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

/// File name of every `tui.toml` layer.
pub const RULES_FILE_NAME: &str = "tui.toml";

/// Auto-mode settings after layering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoSettings {
    /// Classifier model override.
    pub classifier_model: Option<String>,
    /// Whether `cargo build|test|nextest|check|clippy` are Tier 1. Off by
    /// default: builds execute project code (`build.rs`, tests, proc macros).
    pub trust_project_builds: bool,
    /// What to do when the context fills up: warn (default) or run
    /// `/compact` automatically.
    pub compact: CompactMode,
    /// Context usage (percent of the window) at which to warn once.
    pub compact_warn_percent: u8,
    /// Context usage (percent of the window) at which to auto-compact, or to
    /// warn after every turn in warn mode. Never below `compact_warn_percent`
    /// after [`load_layered_with`](super::load_layered_with).
    pub compact_auto_percent: u8,
    /// Whether the stage-2 classifier may read project files (read-only,
    /// bounded). On by default; off makes stage 2 decide from the attached
    /// context only, so file contents never reach the classifier.
    pub classifier_tools: bool,
}

impl Default for AutoSettings {
    fn default() -> Self {
        let compaction = CompactionPolicy::default();
        Self {
            classifier_model: None,
            trust_project_builds: false,
            compact: compaction.mode,
            compact_warn_percent: compaction.warn_percent,
            compact_auto_percent: compaction.auto_percent,
            classifier_tools: true,
        }
    }
}

impl AutoSettings {
    /// The UI's context-compaction policy, with `mode` (from
    /// `--auto-compact`/`--no-auto-compact`) overriding [`Self::compact`].
    #[must_use]
    pub fn compaction_policy(&self, mode: Option<CompactMode>) -> CompactionPolicy {
        CompactionPolicy {
            mode: mode.unwrap_or(self.compact),
            warn_percent: self.compact_warn_percent,
            auto_percent: self.compact_auto_percent,
        }
        .normalized()
        .0
    }
}

/// `[auto]` section of a rules file. Unset fields inherit from lower layers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutoSection {
    /// `true`: start in auto mode, `false`: in manual mode (the older
    /// spelling of `mode`; `mode` wins when a file sets both).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// See [`Layered::mode`](super::Layered::mode): `"default"`, `"accept-edits"`, `"plan"` or
    /// `"auto"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<PermissionMode>,
    /// See [`AutoSettings::classifier_model`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classifier_model: Option<String>,
    /// See [`AutoSettings::trust_project_builds`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust_project_builds: Option<bool>,
    /// See [`AutoSettings::compact`] (`"warn"` or `"auto"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact: Option<CompactMode>,
    /// See [`AutoSettings::compact_warn_percent`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_warn_percent: Option<u8>,
    /// See [`AutoSettings::compact_auto_percent`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_auto_percent: Option<u8>,
    /// See [`AutoSettings::classifier_tools`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classifier_tools: Option<bool>,
}

/// How deep helpers may nest when none of the layers says.
pub const DEFAULT_HELPER_MAX_DEPTH: u8 = 1;

/// Helper settings after layering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HelperSettings {
    /// How many levels of helpers a session may have: at 1 (the default)
    /// the session may spawn helpers and its helpers may not; at 2 those
    /// helpers may spawn helpers of their own, and so on. At 0 every helper
    /// is refused.
    pub max_depth: u8,
}

impl Default for HelperSettings {
    fn default() -> Self {
        Self {
            max_depth: DEFAULT_HELPER_MAX_DEPTH,
        }
    }
}

/// `[helpers]` section of a rules file. Unset fields inherit from lower
/// layers; the project file may only lower them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelpersSection {
    /// See [`HelperSettings::max_depth`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_depth: Option<u8>,
}

/// `[shell]` section of a rules file: `!` shell mode. Unset fields inherit
/// from lower layers; the project file may only lower them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellSection {
    /// See [`ShellSettings::user_timeout_secs`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_timeout_secs: Option<u64>,
}

/// `!` shell-mode settings after layering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShellSettings {
    /// How long a command the user runs with `!` may take before it is
    /// killed, in seconds (at least 1).
    pub user_timeout_secs: u64,
}

impl Default for ShellSettings {
    fn default() -> Self {
        Self {
            user_timeout_secs: crate::user_shell::DEFAULT_USER_TIMEOUT_SECS,
        }
    }
}

/// `[ui]` section of a rules file: how the transcript looks. Unset fields
/// inherit from lower layers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiSection {
    /// See [`UiSettings::helpers`] (`"folded"` or `"expanded"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub helpers: Option<HelperView>,
    /// See [`UiSettings::bell`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bell: Option<bool>,
    /// See [`UiSettings::checklist`] (`"hidden"` or `"shown"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checklist: Option<ChecklistView>,
}

/// `[ui]` settings after layering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UiSettings {
    /// How helper (sub-agent) headers start: one folded line (default) or
    /// showing the helper's work.
    pub helpers: HelperView,
    /// Ring the terminal bell when a turn ends or an approval prompt opens
    /// while the terminal is not focused (default on).
    pub bell: bool,
    /// Whether the task checklist shows under the input (Ctrl+T toggles
    /// it and records the choice; default hidden).
    pub checklist: ChecklistView,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            helpers: HelperView::default(),
            bell: true,
            checklist: ChecklistView::default(),
        }
    }
}

impl UiSettings {
    pub(crate) fn apply(&mut self, section: &UiSection) {
        if let Some(helpers) = section.helpers {
            self.helpers = helpers;
        }
        if let Some(bell) = section.bell {
            self.bell = bell;
        }
        if let Some(checklist) = section.checklist {
            self.checklist = checklist;
        }
    }
}

/// One `[[allow]]` / `[[deny]]` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleEntry {
    /// Tool name or glob.
    pub tool: String,
    /// Optional subject glob.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arg: Option<String>,
    /// Optional exact subject (mutually exclusive with `arg`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub literal: Option<String>,
}

/// One `[endpoints.<name>]` table: an extra LLM endpoint (see
/// [`crate::endpoints`]). Unset fields inherit from lower layers.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointSection {
    /// `anthropic`, `openai` or `gemini` (validated when endpoints are built).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dialect: Option<String>,
    /// Base URL (e.g. `http://localhost:4000` for LiteLLM).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Environment variable holding the API key: the only way to supply
    /// one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// An inline API key. Parsed only so that start-up can refuse it with a
    /// typed error naming `api_key_env`
    /// ([`crate::endpoints::EndpointError::InlineApiKey`]); never used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Tie-break when several endpoints list the same bare model id
    /// (higher wins; default 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i64>,
    /// The OpenAI-dialect API the endpoint is spoken to with
    /// (`responses`, the default, or `chat_completions`); only for the
    /// `openai` dialect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_style: Option<ApiStyle>,
}

impl fmt::Debug for EndpointSection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EndpointSection")
            .field("dialect", &self.dialect)
            .field("base_url", &self.base_url)
            .field("api_key_env", &self.api_key_env)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("priority", &self.priority)
            .field("api_style", &self.api_style)
            .finish()
    }
}

impl EndpointSection {
    /// Overwrite every field `other` sets.
    pub fn merge_from(&mut self, other: &Self) {
        let Self {
            dialect,
            base_url,
            api_key_env,
            api_key,
            priority,
            api_style,
        } = other;
        if dialect.is_some() {
            self.dialect.clone_from(dialect);
        }
        if base_url.is_some() {
            self.base_url.clone_from(base_url);
        }
        if api_key_env.is_some() {
            self.api_key_env.clone_from(api_key_env);
        }
        if api_key.is_some() {
            self.api_key.clone_from(api_key);
        }
        if priority.is_some() {
            self.priority = *priority;
        }
        if api_style.is_some() {
            self.api_style = *api_style;
        }
    }
}

/// The `[pricing]` table: `fetch = false` (the community price list is
/// not fetched, as `--no-pricing-fetch`) beside the `"<model id>"` override
/// tables. It reads as the map of overrides ([`std::ops::Deref`]); a model
/// named `fetch` cannot be overridden.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PricingTable {
    /// Whether the community price list may be fetched (`None`: unset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetch: Option<bool>,
    /// The overrides by model id.
    #[serde(flatten)]
    pub models: BTreeMap<String, PricingSection>,
}

impl PricingTable {
    /// Whether nothing is set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fetch.is_none() && self.models.is_empty()
    }
}

impl std::ops::Deref for PricingTable {
    type Target = BTreeMap<String, PricingSection>;

    fn deref(&self) -> &Self::Target {
        &self.models
    }
}

impl std::ops::DerefMut for PricingTable {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.models
    }
}

impl<'a> IntoIterator for &'a PricingTable {
    type Item = (&'a String, &'a PricingSection);
    type IntoIter = std::collections::btree_map::Iter<'a, String, PricingSection>;

    fn into_iter(self) -> Self::IntoIter {
        self.models.iter()
    }
}

/// One `[pricing."<model id>"]` table: USD per million tokens for the cost
/// readout (see [`crate::pricing`]). Unset fields inherit from lower layers,
/// then from the fetched price lists.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PricingSection {
    /// Uncached input tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<f64>,
    /// Output tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<f64>,
    /// Input tokens read from the prompt cache (default: `input`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input: Option<f64>,
    /// Input tokens written to the prompt cache (default: `input`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
}

impl PricingSection {
    /// Overwrite every field `other` sets.
    pub fn merge_from(&mut self, other: &Self) {
        let Self {
            input,
            output,
            cached_input,
            cache_write,
        } = *other;
        self.input = input.or(self.input);
        self.output = output.or(self.output);
        self.cached_input = cached_input.or(self.cached_input);
        self.cache_write = cache_write.or(self.cache_write);
    }

    /// The first field that is not a finite, non-negative number, if any.
    #[must_use]
    pub fn invalid_field(&self) -> Option<&'static str> {
        [
            ("input", self.input),
            ("output", self.output),
            ("cached_input", self.cached_input),
            ("cache_write", self.cache_write),
        ]
        .into_iter()
        .find(|(_, value)| value.is_some_and(|v| !v.is_finite() || v < 0.0))
        .map(|(name, _)| name)
    }
}

/// An endpoint after layering, with the layer that first defined it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayeredEndpoint {
    /// Merged settings.
    pub section: EndpointSection,
    /// Layer that defined the endpoint (later layers may only refine it).
    pub origin: RuleOrigin,
}

/// On-disk rules file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RulesFile {
    /// Auto-mode settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto: Option<AutoSection>,
    /// Allow rules.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<RuleEntry>,
    /// Deny rules.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<RuleEntry>,
    /// Extra LLM endpoints by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub endpoints: BTreeMap<String, EndpointSection>,
    /// `[pricing]`: `fetch` and the price overrides by model id (USD per
    /// million tokens).
    #[serde(default, skip_serializing_if = "PricingTable::is_empty")]
    pub pricing: PricingTable,
    /// Anthropic thinking by model reference or id (user layers only).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub thinking: BTreeMap<String, ThinkingSetting>,
    /// Extra directories for the confined shell (user layers only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confinement: Option<ConfinementSection>,
    /// Helper (sub-agent) limits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub helpers: Option<HelpersSection>,
    /// `!` shell-mode settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<ShellSection>,
    /// Transcript display settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui: Option<UiSection>,
    /// The `rkat-tui-mob` daemon's settings (user layers only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon: Option<DaemonSection>,
}

/// `[daemon]` section of a rules file: how the `rkat-tui-mob` daemon of
/// the workspace presents itself. User layers only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonSection {
    /// The person every attached client decides approval requests as: the
    /// daemon binds the attach token to this principal (one per daemon).
    /// Default: the login name (`$USER`, else `$LOGNAME`, else `owner`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
}

impl RulesFile {
    /// Load a rules file; a missing file yields an empty one.
    ///
    /// # Errors
    ///
    /// Returns [`RulesError`] on I/O or parse failures.
    pub fn load(path: &Path) -> Result<Self, RulesError> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|source| RulesError::Parse {
                path: path.to_path_buf(),
                source,
            }),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(RulesError::Io {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// The file's rules, tagged with `origin`.
    ///
    /// # Errors
    ///
    /// [`RulesError`] when a pattern is invalid.
    pub fn rules(&self, origin: RuleOrigin) -> Result<Vec<Rule>, RulesError> {
        let allow = self
            .allow
            .iter()
            .map(|e| Rule::from_entry(RuleKind::Allow, e, origin));
        let deny = self
            .deny
            .iter()
            .map(|e| Rule::from_entry(RuleKind::Deny, e, origin));
        allow.chain(deny).collect()
    }
}
