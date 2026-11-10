//! Human escalation broker.
//!
//! The admission gate decides parallel tool calls concurrently, but the human can
//! only answer one dialog at a time, so the broker serializes requests with an
//! async mutex held across the whole wait. Any failure to get an answer (UI
//! gone, responder dropped) resolves as [`HumanDecision::Deny`] without a comment: fail closed.

use super::rules::Rule;
use meerkat_core::SessionId;
use tokio::sync::{Mutex, MutexGuard, mpsc, oneshot};

/// The human's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HumanDecision {
    /// Allow this one call.
    AllowOnce,
    /// Allow and add the given (narrow) allow rule.
    AllowAndAddRule(Box<Rule>),
    /// Deny this call.
    Deny {
        /// What the user told the agent to do instead, if anything.
        comment: Option<String>,
    },
    /// Deny and stop the turn; pending escalations for the session are denied too.
    Stop,
}

/// An escalation put to the human: which call it is, and what the dialog
/// shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EscalationPrompt {
    /// Engine-assigned id; a withdrawn escalation
    /// ([`super::ApprovalEvent::EscalationWithdrawn`]) names it.
    pub id: u64,
    /// Session the call belongs to.
    pub session_id: SessionId,
    /// What the dialog shows.
    pub shown: RecordedPrompt,
}

/// What the escalation dialog shows in a client of the daemon (a durable
/// decision's rendering carries it).
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct RecordedPrompt {
    /// Tool name.
    pub tool: String,
    /// The helper making the call (`None`: the session's own agent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Short rendering of the arguments.
    #[serde(default)]
    pub args_preview: String,
    /// Canonical rule subjects of the action (see
    /// [`super::policy::Action::subjects`]); "allow + add rule" mints a
    /// literal rule from the first one.
    #[serde(default)]
    pub subjects: Vec<String>,
    /// The classifier's opinion, if it produced one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classifier_hint: Option<String>,
    /// Why the policy or the permission mode asks without the classifier
    /// (a protected path: [`super::policy::Tier::AskUser`]), if it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_reason: Option<String>,
}

impl RecordedPrompt {
    /// The allow rule "Yes, allow and add a rule" would add, as the dialog
    /// names it (`shell ls -la`); `None` when the action has no rule
    /// subject (the choice then allows once).
    #[must_use]
    pub fn rule(&self) -> Option<String> {
        self.subjects
            .first()
            .map(|subject| format!("{} {subject}", self.tool))
    }
}

/// An escalation delivered to the UI: the prompt and the responder.
#[derive(Debug)]
pub struct EscalationRequest {
    /// What to show.
    pub prompt: EscalationPrompt,
    /// Where to send the answer.
    pub respond: oneshot::Sender<HumanDecision>,
}

/// Serializes human escalations and forwards them to the UI.
#[derive(Debug)]
pub struct EscalationBroker {
    serial: Mutex<()>,
    tx: mpsc::Sender<EscalationRequest>,
}

impl EscalationBroker {
    /// Create a broker and the receiver the UI reads requests from.
    pub fn new(capacity: usize) -> (Self, mpsc::Receiver<EscalationRequest>) {
        let (tx, rx) = mpsc::channel(capacity.max(1));
        (
            Self {
                serial: Mutex::new(()),
                tx,
            },
            rx,
        )
    }

    /// Acquire the serial lock. Callers bound the wait (the engine waits for
    /// the lock and the answer inside one escalation timeout), re-check their
    /// state after this returns and then call [`Self::ask_locked`].
    pub async fn serial(&self) -> MutexGuard<'_, ()> {
        self.serial.lock().await
    }

    /// Ask the human while holding the serial lock.
    pub async fn ask_locked(
        &self,
        _guard: &MutexGuard<'_, ()>,
        prompt: EscalationPrompt,
    ) -> HumanDecision {
        let (respond, answer) = oneshot::channel();
        if self
            .tx
            .send(EscalationRequest { prompt, respond })
            .await
            .is_err()
        {
            return HumanDecision::Deny { comment: None };
        }
        answer
            .await
            .unwrap_or(HumanDecision::Deny { comment: None })
    }

    /// Acquire the serial lock and ask the human.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn ask(&self, prompt: EscalationPrompt) -> HumanDecision {
        let guard = self.serial().await;
        self.ask_locked(&guard, prompt).await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    fn prompt(tool: &str) -> EscalationPrompt {
        EscalationPrompt {
            id: 0,
            session_id: SessionId::new(),
            shown: RecordedPrompt {
                tool: tool.into(),
                args_preview: "{}".into(),
                subjects: vec!["{}".into()],
                ..RecordedPrompt::default()
            },
        }
    }

    #[tokio::test]
    async fn concurrent_asks_are_served_one_at_a_time() {
        let (broker, mut rx) = EscalationBroker::new(8);
        let broker = Arc::new(broker);
        let a = tokio::spawn({
            let b = Arc::clone(&broker);
            async move { b.ask(prompt("a")).await }
        });
        let b = tokio::spawn({
            let b = Arc::clone(&broker);
            async move { b.ask(prompt("b")).await }
        });

        let first = rx.recv().await.unwrap();
        // While the first request is unanswered, no second request may arrive.
        assert!(
            tokio::time::timeout(Duration::from_millis(100), rx.recv())
                .await
                .is_err(),
            "second escalation delivered before the first was answered"
        );
        let first_tool = first.prompt.shown.tool.clone();
        first.respond.send(HumanDecision::AllowOnce).unwrap();
        let second = rx.recv().await.unwrap();
        assert_ne!(second.prompt.shown.tool, first_tool);
        second
            .respond
            .send(HumanDecision::Deny { comment: None })
            .unwrap();

        let (ra, rb) = (a.await.unwrap(), b.await.unwrap());
        let mut answers = [ra, rb];
        answers.sort_by_key(|d| matches!(d, HumanDecision::Deny { .. }));
        assert_eq!(
            answers,
            [
                HumanDecision::AllowOnce,
                HumanDecision::Deny { comment: None }
            ]
        );
    }

    #[tokio::test]
    async fn dropped_responder_denies() {
        let (broker, mut rx) = EscalationBroker::new(1);
        let task = tokio::spawn(async move { broker.ask(prompt("x")).await });
        let req = rx.recv().await.unwrap();
        drop(req);
        assert_eq!(task.await.unwrap(), HumanDecision::Deny { comment: None });
    }

    #[tokio::test]
    async fn dropped_receiver_denies() {
        let (broker, rx) = EscalationBroker::new(1);
        drop(rx);
        assert_eq!(
            broker.ask(prompt("x")).await,
            HumanDecision::Deny { comment: None }
        );
    }
}
