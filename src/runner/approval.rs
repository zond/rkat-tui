//! The runner's side of the approval gate: escalation dialogs, `/rules`,
//! the permission mode (`Shift+Tab`, `/mode`, `/auto`) and plan mode's
//! system-context notes, decision notices, the mode's status and
//! classifier usage.
//!
//! The engine asks the human through the escalation broker; the runner
//! shows each request as [`Msg::Escalation`] (the UI queues them behind its
//! modal) and answers with the [`HumanDecision`] the UI's
//! [`Effect::RespondEscalation`] maps to. An escalation the engine withdraws
//! (timeout, abandoned call) closes its dialog with
//! [`Msg::EscalationResolved`]. Interrupting a turn stops its session in the
//! engine and answers its open escalations with Stop.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use meerkat_core::SessionId;
use tokio::sync::{mpsc, oneshot};

use super::Runner;
use crate::approval::desk::{
    ModeStatus, RuleCommand, allow_rule, describe_rule, mode_notice, run_rule_command,
};
use crate::approval::escalation::{EscalationRequest, HumanDecision};
use crate::approval::mode::PermissionMode;
use crate::approval::policy::{Decider, Tier, VerdictSummary};
use crate::approval::{ApprovalEngine, ApprovalEvent};
use crate::ui::{
    ApprovalNotice, Effect, EscalationAnswer, EscalationId, EscalationPrompt, Msg, NoticeLevel,
    RuleEffect, StatusUpdate,
};

/// An escalation the UI has not answered yet.
struct PendingEscalation {
    session_id: SessionId,
    respond: oneshot::Sender<HumanDecision>,
    tool: String,
    /// Canonical rule subject "allow always" turns into a literal rule.
    subject: Option<String>,
}

/// The approval engine's channels and the open escalations.
pub(super) struct ApprovalDesk {
    pub(super) engine: Arc<ApprovalEngine>,
    pub(super) escalations: Option<mpsc::Receiver<EscalationRequest>>,
    pub(super) events: Option<mpsc::UnboundedReceiver<ApprovalEvent>>,
    pending: HashMap<u64, PendingEscalation>,
}

impl ApprovalDesk {
    pub(super) fn new(
        engine: Arc<ApprovalEngine>,
        escalations: Option<mpsc::Receiver<EscalationRequest>>,
        events: Option<mpsc::UnboundedReceiver<ApprovalEvent>>,
    ) -> Self {
        Self {
            engine,
            escalations,
            events,
            pending: HashMap::new(),
        }
    }
}

/// The system context plan mode adds when it starts.
pub(crate) const PLAN_ON_NOTE: &str = "\
# Plan mode is on

The user switched to plan mode: research and plan, do not change anything. \
Use read-only tools only; file writes, edits and commands that are not \
read-only are refused while plan mode is on. When you know enough, present a \
concise, numbered plan of the changes you would make and stop: the user \
approves it before anything is changed.";

/// The system context added when plan mode ends.
pub(crate) const PLAN_OFF_NOTE: &str = "\
# Plan mode is off

The user left plan mode: the approval policy of the current permission mode \
applies again, and you may make changes.";

/// A per-process counter for plan-mode notes' idempotency keys.
static PLAN_NOTES: AtomicU64 = AtomicU64::new(0);

/// An idempotency key no other plan-mode note shares (a session keeps the
/// notes of earlier transitions and of earlier processes).
pub(crate) fn plan_note_key(on: bool) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(
        "rkat-tui:plan-mode:{}:{}-{nanos}-{}",
        if on { "on" } else { "off" },
        std::process::id(),
        PLAN_NOTES.fetch_add(1, Ordering::Relaxed)
    )
}

impl Runner {
    /// The permission-mode status segment.
    pub(super) fn push_mode_status(&self) {
        let (mode, paused, denials) = mode_status(&self.approval.engine);
        self.send(Msg::Status(StatusUpdate::Mode {
            mode,
            paused,
            denials,
        }));
    }

    /// [`Effect::SetMode`]: switch, record the mode in the realm rules file
    /// and tell the session when plan mode starts or ends.
    pub(super) async fn set_mode(&mut self, mode: PermissionMode) {
        self.approval.engine.set_mode(mode);
        self.mode_changed().await;
        self.notice(NoticeLevel::Info, mode_notice(mode));
    }

    /// [`Effect::ResumeAutoMode`] (`/auto resume`; in another mode it
    /// switches to auto mode).
    pub(super) async fn resume_auto(&mut self) {
        self.approval.engine.resume_auto();
        self.mode_changed().await;
        self.notice(NoticeLevel::Info, "auto mode resumed");
    }

    async fn mode_changed(&mut self) {
        self.push_mode_status();
        if let Err(err) = self.approval.engine.persist_mode() {
            self.notice(
                NoticeLevel::Warn,
                format!("the mode applies to this run but was not saved: {err}"),
            );
        }
        self.note_plan_mode().await;
    }

    /// Bring the session's plan-mode note up to date: "on" when plan mode
    /// is on and the session has not been told, "off" when it was told and
    /// plan mode ended. A session that cannot take the note now hears it
    /// before its next turn (and the tool errors say it anyway).
    pub(super) async fn note_plan_mode(&mut self) {
        let plan = self.approval.engine.mode() == PermissionMode::Plan;
        let told = self.plan_noted.as_ref() == Some(self.session.id());
        if plan == told {
            return;
        }
        let note = if plan { PLAN_ON_NOTE } else { PLAN_OFF_NOTE };
        match self
            .session
            .append_system_context(note.to_string(), plan_note_key(plan))
            .await
        {
            Ok(_) => self.plan_noted = plan.then(|| self.session.id().clone()),
            Err(err) => self.notice(
                NoticeLevel::Warn,
                format!("the session could not be told about plan mode now: {err}"),
            ),
        }
    }

    /// Answer the approval effects of the UI.
    pub(super) fn handle_approval_effect(&mut self, effect: Effect) {
        match effect {
            Effect::RespondEscalation(id, answer) => self.respond_escalation(id, answer),
            Effect::AddRule {
                effect,
                tool,
                pattern,
            } => self.rule_command(&RuleCommand::Add {
                deny: effect == RuleEffect::Deny,
                tool,
                pattern,
            }),
            Effect::RemoveRule(n) => self.rule_command(&RuleCommand::Remove { n }),
            Effect::ListRules => self.rule_command(&RuleCommand::List),
            other => tracing::debug!(?other, "not an approval effect"),
        }
    }

    /// Show an escalation to the human.
    pub(super) fn on_escalation(&mut self, request: EscalationRequest) {
        let EscalationRequest { prompt, respond } = request;
        let subject = prompt.subjects.into_iter().next();
        let shown = EscalationPrompt {
            agent: prompt.agent,
            tool: prompt.tool.clone(),
            args_preview: prompt.args_preview,
            hint: prompt.classifier_hint,
            reason: prompt.policy_reason,
            rule: subject
                .as_ref()
                .map(|subject| format!("{} {subject}", prompt.tool)),
        };
        self.approval.pending.insert(
            prompt.id,
            PendingEscalation {
                session_id: prompt.session_id,
                respond,
                tool: prompt.tool,
                subject,
            },
        );
        self.send(Msg::Escalation(shown, EscalationId(prompt.id)));
    }

    /// `/rules trust`: show what the realm rules file holds and trust it.
    pub(super) fn trust_realm_rules(&self) {
        match self.approval.engine.trust_realm_rules() {
            Ok(summary) => self.notice(NoticeLevel::Info, summary),
            Err(err) => self.notice(NoticeLevel::Error, format!("not trusted: {err}")),
        }
    }

    fn respond_escalation(&mut self, id: EscalationId, answer: EscalationAnswer) {
        let Some(pending) = self.approval.pending.remove(&id.0) else {
            return;
        };
        let decision = match answer {
            EscalationAnswer::AllowOnce => HumanDecision::AllowOnce,
            EscalationAnswer::Deny { comment } => HumanDecision::Deny { comment },
            EscalationAnswer::Stop => HumanDecision::Stop,
            EscalationAnswer::AllowAndAddRule => {
                // A literal rule over the canonical subject: matches exactly
                // this command / path, no glob semantics to escape.
                match allow_rule(&pending.tool, pending.subject.as_deref()) {
                    Ok(rule) => HumanDecision::AllowAndAddRule(Box::new(rule)),
                    Err(err) => {
                        self.notice(NoticeLevel::Warn, format!("rule not added: {err}"));
                        HumanDecision::AllowOnce
                    }
                }
            }
        };
        let stop = matches!(decision, HumanDecision::Stop);
        let _ = pending.respond.send(decision);
        self.send(Msg::EscalationResolved(id));
        // "Deny and stop" stops the turn, not just this call.
        if stop && &pending.session_id == self.session.id() {
            self.interrupt();
        }
    }

    /// The human interrupted `session`'s turn: further escalations of it are
    /// denied until a new run starts (the engine keys the Stop by the run
    /// current now, so a late start of this run does not lift it), and its
    /// open dialogs are answered with Stop.
    pub(super) fn stop_approvals(&mut self, session: &SessionId) {
        self.approval.engine.stop(session);
        let ids: Vec<u64> = self
            .approval
            .pending
            .iter()
            .filter(|(_, pending)| &pending.session_id == session)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if let Some(pending) = self.approval.pending.remove(&id) {
                let _ = pending.respond.send(HumanDecision::Stop);
                self.send(Msg::EscalationResolved(EscalationId(id)));
            }
        }
    }

    /// Show what the engine reports.
    pub(super) fn on_approval_event(&mut self, event: ApprovalEvent) {
        match event {
            ApprovalEvent::Decision {
                agent,
                tool_use_id,
                tool,
                tier,
                verdict,
                reason,
                ..
            } => {
                let tool = match agent {
                    Some(agent) => format!("[{agent}] {tool}"),
                    None => tool,
                };
                let allowed = verdict.is_allowed();
                // Tier-1/2 allows are routine; a classifier allow is one dim
                // line under its tool block; other reviewed or denied
                // actions (a human's answer to a protected write included)
                // get a line of their own.
                if let VerdictSummary::Allowed {
                    by: Decider::Classifier,
                    ..
                } = verdict
                {
                    self.send(Msg::ToolNote {
                        tool_use_id,
                        text: classifier_allow_note(reason.as_deref()),
                    });
                } else if let VerdictSummary::Allowed { by: decider, .. }
                | VerdictSummary::Denied { by: decider, .. } = verdict
                    && (!allowed
                        || decider == Decider::Human
                        || matches!(tier, Tier::Classifier | Tier::AskUser))
                {
                    self.send(Msg::Approval(ApprovalNotice {
                        tool,
                        tier: format!("{tier} · {decider:?}").to_lowercase(),
                        allowed,
                        reason,
                    }));
                }
                self.push_mode_status();
            }
            ApprovalEvent::Paused(cause) => {
                self.push_mode_status();
                self.notice(
                    NoticeLevel::Warn,
                    format!(
                        "auto mode paused ({cause}): reviewed actions now ask you; /auto resume to continue"
                    ),
                );
            }
            ApprovalEvent::Resumed => self.push_mode_status(),
            ApprovalEvent::RulesDropped(rules) => {
                let listed: Vec<String> = rules.iter().map(describe_rule).collect();
                self.notice(
                    NoticeLevel::Warn,
                    format!(
                        "auto mode dropped {} broad allow rule(s): {}",
                        listed.len(),
                        listed.join(", ")
                    ),
                );
            }
            ApprovalEvent::Classifying(running) => {
                self.send(Msg::Status(StatusUpdate::Classifying(running)));
            }
            ApprovalEvent::ClassifierUsage { model, usage } => {
                self.send(Msg::ClassifierUsage { model, usage });
            }
            ApprovalEvent::Notice(text) => self.notice(NoticeLevel::Warn, text),
            ApprovalEvent::EscalationWithdrawn(id) => {
                if self.approval.pending.remove(&id).is_some() {
                    self.send(Msg::EscalationResolved(EscalationId(id)));
                    self.notice(
                        NoticeLevel::Warn,
                        "an approval request expired unanswered; the action was denied",
                    );
                }
            }
        }
    }

    fn rule_command(&self, command: &RuleCommand) {
        let outcome = run_rule_command(&self.approval.engine, command);
        let level = if outcome.ok {
            NoticeLevel::Info
        } else {
            NoticeLevel::Error
        };
        self.notice(level, outcome.text);
    }
}

/// The permission mode, whether auto mode is paused after classifier
/// blocks, and the denial counters with their limits.
pub(super) fn mode_status(engine: &ApprovalEngine) -> (PermissionMode, bool, (u32, u32, u32, u32)) {
    let status = ModeStatus::of(engine);
    (status.mode, status.paused, status.denials)
}

/// Most characters of the classifier's reason in an allow note.
const MAX_ALLOW_REASON_CHARS: usize = 100;

/// The note under a tool block the classifier allowed:
/// `allowed by classifier: <reason>`, the reason on one line and cut to
/// [`MAX_ALLOW_REASON_CHARS`].
fn classifier_allow_note(reason: Option<&str>) -> String {
    let one_line = reason
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if one_line.is_empty() {
        return "allowed by classifier".to_string();
    }
    let reason = if one_line.chars().count() > MAX_ALLOW_REASON_CHARS {
        let kept: String = one_line.chars().take(MAX_ALLOW_REASON_CHARS - 1).collect();
        format!("{kept}…")
    } else {
        one_line
    };
    format!("allowed by classifier: {reason}")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::approval::desk::{nth_rule_id, numbered_rules, rule_list_line};
    use crate::approval::rules::{Rule, RuleKind, RuleOrigin, RuleSet};

    #[test]
    fn classifier_allow_note_is_one_short_line() {
        assert_eq!(
            classifier_allow_note(Some("in-project scratch file the user asked to clean up")),
            "allowed by classifier: in-project scratch file the user asked to clean up"
        );
        assert_eq!(
            classifier_allow_note(Some("  line one\n  line two ")),
            "allowed by classifier: line one line two"
        );
        assert_eq!(classifier_allow_note(None), "allowed by classifier");
        assert_eq!(classifier_allow_note(Some(" \n")), "allowed by classifier");
        let exact = "x".repeat(MAX_ALLOW_REASON_CHARS);
        assert_eq!(
            classifier_allow_note(Some(&exact)),
            format!("allowed by classifier: {exact}")
        );
        let long = "é".repeat(MAX_ALLOW_REASON_CHARS + 30);
        let note = classifier_allow_note(Some(&long));
        let reason = note.strip_prefix("allowed by classifier: ").unwrap();
        assert_eq!(reason.chars().count(), MAX_ALLOW_REASON_CHARS);
        assert!(reason.ends_with('…'));
        assert!(!note.contains('\n'));
    }

    #[test]
    fn rule_numbers_are_stable_across_removal() {
        let mut rules = RuleSet::default();
        let ls = rules
            .push(Rule::new(RuleKind::Allow, "shell", Some("ls*"), RuleOrigin::Realm).unwrap());
        let rm =
            rules.push(Rule::new(RuleKind::Deny, "shell", Some("rm*"), RuleOrigin::Realm).unwrap());
        let cat = rules
            .push(Rule::new(RuleKind::Allow, "shell", Some("cat*"), RuleOrigin::Realm).unwrap());
        assert_eq!(nth_rule_id(&rules, 0), None);
        assert_eq!(nth_rule_id(&rules, 1), Some(ls));
        assert_eq!(nth_rule_id(&rules, 2), Some(rm));
        assert_eq!(rm.to_string(), "deny#2");
        // Removing rule 1 does not renumber the others.
        assert!(rules.remove(ls).is_some());
        assert_eq!(nth_rule_id(&rules, 1), None);
        assert_eq!(nth_rule_id(&rules, 2), Some(rm));
        assert_eq!(nth_rule_id(&rules, 3), Some(cat));
        assert_eq!(rules.get(rm).map(|r| r.kind), Some(RuleKind::Deny));
        // A new rule never reuses a removed number.
        let new = rules
            .push(Rule::new(RuleKind::Allow, "shell", Some("wc*"), RuleOrigin::Realm).unwrap());
        assert_eq!(new.serial, 4);
        let listed: Vec<u64> = numbered_rules(&rules).map(|(n, _)| n).collect();
        assert_eq!(listed, [3, 4, 2]);
    }

    #[test]
    fn rules_list_shows_the_origin() {
        let rule = Rule::new(RuleKind::Allow, "shell", Some("ls*"), RuleOrigin::Cli).unwrap();
        assert_eq!(rule_list_line(3, &rule), "3. allow shell ls* (cli)");
        let rule = Rule::new(RuleKind::Deny, "shell", None, RuleOrigin::Project).unwrap();
        assert_eq!(rule_list_line(1, &rule), "1. deny shell (project)");
    }
}
