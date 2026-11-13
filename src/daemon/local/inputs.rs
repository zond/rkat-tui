//! The inputs a client sent its agent: `rkat/recall` takes back the
//! queued ones, `rkat/inputs` reads what became of them.

use super::*;

/// The reason recorded on an input `rkat/recall` abandons.
const RECALL_REASON: &str = "rkat-tui: recalled by the user";

impl Local {
    /// `rkat/recall` for `caller` ([`Self::take_back`]); what was taken
    /// back may leave the agent idle.
    ///
    /// # Errors
    ///
    /// As [`Self::take_back`].
    pub(super) async fn recall(
        &self,
        caller: &Caller,
        inputs: &[meerkat_core::lifecycle::InputId],
    ) -> Result<protocol::Recalled, Refusal> {
        let recalled = self.take_back(caller, inputs).await;
        if let Some(agent) = &caller.agent {
            self.agents.activity().changed(agent, false);
        }
        recalled
    }

    /// The queued `inputs` of `caller`'s agent, taken back: only its own
    /// agent's current
    /// conversation is searched, so another agent's input is never
    /// touched. A queued input is abandoned (`cancel_input_if_present`);
    /// any other state means the agent has it, and nothing is cancelled
    /// (that call would interrupt the input's run). A steer staged between
    /// the read and the cancel is the remaining window: an input that starts
    /// its run in between is cancelled while it runs, interrupting it. An
    /// atomic "abandon if still queued" is lukacf/meerkat#2182; this
    /// lookup-then-cancel goes when it ships.
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
        use meerkat_runtime::SessionServiceRuntimeExt as _;
        use meerkat_runtime::input_state::InputLifecycleState;

        let session = self.caller_session(caller, "rkat/recall takes back")?;
        let mut answer = protocol::Recalled::default();
        for input in inputs {
            match self.machine.input_state(&session, input).await {
                Ok(Some(stored)) if stored.seed.phase == InputLifecycleState::Queued => {
                    match self
                        .machine
                        .cancel_input_if_present(&session, input, RECALL_REASON)
                        .await
                    {
                        Ok(true) => answer.recalled.push(input.clone()),
                        Ok(false) => answer.reached.push(input.clone()),
                        Err(err) => answer.errors.push(format!("input {input}: {err}")),
                    }
                }
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
            .and_then(|agent| self.conversations.current(agent))
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
    /// waiting).
    ///
    /// # Errors
    ///
    /// [`protocol::NO_AGENT`] as for `rkat/recall`.
    pub(super) async fn input_fates(
        &self,
        caller: &Caller,
        inputs: &[meerkat_core::lifecycle::InputId],
    ) -> Result<protocol::InputFates, Refusal> {
        use meerkat_runtime::SessionServiceRuntimeExt as _;

        let session = self.caller_session(caller, "rkat/inputs reads")?;
        let mut answer = protocol::InputFates::default();
        for input in inputs {
            let fate = match self.machine.input_state(&session, input).await {
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
pub(super) fn abandon_words(
    reason: &meerkat_runtime::input_state::InputAbandonReason,
) -> Option<String> {
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

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
