//! The approval dialogs of attach mode.
//!
//! A daemon with decisions (its `initialize` lists `decision/notice`, P3b)
//! holds every escalation as a durable decision and pushes it to every
//! attached client: this client shows each `decision/notice` for the
//! orchestrator's session as the in-process dialog ([`Msg::Escalation`];
//! the notice's rendering is the dialog, [`DialogRendering`]), answers with
//! `decision/decide` ([`decision_params`]: the answer, the digest of what
//! was shown, the comment as the reason, and the dialog's answer for the
//! daemon), and closes a dialog another client answered, or that ended
//! unanswered, on the daemon's `rkat/decision_ended`
//! ([`Msg::EscalationResolved`]). A late decision of this client is refused
//! by the engine (`not_open`) and dropped.
//!
//! A daemon of an earlier version records every escalation in the realm's
//! approval store only: this client then lists the pending ones for the
//! orchestrator's session (`approval/list`, on every `tool_call_requested`
//! and while a turn runs or a dialog is open), shows each as the dialog and
//! answers with `approval/decide` ([`decide_params`]); a record another
//! client decided leaves the pending list and its dialog closes on the next
//! list.

use std::collections::{HashMap, HashSet};

use meerkat_core::{ApprovalId, ApprovalRecord, ApprovalStatus, SessionId};
use serde_json::{Value, json};

use crate::approval::escalation::RecordedPrompt;
use crate::approval::ledger::HUMAN;
use crate::approval::remote::{RemoteAnswer, is_gate_record, recorded_prompt};
use crate::daemon::decisions::{DialogRendering, KIND};
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
    /// Records this client answered whose decision may not have landed
    /// yet: a list read before it still has them pending, and they must
    /// not show again. Kept until a list no longer has them pending.
    deciding: HashSet<ApprovalId>,
    /// The decisions whose dialogs are open: their local id and the digest
    /// of the rendering shown.
    decisions: HashMap<String, (u64, String)>,
    decision_by_local: HashMap<u64, String>,
    next: u64,
}

impl Approvals {
    /// Whether a dialog is open.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.shown.is_empty() && self.decisions.is_empty()
    }

    /// A `decision/notice` (its params) the daemon pushed: the dialog to
    /// show, when it is a gate decision of `session` not shown yet.
    pub fn notice(&mut self, session: &SessionId, params: &Value) -> Option<Change> {
        let notice: meerkat_contracts::WireDecisionNotice =
            serde_json::from_value(params.clone()).ok()?;
        if notice.kind != KIND
            || notice.phase != meerkat_contracts::WireDecisionPhase::Open
            || self.decisions.contains_key(&notice.decision_id)
        {
            return None;
        }
        let presentation = notice.presentation?;
        let dialog = DialogRendering::parse(&presentation.rendering)?;
        if &dialog.session_id != session {
            return None;
        }
        self.next += 1;
        let local = self.next;
        self.decisions.insert(
            notice.decision_id.clone(),
            (local, presentation.subject_digest),
        );
        self.decision_by_local.insert(local, notice.decision_id);
        Some(Change::Show(ui_prompt(&dialog.prompt), EscalationId(local)))
    }

    /// The daemon's `rkat/decision_ended` (its params): close the dialog
    /// this client did not answer, saying how it ended.
    pub fn decision_ended(&mut self, params: &Value) -> Option<Change> {
        let decision = params.get("decision_id").and_then(Value::as_str)?;
        let (local, _) = self.decisions.remove(decision)?;
        self.decision_by_local.remove(&local);
        let phase = params.get("phase").and_then(Value::as_str).unwrap_or("");
        let text = match phase {
            "approved" | "executed" => "an approval request was allowed from another client",
            "rejected" => "an approval request was denied from another client",
            _ => {
                "an approval request ended unanswered (it timed out, the turn was interrupted, or its call was abandoned); the action was denied"
            }
        };
        Some(Change::Resolve(EscalationId(local), Some(text.to_string())))
    }

    /// The UI answered dialog `id` of a decision: the decision and the
    /// digest of what was shown (the dialog is closed from now on).
    pub fn answer_decision(&mut self, id: EscalationId) -> Option<(String, String)> {
        let decision = self.decision_by_local.remove(&id.0)?;
        let (_, digest) = self.decisions.remove(&decision)?;
        Some((decision, digest))
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
        self.deciding
            .retain(|id| ours.iter().any(|record| &record.approval_id == id));
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
            .filter(|record| !self.deciding.contains(&record.approval_id))
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
    /// closed from now on, and is not shown again while the record is
    /// still listed pending: the decision is on its way).
    pub fn answer(&mut self, id: EscalationId) -> Option<ApprovalId> {
        let approval = self.by_local.remove(&id.0)?;
        self.shown.remove(&approval);
        self.deciding.insert(approval.clone());
        Some(approval)
    }

    /// The decision for `approval` was not delivered: the next list shows
    /// its dialog again while it is pending.
    pub fn undecided(&mut self, approval: &ApprovalId) {
        self.deciding.remove(approval);
    }

    /// Forget every dialog (the connection is gone; the next list after a
    /// reattach shows what is still pending). Returns the dialogs to close.
    pub fn clear(&mut self) -> Vec<EscalationId> {
        self.shown.clear();
        self.deciding.clear();
        self.decisions.clear();
        self.by_local
            .drain()
            .map(|(local, _)| EscalationId(local))
            .chain(
                self.decision_by_local
                    .drain()
                    .map(|(local, _)| EscalationId(local)),
            )
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

/// The `decision/decide` parameters for the UI's `answer` to `decision`,
/// whose rendering (digest `digest`) the dialog showed: the comment of a
/// denial is the decision's reason; the dialog's answer travels beside it
/// for the daemon, which strips it before the engine sees the request.
#[must_use]
pub fn decision_params(decision: &str, digest: &str, answer: &EscalationAnswer) -> Value {
    let (remote, comment) = remote_answer(answer);
    let mut params = json!({
        "decision_id": decision,
        "approve": remote.decision() == meerkat_core::ApprovalDecision::Approve,
        "presented_digest": digest,
        "answer": remote,
    });
    if let Some(comment) = comment {
        params["reason"] = json!(comment);
    }
    params
}

fn remote_answer(answer: &EscalationAnswer) -> (RemoteAnswer, Option<&str>) {
    match answer {
        EscalationAnswer::AllowOnce => (RemoteAnswer::AllowOnce, None),
        EscalationAnswer::AllowAndAddRule => (RemoteAnswer::AllowAndAddRule, None),
        EscalationAnswer::Deny { comment } => (RemoteAnswer::Deny, comment.as_deref()),
        EscalationAnswer::Stop => (RemoteAnswer::Stop, None),
    }
}

/// The `approval/decide` parameters for the UI's `answer` to record `id`.
#[must_use]
pub fn decide_params(id: &ApprovalId, answer: &EscalationAnswer) -> Value {
    let (remote, comment) = remote_answer(answer);
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

    /// A list read before the decision landed still has the record
    /// pending: the answered dialog does not come back.
    #[test]
    fn an_answered_dialog_stays_closed_while_its_decision_is_on_its_way() {
        let service = ApprovalService::new();
        let session = SessionId::new();
        let record = request(&service, &session, crate::approval::ledger::REQUESTER);
        let pending = std::slice::from_ref(&record);
        let mut approvals = Approvals::default();
        let Some(Change::Show(_, id)) = approvals.update(&session, pending, &[]).into_iter().next()
        else {
            unreachable!("a dialog");
        };
        approvals.answer(id).unwrap();
        // The in-flight list: still pending, nothing shown.
        assert!(approvals.update(&session, pending, &[]).is_empty());
        assert!(approvals.is_empty());
        // The decision landed: the record leaves the list, and is forgotten.
        assert!(approvals.update(&session, &[], &[]).is_empty());
        assert!(approvals.deciding.is_empty());
        // An answer that was not delivered shows its dialog again.
        let Some(Change::Show(_, id)) = approvals.update(&session, pending, &[]).into_iter().next()
        else {
            unreachable!("shown again: it is pending and not being decided");
        };
        let approval = approvals.answer(id).unwrap();
        assert!(approvals.update(&session, pending, &[]).is_empty());
        approvals.undecided(&approval);
        assert!(matches!(
            &approvals.update(&session, pending, &[])[..],
            [Change::Show(..)]
        ));
    }

    fn notice(decision: &str, session: &SessionId) -> Value {
        let rendering = serde_json::to_string(&DialogRendering {
            session_id: session.clone(),
            prompt: RecordedPrompt {
                tool: "write_file".into(),
                subjects: vec!["a.txt".into()],
                ..RecordedPrompt::default()
            },
        })
        .unwrap();
        json!({
            "notice_id": "n1",
            "decision_id": decision,
            "kind": KIND,
            "event": "presented",
            "phase": "open",
            "approver_id": "person:alice",
            "requester": "principal:x",
            "presentation": {
                "rendering": rendering,
                "renderer": "rkat-tui.dialog-v1",
                "subject_digest": "sha256:abc",
                "parts": [],
            },
        })
    }

    #[test]
    fn a_pushed_decision_opens_one_dialog_and_another_clients_answer_closes_it() {
        let session = SessionId::new();
        let mut approvals = Approvals::default();
        // Another conversation's request is not shown here.
        assert!(
            approvals
                .notice(&SessionId::new(), &notice("d1", &session))
                .is_none()
        );
        let Some(Change::Show(prompt, id)) = approvals.notice(&session, &notice("d1", &session))
        else {
            unreachable!("a dialog");
        };
        assert_eq!(prompt.rule.as_deref(), Some("write_file a.txt"));
        assert!(
            approvals
                .notice(&session, &notice("d1", &session))
                .is_none(),
            "once"
        );
        let closed = approvals.decision_ended(&json!({"decision_id": "d1", "phase": "rejected"}));
        assert!(
            matches!(&closed, Some(Change::Resolve(closed, Some(text))) if *closed == id && text.contains("denied from another client")),
            "{closed:?}"
        );
        assert!(approvals.is_empty());
        // Answered here: the decision and the digest shown, closed for good.
        let Some(Change::Show(_, id)) = approvals.notice(&session, &notice("d2", &session)) else {
            unreachable!("a dialog");
        };
        assert_eq!(
            approvals.answer_decision(id),
            Some(("d2".to_string(), "sha256:abc".to_string()))
        );
        assert!(
            approvals
                .decision_ended(&json!({"decision_id": "d2", "phase": "approved"}))
                .is_none()
        );
        let params = decision_params(
            "d2",
            "sha256:abc",
            &EscalationAnswer::Deny {
                comment: Some("no".into()),
            },
        );
        assert_eq!(
            params,
            json!({"decision_id": "d2", "approve": false, "presented_digest": "sha256:abc", "answer": "deny", "reason": "no"})
        );
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
