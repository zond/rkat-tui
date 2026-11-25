//! The approval desk, shared by the daemon and its clients: the human's
//! answer to a durable decision, rule commands, and the permission mode's
//! status.
//!
//! The gate runs in the `rkat-tui-mob` daemon and the dialog in a client:
//! the escalation broker hands each request to the daemon, which holds each
//! escalation as a durable decision (`crate::daemon::decisions`) and pushes
//! it to every attached client, and the client answers with
//! `decision/decide`. The engine's decision vocabulary is approve or
//! reject, so what the dialog distinguishes beyond that (allow + add a
//! rule, deny and stop) travels as the [`RemoteAnswer`] tag beside the
//! decision, and the user's comment as its reason
//! ([`EscalationAnswer::remote`], [`EscalationAnswer::from_decision`]). A
//! decision without a tag (any other client of the engine's methods) is
//! allow once or deny. The daemon turns the human's [`EscalationAnswer`]
//! into the [`HumanDecision`] the engine waits for with [`answered`].

use meerkat_core::ApprovalDecision;
use serde::{Deserialize, Serialize};

use super::ApprovalEngine;
use super::counters::{MAX_CONSECUTIVE_DENIALS, MAX_TOTAL_DENIALS};
use super::escalation::{HumanDecision, RecordedPrompt};
use super::mode::PermissionMode;
use super::rules::{Rule, RuleId, RuleKind, RuleOrigin, RuleSet, RulesError};
use crate::daemon::protocol::Outcome;

/// The human's answer to an escalation dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EscalationAnswer {
    /// Allow this call only.
    AllowOnce,
    /// Allow it and grant what the dialog named for this session
    /// ([`RecordedPrompt::session_grant`]).
    AllowForSession,
    /// Allow and persist the literal allow rule of the call's first
    /// subject ([`RecordedPrompt::rule`]).
    AllowAndAddRule,
    /// Deny this call.
    Deny {
        /// What to tell the agent to do instead (the tool error carries it).
        comment: Option<String>,
    },
    /// Deny and stop the turn.
    Stop,
}

impl EscalationAnswer {
    /// The answer on the wire: its tag, and the comment a denial carries
    /// as the decision's reason.
    #[must_use]
    pub fn remote(&self) -> (RemoteAnswer, Option<&str>) {
        match self {
            Self::AllowOnce => (RemoteAnswer::AllowOnce, None),
            Self::AllowForSession => (RemoteAnswer::AllowForSession, None),
            Self::AllowAndAddRule => (RemoteAnswer::AllowAndAddRule, None),
            Self::Deny { comment } => (RemoteAnswer::Deny, comment.as_deref()),
            Self::Stop => (RemoteAnswer::Stop, None),
        }
    }

    /// The answer a decided durable decision carries: approved or
    /// rejected, the dialog's `answer` tag when the client named it, and
    /// the decider's `reason` (the dialog's comment). An approval without
    /// a tag allows once; a rejection without one denies with the reason.
    #[must_use]
    pub fn from_decision(
        approve: bool,
        answer: Option<RemoteAnswer>,
        reason: Option<&str>,
    ) -> Self {
        if approve {
            match answer {
                Some(RemoteAnswer::AllowAndAddRule) => Self::AllowAndAddRule,
                Some(RemoteAnswer::AllowForSession) => Self::AllowForSession,
                _ => Self::AllowOnce,
            }
        } else if answer == Some(RemoteAnswer::Stop) {
            Self::Stop
        } else {
            Self::Deny {
                comment: reason
                    .filter(|comment| !comment.trim().is_empty())
                    .map(str::to_string),
            }
        }
    }
}

/// The tag of an [`EscalationAnswer`] as a client sends it with
/// `decision/decide`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteAnswer {
    /// Allow this call.
    AllowOnce,
    /// Allow it and grant what the dialog named for this session.
    AllowForSession,
    /// Allow and add the literal allow rule of the call's first subject.
    AllowAndAddRule,
    /// Deny this call (the decision's reason is the user's comment).
    Deny,
    /// Deny and stop the turn.
    Stop,
}

impl RemoteAnswer {
    /// The engine decision this answer records.
    #[must_use]
    pub fn decision(self) -> ApprovalDecision {
        match self {
            Self::AllowOnce | Self::AllowForSession | Self::AllowAndAddRule => {
                ApprovalDecision::Approve
            }
            Self::Deny | Self::Stop => ApprovalDecision::Deny,
        }
    }
}

/// Why "allow + add rule" made no rule.
#[derive(Debug, thiserror::Error)]
pub enum AllowRuleError {
    /// The action has no rule subject (an MCP tool's call, for one).
    #[error("the action has no rule subject")]
    NoSubject,
    /// The subject does not make a valid rule.
    #[error(transparent)]
    Rule(#[from] RulesError),
}

/// The literal allow rule "allow + add rule" adds for `tool` and its first
/// rule `subject` (matches exactly this command or path).
///
/// # Errors
///
/// [`AllowRuleError`]: why no rule can be made.
pub fn allow_rule(tool: &str, subject: Option<&str>) -> Result<Rule, AllowRuleError> {
    let subject = subject.ok_or(AllowRuleError::NoSubject)?;
    Ok(Rule::literal(
        RuleKind::Allow,
        tool,
        subject,
        RuleOrigin::Realm,
    )?)
}

/// What the engine is told for the human's `answer` to the dialog that
/// showed `prompt`. "Allow and add a rule" falls back to allow once when
/// the action has no rule subject; the second value then says why no rule
/// was added.
#[must_use]
pub fn answered(
    answer: EscalationAnswer,
    prompt: &RecordedPrompt,
) -> (HumanDecision, Option<AllowRuleError>) {
    match answer {
        EscalationAnswer::AllowOnce => (HumanDecision::AllowOnce, None),
        EscalationAnswer::AllowForSession => (HumanDecision::AllowForSession, None),
        EscalationAnswer::AllowAndAddRule => {
            // A literal rule over the canonical subject: matches exactly
            // this command or path, no glob semantics to escape.
            match allow_rule(&prompt.tool, prompt.subjects.first().map(String::as_str)) {
                Ok(rule) => (HumanDecision::AllowAndAddRule(Box::new(rule)), None),
                Err(err) => (HumanDecision::AllowOnce, Some(err)),
            }
        }
        EscalationAnswer::Deny { comment } => (HumanDecision::Deny { comment }, None),
        EscalationAnswer::Stop => (HumanDecision::Stop, None),
    }
}

/// The permission mode, whether auto mode is paused after classifier
/// blocks, and the denial counters with their limits, as the status bar
/// shows them (and `rkat/mode` pushes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeStatus {
    /// The permission mode.
    pub mode: PermissionMode,
    /// Auto mode is paused by classifier blocks.
    pub paused: bool,
    /// The classifier's blocks and their limits.
    pub denials: Denials,
}

/// The classifier's blocks against the limits that pause auto mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Denials {
    /// Blocks in a row.
    pub consecutive: u32,
    /// The limit of blocks in a row.
    pub consecutive_limit: u32,
    /// Blocks in all.
    pub total: u32,
    /// The limit of blocks in all.
    pub total_limit: u32,
}

impl Denials {
    /// `consecutive denials c/cm, total t/tm`, as `/auto` and `/status`
    /// say it.
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "consecutive denials {}/{}, total {}/{}",
            self.consecutive, self.consecutive_limit, self.total, self.total_limit
        )
    }
}

impl ModeStatus {
    /// The engine's status now.
    #[must_use]
    pub fn of(engine: &ApprovalEngine) -> Self {
        let (counters, paused) = engine.auto_state();
        Self {
            mode: engine.mode(),
            paused: paused.is_some(),
            denials: Denials {
                consecutive: counters.consecutive,
                consecutive_limit: MAX_CONSECUTIVE_DENIALS,
                total: counters.total,
                total_limit: MAX_TOTAL_DENIALS,
            },
        }
    }
}

/// What a mode switch tells the user.
#[must_use]
pub fn mode_notice(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Manual => {
            "permission mode: manual (every edit and command asks you; Shift+Tab cycles)"
        }
        PermissionMode::AcceptEdits => {
            "permission mode: accept edits (in-project edits run; commands ask you)"
        }
        PermissionMode::Plan => {
            "permission mode: plan (read-only; the agent presents a plan before changing anything)"
        }
        PermissionMode::Auto => {
            "permission mode: auto (the classifier reviews what is not always allowed)"
        }
    }
}

/// A `/permissions` command (`rkat/rules` carries it to the daemon).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RuleCommand {
    /// `/permissions add allow|deny <tool> [pattern]`, or the dialog's `r`
    /// form (a literal one too).
    Add {
        /// The rule.
        #[serde(flatten)]
        spec: RuleSpec,
        /// Written to the user's own `tui.toml` (`--user`) instead of the
        /// workspace's.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        user: bool,
    },
    /// `/permissions remove <n>`.
    Remove {
        /// The number `/permissions list` shows.
        n: u64,
    },
    /// The `/permissions` dialog's `e` on a rule: replace rule `n`, which
    /// reads `old`, with `new`, in place in its own file.
    Edit {
        /// The number `/permissions list` shows.
        n: u64,
        /// The rule as the dialog showed it (a rule renumbered since is
        /// not edited by mistake).
        old: RuleSpec,
        /// The rule as edited.
        new: RuleSpec,
    },
    /// `/permissions list`.
    List,
}

/// A rule as the `/permissions` dialog reads and edits it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleSpec {
    /// A deny rule (else an allow rule).
    pub deny: bool,
    /// The tool's name or pattern.
    pub tool: String,
    /// The argument's pattern, or with `literal` its exact text (`None`:
    /// every call of the tool).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    /// `pattern` is matched exactly.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub literal: bool,
}

impl RuleSpec {
    /// The rule it reads as, of `origin`.
    ///
    /// # Errors
    ///
    /// Why it is no rule: a literal with no text, a pattern or tool that
    /// does not parse.
    pub fn rule(&self, origin: RuleOrigin) -> Result<Rule, String> {
        let kind = if self.deny {
            RuleKind::Deny
        } else {
            RuleKind::Allow
        };
        match (&self.pattern, self.literal) {
            (Some(text), true) => Rule::literal(kind, &self.tool, text.clone(), origin),
            (None, true) => return Err("a literal rule needs the exact text to match".into()),
            (pattern, false) => Rule::new(kind, &self.tool, pattern.as_deref(), origin),
        }
        .map_err(|err| err.to_string())
    }

    /// Whether `rule` is the rule it reads as.
    #[must_use]
    pub fn is(&self, rule: &Rule) -> bool {
        let row = RuleRow::of(0, rule);
        row.deny == self.deny
            && row.tool == self.tool
            && row.arg == self.pattern
            && row.literal == self.literal
    }
}

/// Run `command` on `engine` (rules added or removed are persisted to the
/// realm rules file, trusted).
#[must_use]
pub fn run_rule_command(engine: &ApprovalEngine, command: &RuleCommand) -> Outcome {
    match command {
        RuleCommand::Add { spec, user } => {
            let level = if *user {
                crate::config::registry::Level::User
            } else {
                crate::config::registry::Level::Workspace
            };
            let rule = match spec.rule(RuleOrigin::Realm) {
                Ok(rule) => rule,
                Err(why) => return Outcome::failed(format!("rule not added: {why}")),
            };
            match engine.add_rule(rule, level) {
                Ok(()) if *user => Outcome::ok("rule added to your tui.toml (every workspace)"),
                Ok(()) => Outcome::ok("rule added"),
                Err(err) => Outcome::failed(format!("rule not added: {err}")),
            }
        }
        RuleCommand::Remove { n } => {
            let Some(id) = nth_rule_id(&engine.rules(), *n) else {
                return Outcome::failed(format!("no rule #{n}"));
            };
            match engine.remove_rule(id) {
                Ok(Some(rule)) => Outcome::ok(format!("removed {}", describe_rule(&rule))),
                Ok(None) => Outcome::failed(format!("no rule #{n}")),
                Err(err) => Outcome::failed(format!("rule not removed: {err}")),
            }
        }
        RuleCommand::Edit { n, old, new } => {
            let not = |why: &str| Outcome::failed(format!("rule {n} not changed: {why}"));
            let rules = engine.rules();
            let Some((id, held)) =
                nth_rule_id(&rules, *n).and_then(|id| rules.get(id).map(|rule| (id, rule.clone())))
            else {
                return not("there is no such rule now");
            };
            if !old.is(&held) {
                return not(&format!(
                    "it reads {} now, not what the dialog showed; look at the list again",
                    describe_rule(&held)
                ));
            }
            let rule = match new.rule(held.origin) {
                Ok(rule) => rule,
                Err(why) => return not(&why),
            };
            match engine.edit_rule(id, rule) {
                Ok(RuleEdit::Unchanged) => {
                    Outcome::ok(format!("rule {n} unchanged: nothing was written"))
                }
                Ok(RuleEdit::Edited { old, new }) => Outcome::ok(format!(
                    "rule {n} changed: {} is now {}",
                    describe_rule(&old),
                    describe_rule(&new)
                )),
                Err(why) => not(&why),
            }
        }
        RuleCommand::List => {
            let rules = engine.rules();
            let lines: Vec<String> = numbered_rules(&rules)
                .map(|(n, rule)| rule_list_line(n, rule))
                .collect();
            if lines.is_empty() {
                Outcome::ok("no rules")
            } else {
                Outcome::ok(lines.join("\n"))
            }
        }
    }
}

/// One rule as the `/permissions` picker shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleRow {
    /// The number `/permissions list` shows and `/permissions rm` takes.
    pub n: u64,
    /// A deny rule (else an allow rule).
    pub deny: bool,
    /// The tool pattern as written.
    pub tool: String,
    /// The argument pattern as written (`None`: every call of the tool).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arg: Option<String>,
    /// The argument is an exact subject, not a glob.
    #[serde(default)]
    pub literal: bool,
    /// The layer it came from.
    pub origin: RuleOrigin,
}

impl RuleRow {
    /// `rule`, numbered `n`.
    #[must_use]
    pub fn of(n: u64, rule: &Rule) -> Self {
        Self {
            n,
            deny: rule.kind == RuleKind::Deny,
            tool: rule.tool.as_str().to_string(),
            arg: rule.arg.as_ref().map(|arg| arg.as_str().to_string()),
            literal: rule
                .arg
                .as_ref()
                .is_some_and(super::rules::ArgPattern::is_literal),
            origin: rule.origin,
        }
    }

    /// The rule as the dialog's edit reads it.
    #[must_use]
    pub fn spec(&self) -> RuleSpec {
        RuleSpec {
            deny: self.deny,
            tool: self.tool.clone(),
            pattern: self.arg.clone(),
            literal: self.literal,
        }
    }
}

/// Why a rule of the project's `.rkat/tui.toml` is not edited.
pub const EDIT_PROJECT: &str = "it is in the project's .rkat/tui.toml, and only your own tui.toml and the workspace's are written: edit that file (d removes it until the daemon restarts)";

/// Why a rule of the daemon's command line is not edited.
pub const EDIT_CLI: &str = "it comes from the daemon's command line (--allow, --deny), and only your own tui.toml and the workspace's are written: start the daemon with the rule you want";

/// What [`ApprovalEngine::edit_rule`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleEdit {
    /// The edit says what the rule said: nothing was written.
    Unchanged,
    /// The rule was replaced in place, in the engine and its file.
    Edited {
        /// The rule before.
        old: Rule,
        /// The rule now.
        new: Rule,
    },
}

/// [`crate::daemon::protocol::method::RuleRows`]'s answer: every rule, in
/// `/permissions list`'s order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleRows {
    /// The rules.
    pub rules: Vec<RuleRow>,
    /// The directory and session grants in force for the asking client's
    /// agent ([`super::grants::Grants::rows`]).
    #[serde(default)]
    pub grants: Vec<super::grants::GrantRow>,
    /// The kinds of action the user allows for the asking client's agent
    /// ([`super::statements::Statements::rows`]).
    #[serde(default)]
    pub statements: Vec<super::statements::StatementRow>,
    /// The hosts the user trusts and the project's own
    /// ([`ApprovalEngine::host_rows`]): `/permissions`' Hosts section.
    #[serde(default)]
    pub hosts: super::hosts::HostRows,
}

/// The rules of `engine` as the `/permissions` picker shows them.
#[must_use]
pub fn rule_rows(engine: &ApprovalEngine) -> RuleRows {
    let rules = engine.rules();
    RuleRows {
        rules: numbered_rules(&rules)
            .map(|(n, rule)| RuleRow::of(n, rule))
            .collect(),
        grants: Vec::new(),
        statements: Vec::new(),
        hosts: engine.host_rows(),
    }
}

/// Every rule with the number `/permissions list` shows and `/permissions remove`
/// takes: its stable serial ([`RuleId::serial`]), which never shifts when
/// other rules are removed.
pub fn numbered_rules(rules: &RuleSet) -> impl Iterator<Item = (u64, &Rule)> {
    rules
        .allow()
        .chain(rules.deny())
        .map(|(id, rule)| (id.serial, rule))
}

/// The id of the rule `/permissions list` numbers `n`.
#[must_use]
pub fn nth_rule_id(rules: &RuleSet, serial: u64) -> Option<RuleId> {
    rules
        .allow()
        .chain(rules.deny())
        .map(|(id, _)| id)
        .find(|id| id.serial == serial)
}

/// One `/permissions list` line: number, rule and the layer it came from.
#[must_use]
pub fn rule_list_line(n: u64, rule: &Rule) -> String {
    format!("{n}. {} ({})", describe_rule(rule), rule.origin.as_str())
}

/// A rule as `/permissions` writes it (`deny shell git push*`).
#[must_use]
pub fn describe_rule(rule: &Rule) -> String {
    let kind = match rule.kind {
        RuleKind::Allow => "allow",
        RuleKind::Deny => "deny",
    };
    match &rule.arg {
        Some(arg) => format!("{kind} {} {}", rule.tool.as_str(), arg.as_str()),
        None => format!("{kind} {}", rule.tool.as_str()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::json;

    fn prompt() -> RecordedPrompt {
        RecordedPrompt {
            tool: "shell".into(),
            subjects: vec!["git push".into()],
            args_preview: "git push".into(),
            ..RecordedPrompt::default()
        }
    }

    #[test]
    fn a_decided_decision_carries_every_answer() {
        let shown = prompt();
        let decided = |approve, answer, reason| {
            answered(
                EscalationAnswer::from_decision(approve, answer, reason),
                &shown,
            )
            .0
        };
        assert_eq!(decided(true, None, None), HumanDecision::AllowOnce);
        assert_eq!(
            decided(true, Some(RemoteAnswer::AllowOnce), Some("fine")),
            HumanDecision::AllowOnce
        );
        let HumanDecision::AllowAndAddRule(rule) =
            decided(true, Some(RemoteAnswer::AllowAndAddRule), None)
        else {
            panic!("allow and add rule");
        };
        assert_eq!(describe_rule(&rule), "allow shell git push");
        assert_eq!(
            decided(false, Some(RemoteAnswer::Deny), Some("use git mv")),
            HumanDecision::Deny {
                comment: Some("use git mv".into())
            }
        );
        assert_eq!(
            decided(false, None, Some("  ")),
            HumanDecision::Deny { comment: None }
        );
        assert_eq!(
            decided(false, Some(RemoteAnswer::Stop), None),
            HumanDecision::Stop
        );
    }

    /// Every answer survives the wire: its tag and comment decide back to
    /// the same answer.
    #[test]
    fn answers_round_trip_through_the_wire() {
        for answer in [
            EscalationAnswer::AllowOnce,
            EscalationAnswer::AllowAndAddRule,
            EscalationAnswer::Deny { comment: None },
            EscalationAnswer::Deny {
                comment: Some("no".into()),
            },
            EscalationAnswer::Stop,
        ] {
            let (tag, comment) = answer.remote();
            let approve = tag.decision() == ApprovalDecision::Approve;
            assert_eq!(
                EscalationAnswer::from_decision(approve, Some(tag), comment),
                answer
            );
        }
    }

    /// "Allow and add a rule" without a rule subject allows once and says
    /// why no rule was added.
    #[test]
    fn allow_and_add_rule_without_a_subject_allows_once() {
        let shown = RecordedPrompt {
            tool: "mcp__x__y".into(),
            ..RecordedPrompt::default()
        };
        let (decision, why) = answered(EscalationAnswer::AllowAndAddRule, &shown);
        assert_eq!(decision, HumanDecision::AllowOnce);
        assert!(matches!(why, Some(AllowRuleError::NoSubject)));
    }

    #[test]
    fn rule_commands_and_mode_status_travel_as_json() {
        let add = RuleCommand::Add {
            spec: RuleSpec {
                deny: true,
                tool: "write_file".into(),
                pattern: None,
                literal: false,
            },
            user: false,
        };
        let wire = serde_json::to_value(&add).unwrap();
        assert_eq!(
            wire,
            json!({"op": "add", "deny": true, "tool": "write_file"})
        );
        assert_eq!(serde_json::from_value::<RuleCommand>(wire).unwrap(), add);
        let literal = json!({"op": "add", "deny": false, "tool": "shell", "pattern": "make", "literal": true, "user": true});
        let RuleCommand::Add { spec, user } =
            serde_json::from_value::<RuleCommand>(literal.clone()).unwrap()
        else {
            panic!("an add")
        };
        assert!(user && spec.literal && spec.pattern.as_deref() == Some("make"));
        assert_eq!(
            serde_json::to_value(RuleCommand::Add { spec, user }).unwrap(),
            literal
        );
        assert_eq!(
            serde_json::from_value::<RuleCommand>(json!({"op": "remove", "n": 3})).unwrap(),
            RuleCommand::Remove { n: 3 }
        );
        let status = ModeStatus {
            mode: PermissionMode::Plan,
            paused: false,
            denials: Denials {
                consecutive: 1,
                consecutive_limit: 3,
                total: 2,
                total_limit: 20,
            },
        };
        let wire = serde_json::to_value(status).unwrap();
        assert_eq!(wire["mode"], "plan");
        assert_eq!(serde_json::from_value::<ModeStatus>(wire).unwrap(), status);
    }
}
