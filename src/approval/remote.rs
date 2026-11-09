//! The approval desk's parts a process other than the gate's can serve:
//! the human's answer as an approval record decision, rule commands, and
//! the permission mode's status.
//!
//! In-process, the escalation broker hands each request to the runner and
//! the UI answers through a channel. In attach mode the gate runs in the
//! `rkat-tui-mob` daemon and the dialog in a client: the escalation's record
//! in the realm's approval store (the engine's `ApprovalService`, which
//! serves `approval/list`, `approval/get` and `approval/decide`) is where
//! the two meet. The client decides the record; the daemon's desk
//! (`crate::daemon::approvals`) reads the decision back as the
//! [`HumanDecision`] the engine waits for. The engine's decision vocabulary
//! is approve or deny, so what the in-process dialog distinguishes beyond
//! that (allow + add a rule, deny and stop, the user's comment) travels in
//! the decision's `provenance` under [`PROVENANCE_KEY`]; a decision without
//! it (any other client of the engine's methods) is allow once or deny.

use meerkat_core::{ApprovalDecision, ApprovalRecord, ApprovalStatus};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::ApprovalEngine;
use super::counters::{AutoModeState, MAX_CONSECUTIVE_DENIALS, MAX_TOTAL_DENIALS, PauseCause};
use super::escalation::{HumanDecision, RecordedPrompt};
use super::mode::PermissionMode;
use super::rules::{Rule, RuleId, RuleKind, RuleOrigin, RuleSet};

/// The key of rkat-tui's answer in a decision's provenance.
pub const PROVENANCE_KEY: &str = "rkat_tui";

/// The human's answer as a client sends it with `approval/decide`.
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

    /// The decision's provenance (`{"rkat_tui": {"answer": …, "comment": …}}`).
    #[must_use]
    pub fn provenance(self, comment: Option<&str>) -> Value {
        let mut answer = json!({ "answer": self });
        if let Some(comment) = comment {
            answer["comment"] = json!(comment);
        }
        json!({ PROVENANCE_KEY: answer })
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

/// The dialog an escalation record shows: its request body as the gate
/// wrote it, else what the proposed action says.
#[must_use]
pub fn recorded_prompt(record: &ApprovalRecord) -> RecordedPrompt {
    if let Some(body) = &record.request_body
        && let Ok(prompt) = serde_json::from_value::<RecordedPrompt>(body.clone())
    {
        return prompt;
    }
    let body = record.proposed_action.body.as_ref();
    let tool = body
        .and_then(|body| body.get("tool"))
        .and_then(Value::as_str)
        .unwrap_or(&record.proposed_action.summary)
        .to_string();
    let args_preview = body
        .and_then(|body| body.get("arguments"))
        .map(Value::to_string)
        .unwrap_or_default();
    RecordedPrompt {
        tool,
        args_preview,
        classifier_hint: record
            .request_body
            .as_ref()
            .and_then(Value::as_str)
            .map(str::to_string),
        ..RecordedPrompt::default()
    }
}

/// Whether `record` is an escalation the TUI's gate raised for `session`.
#[must_use]
pub fn is_gate_record(record: &ApprovalRecord, session: &meerkat_core::SessionId) -> bool {
    record.requester.as_str() == super::ledger::REQUESTER
        && matches!(
            &record.owner,
            meerkat_core::ApprovalOwnerRef::Session { session_id } if session_id == session
        )
}

/// The human's answer a settled `record` carries for the escalation that
/// showed `prompt`; `None` while it is pending. Expired, cancelled and
/// denied records deny; an approval without rkat-tui's provenance allows
/// once.
#[must_use]
pub fn human_decision(record: &ApprovalRecord, prompt: &RecordedPrompt) -> Option<HumanDecision> {
    let decided = record.decision.as_ref();
    let ours = decided
        .and_then(|decided| decided.provenance.as_ref())
        .and_then(|provenance| provenance.get(PROVENANCE_KEY));
    let answer = ours
        .and_then(|ours| ours.get("answer"))
        .and_then(|answer| serde_json::from_value::<RemoteAnswer>(answer.clone()).ok());
    let comment = ours
        .and_then(|ours| ours.get("comment"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            // Another client's denial reason is the user's word as well.
            if ours.is_none() {
                decided.and_then(|decided| decided.reason.clone())
            } else {
                None
            }
        })
        .filter(|comment| !comment.trim().is_empty());
    match record.status {
        ApprovalStatus::Pending => None,
        ApprovalStatus::Approved => Some(match answer {
            Some(RemoteAnswer::AllowAndAddRule) => {
                match allow_rule(&prompt.tool, prompt.subjects.first().map(String::as_str)) {
                    Ok(rule) => HumanDecision::AllowAndAddRule(Box::new(rule)),
                    Err(err) => {
                        tracing::warn!(error = %err, "allow rule not added; allowing once");
                        HumanDecision::AllowOnce
                    }
                }
            }
            _ => HumanDecision::AllowOnce,
        }),
        ApprovalStatus::Denied => Some(match answer {
            Some(RemoteAnswer::Stop) => HumanDecision::Stop,
            _ => HumanDecision::Deny { comment },
        }),
        ApprovalStatus::Expired | ApprovalStatus::Cancelled => {
            Some(HumanDecision::Deny { comment: None })
        }
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
        let (counters, state) = engine.auto_state();
        let paused = matches!(
            state,
            AutoModeState::Paused { cause } if cause != PauseCause::UserDisabled
        );
        Self {
            mode: engine.mode(),
            paused,
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
    use meerkat_core::{
        ApprovalActionKind, ApprovalOwnerRef, ApprovalPrincipalId, ApprovalProposedAction,
        ApprovalRequest, ApprovalResourceId, ApprovalResourceKind, ApprovalResourceRef,
        ApprovalRisk, ApprovalService, SessionId, SurfaceMetadata,
    };

    fn prompt() -> RecordedPrompt {
        RecordedPrompt {
            tool: "shell".into(),
            subjects: vec!["git push".into()],
            args_preview: "git push".into(),
            ..RecordedPrompt::default()
        }
    }

    fn pending(service: &ApprovalService, session: &SessionId) -> ApprovalRecord {
        service
            .request(ApprovalRequest {
                requester: ApprovalPrincipalId::new(super::super::ledger::REQUESTER).unwrap(),
                owner: ApprovalOwnerRef::Session {
                    session_id: session.clone(),
                },
                resource: ApprovalResourceRef {
                    kind: ApprovalResourceKind::ToolCall,
                    id: ApprovalResourceId::new("call_1"),
                },
                proposed_action: ApprovalProposedAction {
                    kind: ApprovalActionKind::ShellCommand,
                    summary: "Run shell".into(),
                    body: Some(json!({"tool": "shell", "arguments": {"command": "git push"}})),
                },
                risk: ApprovalRisk::High,
                request_body: Some(serde_json::to_value(prompt()).unwrap()),
                allowed_decisions: [ApprovalDecision::Approve, ApprovalDecision::Deny]
                    .into_iter()
                    .collect(),
                expires_at: None,
                metadata: SurfaceMetadata::default(),
                request_provenance: None,
            })
            .unwrap()
    }

    fn decide(
        service: &ApprovalService,
        record: &ApprovalRecord,
        decision: ApprovalDecision,
        reason: Option<&str>,
        provenance: Option<Value>,
    ) -> ApprovalRecord {
        service
            .decide(
                &record.approval_id,
                decision,
                ApprovalPrincipalId::new("someone").unwrap(),
                reason.map(str::to_string),
                provenance,
            )
            .unwrap()
    }

    #[test]
    fn every_answer_round_trips_through_a_decided_record() {
        let session = SessionId::new();
        let cases = [
            (RemoteAnswer::AllowOnce, None, HumanDecision::AllowOnce),
            (
                RemoteAnswer::Deny,
                Some("use git mv"),
                HumanDecision::Deny {
                    comment: Some("use git mv".into()),
                },
            ),
            (RemoteAnswer::Deny, None, HumanDecision::Deny { comment: None }),
            (RemoteAnswer::Stop, None, HumanDecision::Stop),
        ];
        for (answer, comment, expected) in cases {
            let service = ApprovalService::new();
            let record = pending(&service, &session);
            assert!(is_gate_record(&record, &session));
            assert!(!is_gate_record(&record, &SessionId::new()));
            assert_eq!(human_decision(&record, &prompt()), None, "pending");
            let decided = decide(
                &service,
                &record,
                answer.decision(),
                comment,
                Some(answer.provenance(comment)),
            );
            assert_eq!(human_decision(&decided, &prompt()), Some(expected), "{answer:?}");
        }
        let service = ApprovalService::new();
        let record = pending(&service, &session);
        let decided = decide(
            &service,
            &record,
            ApprovalDecision::Approve,
            None,
            Some(RemoteAnswer::AllowAndAddRule.provenance(None)),
        );
        let Some(HumanDecision::AllowAndAddRule(rule)) = human_decision(&decided, &prompt()) else {
            panic!("allow and add rule");
        };
        assert_eq!(describe_rule(&rule), "allow shell git push");
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
    fn another_clients_plain_decision_allows_once_or_denies_with_its_reason() {
        let session = SessionId::new();
        let service = ApprovalService::new();
        let record = pending(&service, &session);
        let approved = decide(&service, &record, ApprovalDecision::Approve, None, None);
        assert_eq!(human_decision(&approved, &prompt()), Some(HumanDecision::AllowOnce));
        let record = pending(&service, &session);
        let denied = decide(&service, &record, ApprovalDecision::Deny, Some("not now"), None);
        assert_eq!(
            human_decision(&denied, &prompt()),
            Some(HumanDecision::Deny {
                comment: Some("not now".into())
            })
        );
    }

    #[test]
    fn the_recorded_prompt_is_read_back_from_the_request_body() {
        let service = ApprovalService::new();
        let record = pending(&service, &SessionId::new());
        assert_eq!(recorded_prompt(&record), prompt());
        // An older record carries only the hint: the proposed action fills in.
        let mut older = record;
        older.request_body = Some(json!("classifier: BLOCK"));
        let shown = recorded_prompt(&older);
        assert_eq!(shown.tool, "shell");
        assert_eq!(shown.classifier_hint.as_deref(), Some("classifier: BLOCK"));
        assert!(shown.args_preview.contains("git push"));
    }

    #[test]
    fn rule_commands_and_mode_status_travel_as_json() {
        let add = RuleCommand::Add {
            deny: true,
            tool: "write_file".into(),
            pattern: None,
        };
        let wire = serde_json::to_value(&add).unwrap();
        assert_eq!(wire, json!({"op": "add", "deny": true, "tool": "write_file"}));
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
