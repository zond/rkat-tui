//! The approval desk, shared by the in-process runner and the daemon: the
//! human's answer to a durable decision, rule commands, and the permission
//! mode's status.
//!
//! In-process, the escalation broker hands each request to the runner and
//! the UI answers through a channel. In attach mode the gate runs in the
//! `rkat-tui-mob` daemon and the dialog in a client: the daemon holds each
//! escalation as a durable decision (`crate::daemon::decisions`) and pushes
//! it to every attached client, and the client answers with
//! `decision/decide`. The engine's decision vocabulary is approve or
//! reject, so what the dialog distinguishes beyond that (allow + add a
//! rule, deny and stop) travels as the [`RemoteAnswer`] tag beside the
//! decision, and the user's comment as its reason
//! ([`EscalationAnswer::remote`], [`EscalationAnswer::from_decision`]). A
//! decision without a tag (any other client of the engine's methods) is
//! allow once or deny. Both hosts turn the human's [`EscalationAnswer`]
//! into the [`HumanDecision`] the engine waits for with [`answered`].

use meerkat_core::ApprovalDecision;
use serde::{Deserialize, Serialize};

use super::ApprovalEngine;
use super::counters::{MAX_CONSECUTIVE_DENIALS, MAX_TOTAL_DENIALS};
use super::escalation::{HumanDecision, RecordedPrompt};
use super::mode::PermissionMode;
use super::rules::{Rule, RuleId, RuleKind, RuleOrigin, RuleSet};

/// The human's answer to an escalation dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EscalationAnswer {
    /// Allow this call only.
    AllowOnce,
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
            if answer == Some(RemoteAnswer::AllowAndAddRule) {
                Self::AllowAndAddRule
            } else {
                Self::AllowOnce
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
            Self::AllowOnce | Self::AllowAndAddRule => ApprovalDecision::Approve,
            Self::Deny | Self::Stop => ApprovalDecision::Deny,
        }
    }
}

/// The literal allow rule "allow + add rule" adds for `tool` and its first
/// rule `subject` (matches exactly this command or path).
///
/// # Errors
///
/// Why no rule can be made.
pub fn allow_rule(tool: &str, subject: Option<&str>) -> Result<Rule, String> {
    match subject {
        Some(subject) => Rule::literal(RuleKind::Allow, tool, subject, RuleOrigin::Realm)
            .map_err(|e| e.to_string()),
        None => Err("the action has no rule subject".to_string()),
    }
}

/// What the engine is told for the human's `answer` to the dialog that
/// showed `prompt`. "Allow and add a rule" falls back to allow once when
/// the action has no rule subject; the second value then says why no rule
/// was added.
#[must_use]
pub fn answered(
    answer: EscalationAnswer,
    prompt: &RecordedPrompt,
) -> (HumanDecision, Option<String>) {
    match answer {
        EscalationAnswer::AllowOnce => (HumanDecision::AllowOnce, None),
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
/// shows them (and `rkat/get_mode` answers).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeStatus {
    /// The permission mode.
    pub mode: PermissionMode,
    /// Auto mode is paused by classifier blocks.
    pub paused: bool,
    /// Consecutive blocks, their limit, total blocks, their limit.
    pub denials: (u32, u32, u32, u32),
}

impl ModeStatus {
    /// The engine's status now.
    #[must_use]
    pub fn of(engine: &ApprovalEngine) -> Self {
        let (counters, paused) = engine.auto_state();
        Self {
            mode: engine.mode(),
            paused: paused.is_some(),
            denials: (
                counters.consecutive,
                MAX_CONSECUTIVE_DENIALS,
                counters.total,
                MAX_TOTAL_DENIALS,
            ),
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

/// A `/rules` command (`rkat/rules` carries it to the daemon).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RuleCommand {
    /// `/rules add allow|deny <tool> [pattern]`.
    Add {
        /// Allow or deny.
        deny: bool,
        /// Tool name pattern.
        tool: String,
        /// Argument pattern.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pattern: Option<String>,
    },
    /// `/rules remove <n>`.
    Remove {
        /// The number `/rules list` shows.
        n: usize,
    },
    /// `/rules list`.
    List,
}

/// What a rule command did: whether it succeeded and the text to show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleOutcome {
    /// It did what it was asked.
    pub ok: bool,
    /// The notice.
    pub text: String,
}

impl RuleOutcome {
    fn ok(text: impl Into<String>) -> Self {
        Self {
            ok: true,
            text: text.into(),
        }
    }

    fn failed(text: impl Into<String>) -> Self {
        Self {
            ok: false,
            text: text.into(),
        }
    }
}

/// Run `command` on `engine` (rules added or removed are persisted to the
/// realm rules file, trusted, as the in-process `/rules` does).
#[must_use]
pub fn run_rule_command(engine: &ApprovalEngine, command: &RuleCommand) -> RuleOutcome {
    match command {
        RuleCommand::Add {
            deny,
            tool,
            pattern,
        } => {
            let kind = if *deny {
                RuleKind::Deny
            } else {
                RuleKind::Allow
            };
            match Rule::new(kind, tool, pattern.as_deref(), RuleOrigin::Realm)
                .and_then(|rule| engine.add_rule(rule))
            {
                Ok(()) => RuleOutcome::ok("rule added"),
                Err(err) => RuleOutcome::failed(format!("rule not added: {err}")),
            }
        }
        RuleCommand::Remove { n } => {
            let Some(id) = nth_rule_id(&engine.rules(), *n) else {
                return RuleOutcome::failed(format!("no rule #{n}"));
            };
            match engine.remove_rule(id) {
                Ok(Some(rule)) => RuleOutcome::ok(format!("removed {}", describe_rule(&rule))),
                Ok(None) => RuleOutcome::failed(format!("no rule #{n}")),
                Err(err) => RuleOutcome::failed(format!("rule not removed: {err}")),
            }
        }
        RuleCommand::List => {
            let rules = engine.rules();
            let lines: Vec<String> = numbered_rules(&rules)
                .map(|(n, rule)| rule_list_line(n, rule))
                .collect();
            if lines.is_empty() {
                RuleOutcome::ok("no rules")
            } else {
                RuleOutcome::ok(lines.join("\n"))
            }
        }
    }
}

/// Every rule with the number `/rules list` shows and `/rules remove`
/// takes: its stable serial ([`RuleId::serial`]), which never shifts when
/// other rules are removed.
pub fn numbered_rules(rules: &RuleSet) -> impl Iterator<Item = (u64, &Rule)> {
    rules
        .allow()
        .chain(rules.deny())
        .map(|(id, rule)| (id.serial, rule))
}

/// The id of the rule `/rules list` numbers `n`.
#[must_use]
pub fn nth_rule_id(rules: &RuleSet, n: usize) -> Option<RuleId> {
    let serial = u64::try_from(n).ok()?;
    rules
        .allow()
        .chain(rules.deny())
        .map(|(id, _)| id)
        .find(|id| id.serial == serial)
}

/// One `/rules list` line: number, rule and the layer it came from.
#[must_use]
pub fn rule_list_line(n: u64, rule: &Rule) -> String {
    format!("{n}. {} ({})", describe_rule(rule), rule.origin.as_str())
}

/// A rule as `/rules` writes it (`deny shell git push*`).
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
        assert!(why.is_some_and(|why| why.contains("no rule subject")));
    }

    #[test]
    fn rule_commands_and_mode_status_travel_as_json() {
        let add = RuleCommand::Add {
            deny: true,
            tool: "write_file".into(),
            pattern: None,
        };
        let wire = serde_json::to_value(&add).unwrap();
        assert_eq!(
            wire,
            json!({"op": "add", "deny": true, "tool": "write_file"})
        );
        assert_eq!(serde_json::from_value::<RuleCommand>(wire).unwrap(), add);
        assert_eq!(
            serde_json::from_value::<RuleCommand>(json!({"op": "remove", "n": 3})).unwrap(),
            RuleCommand::Remove { n: 3 }
        );
        let status = ModeStatus {
            mode: PermissionMode::Plan,
            paused: false,
            denials: (1, 3, 2, 20),
        };
        let wire = serde_json::to_value(status).unwrap();
        assert_eq!(wire["mode"], "plan");
        assert_eq!(serde_json::from_value::<ModeStatus>(wire).unwrap(), status);
    }
}
