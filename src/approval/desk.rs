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
//! reject, so what the in-process dialog distinguishes beyond that (allow +
//! add a rule, deny and stop) travels as the dialog's [`RemoteAnswer`]
//! beside the decision, and the user's comment as its reason; [`answered`]
//! turns them back into the [`HumanDecision`] the engine waits for. A
//! decision without an answer (any other client of the engine's methods)
//! is allow once or deny.

use meerkat_core::ApprovalDecision;
use serde::{Deserialize, Serialize};

use super::ApprovalEngine;
use super::counters::{MAX_CONSECUTIVE_DENIALS, MAX_TOTAL_DENIALS};
use super::escalation::{HumanDecision, RecordedPrompt};
use super::mode::PermissionMode;
use super::rules::{Rule, RuleId, RuleKind, RuleOrigin, RuleSet};

/// The human's answer as a client sends it with `decision/decide`.
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

/// The human's answer a decided durable decision carries for the
/// escalation that showed `prompt`: approved or rejected, the dialog's
/// `answer` when the client named it, and the decider's `reason` (the
/// dialog's comment). An approval without an answer allows once; a
/// rejection without one denies with the reason.
#[must_use]
pub fn answered(
    approve: bool,
    answer: Option<RemoteAnswer>,
    reason: Option<&str>,
    prompt: &RecordedPrompt,
) -> HumanDecision {
    let comment = reason
        .filter(|comment| !comment.trim().is_empty())
        .map(str::to_string);
    if approve {
        if answer == Some(RemoteAnswer::AllowAndAddRule) {
            match allow_rule(&prompt.tool, prompt.subjects.first().map(String::as_str)) {
                Ok(rule) => return HumanDecision::AllowAndAddRule(Box::new(rule)),
                Err(err) => {
                    tracing::warn!(error = %err, "allow rule not added; allowing once");
                }
            }
        }
        HumanDecision::AllowOnce
    } else if answer == Some(RemoteAnswer::Stop) {
        HumanDecision::Stop
    } else {
        HumanDecision::Deny { comment }
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
        assert_eq!(answered(true, None, None, &shown), HumanDecision::AllowOnce);
        assert_eq!(
            answered(true, Some(RemoteAnswer::AllowOnce), Some("fine"), &shown),
            HumanDecision::AllowOnce
        );
        let Some(HumanDecision::AllowAndAddRule(rule)) = Some(answered(
            true,
            Some(RemoteAnswer::AllowAndAddRule),
            None,
            &shown,
        )) else {
            panic!("allow and add rule");
        };
        assert_eq!(describe_rule(&rule), "allow shell git push");
        assert_eq!(
            answered(false, Some(RemoteAnswer::Deny), Some("use git mv"), &shown),
            HumanDecision::Deny {
                comment: Some("use git mv".into())
            }
        );
        assert_eq!(
            answered(false, None, Some("  "), &shown),
            HumanDecision::Deny { comment: None }
        );
        assert_eq!(
            answered(false, Some(RemoteAnswer::Stop), None, &shown),
            HumanDecision::Stop
        );
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
