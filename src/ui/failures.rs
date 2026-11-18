//! How a run's failure is said: once, and an empty answer in plain words.
//!
//! The engine fails a run whose model call returns no visible text, no
//! image and no tool call (and nothing visible came earlier in the run):
//! it retries the call (`⟳ retry n/3`), then ends the run with its
//! `llm_empty_response` error (`MeerkatMachine`'s empty-response terminal;
//! not configurable per run or per host). A run nobody typed (another
//! agent's message, a schedule, a helper's ending) whose model has nothing
//! to add ends so too; that is no failure the user needs to act on. And a
//! failed run was said twice: as the run's `error: …` and as the turn's
//! `turn failed: …`, with the same text.
//!
//! So, per run ([`RunFailures`]): the empty-answer retries draw nothing;
//! the empty answer's failure is one quiet line on a run the user did not
//! start (`the agent had nothing to add to main-ab12's message`, `… (scheduled: nightly)`), one
//! plain error on a run the user started (`the model returned an empty
//! answer 4 times: …`); any failure the run's end said already is not
//! said again by the turn's end.
//!
//! The empty answer is recognised by its typed cause where there is one:
//! a run failed at the first empty answer reports the provider error kind
//! `IncompleteResponse` with the engine's `reason` detail. Only prose says
//! it elsewhere, so there the engine's words are read ([`EMPTY_ANSWER`]):
//! a retry's schedule carries only `RetryableProviderError` and that
//! message, and a run failed after its retries reports only the terminal
//! cause `RetryExhausted`, the retried error in its message.

use meerkat_core::error::LlmProviderErrorKind;
use meerkat_core::event::{AgentErrorReason, AgentErrorReport};
use meerkat_core::{LlmRetryFailureKind, LlmRetrySchedule, TurnTerminalCauseKind};

/// The engine's words for an empty answer (`AgentError::llm_empty_response`).
pub const EMPTY_ANSWER: &str = "completed without user-visible text, images, or tool calls";

/// Whether a run's failure `report` is the engine's empty answer.
#[must_use]
pub fn is_empty_answer(report: &AgentErrorReport) -> bool {
    match &report.reason {
        Some(AgentErrorReason::LlmProviderError {
            provider_error_kind: LlmProviderErrorKind::IncompleteResponse,
            provider_error,
            ..
        }) => provider_error
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|reason| reason.contains(EMPTY_ANSWER)),
        // After its retries the run's report keeps only the terminal
        // cause (`RetryExhausted`), not the provider's kind: the message
        // is all that says which error was retried.
        Some(AgentErrorReason::TurnTerminalCause {
            cause_kind: TurnTerminalCauseKind::RetryExhausted,
            ..
        }) => report.message.ends_with(EMPTY_ANSWER),
        _ => false,
    }
}

/// Whether a retry is of an empty answer (only its message says so).
#[must_use]
pub fn is_empty_answer_retry(retry: &LlmRetrySchedule) -> bool {
    retry.failure.kind == LlmRetryFailureKind::RetryableProviderError
        && retry.failure.message.contains(EMPTY_ANSWER)
}

/// What one run's failures have said so far.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct RunFailures {
    /// Who started the run when the user did not, as the quiet line ends
    /// (` to main-ab12's message`, ` (scheduled: nightly)`); `None`: the
    /// user (or not known).
    pub(super) origin: Option<String>,
    /// This client did not start the run (it joined it: another client's
    /// prompt, or one nobody typed).
    pub(super) remote: bool,
    /// The empty-answer retries not drawn.
    pub(super) empty_retries: u32,
    /// The run's failure was said already: the turn's end says nothing more.
    pub(super) said: bool,
}

impl RunFailures {
    /// A run starts: `origin` started it (`None`: the user); `remote`:
    /// not this client.
    pub(super) fn start(origin: Option<String>, remote: bool) -> Self {
        Self {
            origin,
            remote,
            ..Self::default()
        }
    }

    /// The line an empty answer's failure is said in, and whether it is
    /// an error (a run the user started) or a quiet line.
    pub(super) fn empty_answer_line(&self) -> (super::NoticeLevel, String) {
        match &self.origin {
            Some(origin) => (
                super::NoticeLevel::Info,
                format!("the agent had nothing to add{origin}"),
            ),
            None => {
                let calls = self.empty_retries + 1;
                let times = if calls == 1 {
                    "an empty answer".to_string()
                } else {
                    format!("an empty answer {calls} times")
                };
                (
                    super::NoticeLevel::Error,
                    format!(
                        "the model returned {times}: send the message again, or switch the model (/model)"
                    ),
                )
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use meerkat_core::event::AgentErrorClass;

    fn report(reason: Option<AgentErrorReason>) -> AgentErrorReport {
        AgentErrorReport {
            class: AgentErrorClass::Llm,
            reason,
            message: "x".into(),
        }
    }

    /// The engine's own empty-answer error is recognised by its typed
    /// cause, from its report as the engine makes it; another incomplete
    /// response or another failure is not.
    #[test]
    fn an_empty_answer_is_recognised_by_its_typed_cause() {
        let engine = AgentErrorReport::from_agent_error(
            &meerkat_core::AgentError::llm_empty_response("self_hosted"),
        );
        assert!(is_empty_answer(&engine), "{engine:?}");
        // After the retries (as the rig's engine reports it).
        let exhausted = |message: &str| AgentErrorReport {
            class: AgentErrorClass::Llm,
            reason: Some(AgentErrorReason::TurnTerminalCause {
                outcome: meerkat_core::TurnTerminalOutcome::Failed,
                cause_kind: TurnTerminalCauseKind::RetryExhausted,
            }),
            message: message.into(),
        };
        assert!(is_empty_answer(&exhausted(
            "Terminal failure: Failed (RetryExhausted): retry exhausted: LLM error (anthropic): LLM completed without user-visible text, images, or tool calls"
        )));
        assert!(!is_empty_answer(&exhausted(
            "Terminal failure: Failed (RetryExhausted): retry exhausted: LLM error (anthropic): rate limited"
        )));
        let other = report(Some(AgentErrorReason::LlmProviderError {
            provider_error_kind: LlmProviderErrorKind::IncompleteResponse,
            provider_error_retryability:
                meerkat_core::error::LlmProviderErrorRetryability::Retryable,
            provider_error: serde_json::json!({"reason": "the stream ended early"}),
        }));
        assert!(!is_empty_answer(&other));
        assert!(!is_empty_answer(&report(None)));
        assert!(!is_empty_answer(&report(Some(
            AgentErrorReason::LlmAuthError
        ))));
    }

    /// A user's run says the empty answer as one plain error with the
    /// count; another's run as one quiet line naming who started it.
    #[test]
    fn the_empty_answer_is_said_by_who_started_the_run() {
        let mut user = RunFailures::start(None, false);
        user.empty_retries = 3;
        let (level, text) = user.empty_answer_line();
        assert_eq!(level, super::super::NoticeLevel::Error);
        assert_eq!(
            text,
            "the model returned an empty answer 4 times: send the message again, or switch the model (/model)"
        );
        let peer = RunFailures::start(Some(" to main-ab12's message".into()), true);
        assert_eq!(
            peer.empty_answer_line(),
            (
                super::super::NoticeLevel::Info,
                "the agent had nothing to add to main-ab12's message".into()
            )
        );
    }
}
