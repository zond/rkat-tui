//! The runner's side of the approval gate: escalation dialogs, `/rules`,
//! the permission mode (`Shift+Tab`, `/mode`, `/auto`) and plan mode's
//! system-context notes, decision notices, the mode's status and
//! classifier usage.
//!
//! The engine asks the human through the escalation broker; the runner
//! shows each request as [`Msg::Escalation`] (the UI queues them behind its
//! modal) and answers with the [`HumanDecision`] the UI's
//! [`Effect::RespondEscalation`] maps to ([`answered`]). An escalation the engine withdraws
//! (timeout, abandoned call) closes its dialog with
//! [`Msg::EscalationResolved`]. Interrupting a turn stops its session in the
//! engine and answers its open escalations with Stop.

use std::collections::HashMap;
use std::sync::Arc;

use meerkat_core::SessionId;
use tokio::sync::{mpsc, oneshot};

use super::Runner;
use crate::approval::desk::{
    ModeStatus, RuleCommand, answered, describe_rule, mode_notice, run_rule_command,
};
use crate::approval::escalation::{EscalationRequest, HumanDecision, RecordedPrompt};
use crate::approval::mode::{PLAN_OFF_NOTE, PLAN_ON_NOTE, PermissionMode, plan_note_key};
use crate::approval::policy::{Decider, Tier, VerdictSummary};
use crate::approval::{ApprovalEngine, ApprovalEvent};
use crate::ui::{
    ApprovalNotice, Effect, EscalationAnswer, EscalationId, Msg, NoticeLevel, RuleEffect,
    StatusUpdate,
};

/// An escalation the UI has not answered yet.
struct PendingEscalation {
    session_id: SessionId,
    respond: oneshot::Sender<HumanDecision>,
    /// What the dialog shows ("allow always" mints its rule from it).
    shown: RecordedPrompt,
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
        self.send(Msg::Escalation(
            prompt.shown.clone(),
            EscalationId(prompt.id),
        ));
        self.approval.pending.insert(
            prompt.id,
            PendingEscalation {
                session_id: prompt.session_id,
                respond,
                shown: prompt.shown,
            },
        );
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
        let (decision, no_rule) = answered(answer, &pending.shown);
        if let Some(err) = no_rule {
            self.notice(NoticeLevel::Warn, format!("rule not added: {err}"));
        }
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
                        text: crate::client::approvals::classifier_allow_note(reason.as_deref()),
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use crate::approval::desk::{nth_rule_id, numbered_rules, rule_list_line};
    use crate::approval::rules::{Rule, RuleKind, RuleOrigin, RuleSet};

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
