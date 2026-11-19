//! The inputs a client sent its agent: `rkat/recall` takes back the
//! queued ones, `rkat/inputs` reads what became of them.

use super::*;

impl Local {
    /// `rkat/recall` for `caller` ([`Self::take_back`]); what was taken
    /// back may leave the agent idle.
    pub(super) async fn recall(
        &self,
        caller: &Caller,
        inputs: &[meerkat_core::lifecycle::InputId],
    ) -> Result<protocol::Recalled, Refusal> {
        let recalled = self.take_back(caller, inputs).await;
        if let Some(agent) = &caller.agent {
            self.core.activity.changed(agent, false);
        }
        recalled
    }

    /// The queued `inputs` of `caller`'s agent, taken back: only its own
    /// agent's current conversation is searched, so another agent's input
    /// is never touched. The engine decides each in one step
    /// (`MeerkatMachine::recall_queued_input`): an input still queued is
    /// abandoned; one a run has taken, or one that ended, is left as it is
    /// (the agent has it), and no run is ever interrupted.
    ///
    /// # Errors
    ///
    /// [`protocol::NO_AGENT`] when the connection is bound to no agent or
    /// its agent has no conversation.
    async fn take_back(
        &self,
        caller: &Caller,
        inputs: &[meerkat_core::lifecycle::InputId],
    ) -> Result<protocol::Recalled, Refusal> {
        let session = self.caller_session(caller, "rkat/recall takes back")?;
        let mut answer = protocol::Recalled::default();
        for input in inputs {
            match self
                .parts
                .machine
                .recall_queued_input(&session, input)
                .await
            {
                Ok(Some(recall)) if recall.abandoned() => answer.recalled.push(input.clone()),
                Ok(Some(_)) => answer.reached.push(input.clone()),
                Ok(None) => answer.foreign.push(input.clone()),
                Err(err) => answer.errors.push(format!("input {input}: {err}")),
            }
        }
        if !answer.recalled.is_empty() {
            tracing::info!(recalled = answer.recalled.len(), %session, "rkat/recall");
        }
        Ok(answer)
    }

    /// The caller's agent's current conversation; `doing` names the method
    /// in its refusal.
    pub(super) fn caller_session(
        &self,
        caller: &Caller,
        doing: &str,
    ) -> Result<meerkat_core::SessionId, Refusal> {
        caller
            .agent
            .as_deref()
            .and_then(|agent| self.core.conversations.current(agent))
            .ok_or_else(|| {
                Refusal::new(
                    protocol::NO_AGENT,
                    format!("{doing} only the messages of the agent this connection works with"),
                )
            })
    }

    /// `rkat/inputs` for `caller`: what the runtime records of each input
    /// of its agent's current conversation ([`protocol::InputFate`]). Read
    /// only; an input that cannot be read is left out (its sender keeps
    /// waiting); [`protocol::NO_AGENT`] as for `rkat/recall`.
    pub(super) async fn input_fates(
        &self,
        caller: &Caller,
        inputs: &[meerkat_core::lifecycle::InputId],
    ) -> Result<protocol::InputFates, Refusal> {
        use meerkat_runtime::SessionServiceRuntimeExt as _;

        let session = self.caller_session(caller, "rkat/inputs reads")?;
        let mut answer = protocol::InputFates::default();
        for input in inputs {
            let fate = match self.parts.machine.input_state(&session, input).await {
                Ok(Some(stored)) => input_fate(&stored.seed),
                Ok(None) => protocol::InputFate::Unknown,
                Err(err) => {
                    tracing::warn!(%input, %err, "rkat/inputs: the input's state was not read");
                    continue;
                }
            };
            answer.inputs.push(protocol::InputRow {
                input_id: input.clone(),
                fate,
            });
        }
        Ok(answer)
    }
}

/// What the runtime's record of an input says became of it: still held,
/// written into a run, or ended without one (and why, in words).
pub(super) fn input_fate(
    seed: &meerkat_runtime::input_state::InputStateSeed,
) -> protocol::InputFate {
    use meerkat_runtime::input_state::{InputLifecycleState as Phase, InputTerminalOutcome};

    match seed.phase {
        Phase::Applied | Phase::AppliedPendingConsumption | Phase::Consumed | Phase::Coalesced => {
            protocol::InputFate::Taken
        }
        Phase::Superseded | Phase::Abandoned => protocol::InputFate::Dropped {
            why: match &seed.terminal_outcome {
                Some(InputTerminalOutcome::Abandoned { reason }) => abandon_words(reason),
                Some(InputTerminalOutcome::Superseded { .. }) => {
                    Some("a later message replaced it".to_string())
                }
                _ => None,
            },
        },
        // Accepted, queued, staged, or a phase this build does not know:
        // nothing says it ended.
        _ => protocol::InputFate::Pending,
    }
}

/// Why the runtime abandoned an input, in words (`None`: a reason this
/// build does not know).
fn abandon_words(reason: &meerkat_runtime::input_state::InputAbandonReason) -> Option<String> {
    use meerkat_runtime::input_state::InputAbandonReason as Reason;

    Some(match reason {
        Reason::Cancelled => "it was cancelled".to_string(),
        Reason::Stopped => "the agent's runtime was stopped".to_string(),
        Reason::Retired => "the agent retired".to_string(),
        Reason::Reset => "its conversation was reset".to_string(),
        Reason::Destroyed => "the agent was destroyed".to_string(),
        Reason::MaxAttemptsExhausted { attempts } => {
            format!("its run failed {attempts} times")
        }
        Reason::NeverExecuted => "the run it was staged on never started".to_string(),
        Reason::ToolProcessInterrupted => {
            "its run was cut off while a tool ran (the daemon stopped)".to_string()
        }
        Reason::RequeueExpired => "it waited too long across a restart".to_string(),
        Reason::ResumeRefused => "its run could not be resumed after a restart".to_string(),
        _ => return None,
    })
}

/// How many sends [`SentLog`] remembers (the oldest is forgotten first).
const SENT_KEPT: usize = 256;

/// How long `rkat/sent` waits for the engine's answer to a send it has
/// forwarded and not had answered yet.
const SENT_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// What the daemon did with the clients' recent sends, by the key each
/// client gave its send ([`protocol::SEND_KEY`]): forwarded, then the
/// engine's answer. A client whose send brought no answer asks
/// ([`protocol::method::Sent`]) rather than guess: a send the engine
/// admitted is not "not sent", and sending it again would make the agent
/// read it twice.
#[derive(Debug, Default)]
pub struct SentLog {
    sends: parking_lot::Mutex<std::collections::VecDeque<(String, protocol::SentFate)>>,
    changed: tokio::sync::Notify,
}

impl SentLog {
    /// The send `key` goes to the engine now.
    pub fn forwarded(&self, key: &str) {
        let mut sends = self.sends.lock();
        sends.retain(|(sent, _)| sent != key);
        if sends.len() >= SENT_KEPT {
            sends.pop_front();
        }
        sends.push_back((key.to_string(), protocol::SentFate::OnItsWay));
    }

    /// The engine answered the send `key` with `answer` (a JSON-RPC
    /// response): admitted, as the input its result names, or refused.
    pub fn answered(&self, key: &str, answer: &Value) {
        let fate = match answer.get("error") {
            Some(error) => protocol::SentFate::Refused {
                why: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("the engine refused it")
                    .to_string(),
            },
            None => protocol::SentFate::Admitted {
                input_id: answer
                    .pointer("/result/admission/input_id")
                    .and_then(Value::as_str)
                    .and_then(|id| uuid::Uuid::parse_str(id).ok())
                    .map(meerkat_core::lifecycle::InputId::from_uuid),
            },
        };
        if let Some((_, known)) = self.sends.lock().iter_mut().find(|(sent, _)| sent == key) {
            *known = fate;
        }
        self.changed.notify_waiters();
    }

    /// What became of the send `key` now.
    #[must_use]
    pub fn fate(&self, key: &str) -> protocol::SentFate {
        self.sends
            .lock()
            .iter()
            .find(|(sent, _)| sent == key)
            .map_or(protocol::SentFate::NotReceived, |(_, fate)| fate.clone())
    }

    /// What became of the send `key`, once the engine answered it or
    /// `limit` passed.
    pub async fn settled(&self, key: &str, limit: std::time::Duration) -> protocol::SentFate {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let changed = self.changed.notified();
            let fate = self.fate(key);
            if fate != protocol::SentFate::OnItsWay {
                return fate;
            }
            // Re-checked after every wake (and at the deadline).
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                return self.fate(key);
            }
        }
    }
}

impl Local {
    /// `rkat/sent`: what became of the send `key` (waiting a little for
    /// the engine's answer to one it has not answered yet).
    pub(super) async fn sent_fate(&self, key: &str) -> protocol::SentFate {
        self.sent.settled(key, SENT_WAIT).await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// A send is not received until forwarded, on its way until the
    /// engine answers, then admitted (with the input it names) or refused;
    /// a wait for one on its way ends with the answer, or at its bound.
    #[tokio::test(start_paused = true)]
    async fn a_send_s_fate_follows_the_engine_s_answer() {
        let log = std::sync::Arc::new(SentLog::default());
        assert_eq!(log.fate("k1"), protocol::SentFate::NotReceived);
        log.forwarded("k1");
        assert_eq!(log.fate("k1"), protocol::SentFate::OnItsWay);
        let input = meerkat_core::lifecycle::InputId::new();
        let waiting = tokio::spawn({
            let log = std::sync::Arc::clone(&log);
            async move { log.settled("k1", SENT_WAIT).await }
        });
        tokio::task::yield_now().await;
        log.answered(
            "k1",
            &serde_json::json!({"id": 1, "result": {"admission": {"input_id": input.to_string()}}}),
        );
        assert_eq!(
            waiting.await.unwrap(),
            protocol::SentFate::Admitted {
                input_id: Some(input)
            }
        );
        log.forwarded("k2");
        log.answered(
            "k2",
            &serde_json::json!({"id": 2, "error": {"code": -32000, "message": "no such member"}}),
        );
        assert_eq!(
            log.fate("k2"),
            protocol::SentFate::Refused {
                why: "no such member".into()
            }
        );
        // Unanswered: the wait gives up at its bound.
        log.forwarded("k3");
        assert_eq!(
            log.settled("k3", SENT_WAIT).await,
            protocol::SentFate::OnItsWay
        );
        // The oldest are forgotten past the bound.
        for n in 0..SENT_KEPT {
            log.forwarded(&format!("x{n}"));
        }
        assert_eq!(log.fate("k1"), protocol::SentFate::NotReceived);
    }

    /// An input's fate is read from its record: held, written into a run,
    /// or ended without one, with why in words.
    #[test]
    fn an_inputs_fate_is_its_records_phase() {
        use meerkat_runtime::input_state::{
            InputAbandonReason, InputLifecycleState as Phase, InputStateSeed, InputTerminalOutcome,
        };
        use protocol::InputFate;

        let seed = |phase, terminal_outcome| InputStateSeed {
            phase,
            terminal_outcome,
            ..InputStateSeed::new_accepted()
        };
        for phase in [Phase::Accepted, Phase::Queued, Phase::Staged] {
            assert_eq!(input_fate(&seed(phase, None)), InputFate::Pending);
        }
        for phase in [
            Phase::Applied,
            Phase::AppliedPendingConsumption,
            Phase::Consumed,
            Phase::Coalesced,
        ] {
            assert_eq!(input_fate(&seed(phase, None)), InputFate::Taken);
        }
        let abandoned = |reason| {
            input_fate(&seed(
                Phase::Abandoned,
                Some(InputTerminalOutcome::Abandoned { reason }),
            ))
        };
        assert_eq!(
            abandoned(InputAbandonReason::Cancelled),
            InputFate::Dropped {
                why: Some("it was cancelled".into())
            }
        );
        assert_eq!(
            abandoned(InputAbandonReason::MaxAttemptsExhausted { attempts: 3 }),
            InputFate::Dropped {
                why: Some("its run failed 3 times".into())
            }
        );
        assert_eq!(
            input_fate(&seed(Phase::Abandoned, None)),
            InputFate::Dropped { why: None }
        );
        assert!(matches!(
            input_fate(&seed(
                Phase::Superseded,
                Some(InputTerminalOutcome::Superseded {
                    superseded_by: meerkat_core::lifecycle::InputId::new()
                })
            )),
            InputFate::Dropped { why: Some(_) }
        ));
    }
}
