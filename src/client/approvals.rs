//! The approval dialogs of attach mode.
//!
//! The daemon (its `initialize` lists `decision/notice`, P3b; a daemon
//! without it is refused at attach) holds every escalation as a durable decision and pushes it to every
//! attached client: this client shows each `decision/notice` for the
//! orchestrator's session as the in-process dialog ([`Msg::Escalation`];
//! the notice's rendering is the dialog, [`DialogRendering`]), answers with
//! `decision/decide` ([`decision_params`]: the answer, the digest of what
//! was shown, the comment as the reason, and the dialog's answer for the
//! daemon), and closes a dialog another client answered, or that ended
//! unanswered, on the daemon's `rkat/decision_ended`
//! ([`Msg::EscalationResolved`]). A late decision of this client is refused
//! by the engine (`not_open`) and dropped.

use std::collections::HashMap;

use meerkat_core::SessionId;
use serde_json::{Value, json};

use crate::approval::escalation::RecordedPrompt;
use crate::approval::remote::RemoteAnswer;
use crate::daemon::decisions::{DialogRendering, KIND};
use crate::ui::{EscalationAnswer, EscalationId, EscalationPrompt, Msg};

/// What a pushed decision changes in the UI.
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
        self.decisions.is_empty()
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

    /// Forget every dialog (the connection is gone; the daemon pushes what
    /// is still open again after a reattach). Returns the dialogs to close.
    pub fn clear(&mut self) -> Vec<EscalationId> {
        self.decisions.clear();
        self.decision_by_local
            .drain()
            .map(|(local, _)| EscalationId(local))
            .collect()
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
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
}
