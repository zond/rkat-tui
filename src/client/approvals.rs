//! The approval dialogs of attach mode.
//!
//! The daemon's gate records every escalation in the realm's approval
//! store; this client lists the pending ones for the orchestrator's session
//! (`approval/list`, on every `tool_call_requested` and while a turn runs
//! or a dialog is open: the engine has no push notification for a plain
//! approval yet), shows each as the in-process dialog
//! ([`Msg::Escalation`]), and answers with `approval/decide`
//! ([`decide_params`]). Another client may decide first: the record leaves
//! the pending list and the dialog closes on the next list
//! ([`Msg::EscalationResolved`]); a late decision of this client is
//! refused by the engine and dropped.

use std::collections::HashMap;

use meerkat_core::{ApprovalId, ApprovalRecord, ApprovalStatus, SessionId};
use serde_json::{Value, json};

use crate::approval::escalation::RecordedPrompt;
use crate::approval::ledger::HUMAN;
use crate::approval::remote::{RemoteAnswer, is_gate_record, recorded_prompt};
use crate::ui::{EscalationAnswer, EscalationId, EscalationPrompt, Msg};

/// What a list of the pending records changes in the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Show a new dialog.
    Show(EscalationPrompt, EscalationId),
    /// Close a dialog; with a notice when it ended without this client.
    Resolve(EscalationId, Option<String>),
}

impl Change {
    /// The UI messages for this change.
    #[must_use]
    pub fn messages(self) -> Vec<Msg> {
        match self {
            Self::Show(prompt, id) => vec![Msg::Escalation(prompt, id)],
            Self::Resolve(id, notice) => {
                let mut msgs = vec![Msg::EscalationResolved(id)];
                if let Some(text) = notice {
                    msgs.push(Msg::Notice {
                        level: crate::ui::NoticeLevel::Warn,
                        text,
                    });
                }
                msgs
            }
        }
    }
}

/// The dialogs this client shows.
#[derive(Debug, Default)]
pub struct Approvals {
    shown: HashMap<ApprovalId, u64>,
    by_local: HashMap<u64, ApprovalId>,
    next: u64,
}

impl Approvals {
    /// Whether a dialog is open.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.shown.is_empty()
    }

    /// The records whose dialogs are open.
    #[must_use]
    pub fn shown(&self) -> Vec<ApprovalId> {
        self.shown.keys().cloned().collect()
    }

    /// Bring the dialogs up to date with `pending` (the store's pending
    /// records) for `session`; `ended` are records shown before that are no
    /// longer pending, as read back.
    pub fn update(
        &mut self,
        session: &SessionId,
        pending: &[ApprovalRecord],
        ended: &[ApprovalRecord],
    ) -> Vec<Change> {
        let mut changes = Vec::new();
        let ours: Vec<&ApprovalRecord> = pending
            .iter()
            .filter(|record| record.status == ApprovalStatus::Pending)
            .filter(|record| is_gate_record(record, session))
            .collect();
        let gone: Vec<ApprovalId> = self
            .shown
            .keys()
            .filter(|id| !ours.iter().any(|record| &record.approval_id == *id))
            .cloned()
            .collect();
        for id in gone {
            if let Some(local) = self.shown.remove(&id) {
                self.by_local.remove(&local);
                let record = ended.iter().find(|record| record.approval_id == id);
                changes.push(Change::Resolve(EscalationId(local), ended_notice(record)));
            }
        }
        let mut fresh: Vec<&ApprovalRecord> = ours
            .into_iter()
            .filter(|record| !self.shown.contains_key(&record.approval_id))
            .collect();
        fresh.sort_by_key(|record| record.created_at);
        for record in fresh {
            self.next += 1;
            let local = self.next;
            self.shown.insert(record.approval_id.clone(), local);
            self.by_local.insert(local, record.approval_id.clone());
            changes.push(Change::Show(
                ui_prompt(&recorded_prompt(record)),
                EscalationId(local),
            ));
        }
        changes
    }

    /// The UI answered dialog `id`: the record to decide (the dialog is
    /// closed from now on).
    pub fn answer(&mut self, id: EscalationId) -> Option<ApprovalId> {
        let approval = self.by_local.remove(&id.0)?;
        self.shown.remove(&approval);
        Some(approval)
    }

    /// Forget every dialog (the connection is gone; the next list after a
    /// reattach shows what is still pending). Returns the dialogs to close.
    pub fn clear(&mut self) -> Vec<EscalationId> {
        self.shown.clear();
        self.by_local
            .drain()
            .map(|(local, _)| EscalationId(local))
            .collect()
    }
}

/// Why a dialog closed without this client's answer.
fn ended_notice(record: Option<&ApprovalRecord>) -> Option<String> {
    let record = record?;
    match record.status {
        ApprovalStatus::Expired => {
            Some("an approval request expired unanswered; the action was denied".to_string())
        }
        ApprovalStatus::Approved | ApprovalStatus::Denied => {
            let by_human = record
                .decision
                .as_ref()
                .is_some_and(|decision| decision.actor.as_str() == HUMAN);
            if by_human {
                let what = if record.status == ApprovalStatus::Approved {
                    "allowed"
                } else {
                    "denied"
                };
                Some(format!(
                    "an approval request was {what} from another client"
                ))
            } else {
                Some(
                    "an approval request ended unanswered (it timed out or its call was abandoned); the action was denied"
                        .to_string(),
                )
            }
        }
        ApprovalStatus::Cancelled => {
            Some("an approval request was cancelled; the action was denied".to_string())
        }
        ApprovalStatus::Pending => None,
    }
}

/// The dialog of a recorded prompt, as the in-process runner shows it.
#[must_use]
pub fn ui_prompt(prompt: &RecordedPrompt) -> EscalationPrompt {
    EscalationPrompt {
        agent: prompt.agent.clone(),
        tool: prompt.tool.clone(),
        args_preview: prompt.args_preview.clone(),
        hint: prompt.classifier_hint.clone(),
        reason: prompt.policy_reason.clone(),
        rule: prompt
            .subjects
            .first()
            .map(|subject| format!("{} {subject}", prompt.tool)),
    }
}

/// The `approval/decide` parameters for the UI's `answer` to record `id`.
#[must_use]
pub fn decide_params(id: &ApprovalId, answer: &EscalationAnswer) -> Value {
    let (remote, comment) = match answer {
        EscalationAnswer::AllowOnce => (RemoteAnswer::AllowOnce, None),
        EscalationAnswer::AllowAndAddRule => (RemoteAnswer::AllowAndAddRule, None),
        EscalationAnswer::Deny { comment } => (RemoteAnswer::Deny, comment.as_deref()),
        EscalationAnswer::Stop => (RemoteAnswer::Stop, None),
    };
    let mut params = json!({
        "approval_id": id,
        "decision": remote.decision(),
        "actor": HUMAN,
        "provenance": remote.provenance(comment),
    });
    if let Some(comment) = comment {
        params["reason"] = json!(comment);
    }
    params
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use meerkat_core::{
        ApprovalActionKind, ApprovalDecision, ApprovalOwnerRef, ApprovalPrincipalId,
        ApprovalProposedAction, ApprovalRequest, ApprovalResourceId, ApprovalResourceKind,
        ApprovalResourceRef, ApprovalRisk, ApprovalService, SurfaceMetadata,
    };

    fn request(service: &ApprovalService, session: &SessionId, requester: &str) -> ApprovalRecord {
        let prompt = RecordedPrompt {
            tool: "write_file".into(),
            subjects: vec!["a.txt".into()],
            args_preview: "a.txt".into(),
            policy_reason: Some("manual mode asks".into()),
            ..RecordedPrompt::default()
        };
        service
            .request(ApprovalRequest {
                requester: ApprovalPrincipalId::new(requester).unwrap(),
                owner: ApprovalOwnerRef::Session {
                    session_id: session.clone(),
                },
                resource: ApprovalResourceRef {
                    kind: ApprovalResourceKind::ToolCall,
                    id: ApprovalResourceId::new("call"),
                },
                proposed_action: ApprovalProposedAction {
                    kind: ApprovalActionKind::FileWrite,
                    summary: "Run write_file".into(),
                    body: None,
                },
                risk: ApprovalRisk::Medium,
                request_body: Some(serde_json::to_value(prompt).unwrap()),
                allowed_decisions: [ApprovalDecision::Approve, ApprovalDecision::Deny]
                    .into_iter()
                    .collect(),
                expires_at: None,
                metadata: SurfaceMetadata::default(),
                request_provenance: None,
            })
            .unwrap()
    }

    #[test]
    fn pending_gate_records_of_the_session_open_dialogs_once_and_close_when_gone() {
        let service = ApprovalService::new();
        let session = SessionId::new();
        let ours = request(&service, &session, crate::approval::ledger::REQUESTER);
        let other_session = request(
            &service,
            &SessionId::new(),
            crate::approval::ledger::REQUESTER,
        );
        let other_host = request(&service, &session, "another-host");
        let mut approvals = Approvals::default();
        let pending = vec![ours.clone(), other_session, other_host];
        let changes = approvals.update(&session, &pending, &[]);
        assert_eq!(changes.len(), 1, "{changes:?}");
        let Change::Show(prompt, id) = &changes[0] else {
            unreachable!("{changes:?}");
        };
        assert_eq!(prompt.tool, "write_file");
        assert_eq!(prompt.rule.as_deref(), Some("write_file a.txt"));
        assert_eq!(prompt.reason.as_deref(), Some("manual mode asks"));
        // Listed again: nothing new.
        assert!(approvals.update(&session, &pending, &[]).is_empty());
        // Decided by another client: the dialog closes with a notice.
        let decided = service
            .decide(
                &ours.approval_id,
                ApprovalDecision::Approve,
                ApprovalPrincipalId::new(HUMAN).unwrap(),
                None,
                None,
            )
            .unwrap();
        let changes = approvals.update(&session, &[], &[decided]);
        assert!(
            matches!(&changes[..], [Change::Resolve(closed, Some(text))] if closed == id && text.contains("another client")),
            "{changes:?}"
        );
        assert!(approvals.is_empty());
    }

    #[test]
    fn an_answered_dialog_names_its_record_and_closes() {
        let service = ApprovalService::new();
        let session = SessionId::new();
        let record = request(&service, &session, crate::approval::ledger::REQUESTER);
        let mut approvals = Approvals::default();
        let changes = approvals.update(&session, std::slice::from_ref(&record), &[]);
        let Some(Change::Show(_, id)) = changes.into_iter().next() else {
            unreachable!("a dialog");
        };
        assert_eq!(approvals.answer(id), Some(record.approval_id.clone()));
        assert_eq!(approvals.answer(id), None, "answered once");
        assert!(approvals.is_empty());
    }

    #[test]
    fn decide_params_carry_the_answer_and_the_comment() {
        let id = ApprovalId::new();
        let params = decide_params(
            &id,
            &EscalationAnswer::Deny {
                comment: Some("use git mv".into()),
            },
        );
        assert_eq!(params["decision"], "deny");
        assert_eq!(params["actor"], HUMAN);
        assert_eq!(params["reason"], "use git mv");
        assert_eq!(params["provenance"]["rkat_tui"]["answer"], "deny");
        assert_eq!(params["provenance"]["rkat_tui"]["comment"], "use git mv");
        let params = decide_params(&id, &EscalationAnswer::AllowAndAddRule);
        assert_eq!(params["decision"], "approve");
        assert_eq!(
            params["provenance"]["rkat_tui"]["answer"],
            "allow_and_add_rule"
        );
        assert!(params.get("reason").is_none());
        // The engine's own params type reads them.
        let parsed: meerkat_contracts::ApprovalDecideParams =
            serde_json::from_value(params).unwrap();
        assert_eq!(parsed.decision, ApprovalDecision::Approve);
    }
}
