//! `rkat/interrupt`: the user's Esc, bound to one run.
//!
//! The client names the run its turn showed (or asks for the current one)
//! and the messages its user typed that the agent may not have read. The
//! daemon, in this order:
//!
//! 1. takes back every named message the runtime still holds queued
//!    (`MeerkatMachine::recall_queued_input`, one transition guarded on the
//!    input being queued, so a run cannot take it meanwhile): a message
//!    taken back here never starts a follow-up run when the run stops;
//! 2. marks the gate's Stop for that run only
//!    ([`crate::approval::ApprovalEngine::interrupted`]: a run that is no
//!    longer the current one is not marked) and answers its own agent's
//!    open dialogs with Stop;
//! 3. stops that run and no other (`MeerkatMachine::stop_run`, or
//!    `hard_cancel_run_if_current` for a forced one: both compare the run
//!    with the machine's current run under the session's gate, so a run
//!    that ended is never exchanged for a newer one);
//! 4. stops a follow-up run that took one of the messages (it belongs to
//!    the interrupted turn), and reads what became of each message: one
//!    the agent never read is returned to the user, one it read stays.
//!
//! The engine's own words for a failure go to the daemon's log and the
//! audit log; the client is told what the failure means for the user.

use super::*;

use meerkat_core::lifecycle::{InputId, RunId};
use meerkat_runtime::{QueuedInputRecall, RunStopReceipt, RuntimeDriverError};
use protocol::{InputFate, InterruptParams, InterruptTarget, Interrupted, RunInterrupt};

/// The reason the runtime records with the user's interrupt.
pub(crate) const INTERRUPT_REASON: &str = "rkat-tui: interrupted by the user";

/// How many times a message's fate is read again while it is still held
/// (between a stop and its contributors' terminals).
const FATE_PASSES: u32 = 3;

/// What became of one of the caller's messages.
enum Fate {
    Returned,
    Read,
    /// Held by `run` (a run other than the interrupted one: a follow-up).
    HeldBy(RunId),
    /// Not settled yet, or not the caller's agent's.
    Open,
}

impl Local {
    /// `rkat/interrupt` for `caller` (the module docs).
    ///
    /// # Errors
    ///
    /// [`protocol::NO_AGENT`] when the connection is bound to no agent or
    /// its agent has no conversation.
    pub(super) async fn interrupt(
        &self,
        caller: &Caller,
        params: InterruptParams,
    ) -> Result<Interrupted, Refusal> {
        let session = self.caller_session(caller, "rkat/interrupt interrupts")?;
        let agent = caller.agent.clone().unwrap_or_default();
        let InterruptParams {
            target,
            input_ids,
            hard,
        } = params;
        tracing::info!(%agent, ?target, hard, inputs = input_ids.len(), "the client interrupts the agent");
        // A checkpoint under way is cancelled: nothing continues.
        self.agents.user_acted(&agent, false);
        let machine = &self.parts.machine;

        // 1. The messages still queued come back before the run stops.
        let mut answer = Interrupted {
            run_id: None,
            run: RunInterrupt::Idle,
            follow_ups: 0,
            returned: Vec::new(),
            read: Vec::new(),
        };
        // Each engine input with the ids the client named of it: a message
        // the daemon held comes back at once (the engine never had it);
        // held ones it sent together are the input they went as.
        let mut open: Vec<(InputId, Vec<InputId>)> = Vec::new();
        let mut follow_ups: Vec<RunId> = Vec::new();
        let mut settled: Vec<InputId> = Vec::new();
        let run = match &target {
            InterruptTarget::Run { run_id } => Some(run_id.clone()),
            InterruptTarget::Current => machine.current_run(&session).await,
            InterruptTarget::Inputs => None,
        };
        let (taken, reached) = self.core.activity.take_back(&agent, &input_ids);
        answer.returned.extend(taken.held);
        let mut named: Vec<(InputId, Vec<InputId>)> = taken
            .other
            .into_iter()
            .map(|input| (input.clone(), vec![input]))
            .chain(taken.sent)
            .collect();
        for id in reached {
            if let crate::daemon::activity::HeldWhere::Sent(input) =
                self.core.activity.held_where(&agent, &id)
            {
                match named.iter_mut().find(|(sent, _)| *sent == input) {
                    Some((_, ids)) => ids.push(id),
                    None => named.push((input, vec![id])),
                }
            }
        }
        for (input, ids) in named {
            match self.take_back_one(&session, &input).await {
                Fate::Returned => {
                    settled.push(input);
                    answer.returned.extend(ids);
                }
                Fate::Read => {
                    settled.push(input);
                    answer.read.extend(ids);
                }
                Fate::HeldBy(holder) if run.as_ref() != Some(&holder) => {
                    if !follow_ups.contains(&holder) {
                        follow_ups.push(holder);
                    }
                    open.push((input, ids));
                }
                Fate::HeldBy(_) | Fate::Open => open.push((input, ids)),
            }
        }

        // 2 and 3. That run, and only that run.
        if let Some(run) = &run {
            answer.run_id = Some(run.clone());
            answer.run = self.stop_one(&agent, &session, run, hard).await;
        }

        // 4. A follow-up run that took a message is part of the turn.
        for holder in follow_ups {
            if matches!(
                self.stop_one(&agent, &session, &holder, hard).await,
                RunInterrupt::Stopped
            ) {
                answer.follow_ups += 1;
            }
        }
        for _ in 0..FATE_PASSES {
            if open.is_empty() {
                break;
            }
            let mut still = Vec::new();
            for (input, ids) in open {
                match self.take_back_one(&session, &input).await {
                    Fate::Returned => {
                        settled.push(input);
                        answer.returned.extend(ids);
                    }
                    Fate::Read => {
                        settled.push(input);
                        answer.read.extend(ids);
                    }
                    Fate::HeldBy(holder) => {
                        // Taken by a run after the stop (or not settled yet):
                        // stop that one too, then read again.
                        if run.as_ref() != Some(&holder)
                            && matches!(
                                self.stop_one(&agent, &session, &holder, hard).await,
                                RunInterrupt::Stopped
                            )
                        {
                            answer.follow_ups += 1;
                        }
                        still.push((input, ids));
                    }
                    Fate::Open => still.push((input, ids)),
                }
            }
            open = still;
        }
        if !open.is_empty() {
            tracing::info!(
                left = open.len(),
                "rkat/interrupt: messages not settled yet"
            );
        }
        // The message in flight came back or was read: what another
        // client has held goes at the run's end.
        if let Some(releasing) = self.core.activity.user_settled(&agent, &settled, true) {
            let agents = Arc::clone(&self.agents);
            tokio::spawn(async move { agents.release_user(releasing).await });
        }
        self.core.activity.changed(&agent, false);
        #[cfg(any(test, feature = "test-support"))]
        self.probe.interrupted(&answer);
        Ok(answer)
    }

    /// Take `input` back if the runtime still holds it queued, else read
    /// what became of it.
    async fn take_back_one(&self, session: &meerkat_core::SessionId, input: &InputId) -> Fate {
        use meerkat_runtime::SessionServiceRuntimeExt as _;
        use meerkat_runtime::input_state::{InputLifecycleState as Phase, InputTerminalOutcome};

        let machine = &self.parts.machine;
        match machine.recall_queued_input(session, input).await {
            Ok(Some(QueuedInputRecall::Abandoned { .. })) => return Fate::Returned,
            Ok(Some(QueuedInputRecall::NotQueued { phase, run_id, .. })) => {
                return match (phase, run_id) {
                    // Written into the conversation: the agent has read it.
                    (Phase::Applied | Phase::AppliedPendingConsumption, None) => Fate::Read,
                    (_, Some(run)) => Fate::HeldBy(run),
                    (_, None) => Fate::Open,
                };
            }
            Ok(Some(QueuedInputRecall::AlreadyTerminal { terminal, .. })) => {
                return match terminal {
                    InputTerminalOutcome::Abandoned { .. }
                    | InputTerminalOutcome::Superseded { .. } => Fate::Returned,
                    _ => Fate::Read,
                };
            }
            Ok(Some(_)) => {}
            Ok(None) => return Fate::Open,
            Err(err) => {
                tracing::warn!(%input, %err, "rkat/interrupt: the message was not taken back");
            }
        }
        match machine.input_state(session, input).await {
            Ok(Some(stored)) => match super::inputs::input_fate(&stored.seed) {
                InputFate::Taken => Fate::Read,
                InputFate::Dropped { .. } => Fate::Returned,
                InputFate::Pending | InputFate::Unknown => Fate::Open,
            },
            _ => Fate::Open,
        }
    }

    /// Stop `run` of `agent`'s `session`, and only it: the gate's Stop
    /// first (so a call it makes meanwhile is denied, saying the user
    /// interrupted the turn), then the runtime's stop of exactly that run.
    async fn stop_one(
        &self,
        agent: &str,
        session: &meerkat_core::SessionId,
        run: &RunId,
        hard: bool,
    ) -> RunInterrupt {
        let machine = &self.parts.machine;
        if machine.current_run(session).await.as_ref() != Some(run) {
            return RunInterrupt::Ended;
        }
        // Its helpers keep running, and their endings are held for the
        // user's next message (the owner's decision).
        self.agents.interrupted(agent);
        if self.core.engine.interrupted(session, Some(run)) {
            self.parts.desk.stop(session);
        }
        let outcome = if hard {
            match machine
                .hard_cancel_run_if_current(session, run, INTERRUPT_REASON)
                .await
            {
                Ok(true) => RunInterrupt::Stopped,
                Ok(false) => RunInterrupt::Ended,
                Err(err) => self.refused(agent, session, run, &err).await,
            }
        } else {
            match machine.stop_run(session, run, INTERRUPT_REASON).await {
                Ok(RunStopReceipt::Stopped { .. }) => RunInterrupt::Stopped,
                Ok(RunStopReceipt::NotCurrent { .. }) => RunInterrupt::Ended,
                // A runtime stop or teardown owns the run: it is ending.
                Ok(RunStopReceipt::NotStoppable { state, .. }) => {
                    tracing::info!(%run, ?state, "rkat/interrupt: the run is being stopped already");
                    RunInterrupt::Stopped
                }
                Ok(other) => {
                    tracing::warn!(%run, ?other, "rkat/interrupt: a stop receipt this build does not know");
                    RunInterrupt::Stopped
                }
                Err(err) => self.refused(agent, session, run, &err).await,
            }
        };
        self.audit(
            super::super::audit::Line::event(
                "interrupt",
                format!(
                    "run {run}{}: {}",
                    if hard { " (forced)" } else { "" },
                    match &outcome {
                        RunInterrupt::Stopped => "stopped",
                        RunInterrupt::Ended => "had ended",
                        RunInterrupt::Idle => "nothing ran",
                        RunInterrupt::Failed { .. } => "refused",
                    }
                ),
            )
            .of_agent(agent),
        );
        outcome
    }

    /// The runtime refused to stop `run`: the engine's words go to the log
    /// and the audit log; the answer says what it means for the user.
    async fn refused(
        &self,
        agent: &str,
        session: &meerkat_core::SessionId,
        run: &RunId,
        err: &RuntimeDriverError,
    ) -> RunInterrupt {
        tracing::warn!(%agent, %run, %err, "rkat/interrupt: the runtime refused the stop");
        self.audit(
            super::super::audit::Line::event("interrupt", format!("run {run}: refused: {err}"))
                .of_agent(agent),
        );
        // A refusal for a run that ended meanwhile is that end.
        if self.parts.machine.current_run(session).await.as_ref() != Some(run) {
            return RunInterrupt::Ended;
        }
        RunInterrupt::Failed {
            why: plain_refusal(err),
        }
    }
}

/// What a refused stop means for the user, and what to do.
fn plain_refusal(err: &RuntimeDriverError) -> String {
    match err {
        RuntimeDriverError::NotReady { .. } => {
            "the agent's runtime is between steps and did not take the interrupt; press Esc again in a moment".to_string()
        }
        RuntimeDriverError::Destroyed | RuntimeDriverError::NotFound { .. } => {
            "the agent's session is being brought back and did not take the interrupt; press Esc again in a moment".to_string()
        }
        _ => "the agent's runtime did not take the interrupt; press Esc again, and if the run goes on, /quit stops it (the daemon's log has the details)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No engine text reaches the user from a refused stop.
    #[test]
    fn a_refused_stop_is_said_in_plain_words() {
        for err in [
            RuntimeDriverError::NotReady {
                state: meerkat_runtime::RuntimeState::Running,
            },
            RuntimeDriverError::Destroyed,
            RuntimeDriverError::Internal("must resolve through MeerkatMachine".into()),
        ] {
            let text = plain_refusal(&err);
            for raw in ["MeerkatMachine", "Runtime not ready", "running", "actor"] {
                assert!(!text.contains(raw), "{raw}: {text}");
            }
            assert!(text.contains("press Esc again"), "{text}");
        }
    }
}
