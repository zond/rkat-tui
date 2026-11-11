//! Human escalation broker.
//!
//! The admission gate decides parallel tool calls concurrently, but a
//! terminal shows one dialog at a time, so the broker serializes the
//! requests of one session (an agent's conversation; a helper's calls
//! belong to its owner's) with an async mutex held across the whole wait.
//! A client shows the dialogs of its own agent's session only, so
//! different sessions never wait on each other: agent B's question is shown
//! on B's terminal while agent A's dialog stays open on A's. Any failure to
//! get an answer (UI gone, responder dropped) resolves as
//! [`HumanDecision::Deny`] without a comment: fail closed.

use std::collections::HashMap;
use std::sync::Arc;

use super::rules::Rule;
use meerkat_core::SessionId;
use tokio::sync::{Mutex, OwnedMutexGuard, mpsc, oneshot};

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
    /// Short rendering of the arguments (says when it is abbreviated).
    #[serde(default)]
    pub args_preview: String,
    /// The whole arguments when the preview abbreviates them (the
    /// dialog's Ctrl+O shows them).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args_full: Option<String>,
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

/// What one session's serial lock remembers between its dialogs.
#[derive(Debug, Default)]
pub struct Serial {
    /// When a dialog of the session last went unanswered for its whole
    /// budget: the human is not at that terminal, so a call that queued
    /// behind it is denied as timed out too, rather than shown to nobody
    /// for a budget of its own.
    pub unanswered_at: Option<tokio::time::Instant>,
}

/// One session's serial lock, held while its dialog is open.
pub type SerialGuard = OwnedMutexGuard<Serial>;

/// Serializes each session's human escalations and forwards them to the
/// UI.
#[derive(Debug)]
pub struct EscalationBroker {
    /// One lock per session with a dialog open or queued (an entry nobody
    /// holds or waits for is dropped at the next acquisition).
    serial: parking_lot::Mutex<HashMap<SessionId, Arc<Mutex<Serial>>>>,
    tx: mpsc::Sender<EscalationRequest>,
}

impl EscalationBroker {
    /// Create a broker and the receiver the UI reads requests from.
    pub fn new(capacity: usize) -> (Self, mpsc::Receiver<EscalationRequest>) {
        let (tx, rx) = mpsc::channel(capacity.max(1));
        (
            Self {
                serial: parking_lot::Mutex::new(HashMap::new()),
                tx,
            },
            rx,
        )
    }

    /// Acquire `session`'s serial lock: it waits while an earlier dialog
    /// of the same session is open (each bounded by its own budget), never
    /// for another session's. Callers re-check their state after this
    /// returns and then call [`Self::ask_locked`].
    pub async fn serial(&self, session: &SessionId) -> SerialGuard {
        let lock = {
            let mut locks = self.serial.lock();
            // Nobody holds or waits for these: their sessions have no
            // dialog open.
            locks.retain(|_, lock| Arc::strong_count(lock) > 1);
            Arc::clone(locks.entry(session.clone()).or_default())
        };
        lock.lock_owned().await
    }

    /// How many sessions have a serial lock now (tests).
    #[cfg(test)]
    fn sessions(&self) -> usize {
        self.serial.lock().len()
    }

    /// Ask the human while holding the session's serial lock.
    pub async fn ask_locked(
        &self,
        _guard: &SerialGuard,
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

    /// Acquire the prompt's session's serial lock and ask the human.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn ask(&self, prompt: EscalationPrompt) -> HumanDecision {
        let guard = self.serial(&prompt.session_id).await;
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
        prompt_in(tool, &SessionId::new())
    }

    fn prompt_in(tool: &str, session: &SessionId) -> EscalationPrompt {
        EscalationPrompt {
            id: 0,
            session_id: session.clone(),
            shown: RecordedPrompt {
                tool: tool.into(),
                args_preview: "{}".into(),
                subjects: vec!["{}".into()],
                ..RecordedPrompt::default()
            },
        }
    }

    #[tokio::test]
    async fn concurrent_asks_of_one_session_are_served_one_at_a_time() {
        let (broker, mut rx) = EscalationBroker::new(8);
        let broker = Arc::new(broker);
        let session = SessionId::new();
        let a = tokio::spawn({
            let b = Arc::clone(&broker);
            let session = session.clone();
            async move { b.ask(prompt_in("a", &session)).await }
        });
        let b = tokio::spawn({
            let b = Arc::clone(&broker);
            let session = session.clone();
            async move { b.ask(prompt_in("b", &session)).await }
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

    /// Another session's question is never queued behind an open dialog:
    /// both are delivered while neither is answered, and the locks of
    /// sessions with nothing open are dropped.
    #[tokio::test]
    async fn another_sessions_ask_is_not_held_by_an_open_dialog() {
        let (broker, mut rx) = EscalationBroker::new(8);
        let broker = Arc::new(broker);
        let a = tokio::spawn({
            let b = Arc::clone(&broker);
            async move { b.ask(prompt("a")).await }
        });
        let first = rx.recv().await.unwrap();
        let b = tokio::spawn({
            let b = Arc::clone(&broker);
            async move { b.ask(prompt("b")).await }
        });
        let second = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("the other session's dialog waited behind the open one")
            .unwrap();
        assert_ne!(first.prompt.session_id, second.prompt.session_id);
        second.respond.send(HumanDecision::AllowOnce).unwrap();
        assert_eq!(b.await.unwrap(), HumanDecision::AllowOnce);
        first.respond.send(HumanDecision::AllowOnce).unwrap();
        assert_eq!(a.await.unwrap(), HumanDecision::AllowOnce);
        // A third ask drops the two idle locks before taking its own.
        let c = tokio::spawn({
            let b = Arc::clone(&broker);
            async move { b.ask(prompt("c")).await }
        });
        let third = rx.recv().await.unwrap();
        assert_eq!(broker.sessions(), 1);
        third.respond.send(HumanDecision::AllowOnce).unwrap();
        c.await.unwrap();
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
