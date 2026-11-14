//! The approval dialogs of the client.
//!
//! The daemon (its `initialize` lists `decision/notice`; a daemon
//! without it is refused at attach) holds every escalation as a durable decision and pushes it to every
//! attached client: this client shows each `decision/notice` for the
//! agent's session as the approval dialog ([`Msg::Escalation`];
//! the notice's rendering is the dialog, [`DialogRendering`]), answers with
//! `decision/decide` ([`decision_params`]: the answer, the digest of what
//! was shown, the comment as the reason, and the dialog's answer for the
//! daemon), and closes a dialog another client answered, or that ended
//! unanswered, on the daemon's `rkat/decision_ended`
//! ([`Msg::EscalationResolved`]). A late decision of this client is refused
//! by the engine (`not_open`) and dropped.

use std::collections::HashMap;

use meerkat_contracts::WireDecisionPhase;
use meerkat_core::SessionId;

use crate::approval::escalation::RecordedPrompt;
use crate::daemon::decisions::{DialogRendering, KIND};
use crate::daemon::protocol::{ApprovalNote, DecideParams, DecisionEnded};
use crate::ui::{ApprovalNotice, EscalationAnswer, EscalationId, Msg, NoticeLevel, StatusUpdate};

/// Most characters of the classifier's reason in an allow note.
pub const MAX_ALLOW_REASON_CHARS: usize = 100;

/// The note under a tool block the classifier allowed:
/// `allowed by classifier: <reason>`, the reason on one line and cut to
/// [`MAX_ALLOW_REASON_CHARS`].
#[must_use]
pub fn classifier_allow_note(reason: Option<&str>) -> String {
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

/// What the UI shows for an approval note the daemon pushed
/// ([`crate::daemon::protocol::APPROVAL_NOTE`]) from the engine's events
/// (the mode and denial counters they move come as a push of their own,
/// [`crate::daemon::protocol::MODE`]). Tier-1/2 allows are routine and show
/// nothing; a classifier allow is one dim line under its tool block; other
/// reviewed or denied actions get a line of their own.
#[must_use]
pub fn note_messages(note: ApprovalNote) -> Vec<Msg> {
    use crate::approval::policy::{Decider, Tier, VerdictSummary};
    match note {
        ApprovalNote::Decision {
            agent,
            tool_use_id,
            tool,
            verdict,
            reason,
            ..
        } => {
            let tool = match agent {
                Some(agent) => format!("[{agent}] {tool}"),
                None => tool,
            };
            let (tier, decider, allowed) = match verdict {
                VerdictSummary::Allowed { tier, by } => (tier, by, true),
                VerdictSummary::Denied { tier, by } => (tier, by, false),
            };
            if allowed && decider == Decider::Classifier {
                vec![Msg::ToolNote {
                    tool_use_id,
                    text: classifier_allow_note(reason.as_deref()),
                }]
            } else if !allowed
                || decider == Decider::Human
                || matches!(tier, Tier::Classifier | Tier::AskUser)
            {
                // A classifier block can be approved: the notice says how.
                let reason = if !allowed && decider == Decider::Classifier {
                    reason.map(|reason| format!("{reason} (/approve lists it)"))
                } else {
                    reason
                };
                vec![Msg::Approval(ApprovalNotice {
                    tool,
                    tier: format!("{tier} · {decider:?}").to_lowercase(),
                    allowed,
                    reason,
                })]
            } else {
                Vec::new()
            }
        }
        ApprovalNote::Paused { cause } => vec![Msg::Notice {
            level: NoticeLevel::Warn,
            text: format!(
                "auto mode paused ({cause}): reviewed actions now ask you; /auto resume to continue"
            ),
        }],
        ApprovalNote::Resumed => Vec::new(),
        ApprovalNote::RulesDropped { rules } => vec![Msg::Notice {
            level: NoticeLevel::Warn,
            text: format!(
                "auto mode dropped {} broad allow rule(s): {}",
                rules.len(),
                rules.join(", ")
            ),
        }],
        ApprovalNote::Classifying { running } => {
            vec![Msg::Status(StatusUpdate::Classifying(running))]
        }
        ApprovalNote::Notice { text } => vec![Msg::Notice {
            level: NoticeLevel::Warn,
            text,
        }],
        ApprovalNote::CheckProgress {
            done,
            total,
            running,
        } => vec![Msg::Status(StatusUpdate::ClassifierCheck(
            running.then_some((done, total)),
        ))],
    }
}

/// What a pushed decision changes in the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Show a new dialog.
    Show(RecordedPrompt, EscalationId),
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
    #[cfg(any(test, feature = "test-support"))]
    pub fn is_empty(&self) -> bool {
        self.decisions.is_empty()
    }

    /// A `decision/notice` (its params) the daemon pushed: the dialog to
    /// show, when it is a gate decision of `session` not shown yet.
    pub fn notice(
        &mut self,
        session: &SessionId,
        notice: meerkat_contracts::WireDecisionNotice,
    ) -> Option<Change> {
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
        Some(Change::Show(dialog.prompt, EscalationId(local)))
    }

    /// The daemon's `rkat/decision_ended` (its params): close the dialog
    /// this client did not answer, saying how it ended.
    pub fn decision_ended(&mut self, ended: &DecisionEnded) -> Option<Change> {
        let (local, _) = self.decisions.remove(&ended.decision_id)?;
        self.decision_by_local.remove(&local);
        let text = match ended.phase {
            Some(WireDecisionPhase::Approved | WireDecisionPhase::Executed) => {
                "an approval request was allowed from another client"
            }
            Some(WireDecisionPhase::Rejected) => {
                "an approval request was denied from another client"
            }
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

/// The `decision/decide` parameters for the UI's `answer` to `decision`,
/// whose rendering (digest `digest`) the dialog showed: the comment of a
/// denial is the decision's reason; the dialog's answer travels beside it
/// for the daemon, which strips it before the engine sees the request.
#[must_use]
pub fn decision_params(decision: &str, digest: &str, answer: &EscalationAnswer) -> DecideParams {
    let (remote, comment) = answer.remote();
    DecideParams {
        decision_id: decision.to_string(),
        approve: remote.decision() == meerkat_core::ApprovalDecision::Approve,
        presented_digest: digest.to_string(),
        reason: comment.map(ToString::to_string),
        answer: Some(remote),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

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
    use serde_json::json;
    fn notice(decision: &str, session: &SessionId) -> meerkat_contracts::WireDecisionNotice {
        let rendering = serde_json::to_string(&DialogRendering {
            session_id: session.clone(),
            prompt: RecordedPrompt {
                tool: "write_file".into(),
                subjects: vec!["a.txt".into()],
                ..RecordedPrompt::default()
            },
        })
        .unwrap();
        serde_json::from_value(json!({
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
        }))
        .unwrap()
    }

    fn ended(decision: &str, phase: &str) -> DecisionEnded {
        serde_json::from_value(json!({"decision_id": decision, "phase": phase})).unwrap()
    }

    #[test]
    fn a_pushed_decision_opens_one_dialog_and_another_clients_answer_closes_it() {
        let session = SessionId::new();
        let mut approvals = Approvals::default();
        // Another conversation's request is not shown here.
        assert!(
            approvals
                .notice(&SessionId::new(), notice("d1", &session))
                .is_none()
        );
        let Some(Change::Show(prompt, id)) = approvals.notice(&session, notice("d1", &session))
        else {
            unreachable!("a dialog");
        };
        assert_eq!(prompt.rule().as_deref(), Some("write_file a.txt"));
        assert!(
            approvals.notice(&session, notice("d1", &session)).is_none(),
            "once"
        );
        let closed = approvals.decision_ended(&ended("d1", "rejected"));
        assert!(
            matches!(&closed, Some(Change::Resolve(closed, Some(text))) if *closed == id && text.contains("denied from another client")),
            "{closed:?}"
        );
        assert!(approvals.is_empty());
        // Answered here: the decision and the digest shown, closed for good.
        let Some(Change::Show(_, id)) = approvals.notice(&session, notice("d2", &session)) else {
            unreachable!("a dialog");
        };
        assert_eq!(
            approvals.answer_decision(id),
            Some(("d2".to_string(), "sha256:abc".to_string()))
        );
        assert!(approvals.decision_ended(&ended("d2", "approved")).is_none());
        let params = decision_params(
            "d2",
            "sha256:abc",
            &EscalationAnswer::Deny {
                comment: Some("no".into()),
            },
        );
        assert_eq!(
            serde_json::to_value(params).unwrap(),
            json!({"decision_id": "d2", "approve": false, "presented_digest": "sha256:abc", "answer": "deny", "reason": "no"})
        );
    }
}
