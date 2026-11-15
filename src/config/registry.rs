//! Every scalar `tui.toml` setting, declared once.
//!
//! A [`Setting`] names its dotted key, the kind of value it takes (and so
//! how a value is parsed, checked and printed), its default, what a
//! project's `.rkat/tui.toml` may do with it ([`ProjectRule`]), when a
//! change takes effect ([`Applies`]), a one-line description, and the
//! dedicated command-line flags and friendly commands that are shorthands
//! for it, and what the daemon does when it changes ([`Hook`]).
//! [`SETTINGS`] is the table; everything that handles a setting by its key
//! reads it: the layering of the typed view ([`super::load_layered_with`]
//! resolves every setting with [`Setting::resolve`] into one table,
//! [`merged`], and the typed settings deserialize from it), `--set` and the flags
//! ([`crate::overlay`]), and the manual's settings table (generated from
//! it, a test keeps `docs/manual.md` in step).
//!
//! The structured sections (rules, endpoints, prices, thinking, the
//! confinement grants and the `[auto] allow` statements) are not scalar
//! settings: [`SECTIONS`] lists them with a pointer to their own commands.
//!
//! Precedence: default < user (`~/.rkat/tui/tui.toml`) < workspace (the
//! realm's `tui.toml`) < command line; a project file's value applies as
//! its [`ProjectRule`] says (where it may set the key, the command line
//! still wins; where it may only lower a value, its lowering applies over
//! every layer).

use std::fmt;

use serde::{Deserialize, Serialize};
use toml::{Table, Value};

use super::settings::{DEFAULT_AUTO_PERCENT, DEFAULT_PREPARE_AT, DEFAULT_WARN_PERCENT, PrepareAt};
use crate::cli::Args;

/// Where a setting's value comes from: the layers in precedence order
/// (the project's as its [`ProjectRule`] says).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    /// No layer sets it.
    #[default]
    Default,
    /// The user's own `~/.rkat/tui/tui.toml`.
    User,
    /// The workspace's realm `tui.toml`.
    Workspace,
    /// `--set` or a dedicated flag on the daemon's command line.
    CommandLine,
    /// The project's `.rkat/tui.toml`.
    Project,
}

impl Level {
    /// The layers a file or the command line holds, lowest first.
    pub const LAYERS: [Self; 4] = [
        Self::User,
        Self::Workspace,
        Self::CommandLine,
        Self::Project,
    ];

    /// Where a value in force comes from, as the notices say it: `the
    /// default`, `from your tui.toml`.
    #[must_use]
    pub fn source_words(self) -> String {
        match self {
            Self::Default => "the default".to_string(),
            level => format!("from {}", level.name()),
        }
    }

    /// The file of the level, in words (`this workspace's tui.toml`).
    #[must_use]
    pub fn file_words(self) -> &'static str {
        match self {
            Self::Default => "no file",
            Self::User => "your tui.toml",
            Self::Workspace => "this workspace's tui.toml",
            Self::CommandLine => "the command line",
            Self::Project => "the project's .rkat/tui.toml",
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

    /// A short label (`/settings` columns).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::User => "user",
            Self::Workspace => "workspace",
            Self::CommandLine => "command line",
            Self::Project => "project",
        }
    }
}

/// What a project's `.rkat/tui.toml` (which a cloned repository writes)
/// may do with a setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectRule {
    /// It may set it; the command line still wins.
    Takes,
    /// It may not set it (ignored, with a notice where one exists).
    Never,
    /// It may only lower it: its value applies when it is not above the
    /// one in force, over every other layer.
    OnlyLower,
    /// It may only set `false`, which applies over every other layer.
    OnlyFalse,
    /// The permission mode: it may not pick one; its `enabled = false`
    /// turns auto mode into manual unless the command line set the mode.
    OnlyManual,
}

impl ProjectRule {
    /// The rule in words (the manual's table).
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::Takes => "may set it (the command line wins)",
            Self::Never => "may not set it",
            Self::OnlyLower => "may only lower it",
            Self::OnlyFalse => "may only set `false`",
            Self::OnlyManual => "may only turn auto mode off (`enabled = false`)",
        }
    }
}

/// When a change of a setting takes effect in a running daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Applies {
    /// At once.
    Live,
    /// For the conversations started from then on.
    NextConversation,
    /// When the daemon (or, for a client-side setting, the TUI) starts
    /// next.
    NextStart,
}

impl Applies {
    /// In words (the manual's table, the confirmations).
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::Live => "at once",
            Self::NextConversation => "from the next conversation",
            Self::NextStart => "at the next start",
        }
    }
}

/// The kind of value a setting takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `true` or `false`.
    Bool,
    /// A whole number in `min..=max`. With `clamp`, a file's value below
    /// `min` is read as `min` (what the layering always did).
    Int {
        /// The least value.
        min: u64,
        /// The greatest value.
        max: u64,
        /// A file's value below `min` reads as `min`.
        clamp: bool,
    },
    /// One of these words.
    Choice(&'static [&'static str]),
    /// Tokens (`300000`, `300k`, `1.2m`) or a share of the window
    /// (`"70%"`): [`PrepareAt`].
    Threshold,
    /// Free text of at most this many characters (empty allowed).
    Text {
        /// The most characters.
        max: usize,
    },
    /// A model reference, `[endpoint:]model`.
    Model,
}

impl Kind {
    /// What the kind takes, in words (refusals, the manual).
    #[must_use]
    pub fn describe(self) -> String {
        match self {
            Self::Bool => "`true` or `false`".to_string(),
            Self::Int { min, max, .. } if max == u64::MAX => format!("a whole number from {min}"),
            Self::Int { min, max, .. } => format!("a whole number from {min} to {max}"),
            Self::Choice(words) => words
                .iter()
                .map(|word| format!("`{word}`"))
                .collect::<Vec<_>>()
                .join(", "),
            Self::Threshold => {
                "tokens (`300000`, `300k`, `1.2m`) or a share of the window (`\"65%\"`)".to_string()
            }
            Self::Text { max } => format!("text (at most {max} characters)"),
            Self::Model => "a model, `[endpoint:]model`".to_string(),
        }
    }
}

/// A setting's value when no layer sets it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fallback {
    /// No value; the text says what that means.
    Unset(&'static str),
    /// This boolean.
    Bool(bool),
    /// This number.
    Int(u64),
    /// This word or text.
    Text(&'static str),
    /// This threshold (a [`Kind::Threshold`]'s).
    Threshold(PrepareAt),
}

impl Fallback {
    /// The default as a value (`None`: unset).
    #[must_use]
    pub fn value(self) -> Option<Value> {
        match self {
            Self::Unset(_) => None,
            Self::Bool(value) => Some(Value::Boolean(value)),
            Self::Int(value) => Some(Value::Integer(i64::try_from(value).unwrap_or(i64::MAX))),
            Self::Text(text) => Some(Value::String(text.to_string())),
            Self::Threshold(at) => Some(threshold_value(at)),
        }
    }

    /// The default in words.
    #[must_use]
    pub fn describe(self) -> String {
        match self {
            Self::Unset(what) => what.to_string(),
            other => other.value().map_or_else(String::new, |value| show(&value)),
        }
    }
}

/// What a dedicated flag gives its setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagValue {
    /// This TOML value (`false`, `"auto"`).
    Fixed(&'static str),
    /// The flag's argument, as a string.
    Argument,
}

/// A dedicated command-line flag that is a shorthand for a setting.
#[derive(Debug, Clone, Copy)]
pub struct Flag {
    /// The flag (`--no-auto`).
    pub flag: &'static str,
    /// The key it writes in the command-line layer (the setting's own, or
    /// an older spelling the setting reads, as `--no-auto`'s
    /// `auto.enabled`).
    pub key: &'static str,
    /// What it writes.
    pub value: FlagValue,
    /// Whether it was given, with its argument.
    pub given: fn(&Args) -> Option<String>,
}

impl fmt::Debug for Setting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Setting").field("key", &self.key).finish()
    }
}

/// An older spelling a setting also reads, with how its value maps.
#[derive(Debug, Clone, Copy)]
pub struct Alias {
    /// Its dotted key.
    pub key: &'static str,
    /// Its value as the setting's (`None`: not one).
    pub map: fn(&Value) -> Option<Value>,
}

/// One scalar setting.
pub struct Setting {
    /// The dotted key (`auto.compact_prepare_at`).
    pub key: &'static str,
    /// The value it takes.
    pub kind: Kind,
    /// Its value when no layer sets it.
    pub default: Fallback,
    /// What a project file may do with it.
    pub project: ProjectRule,
    /// When a change takes effect.
    pub applies: Applies,
    /// One line on what it does.
    pub about: &'static str,
    /// The dedicated flags that are shorthands for it.
    pub flags: &'static [Flag],
    /// The friendly command that sets it, when there is one (Markdown:
    /// commands in backticks).
    pub command: Option<&'static str>,
    /// An older spelling it also reads (the same file's own key wins).
    pub alias: Option<Alias>,
    /// What the daemon does when its value in force changes.
    pub hook: Hook,
}

/// What the daemon does when a setting's value in force changes while it
/// runs (`/set`, a shorthand, a file change); `daemon::local`'s
/// `apply_setting` matches it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hook {
    /// Nothing: the setting is read where it applies (at the next start,
    /// or the next conversation).
    None,
    /// The permission mode switches.
    Mode,
    /// The classifier reviews with the model from then on.
    ClassifierModel,
    /// The gate takes the new `trust_project_builds`.
    TrustBuilds,
    /// The prepare instruction and its threshold are taken again.
    Prepare,
    /// Every client follows a `[ui]` setting.
    Ui,
}

/// Every scalar setting in force over `layers` as one table, each value at
/// its dotted key (a default included; an unset setting without one
/// absent): what the typed view deserializes from
/// ([`super::load_layered_with`]).
#[must_use]
pub fn merged(layers: &Layers) -> Table {
    let mut table = Table::new();
    for setting in SETTINGS {
        if let Some(value) = setting.resolve(layers).value {
            put(&mut table, setting.key, Some(value));
        }
    }
    table
}

/// A setting's value in force and where it comes from.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    /// The value (`None`: no layer sets it and it has no default).
    pub value: Option<Value>,
    /// The layer it comes from.
    pub source: Level,
}

/// The raw tables of every layer (an absent file is an empty table).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Layers {
    /// `~/.rkat/tui/tui.toml`.
    pub user: Table,
    /// The realm's `tui.toml`.
    pub workspace: Table,
    /// The command-line overlay.
    pub cli: Table,
    /// The project's `.rkat/tui.toml`.
    pub project: Table,
}

impl Layers {
    /// The table of `level` (the default's is empty).
    #[must_use]
    pub fn of(&self, level: Level) -> Option<&Table> {
        match level {
            Level::Default => None,
            Level::User => Some(&self.user),
            Level::Workspace => Some(&self.workspace),
            Level::CommandLine => Some(&self.cli),
            Level::Project => Some(&self.project),
        }
    }

    /// The table of `level`, to replace (none for the default).
    pub fn of_mut(&mut self, level: Level) -> Option<&mut Table> {
        match level {
            Level::Default => None,
            Level::User => Some(&mut self.user),
            Level::Workspace => Some(&mut self.workspace),
            Level::CommandLine => Some(&mut self.cli),
            Level::Project => Some(&mut self.project),
        }
    }
}

/// The value at the dotted `key` of `table`.
#[must_use]
pub fn lookup<'a>(table: &'a Table, key: &str) -> Option<&'a Value> {
    let mut parts = key.split('.');
    let mut value = table.get(parts.next()?)?;
    for part in parts {
        value = value.as_table()?.get(part)?;
    }
    Some(value)
}

/// Set the dotted `key` of `table` to `value` (`None`: remove it), the
/// tables on the way created as needed.
pub fn put(table: &mut Table, key: &str, value: Option<Value>) {
    let (parents, last) = match key.rsplit_once('.') {
        Some((parents, last)) => (Some(parents), last),
        None => (None, key),
    };
    let mut at = table;
    for part in parents.into_iter().flat_map(|parents| parents.split('.')) {
        let entry = at
            .entry(part.to_string())
            .or_insert_with(|| Value::Table(Table::new()));
        if !entry.is_table() {
            if value.is_none() {
                return;
            }
            *entry = Value::Table(Table::new());
        }
        let Some(next) = entry.as_table_mut() else {
            return;
        };
        at = next;
    }
    match value {
        Some(value) => {
            at.insert(last.to_string(), value);
        }
        None => {
            at.remove(last);
        }
    }
}

/// The dotted keys of every scalar setting and the older spellings they
/// read: what is not a structured section.
fn scalar_keys() -> impl Iterator<Item = &'static str> {
    SETTINGS.iter().flat_map(|setting| {
        std::iter::once(setting.key).chain(setting.alias.map(|alias| alias.key))
    })
}

/// `table`'s structured sections (rules, statements, grants, endpoints,
/// prices, thinking): everything but the scalar settings, empty tables
/// left out.
#[must_use]
pub fn sections_of(table: &Table) -> Table {
    let mut rest = table.clone();
    for key in scalar_keys() {
        put(&mut rest, key, None);
    }
    prune(&mut rest);
    rest
}

/// `sections` (a file's structured sections) with `scalars`' scalar
/// settings (as held) in place of the file's.
#[must_use]
pub fn with_scalars(sections: &Table, scalars: &Table) -> Table {
    let mut table = sections_of(sections);
    for key in scalar_keys() {
        if let Some(value) = lookup(scalars, key) {
            put(&mut table, key, Some(value.clone()));
        }
    }
    table
}

/// Remove the empty tables of `table`, deepest first.
fn prune(table: &mut Table) {
    table.retain(|_, value| {
        if let Value::Table(inner) = value {
            prune(inner);
            !inner.is_empty()
        } else {
            true
        }
    });
}

/// `value` as the setting files and commands show it (`true`, `65%`,
/// `auto`, `"update PLAN.md"` for text with spaces).
#[must_use]
pub fn show(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

impl Setting {
    /// The setting `key` names.
    #[must_use]
    pub fn find(key: &str) -> Option<&'static Self> {
        SETTINGS.iter().find(|setting| setting.key == key)
    }

    /// The value `level`'s table holds, read as the layering reads a file
    /// (its own key, else its alias; a clamped number brought to its
    /// least).
    #[must_use]
    pub fn at(&self, layers: &Layers, level: Level) -> Option<Value> {
        let table = layers.of(level)?;
        let value = match lookup(table, self.key) {
            // The project's mode is never read (only `enabled = false`).
            Some(_) if self.project == ProjectRule::OnlyManual && level == Level::Project => None,
            Some(value) => Some(value.clone()),
            None => None,
        };
        let value = value.or_else(|| {
            let alias = self.alias?;
            lookup(table, alias.key).and_then(alias.map)
        })?;
        Some(match (self.kind, value) {
            (
                Kind::Int {
                    min, clamp: true, ..
                },
                Value::Integer(n),
            ) if u64::try_from(n).map_or(true, |n| n < min) => {
                Value::Integer(i64::try_from(min).unwrap_or(i64::MAX))
            }
            (_, value) => value,
        })
    }

    /// The value in force over `layers`, and its layer.
    #[must_use]
    pub fn resolve(&self, layers: &Layers) -> Resolved {
        self.resolve_without(layers, None)
    }

    /// The value in force over `layers` with `skip`'s value left out
    /// (what removing it would leave).
    #[must_use]
    pub fn resolve_without(&self, layers: &Layers, skip: Option<Level>) -> Resolved {
        let read = |level: Level| {
            if Some(level) == skip {
                None
            } else {
                self.at(layers, level)
            }
        };
        let mut resolved = Resolved {
            value: self.default.value(),
            source: Level::Default,
        };
        for level in [Level::User, Level::Workspace, Level::CommandLine] {
            if let Some(value) = read(level) {
                resolved = Resolved {
                    value: Some(value),
                    source: level,
                };
            }
        }
        let Some(project) = read(Level::Project) else {
            return resolved;
        };
        let cli_set = read(Level::CommandLine).is_some();
        let applies = match self.project {
            ProjectRule::Takes => !cli_set,
            ProjectRule::Never => false,
            ProjectRule::OnlyLower => match (&resolved.value, &project) {
                (Some(Value::Integer(now)), Value::Integer(lower)) => lower <= now,
                (None, _) => true,
                _ => false,
            },
            ProjectRule::OnlyFalse => project == Value::Boolean(false),
            ProjectRule::OnlyManual => {
                !cli_set
                    && project.as_str() == Some("default")
                    && resolved.value.as_ref().and_then(Value::as_str) == Some("auto")
            }
        };
        if applies {
            resolved = Resolved {
                value: Some(project),
                source: Level::Project,
            };
        }
        resolved
    }

    /// A value given on the command line or in a command, checked: the
    /// value as the file holds it, or what is wrong with it.
    ///
    /// # Errors
    ///
    /// Why the value is not one this setting takes, naming the key.
    pub fn check(&self, value: &Value) -> Result<Value, String> {
        let bad = || format!("{} takes {}", self.key, self.kind.describe());
        match (self.kind, value) {
            (Kind::Bool, Value::Boolean(_)) => Ok(value.clone()),
            (Kind::Int { min, max, .. }, Value::Integer(n)) => match u64::try_from(*n) {
                Ok(n) if (min..=max).contains(&n) => Ok(value.clone()),
                _ => Err(bad()),
            },
            (Kind::Choice(words), Value::String(word)) if words.contains(&word.as_str()) => {
                Ok(value.clone())
            }
            (Kind::Threshold, Value::Integer(n)) if *n > 0 => Ok(value.clone()),
            (Kind::Threshold, Value::String(text)) => match PrepareAt::parse(text) {
                Ok(at) => Ok(threshold_value(at)),
                Err(why) => Err(format!("{}: {why}", self.key)),
            },
            (Kind::Text { max }, Value::String(text)) if text.chars().count() <= max => {
                Ok(value.clone())
            }
            (Kind::Model, Value::String(model))
                if !model.trim().is_empty() && !model.contains(char::is_whitespace) =>
            {
                Ok(value.clone())
            }
            _ => Err(bad()),
        }
    }

    /// `text` as typed in a command (`/set`): read as this setting's kind
    /// and checked.
    ///
    /// # Errors
    ///
    /// As [`Self::check`].
    pub fn parse(&self, text: &str) -> Result<Value, String> {
        let text = text.trim();
        let value = match self.kind {
            Kind::Bool => match text.to_ascii_lowercase().as_str() {
                "true" | "on" | "yes" => Value::Boolean(true),
                "false" | "off" | "no" => Value::Boolean(false),
                _ => return Err(format!("{} takes {}", self.key, self.kind.describe())),
            },
            Kind::Int { .. } => match text.replace('_', "").parse::<i64>() {
                Ok(n) => Value::Integer(n),
                Err(_) => return Err(format!("{} takes {}", self.key, self.kind.describe())),
            },
            Kind::Threshold => {
                return PrepareAt::parse(text)
                    .map(threshold_value)
                    .map_err(|why| format!("{}: {why}", self.key));
            }
            Kind::Text { .. } => Value::String(unquote(text).to_string()),
            Kind::Choice(_) | Kind::Model => Value::String(unquote(text).to_string()),
        };
        self.check(&value)
    }
}

/// `"text"` without its quotes (a typed value may be quoted).
fn unquote(text: &str) -> &str {
    text.strip_prefix('"')
        .and_then(|text| text.strip_suffix('"'))
        .unwrap_or(text)
}

/// A threshold as the file holds it: tokens as a number, a share as
/// `"N%"`.
fn threshold_value(at: PrepareAt) -> Value {
    match at {
        PrepareAt::Tokens(tokens) => Value::Integer(i64::try_from(tokens).unwrap_or(i64::MAX)),
        PrepareAt::Percent(_) => Value::String(at.to_string()),
    }
}

/// `auto.enabled`'s value as a mode.
fn enabled_as_mode(value: &Value) -> Option<Value> {
    value
        .as_bool()
        .map(|on| Value::String(if on { "auto" } else { "default" }.to_string()))
}

const MODES: &[&str] = &["default", "accept-edits", "plan", "auto"];
const COMPACT_MODES: &[&str] = &["native", "warn", "auto"];
/// The most characters `/set` takes for an instruction or a name.
const TEXT_MAX: usize = 2_000;

/// The permission mode at start (`rkat/set_mode` writes it).
pub const MODE: Setting = Setting {
    key: "auto.mode",
    kind: Kind::Choice(MODES),
    default: Fallback::Text("auto"),
    project: ProjectRule::OnlyManual,
    applies: Applies::Live,
    about: "The permission mode at start (`enabled = false` is the older `mode = \"default\"`).",
    flags: &[Flag {
        flag: "--no-auto",
        key: "auto.enabled",
        value: FlagValue::Fixed("false"),
        given: |args| args.no_auto.then(String::new),
    }],
    command: Some("`/mode`, Shift+Tab"),
    alias: Some(Alias {
        key: "auto.enabled",
        map: enabled_as_mode,
    }),
    hook: Hook::Mode,
};

/// The instruction the agents are asked to prepare with
/// ([`super::settings::PrepareSetting`] reads it).
pub const PREPARE: Setting = Setting {
    key: "auto.compact_prepare",
    kind: Kind::Text { max: TEXT_MAX },
    default: Fallback::Unset("none"),
    project: ProjectRule::Takes,
    applies: Applies::Live,
    about: "What every agent is asked to do before an automatic compaction (empty: none here).",
    flags: &[],
    command: Some("`/compact prepare <text>|off|default`"),
    alias: None,
    hook: Hook::Prepare,
};

/// Where the agents are asked to prepare
/// ([`super::settings::PrepareSetting`] reads it).
pub const PREPARE_AT: Setting = Setting {
    key: "auto.compact_prepare_at",
    kind: Kind::Threshold,
    default: Fallback::Threshold(DEFAULT_PREPARE_AT),
    project: ProjectRule::Takes,
    applies: Applies::Live,
    about: "Where the agents are asked to prepare: tokens, or a share of the window.",
    flags: &[],
    command: Some("`/compact prepare at <threshold>|default`"),
    alias: None,
    hook: Hook::Prepare,
};

/// Every scalar setting, in the order the manual and `/settings` list
/// them.
pub static SETTINGS: &[Setting] = &[
    MODE,
    Setting {
        key: "auto.classifier_model",
        kind: Kind::Model,
        default: Fallback::Unset("the start-up endpoint's per-dialect default"),
        project: ProjectRule::Never,
        applies: Applies::Live,
        about: "The model that reviews the agent's actions in auto mode.",
        flags: &[Flag {
            flag: "--classifier-model",
            key: "auto.classifier_model",
            value: FlagValue::Argument,
            given: |args| args.classifier_model.clone(),
        }],
        command: Some("`/auto model`"),
        alias: None,
        hook: Hook::ClassifierModel,
    },
    Setting {
        key: "auto.trust_project_builds",
        kind: Kind::Bool,
        default: Fallback::Unset(
            "unset: trusted while a sandbox contains the shell; uncontained, a build after a dependency change asks",
        ),
        project: ProjectRule::OnlyFalse,
        applies: Applies::Live,
        about: "Whether `cargo build|test|nextest|check|clippy` are tier 1.",
        flags: &[
            Flag {
                flag: "--trust-project-builds",
                key: "auto.trust_project_builds",
                value: FlagValue::Fixed("true"),
                given: |args| args.trust_project_builds.then(String::new),
            },
            Flag {
                flag: "--no-trust-project-builds",
                key: "auto.trust_project_builds",
                value: FlagValue::Fixed("false"),
                given: |args| args.no_trust_project_builds.then(String::new),
            },
        ],
        command: None,
        alias: None,
        hook: Hook::TrustBuilds,
    },
    Setting {
        key: "auto.classifier_tools",
        kind: Kind::Bool,
        default: Fallback::Bool(true),
        project: ProjectRule::OnlyFalse,
        applies: Applies::NextStart,
        about: "Whether the classifier's second stage may read project files.",
        flags: &[],
        command: None,
        alias: None,
        hook: Hook::None,
    },
    Setting {
        key: "auto.classifier_probe",
        kind: Kind::Bool,
        default: Fallback::Bool(true),
        project: ProjectRule::Takes,
        applies: Applies::NextStart,
        about: "Whether the daemon checks the classifier model at start with two fixed cases.",
        flags: &[],
        command: None,
        alias: None,
        hook: Hook::None,
    },
    Setting {
        key: "auto.compact",
        kind: Kind::Choice(COMPACT_MODES),
        default: Fallback::Text("native"),
        project: ProjectRule::Takes,
        applies: Applies::NextStart,
        about: "When the context fills up: the engine's own threshold, only warn, or compact at `compact_auto_percent`.",
        flags: &[
            Flag {
                flag: "--auto-compact",
                key: "auto.compact",
                value: FlagValue::Fixed("\"auto\""),
                given: |args| args.auto_compact.then(String::new),
            },
            Flag {
                flag: "--no-auto-compact",
                key: "auto.compact",
                value: FlagValue::Fixed("\"warn\""),
                given: |args| args.no_auto_compact.then(String::new),
            },
        ],
        command: None,
        alias: None,
        hook: Hook::None,
    },
    Setting {
        key: "auto.compact_warn_percent",
        kind: Kind::Int {
            min: 1,
            max: 100,
            clamp: false,
        },
        default: Fallback::Int(DEFAULT_WARN_PERCENT as u64),
        project: ProjectRule::Takes,
        applies: Applies::NextStart,
        about: "Context usage (percent of the window) at which to warn once.",
        flags: &[],
        command: None,
        alias: None,
        hook: Hook::None,
    },
    Setting {
        key: "auto.compact_auto_percent",
        kind: Kind::Int {
            min: 1,
            max: 100,
            clamp: false,
        },
        default: Fallback::Int(DEFAULT_AUTO_PERCENT as u64),
        project: ProjectRule::Takes,
        applies: Applies::NextStart,
        about: "Context usage (percent of the window) at which to compact (`compact = \"auto\"`) or warn every turn.",
        flags: &[],
        command: None,
        alias: None,
        hook: Hook::None,
    },
    PREPARE,
    PREPARE_AT,
    Setting {
        key: "helpers.max_depth",
        kind: Kind::Int {
            min: 0,
            max: 255,
            clamp: false,
        },
        default: Fallback::Int(super::DEFAULT_HELPER_MAX_DEPTH as u64),
        project: ProjectRule::OnlyLower,
        applies: Applies::NextStart,
        about: "How many levels of helpers a session may have (0: none).",
        flags: &[],
        command: None,
        alias: None,
        hook: Hook::None,
    },
    Setting {
        key: "shell.user_timeout_secs",
        kind: Kind::Int {
            min: 1,
            max: u64::MAX,
            clamp: true,
        },
        default: Fallback::Int(crate::user_shell::DEFAULT_USER_TIMEOUT_SECS),
        project: ProjectRule::OnlyLower,
        applies: Applies::NextStart,
        about: "How long a `!` command may run, in seconds.",
        flags: &[],
        command: None,
        alias: None,
        hook: Hook::None,
    },
    Setting {
        key: "search.budget_secs",
        kind: Kind::Int {
            min: 1,
            max: u64::MAX,
            clamp: true,
        },
        default: Fallback::Int(crate::tools::DEFAULT_SEARCH_BUDGET_SECS),
        project: ProjectRule::OnlyLower,
        applies: Applies::NextStart,
        about: "How long one `glob` or `grep` may run before it answers with what it found, in seconds.",
        flags: &[],
        command: None,
        alias: None,
        hook: Hook::None,
    },
    Setting {
        key: "search.max_entries",
        kind: Kind::Int {
            min: 1,
            max: u64::MAX,
            clamp: true,
        },
        default: Fallback::Int(crate::tools::DEFAULT_SEARCH_MAX_ENTRIES),
        project: ProjectRule::OnlyLower,
        applies: Applies::NextStart,
        about: "How many files and directories one `glob` or `grep` may examine.",
        flags: &[],
        command: None,
        alias: None,
        hook: Hook::None,
    },
    Setting {
        key: "ui.helpers",
        kind: Kind::Choice(&["folded", "expanded"]),
        default: Fallback::Text("folded"),
        project: ProjectRule::Takes,
        applies: Applies::Live,
        about: "How helper headers start in the transcript.",
        flags: &[],
        command: None,
        alias: None,
        hook: Hook::Ui,
    },
    Setting {
        key: "ui.bell",
        kind: Kind::Bool,
        default: Fallback::Bool(true),
        project: ProjectRule::Takes,
        applies: Applies::Live,
        about: "Ring the bell when a turn ends or a prompt opens while the terminal is not focused.",
        flags: &[],
        command: None,
        alias: None,
        hook: Hook::Ui,
    },
    Setting {
        key: "ui.checklist",
        kind: Kind::Choice(&["hidden", "shown"]),
        default: Fallback::Text("hidden"),
        project: ProjectRule::Takes,
        applies: Applies::Live,
        about: "Whether the task checklist shows under the input.",
        flags: &[],
        command: Some("Ctrl+T"),
        alias: None,
        hook: Hook::Ui,
    },
    Setting {
        key: "pricing.fetch",
        kind: Kind::Bool,
        default: Fallback::Bool(true),
        project: ProjectRule::OnlyFalse,
        applies: Applies::NextStart,
        about: "Whether the community price list is fetched for the cost readout.",
        flags: &[Flag {
            flag: "--no-pricing-fetch",
            key: "pricing.fetch",
            value: FlagValue::Fixed("false"),
            given: |args| args.no_pricing_fetch.then(String::new),
        }],
        command: None,
        alias: None,
        hook: Hook::None,
    },
    Setting {
        key: "daemon.principal",
        kind: Kind::Text { max: 200 },
        default: Fallback::Unset("the login name ($USER, else $LOGNAME, else owner)"),
        project: ProjectRule::Never,
        applies: Applies::NextStart,
        about: "The person every attached client decides approval requests as.",
        flags: &[],
        command: None,
        alias: None,
        hook: Hook::None,
    },
];

/// A structured section of `tui.toml`: not a scalar setting, with its own
/// commands.
#[derive(Debug, Clone, Copy)]
pub struct Section {
    /// How it is written (`[[allow]]` / `[[deny]]`).
    pub key: &'static str,
    /// What it holds.
    pub about: &'static str,
    /// What a project file may do with it.
    pub project: &'static str,
    /// Its flags, when it has some.
    pub flags: &'static str,
    /// The commands that change it.
    pub commands: &'static str,
}

/// The structured sections, in the order the manual lists them.
pub static SECTIONS: &[Section] = &[
    Section {
        key: "[[allow]] / [[deny]]",
        about: "Rules: `tool`, then `arg` (a glob) or `literal`.",
        project: "may only add deny rules",
        flags: "`--allow` / `--deny <tool>[=<glob>]`",
        commands: "/permissions, /approve",
    },
    Section {
        key: "auto.allow",
        about: "Statements: kinds of action you allow, which the classifier reads.",
        project: "may not set it",
        flags: "—",
        commands: "/approve, /permissions",
    },
    Section {
        key: "confinement.extra_read / extra_write",
        about: "Extra directories for the confined shell.",
        project: "may not set it",
        flags: "`--confinement-read` / `--confinement-write <dir>`",
        commands: "/approve",
    },
    Section {
        key: "endpoints.<name>",
        about: "`dialect`, `base_url`, `api_key_env`, `priority`, `api_style` (an inline `api_key` is refused).",
        project: "may not set it",
        flags: "`--endpoint-def <name>=<dialect>=<url>[,key_env=VAR][,api_style=STYLE]`",
        commands: "—",
    },
    Section {
        key: "pricing.\"<model id>\"",
        about: "`input`, `output`, `cached_input`, `cache_write` in USD per million tokens.",
        project: "may not set it",
        flags: "`--set 'pricing.\"<id>\".input=15'`",
        commands: "—",
    },
    Section {
        key: "thinking.\"<endpoint:id or id>\"",
        about: "Anthropic thinking: `\"adaptive\"`, `{ enabled = <budget tokens> }` or `\"default\"`.",
        project: "may not set it",
        flags: "`--set 'thinking.\"proxy:my-alias\"=adaptive'`",
        commands: "—",
    },
];

/// The manual's settings table, generated from [`SETTINGS`] and
/// [`SECTIONS`] (`docs/manual.md` holds it between the
/// `settings-table` markers; a test keeps it in step).
#[cfg(any(test, feature = "test-support"))]
#[must_use]
pub fn manual_table() -> String {
    let mut out = String::from(
        "| Key | Takes | Default | A project file | Takes effect | Flag, command |\n|---|---|---|---|---|---|\n",
    );
    for setting in SETTINGS {
        let flags: Vec<String> = setting
            .flags
            .iter()
            .map(|flag| format!("`{}`", flag.flag))
            .chain(setting.command.map(str::to_string))
            .collect();
        let cells = [
            format!("`{}`: {}", setting.key, setting.about),
            setting.kind.describe(),
            setting.default.describe(),
            setting.project.describe().to_string(),
            setting.applies.describe().to_string(),
            if flags.is_empty() {
                "—".to_string()
            } else {
                flags.join(", ")
            },
        ];
        row(&mut out, &cells);
    }
    for section in SECTIONS {
        row(
            &mut out,
            &[
                format!("`{}`: {}", section.key, section.about),
                "a section".to_string(),
                "none".to_string(),
                section.project.to_string(),
                "—".to_string(),
                format!("{}; {}", section.flags, section.commands),
            ],
        );
    }
    out
}

/// One row of [`manual_table`], a `|` inside a cell escaped.
#[cfg(any(test, feature = "test-support"))]
fn row(out: &mut String, cells: &[String]) {
    out.push('|');
    for cell in cells {
        out.push(' ');
        out.push_str(&cell.replace('|', "\\|"));
        out.push_str(" |");
    }
    out.push('\n');
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
