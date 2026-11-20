//! The UI's turn: prompts and steers (sent in order by one task), Esc,
//! the agent's events and when the turn ends, and the model the turn runs
//! on. The turn's state and its transitions are [`state`]'s.

use super::*;

pub(super) mod state;

use super::steers::FatesAsk;
pub(super) use state::Turn;
use state::{Kind, Overdue, Press, RunStart};

/// What outlives one [`Turn`]: the turns' numbers and the task that sends
/// them, the last failed prompt, the runs the last turn saw end, the
/// daemon's last idle notice, and the interrupts' numbers and the press
/// after one.
#[derive(Default)]
pub(super) struct Turns {
    /// Numbers the UI's turns: a prompt's send failure ends the turn it
    /// was sent for, not a later one.
    count: u64,
    /// The prompts and steers on their way, sent in order by one task
    /// ([`Runner::outbound`]).
    outbound: Option<mpsc::UnboundedSender<Outbound>>,
    /// The prompt of the turn that just failed, in its conversation.
    failed_prompt: Option<(SessionId, String)>,
    /// The runs the last turn saw end: a note of one, or its end arriving
    /// after the turn ended on the daemon's idle notice, joins no turn.
    finished_runs: HashSet<RunId>,
    /// What the daemon said last of this agent's being idle.
    pub(super) idle: Option<protocol::IdleNote>,
    /// How many interrupts this runner started (each turn's interrupt is
    /// numbered, [`state::Interrupt::epoch`]).
    interrupts: u64,
    /// The last interrupted turn's last press, and whether "already
    /// stopped" was said for it: a press soon after its end acts on
    /// nothing ([`INTERRUPT_REPEAT`]).
    after_interrupt: Option<(Instant, bool)>,
}

impl Turns {
    /// The next turn's number.
    fn next(&mut self) -> u64 {
        self.count += 1;
        self.count
    }
}

/// The model the agent runs on, as this client knows it.
#[derive(Default)]
pub(super) struct Models {
    /// The model and endpoint as last read or switched to.
    pub(super) shown: Option<ShownModel>,
    /// The dialect of the current model's provider.
    dialect: Option<WireDialect>,
    /// The last `/model` switch (carried to a `/new` conversation).
    pub(super) last_switch: Option<ModelSwitch>,
    /// A model switch (`rkat/set_model`) is on its way: prompts are held
    /// until it is answered (a `/model` that waited for a turn's end goes
    /// before the message typed after it).
    pub(super) switching: bool,
}

impl Runner {
    pub(super) fn start_turn(&mut self, text: &str) {
        let Some(connection) = self.connection() else {
            self.send(Msg::TurnFinished(Err(TurnFailure::Failed(format!(
                "{}; try again once it is",
                self.unattached()
            )))));
            return;
        };
        let text = self.expand_mentions(text);
        // The prompt of the turn that just failed, typed again in its
        // conversation: the task checks whether it is still the last
        // message there.
        let retry = self
            .turns
            .failed_prompt
            .take()
            .and_then(|(session, failed)| {
                (self.conn.link().is_some_and(|link| link.session == session)
                    && failed.trim() == text.trim())
                .then_some(session)
            });
        let number = self.turns.next();
        match &mut self.turn {
            // A run this client did not start goes (a helper's or a peer's
            // message, a note of the daemon's, begun as the UI sent this):
            // the prompt joins it at its next model request, and the turn
            // keeps the runs it saw (their ends end it, an interrupt stops
            // them). Its own run, when it gets one, is the one that carries
            // it ([`Turn::run_start`]).
            Turn::Open(open) if !matches!(open.kind, Kind::Compaction { .. }) => {
                open.kind = Kind::Prompt {
                    id: number,
                    text: text.clone(),
                    started: false,
                    steered: false,
                };
            }
            _ => self.turn = Turn::prompt(number, text.clone()),
        }
        self.outbound(Outbound {
            connection,
            text,
            steer: None,
            queue: false,
            retry,
            turn: number,
            reports: self.reporter(),
            ui_tx: self.ui_tx.clone(),
        });
    }

    pub(super) fn steer(&mut self, id: PendingId, text: &str) {
        let Some(turn) = self.turn.id() else {
            self.send(Msg::SteerAck {
                id,
                result: Err(SteerFailure::NotRunning),
            });
            return;
        };
        let Some(connection) = self.connection() else {
            self.send(Msg::SteerAck {
                id,
                result: Err(SteerFailure::Rejected(self.unattached())),
            });
            return;
        };
        let text = self.expand_mentions(text);
        // Expected at the agent's next boundary from now on (the stream may
        // deliver it before the send's answer); taken back if the send
        // fails.
        self.steering.steers.sending(id);
        self.steering.steers.sent_text(id, &text);
        self.outbound(Outbound {
            connection,
            text,
            steer: Some(id),
            queue: false,
            retry: None,
            turn,
            reports: self.reporter(),
            ui_tx: self.ui_tx.clone(),
        });
    }

    /// The daemon reloaded the agent after a storage failure and turned
    /// back `text` (the prompt of turn `turn`, or the steer `steer`): the
    /// agent's stream is opened again (its old one ended with the old
    /// registration), then the input goes once more ([`Self::send_again`]).
    pub(super) fn send_after_reload(&mut self, turn: u64, steer: Option<PendingId>, text: String) {
        self.switches.again = Some((turn, steer, text));
        // A reopen under way (the daemon's [`protocol::RELOADED`], or the
        // old stream's end) is the reload's; else one starts here.
        if self.switches.running {
            self.switches.reloaded = true;
        } else if let Some(link) = self.conn.link() {
            let session = link.session.clone();
            self.rerender(session, Shown::Reloaded);
        }
    }

    /// Send `text` of turn `turn` once more after the agent's reload (a
    /// turn that ended meanwhile, or a steer no longer followed, is not).
    pub(super) fn send_again(&mut self, turn: u64, steer: Option<PendingId>, text: String) {
        if self.turn.id() != Some(turn) {
            if let Some(id) = steer {
                self.steering.steers.forget(id);
            }
            return;
        }
        let Some(connection) = self.connection() else {
            return;
        };
        // The quiet compaction run's prompt stays a run of its own.
        let queue = steer.is_none() && self.turn.is_quiet();
        self.outbound(Outbound {
            connection,
            text,
            steer,
            queue,
            retry: None,
            turn,
            reports: self.reporter(),
            ui_tx: self.ui_tx.clone(),
        });
    }

    /// Other agents' messages a run input carried, each to the UI with its
    /// sender's short name.
    fn peer_messages(&self, messages: Vec<(String, String)>) {
        for (from, text) in messages {
            self.send(Msg::Peer {
                from: crate::ui::peer::short_name(&from).to_string(),
                text,
            });
        }
    }

    /// A prompt or a steer of turn `turn` was sent (`result`): a steer is
    /// acknowledged (taken back from the turn when it failed); a prompt
    /// that failed ends its turn, if that turn is still the UI's.
    ///
    /// A steer's answer names the input the runtime admitted it as, which
    /// the agent's `boundary_append_applied` names when it joins the run
    /// ([`super::steers::Steers`]). An answer that names none ([`UNNAMED_INPUT`]) is
    /// acknowledged (the runtime has the message) and shown delivered at
    /// once with a notice: no other client's delivery is taken for it.
    pub(super) fn sent(
        &mut self,
        turn: u64,
        steer: Option<PendingId>,
        result: Result<Option<InputId>, SendFailure>,
    ) {
        let current = self.turn.id() == Some(turn);
        match (steer, result) {
            (Some(id), Ok(input)) => {
                if current && let Some(open) = self.turn.open_mut() {
                    open.steered();
                }
                self.steering.admitted.push(id);
                self.send(Msg::SteerAck { id, result: Ok(()) });
                let delivered = match input {
                    Some(input) => self.steering.steers.admitted(id, input),
                    None => {
                        self.steering.steers.forget(id);
                        self.notice(NoticeLevel::Warn, UNNAMED_INPUT);
                        true
                    }
                };
                if delivered {
                    self.send(Msg::SteerDelivered(id));
                } else if current && self.turn.interrupted() {
                    // Admitted after the user interrupted the turn: it is
                    // taken back too (with a run it started).
                    self.send_interrupt(Some(protocol::InterruptTarget::Inputs), false);
                }
            }
            (Some(id), Err(SendFailure::NotSent(why))) => {
                self.steering.steers.forget(id);
                self.send(Msg::SteerAck {
                    id,
                    result: Err(SteerFailure::Rejected(why)),
                });
            }
            // Not followed any more (no input id to follow it by), and not
            // back in the editor: sent again it could reach the agent twice.
            (Some(id), Err(SendFailure::Unknown(why))) => {
                self.steering.steers.forget(id);
                self.send(Msg::SteerAck {
                    id,
                    result: Err(SteerFailure::Unknown(why)),
                });
            }
            (None, Ok(_)) => {}
            (None, Err(SendFailure::NotSent(why))) if current => {
                self.turn.record(Err(TurnFailure::Failed(format!(
                    "the prompt was not sent: {why}"
                ))));
                self.finish_turn();
            }
            // A run it started shows here when it reports, as another
            // client's would.
            (None, Err(SendFailure::Unknown(why))) if current => {
                self.turn
                    .record(Err(TurnFailure::Failed(format!("{UNKNOWN_FATE} ({why})"))));
                self.finish_turn();
            }
            (None, Err(why)) => {
                tracing::info!(?why, "a prompt of an earlier turn was not sent");
            }
        }
        // A steer no longer on its way may have been all the turn waited
        // for.
        self.settle_turn();
    }

    /// ↑ on an empty editor while the agent holds this client's steers:
    /// ask the daemon to take back the ones it still holds queued
    /// (`rkat/recall`, by the inputs their sends admitted); answered with
    /// [`Msg::Recalled`] when the daemon has (or with nothing to ask, at
    /// once).
    pub(super) fn recall(&mut self) {
        let outstanding = self.steering.steers.outstanding();
        if outstanding.is_empty() {
            self.send(Msg::Recalled {
                recalled: Vec::new(),
                errors: Vec::new(),
            });
            return;
        }
        let Some(connection) = self.connection() else {
            self.send(Msg::Recalled {
                recalled: Vec::new(),
                errors: vec![self.unattached()],
            });
            return;
        };
        let reports = self.reporter();
        tokio::spawn(async move {
            let params = protocol::RecallParams {
                input_ids: outstanding.into_iter().map(|(_, input)| input).collect(),
            };
            let result = bounded(
                CALL_TIMEOUT,
                remote::call::<protocol::method::Recall>(&connection, &params),
            )
            .await
            .map_err(|err| err.to_string());
            reports.send(Report::Recalled(result));
        });
    }

    /// The daemon's answer to [`Self::recall`]: the steers it took back
    /// return to the editor (the UI's [`Msg::Recalled`]); the others
    /// already reached the agent.
    pub(super) fn recalled(&mut self, result: Result<protocol::Recalled, String>) {
        let (recalled, errors) = match result {
            Ok(answer) => (
                self.steering.steers.recalled(&answer.recalled),
                answer.errors,
            ),
            Err(why) => (Vec::new(), vec![why]),
        };
        self.steering.admitted.retain(|id| !recalled.contains(id));
        self.send(Msg::Recalled { recalled, errors });
    }

    /// Hand `outbound` to the task that sends the prompts and steers, one
    /// after another in the order they were made (a steer must not reach
    /// the agent before the prompt it follows); the task is started on
    /// first use.
    pub(super) fn outbound(&mut self, outbound: Outbound) {
        let sender = self.turns.outbound.get_or_insert_with(|| {
            let (tx, mut rx) = mpsc::unbounded_channel::<Outbound>();
            tokio::spawn(async move {
                while let Some(outbound) = rx.recv().await {
                    send_outbound(outbound).await;
                }
            });
            tx
        });
        if let Err(mpsc::error::SendError(outbound)) = sender.send(outbound) {
            // The task is gone (it never ends while the sender lives).
            outbound.reports.send(Report::Sent {
                turn: outbound.turn,
                steer: outbound.steer,
                result: Err(SendFailure::NotSent(
                    "the client could not send it".to_string(),
                )),
                again: None,
            });
        }
    }

    /// Esc. The first press interrupts the run the stream reported, or the
    /// one the daemon said was open as this client attached
    /// ([`Self::run_noted`]), or (knowing no run's id) the one the daemon
    /// finds current, which its answer names; with it go the messages of
    /// this client's the agent has not read ([`Self::send_interrupt`]).
    /// From then on the interrupt is bound to that run: a press within
    /// [`STOP_GRACE`] says when the next one forces it, a later one forces
    /// it, and one after that says it is forced. Once that run has ended, a
    /// press within [`INTERRUPT_REPEAT`] of the last one acts on nothing
    /// and says once that the turn is already stopped (a run that started
    /// since is never touched); a press after a pause is a new interrupt.
    /// With no turn of this client's open the daemon is asked to interrupt
    /// whatever the agent runs ([`Self::interrupt_unseen`]). Every press
    /// acts or says why not.
    pub(super) fn interrupt(&mut self) {
        let now = Instant::now();
        if !self.turn.is_open() {
            if let Some((last, said)) = &mut self.turns.after_interrupt
                && now.duration_since(*last) < INTERRUPT_REPEAT
            {
                *last = now;
                if !std::mem::replace(said, true) {
                    self.notice(NoticeLevel::Info, ALREADY_STOPPED);
                }
                return;
            }
            self.turns.after_interrupt = None;
            self.interrupt_unseen();
            return;
        }
        if self.connection().is_none() {
            self.turn.record(Err(TurnFailure::Interrupted));
            self.finish_turn();
            return;
        }
        let Some(turn) = self.turn.open_mut() else {
            return;
        };
        match turn.press(now, INTERRUPT_REPEAT, STOP_GRACE) {
            Press::Stopped { say } => {
                if say {
                    self.notice(NoticeLevel::Info, ALREADY_STOPPED);
                }
            }
            Press::Forced => self.notice(
                NoticeLevel::Info,
                format!(
                    "the interrupt was forced; waiting for the run to end (it is cancelled at once if it has not within {}s)",
                    INTERRUPT_DEADLINE.as_secs()
                ),
            ),
            Press::Grace(left) => self.notice(
                NoticeLevel::Info,
                format!(
                    "the interrupt was sent; waiting for the run to end (press again in {} ms to force it)",
                    left.as_millis().max(1)
                ),
            ),
            Press::Force => self.send_interrupt(None, true),
            // The first press, or a pause, then Esc: a new interrupt of
            // what the turn runs now (a follow-up run another client's
            // message started).
            Press::New => {
                self.turns.interrupts += 1;
                let target =
                    turn.begin_interrupt(self.turns.interrupts, now, now + INTERRUPT_DEADLINE);
                self.send_interrupt(Some(target), false);
            }
        }
    }

    /// Send the turn's interrupt to the daemon (`rkat/interrupt`): for
    /// `target` (`None`: the run the interrupt is for), with every steer
    /// of this client the agent has not read yet, which the daemon takes
    /// back; the turn does not end until it answers
    /// ([`Self::interrupt_answered`]).
    fn send_interrupt(&mut self, target: Option<protocol::InterruptTarget>, hard: bool) {
        let Some(connection) = self.connection() else {
            return;
        };
        let Some(turn) = self.turn.open_mut() else {
            return;
        };
        let target = match target {
            Some(target) => target,
            None => match &turn.interrupt {
                Some(interrupt) => interrupt.target(),
                None => protocol::InterruptTarget::Current,
            },
        };
        let Some(epoch) = turn.interrupt_sent() else {
            return;
        };
        let epoch = Some(epoch);
        let params = protocol::InterruptParams {
            target,
            input_ids: self
                .steering
                .steers
                .outstanding()
                .into_iter()
                .map(|(_, input)| input)
                .collect(),
            hard,
        };
        let reports = self.reporter();
        tokio::spawn(async move {
            let result = bounded(
                CALL_TIMEOUT,
                remote::call::<protocol::method::Interrupt>(&connection, &params),
            )
            .await
            .map_err(|err| interrupt_failure(&err));
            reports.send(Report::Interrupted { epoch, result });
        });
    }

    /// The daemon answered an interrupt: the run it was for (when the
    /// client named none), the messages it took back (to the editor, in
    /// order: [`Msg::Returned`]) and the ones the agent had read (shown
    /// delivered), and a failure in plain words. A press with no turn
    /// (`epoch` `None`) says what it found.
    pub(super) fn interrupt_answered(
        &mut self,
        epoch: Option<u64>,
        result: Result<protocol::Interrupted, String>,
    ) {
        // Any call of this turn's is answered now, of its current
        // interrupt or an earlier one (epochs are the runner's, so a call
        // of another turn's is never found here).
        if let Some(epoch) = epoch
            && let Some(turn) = self.turn.open_mut()
        {
            let run = result
                .as_ref()
                .ok()
                .and_then(|answer| answer.run_id.as_ref());
            turn.interrupt_answered(epoch, run);
        }
        match result {
            Ok(answer) => {
                let returned = self.steering.steers.recalled(&answer.returned);
                self.steering.admitted.retain(|id| !returned.contains(id));
                if !returned.is_empty() {
                    self.send(Msg::Returned(returned));
                }
                for input in &answer.read {
                    if let Some(id) = self.steering.steers.settled(input) {
                        self.send(Msg::SteerDelivered(id));
                    }
                }
                match (answer.run, epoch) {
                    (protocol::RunInterrupt::Failed { why }, _) => {
                        self.notice(NoticeLevel::Warn, why);
                    }
                    (protocol::RunInterrupt::Stopped, None) => {
                        self.notice(NoticeLevel::Info, INTERRUPTED_UNSEEN);
                    }
                    (protocol::RunInterrupt::Ended | protocol::RunInterrupt::Idle, None) => {
                        self.notice(NoticeLevel::Info, NOTHING_RUNS);
                    }
                    _ => {}
                }
            }
            Err(why) => self.notice(NoticeLevel::Warn, why),
        }
        self.settle_turn();
        // A steer the interrupt left unsettled, with the agent idle and
        // holding nothing: its fate is asked now (held back meanwhile).
        if self.turns.idle.as_ref().is_some_and(|idle| idle.held == 0) {
            self.ask_fates();
        }
    }

    /// Esc with no turn of this client's open. The agent may run all the
    /// same: a run this client did not see start (it attached mid-run), or
    /// one that went on after this client's turn ended as unconfirmed. The
    /// daemon interrupts the run its agent has now, if any
    /// ([`protocol::InterruptTarget::Current`]), and the answer says which
    /// ([`Self::interrupt_answered`]).
    fn interrupt_unseen(&self) {
        let Some(connection) = self.connection() else {
            self.notice(
                NoticeLevel::Warn,
                format!("{}: nothing to interrupt", self.unattached()),
            );
            return;
        };
        let reports = self.reporter();
        tokio::spawn(async move {
            let params = protocol::InterruptParams {
                target: protocol::InterruptTarget::Current,
                input_ids: Vec::new(),
                hard: false,
            };
            let result = bounded(
                CALL_TIMEOUT,
                remote::call::<protocol::method::Interrupt>(&connection, &params),
            )
            .await
            .map_err(|err| interrupt_failure(&err));
            reports.send(Report::Interrupted {
                epoch: None,
                result,
            });
        });
    }

    /// The interrupt's deadline passed and its run has not ended: the
    /// first time, cancel that run at once (and that run only) and wait
    /// once more.
    fn escalate_interrupt(&mut self) {
        if let Some(interrupt) = self
            .turn
            .open_mut()
            .and_then(|turn| turn.interrupt.as_mut())
        {
            interrupt.deadline = state::Deadline::Escalated(Instant::now() + INTERRUPT_DEADLINE);
        }
        self.notice(
            NoticeLevel::Warn,
            format!(
                "the interrupted run did not end within {}s; cancelling it at once",
                INTERRUPT_DEADLINE.as_secs()
            ),
        );
        self.send_interrupt(None, true);
    }

    /// One of the member's events (live, or replayed after the cursor; of
    /// the space `epoch`): shown once ([`Mark`]), and what it means for the
    /// turn. A stream that reports it lost events the member no longer
    /// retains (the cursor was older than its window) is rendered again
    /// from the conversation's start.
    pub(super) fn on_event(
        &mut self,
        mut envelope: EventEnvelope<AgentEvent>,
        epoch: Option<&str>,
    ) {
        if matches!(envelope.payload, AgentEvent::StreamTruncated { .. }) {
            if let Some(link) = self.conn.link()
                && !self.switches.running
            {
                let session = link.session.clone();
                self.rerender(session, Shown::LostEvents);
            }
            return;
        }
        if let Some(mark) = &mut self.seen.mark
            && !mark.admit(epoch, envelope.seq)
        {
            return;
        }
        self.helpers.followers.convening(&envelope.payload, None);
        if matches!(envelope.payload, AgentEvent::RunStarted { .. }) {
            self.on_run_started(envelope);
            return;
        }
        if self.turn.is_quiet() {
            self.on_quiet_event(envelope);
            return;
        }
        self.plain_storage_failure(&mut envelope.payload);
        self.join_running(&envelope.payload);
        match &envelope.payload {
            AgentEvent::PeerContentIngested {
                kind: meerkat_core::CommsNoticeKind::Message,
                peer: Some(peer),
                ..
            } => {
                self.peer_arrived(peer.display_name.as_deref());
                return;
            }
            // A run the engine resumed at a daemon's start, in place of its
            // `run_started`: the same run goes on under its id, so the
            // interrupt stops it by that id (a force-cancel waits for its
            // next boundary, which a resumed run's long call holds off).
            AgentEvent::RunResumed { run_id, .. } => {
                if let Some(turn) = self.turn.open_mut() {
                    turn.run_seen(Some(run_id), true);
                }
            }
            AgentEvent::ToolExecutionCompleted { name, .. }
                if crate::daemon::tasks::changes_tasks(name) =>
            {
                self.refresh_tasks();
            }
            // This client's steer only: another client's input marks
            // nothing here.
            AgentEvent::BoundaryAppendApplied { input_id, .. } => {
                if let Some(id) = self.steering.steers.applied(input_id) {
                    self.send(Msg::SteerDelivered(id));
                }
            }
            _ => {}
        }
        let terminal = self.on_run_end(&envelope.payload);
        self.send(Msg::Agent(envelope));
        if terminal {
            self.save_cursor();
            self.settle_turn();
        }
    }

    /// A run that failed because the realm's storage did is told in plain
    /// words (`crate::daemon::storage`; the daemon logs and audits the
    /// engine's).
    fn plain_storage_failure(&self, payload: &mut AgentEvent) {
        if let AgentEvent::RunFailed { error_report, .. } = payload
            && crate::daemon::storage::is_storage_failure(&error_report.message)
        {
            error_report.message =
                crate::daemon::storage::failed_line(&self.config.scope.realm_root());
        }
    }

    /// With no turn open, an event only a running run publishes joins its
    /// turn: a run already going when this client attached (its start was
    /// shown before it detached), and the UI is busy until its end. Not
    /// the end of a run the last turn saw end (on the idle notice, before
    /// its end event arrived).
    fn join_running(&mut self, payload: &AgentEvent) {
        if !self.turn.is_open()
            && joins_a_run(payload)
            && !run_of(payload).is_some_and(|run| self.turns.finished_runs.contains(run))
        {
            self.turn = Turn::open(Kind::Joined);
            self.send(Msg::RemoteTurn { prompt: None });
        }
    }

    /// A run's end, recorded in the turn; whether `payload` was one.
    fn on_run_end(&mut self, payload: &AgentEvent) -> bool {
        match payload {
            AgentEvent::RunCompleted {
                identity,
                result,
                usage,
                ..
            } => {
                let usage = usage.as_usage();
                let summary = TurnSummary {
                    text: result.clone(),
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                };
                self.run_ended(identity.run_id.as_ref(), Ok(summary));
                true
            }
            AgentEvent::RunFailed {
                identity,
                error_report,
                ..
            } => {
                let end = self.failure_of(error_report);
                self.run_ended(identity.run_id.as_ref(), end);
                true
            }
            _ => false,
        }
    }

    /// A run's start: what it means for the turn is [`Turn::run_start`]'s.
    fn on_run_started(&mut self, envelope: EventEnvelope<AgentEvent>) {
        let AgentEvent::RunStarted {
            identity, input, ..
        } = &envelope.payload
        else {
            return;
        };
        let run = identity.run_id.clone();
        let prompt = input.prompt_text();
        let start = self
            .turn
            .run_start(prompt.as_deref() == Some(COMPACT_PROMPT), prompt.as_deref());
        match start {
            // Another client's `/compact` run: quiet here too.
            RunStart::OpenQuiet => {
                self.turn = Turn::open(Kind::Compaction { id: 0 });
                self.send(Msg::CompactionRun(true));
            }
            RunStart::LeaveQuiet => {
                if let Turn::Open(quiet) = std::mem::take(&mut self.turn) {
                    // The compaction's run, when it ran, ended in it.
                    self.turns.finished_runs.extend(quiet.runs.ended);
                }
            }
            RunStart::Quiet | RunStart::Join | RunStart::First | RunStart::Later => {}
        }
        if matches!(start, RunStart::OpenQuiet | RunStart::Quiet) {
            // Its prompt and reply are not shown.
            if let Some(turn) = self.turn.open_mut() {
                turn.run_seen(run.as_ref(), false);
            }
            return;
        }
        if !self.turn.is_open() {
            self.turn = Turn::open(Kind::Joined);
        }
        self.show_run_input(start, prompt);
        if let Some(turn) = self.turn.open_mut() {
            turn.run_seen(run.as_ref(), start == RunStart::First);
        }
        self.send(Msg::Agent(envelope));
        // Told after the run's turn opened, so what the UI held for the
        // compaction is steered into the run rather than starting a turn
        // of its own.
        if start == RunStart::LeaveQuiet {
            self.send(Msg::CompactionRun(false));
        }
    }

    /// What a starting run's input (`prompt`) shows: this client's steers
    /// it carries, delivered; else, for a run this client did not start or
    /// a later run of the turn, the input as another client's.
    fn show_run_input(&mut self, start: RunStart, prompt: Option<String>) {
        // A run after the first (a follow-up run, or a run of a turn that
        // starts now while steers of this client are still with the agent:
        // a turn ended before the runtime ran them) may carry the steers
        // that missed the boundary: those whose text its content holds (the
        // start names no inputs). A run of a daemon's note or a peer's
        // message carries none, and they stay followed. Not after the user
        // interrupted the turn: a follow-up run that took one of them is
        // stopped with it, and the interrupt's answer says which the agent
        // read.
        let own = if start == RunStart::First || self.turn.interrupted() {
            Vec::new()
        } else {
            prompt
                .as_deref()
                .map(|content| self.steering.steers.carried(content))
                .unwrap_or_default()
        };
        match start {
            // A run this client did not start: no turn is open, or the turn
            // was joined on the daemon's note and shows its first run start
            // now.
            RunStart::Join | RunStart::LeaveQuiet => {
                // Another client's `/compact` run, shown as the turn joined
                // on the note already: its prompt is not.
                let prompt = prompt
                    .filter(|_| own.is_empty())
                    .filter(|prompt| prompt != COMPACT_PROMPT);
                match prompt.as_deref().and_then(crate::ui::peer::of_inputs) {
                    Some(messages) => {
                        self.peer_messages(messages);
                        self.send(Msg::RemoteTurn { prompt: None });
                    }
                    None => self.send(Msg::RemoteTurn { prompt }),
                }
            }
            // What reached the agent during the turn joins it: other
            // agents' messages, each as its labelled block (the block that
            // named its sender when it arrived gets its text), or another
            // client's steer.
            RunStart::Later if own.is_empty() => {
                if let Some(prompt) = prompt {
                    match crate::ui::peer::of_inputs(&prompt) {
                        Some(messages) => self.peer_messages(messages),
                        None => self.send(Msg::RemoteTurn {
                            prompt: Some(prompt),
                        }),
                    }
                }
            }
            _ => {}
        }
        for id in own {
            self.send(Msg::SteerDelivered(id));
        }
    }

    /// Another agent's message. Into a running turn it is delivered at the
    /// next boundary, and the events carry no text: the block names the
    /// sender (`from`). An idle agent wakes for it: the run's input
    /// carries the text ([`Self::on_run_started`]).
    fn peer_arrived(&self, from: Option<&str>) {
        if self.turn.is_open() {
            let from = from.unwrap_or("another agent");
            self.send(Msg::Peer {
                from: crate::ui::peer::short_name(from).to_string(),
                text: String::new(),
            });
        }
    }

    /// How a failed run ends its turn: interrupted (by the user, or
    /// cancelled), at its checkpoint (no failure: the daemon asks for the
    /// agent's status and says what follows, `crate::daemon::checkpoint`),
    /// or failed.
    fn failure_of(
        &self,
        error_report: &meerkat_core::AgentErrorReport,
    ) -> Result<TurnSummary, TurnFailure> {
        if self.turn.interrupted() || error_report.class == AgentErrorClass::Cancelled {
            Err(TurnFailure::Interrupted)
        } else if error_report.class == AgentErrorClass::MaxTurns {
            Ok(TurnSummary::default())
        } else {
            Err(TurnFailure::Failed(error_report.message.clone()))
        }
    }

    /// The turn's run `run` (when named) ended as `end`.
    fn run_ended(&mut self, run: Option<&RunId>, end: Result<TurnSummary, TurnFailure>) {
        if let Some(turn) = self.turn.open_mut() {
            if let Some(run) = run {
                turn.runs.over(run);
            }
            turn.record(end);
        }
    }

    /// An event of the quiet `/compact` run ([`Self::compact_asked`]): its
    /// compaction events reach the UI, its prompt and reply do not; it
    /// ends like any turn ([`Self::settle_turn`]), quietly.
    fn on_quiet_event(&mut self, envelope: EventEnvelope<AgentEvent>) {
        let terminal = match &envelope.payload {
            AgentEvent::RunCompleted { identity, .. } => {
                self.run_ended(identity.run_id.as_ref(), Ok(TurnSummary::default()));
                true
            }
            AgentEvent::RunFailed {
                identity,
                error_report,
                ..
            } => {
                let end = Err(TurnFailure::Failed(error_report.message.clone()));
                self.run_ended(identity.run_id.as_ref(), end);
                true
            }
            AgentEvent::CompactionStarted { .. }
            | AgentEvent::CompactionCompleted { .. }
            | AgentEvent::CompactionFailed { .. }
            | AgentEvent::RequestedCompactionSkipped { .. } => {
                self.send(Msg::Agent(envelope));
                false
            }
            _ => false,
        };
        if terminal {
            self.save_cursor();
            self.settle_turn();
        }
    }

    /// The daemon says the agent's run `run` is open (`rkat/running`): as
    /// this client attached, or at the start of a run the engine ran then
    /// whose start the daemon had not read yet. Either way the run started
    /// before this client's stream opened, so no start of it shows here:
    /// with no turn open the UI joins the run's turn, and Esc stops the run
    /// by this id. A run this client saw start already, or saw end (in this
    /// turn or the last), changes nothing; one it did not is the run to
    /// stop from now on.
    pub(super) fn run_noted(&mut self, run: RunId) {
        if self.turns.finished_runs.contains(&run) || self.turn.knows(&run) {
            return;
        }
        if !self.turn.is_open() {
            self.turn = Turn::open(Kind::Noted);
            self.send(Msg::RemoteTurn { prompt: None });
        }
        if let Some(turn) = self.turn.open_mut() {
            turn.runs.note(run);
        }
    }

    /// The daemon says the agent is idle (`rkat/idle`): no run open and
    /// no input of the conversation unfinished.
    pub(super) fn idle_reported(&mut self, note: protocol::IdleNote) {
        let holds_nothing = note.held == 0;
        self.turns.idle = Some(note);
        self.settle_turn();
        if holds_nothing {
            self.ask_fates();
        }
    }

    /// The agent is idle and its runtime holds nothing, yet this client
    /// still follows admitted steers: each was either written into a run
    /// (its event not seen here yet) or ended without one. Ask the daemon
    /// which (`rkat/inputs`) rather than conclude from the absence of an
    /// event; [`Self::input_fates`] takes the answer.
    pub(super) fn ask_fates(&mut self) {
        let outstanding = self.steering.steers.outstanding();
        // While the turn's interrupt is unanswered its answer settles the
        // steers it named (returned to the editor, or read): asking here
        // too would race it, and a message the stopped run dropped would
        // be told as not delivered instead of coming back
        // ([`Self::interrupt_answered`] asks after it).
        if outstanding.is_empty() || self.turn.awaits_interrupt() {
            return;
        }
        if self.steering.fates != FatesAsk::NotAsked {
            self.steering.fates = FatesAsk::AskAgain;
            return;
        }
        let Some(connection) = self.connection() else {
            return;
        };
        self.steering.fates = FatesAsk::Asking;
        let reports = self.reporter();
        tokio::spawn(async move {
            let params = protocol::RecallParams {
                input_ids: outstanding.into_iter().map(|(_, input)| input).collect(),
            };
            let result = bounded(
                CALL_TIMEOUT,
                remote::call::<protocol::method::Inputs>(&connection, &params),
            )
            .await
            .map_err(|err| err.to_string());
            reports.send(Report::InputFates(result));
        });
    }

    /// The daemon's answer to [`Self::ask_fates`]: a steer written into a
    /// run is shown delivered; one that ended without a run returns to the
    /// editor ([`Msg::NotDelivered`]); one still held, or not found in the
    /// agent's conversation, stays followed.
    pub(super) fn input_fates(&mut self, result: Result<protocol::InputFates, String>) {
        let again = std::mem::take(&mut self.steering.fates) == FatesAsk::AskAgain;
        match result {
            Ok(answer) => {
                for row in answer.inputs {
                    match row.fate {
                        protocol::InputFate::Taken => {
                            if let Some(id) = self.steering.steers.settled(&row.input_id) {
                                self.send(Msg::SteerDelivered(id));
                            }
                        }
                        protocol::InputFate::Dropped { why } => {
                            if let Some(id) = self.steering.steers.settled(&row.input_id) {
                                self.steering.admitted.retain(|admitted| *admitted != id);
                                self.send(Msg::NotDelivered { id, why });
                            }
                        }
                        protocol::InputFate::Pending | protocol::InputFate::Unknown => {}
                    }
                }
            }
            Err(why) => tracing::info!(%why, "rkat/inputs failed: the steers stay followed"),
        }
        if again {
            self.ask_fates();
        }
    }

    /// End the turn once the daemon said the agent is idle after a run
    /// this turn saw end, no run of it is open and no steer of this client
    /// is still on its way (its send unanswered: it runs as a follow-up
    /// run). A stale notice (of a run before this turn) ends nothing.
    pub(super) fn settle_turn(&mut self) {
        let Some(idle) = &self.turns.idle else {
            return;
        };
        let Some(turn) = self.turn.open_mut() else {
            return;
        };
        let (last_run, held) = (idle.last_run.as_ref(), idle.held);
        // The noted run's end, which may have fallen before this client's
        // stream opened (or before the render's last event) and so never
        // shows here: the idle notice after that run says it ended.
        let mut late = false;
        if let Some(run) = turn.runs.noted.take_if(|noted| Some(&*noted) == last_run) {
            turn.runs.ended.insert(run);
            late = turn.interrupt.is_some() && turn.result.is_none();
        }
        let over = turn.is_over(last_run);
        let interrupted = turn.interrupt.is_some();
        if late {
            self.notice(
                NoticeLevel::Info,
                "the agent's run had already ended when the interrupt reached it",
            );
        }
        if !over || self.steering.steers.sending_any() {
            return;
        }
        // After an interrupt the messages it took back are not held.
        if held > 0 && !interrupted {
            self.notice(
                NoticeLevel::Warn,
                "the agent still holds a message for this turn; it runs when the agent takes it",
            );
        }
        self.finish_turn();
    }

    /// An interrupt the daemon has not confirmed in time: the first time
    /// the current run is cancelled hard ([`Self::escalate_interrupt`]);
    /// after that the turn ends as unconfirmed (not as interrupted: the
    /// run may still be going, and it shows here again when it reports).
    pub(super) fn on_deadline(&mut self) {
        // Nothing is due, or the interrupted run ended: nothing is
        // cancelled (a run that started since is not the interrupt's); the
        // turn ends as its runs do.
        let Some(overdue) = self
            .turn
            .open_mut()
            .and_then(|turn| turn.overdue(Instant::now()))
        else {
            return;
        };
        if overdue == Overdue::First && self.connection().is_some() {
            self.escalate_interrupt();
            return;
        }
        self.turn.record(Err(TurnFailure::Unconfirmed));
        self.finish_turn();
    }

    pub(super) fn finish_turn(&mut self) {
        let Turn::Open(turn) = std::mem::take(&mut self.turn) else {
            return;
        };
        // Not the runs still open: an unconfirmed turn's run shows again
        // when it reports.
        self.turns.finished_runs = turn.runs.ended;
        // A press right after an interrupted turn's end acts on nothing.
        self.turns.after_interrupt = turn
            .interrupt
            .map(|interrupt| (interrupt.last_press, interrupt.said_stopped));
        let prompt = match turn.kind {
            Kind::Prompt { text, steered, .. } => Some((text, steered)),
            Kind::Noted | Kind::Joined => None,
            Kind::Compaction { .. } => {
                match &turn.result {
                    Some(Err(TurnFailure::Failed(why))) => self.notice(
                        NoticeLevel::Warn,
                        format!("the run that lets the agent compact failed: {why}"),
                    ),
                    Some(Err(_)) => self.notice(
                        NoticeLevel::Warn,
                        "the run that lets the agent compact was interrupted",
                    ),
                    _ => {}
                }
                self.send(Msg::CompactionRun(false));
                self.save_cursor();
                return;
            }
        };
        // A failed turn's prompt stays committed: the same text typed again
        // is a retry, not a second copy ([`Runner::retry_input`]).
        self.turns.failed_prompt = match (&turn.result, prompt, self.conn.link()) {
            (Some(Err(TurnFailure::Failed(_))), Some((prompt, false)), Some(link)) => {
                Some((link.session.clone(), prompt))
            }
            _ => None,
        };
        // A steer still followed stays with the agent: it is marked when
        // its own boundary event or the run carrying it arrives.
        self.send(Msg::TurnFinished(
            turn.result.unwrap_or_else(|| Ok(TurnSummary::default())),
        ));
        self.save_cursor();
    }

    /// The status bar names `model` on `endpoint` (of `dialect`).
    pub(super) fn show_model(&mut self, endpoint: &str, dialect: Option<WireDialect>, model: &str) {
        self.models.shown = Some(ShownModel {
            endpoint: endpoint.to_string(),
            model: model.to_string(),
        });
        self.send(Msg::Status(StatusUpdate::Model {
            endpoint: endpoint.to_string(),
            model: model.to_string(),
        }));
        self.models.dialect = dialect;
        let configured = self.bench.is_configured_endpoint(endpoint);
        if configured && self.bench.models().is_none() {
            // Its window is the endpoint's: fetch the lists, and show the
            // window again when they arrive (`model_lists`).
            self.bench.ensure_fetch(false);
        }
        self.send_context_window();
    }

    /// The status bar's context window for the current model: for a
    /// configured endpoint its own listing, else the realm config's model
    /// registry this client read at start; for a built-in endpoint the
    /// other way round (the listing then covers a model only an endpoint
    /// knows, whose profile the daemon wrote since).
    pub(super) fn send_context_window(&self) {
        let Some(ShownModel { endpoint, model }) = &self.models.shown else {
            return;
        };
        let registry = || {
            let config = self.config.config.as_ref()?;
            // A self-hosted endpoint's model is registered as its alias.
            if let Some(endpoint) = self.bench.endpoint(endpoint)
                && endpoint.is_self_hosted()
            {
                return crate::endpoints::registry_context_window(
                    config,
                    endpoint.engine_provider(),
                    &endpoint.engine_model(model),
                );
            }
            let provider = self.models.dialect?.into();
            crate::endpoints::registry_context_window(config, provider, model)
        };
        let listed = || {
            self.bench
                .models()
                .and_then(|models| models.entry(endpoint, model))
                .and_then(|entry| entry.context_window)
        };
        let window = if self.bench.is_configured_endpoint(endpoint) {
            listed().or_else(registry)
        } else {
            registry().or_else(listed)
        };
        self.send(Msg::Status(StatusUpdate::ContextWindow(window)));
    }

    /// `/model <reference>` or a picker selection: the daemon switches the
    /// session in place ([`Runner::switch_model`], with the model,
    /// provider and the endpoint's auth binding); it keeps the model.
    /// A bare id is resolved against this process's endpoints (their lists
    /// fetched first when several could serve it).
    pub(super) fn set_model(&mut self, reference: &str) {
        match model_switch(
            reference,
            &self.config.endpoints,
            self.bench.models(),
            self.models
                .shown
                .as_ref()
                .map(|shown| shown.endpoint.as_str()),
        ) {
            Ok(switch) => self.switch_model(switch),
            Err(err) => {
                if !self.bench.defer_until_listed(reference) {
                    self.notice(NoticeLevel::Warn, format!("model not changed: {err}"));
                }
            }
        }
    }

    /// Switch the agent's session to `switch` in place
    /// (`rkat/set_model`): refused while a
    /// turn runs; the next prompt is an ordinary send.
    pub(super) fn switch_model(&mut self, switch: ModelSwitch) {
        if self.turn.is_open() {
            self.notice(
                NoticeLevel::Warn,
                "a turn is running; switch models once it ends (or interrupt it)",
            );
            return;
        }
        let Some(connection) = self.link_or_say("the model was not changed") else {
            return;
        };
        let reports = self.reporter();
        self.models.switching = true;
        tokio::spawn(async move {
            let params = protocol::SetModelParams {
                model: switch.model.clone(),
                provider: switch.provider.clone(),
                auth_binding: switch.auth_binding.clone(),
                listed: switch.listed,
            };
            let result = bounded(
                SWITCH_TIMEOUT,
                remote::call::<protocol::method::SetModel>(&connection, &params),
            )
            .await
            .map_err(|err| match err {
                CallFailure::Failed(ClientError::Rpc { message, .. }) => message,
                other => other.to_string(),
            });
            reports.send(Report::ModelSet { switch, result });
        });
    }

    /// The daemon answered a model switch to `switch`.
    pub(super) fn model_set(
        &mut self,
        switch: ModelSwitch,
        result: Result<protocol::ModelSet, String>,
    ) {
        match result {
            Ok(set) => {
                if self
                    .conn
                    .link()
                    .is_some_and(|link| link.session == set.session_id)
                {
                    let dialect = WireDialect::parse(&set.provider);
                    self.show_model(&switch.endpoint, dialect, &set.model);
                    // A helper that names no model runs on the agent's
                    // model as it is now: its view shows that until the
                    // helper's own session is read.
                    self.helpers.followers.set_default_model(&set.model);
                    // The agent was seated again: its stream is new.
                    if set.reseated {
                        self.rerender(set.session_id.clone(), Shown::Reseated);
                    }
                }
                self.notice(
                    NoticeLevel::Info,
                    format!("switched to {} on {}", set.model, switch.endpoint),
                );
                self.models.last_switch = Some(switch);
            }
            Err(why) => self.notice(NoticeLevel::Error, format!("model not changed: {why}")),
        }
    }

    /// The model lists arrived ([`Workbench::background`]): a `/model
    /// <ref>` that waited for them switches now.
    pub(super) fn model_lists(&mut self, joined: Result<FetchResults, tokio::task::JoinError>) {
        let current = current(self.models.shown.as_ref());
        let reference = self.bench.lists_arrived(joined, current);
        // A configured endpoint's window is its listing's.
        if self
            .models
            .shown
            .as_ref()
            .is_some_and(|shown| self.bench.is_configured_endpoint(&shown.endpoint))
        {
            self.send_context_window();
        }
        if let Some(reference) = reference {
            self.set_model(&reference);
        }
    }

    /// `/compact now`: ask the agent's next compaction check to
    /// compact (`mob/compact_member`, no rebuild); its next model request
    /// compacts, and the gauge follows the `compaction_completed` event.
    /// A member the engine refuses (dormant after a daemon restart, before
    /// its first run) is asked again through its session
    /// (`session/compact`), which refuses the same way when it is not live.
    pub(super) fn compact_now(&self) {
        let Some(link) = self.conn.link() else {
            self.send(Msg::CompactRequested(Err(CompactFailure::Request(
                self.unattached(),
            ))));
            return;
        };
        let connection = Arc::clone(&link.connection);
        let session = link.session.clone();
        let reports = self.reporter();
        tokio::spawn(async move {
            let asked = tokio::time::timeout(CALL_TIMEOUT, async {
                match remote::compact_member(&connection).await {
                    Ok(()) => Ok(()),
                    Err(member) => remote::compact_session(&connection, &session)
                        .await
                        .map_err(|session| format!("{member}; {session}")),
                }
            })
            .await
            .unwrap_or_else(|_| Err("the daemon did not answer in time".to_string()));
            reports.send(Report::CompactAsked(asked));
        });
    }

    /// The compaction request answered ([`Self::compact_now`]). The engine
    /// compacts only at the compaction check before a run's next model
    /// request, so an idle agent would compact only when the user sends
    /// something; the client starts that run itself ([`COMPACT_PROMPT`],
    /// a quiet turn: the UI shows its compaction events, not its prompt or
    /// reply) and tells the UI while it runs ([`Msg::CompactionRun`]),
    /// which holds what the user types until it ends. A turn already
    /// running compacts at its own next check.
    pub(super) fn compact_asked(&mut self, asked: Result<(), String>) {
        if asked.is_ok()
            && !self.turn.is_open()
            && let Some(connection) = self.connection()
        {
            let number = self.turns.next();
            self.turn = Turn::open(Kind::Compaction { id: number });
            self.send(Msg::CompactionRun(true));
            self.outbound(Outbound {
                connection,
                text: COMPACT_PROMPT.to_string(),
                steer: None,
                queue: true,
                retry: None,
                turn: number,
                reports: self.reporter(),
                ui_tx: self.ui_tx.clone(),
            });
        }
        self.send(Msg::CompactRequested(
            asked.map_err(CompactFailure::Request),
        ));
    }
}

use crate::ui::COMPACT_PROMPT;

/// What an interrupt says when the agent runs nothing.
pub const NOTHING_RUNS: &str = "nothing is running";

/// What a press says, once, after the run its interrupt was for ended.
pub const ALREADY_STOPPED: &str = "the turn is already stopped; nothing else was interrupted";

/// An interrupt call that brought no answer, in plain words.
fn interrupt_failure(err: &CallFailure) -> String {
    match err {
        CallFailure::TimedOut => {
            "the daemon did not answer the interrupt in time; press Esc again".to_string()
        }
        // The daemon's own refusals and the connection's errors are this
        // application's words.
        CallFailure::Failed(err) => {
            format!("the interrupt did not reach the agent ({err}); press Esc again")
        }
    }
}

/// What an interrupt says when it cancelled a run this client had no
/// turn for.
pub const INTERRUPTED_UNSEEN: &str =
    "interrupted the agent's run (one this client had not started or no longer followed)";

/// What the UI is told when the daemon's answer to a steer names no
/// input: its delivery cannot be told from another client's.
pub const UNNAMED_INPUT: &str = "the daemon did not name the input your message became, so this client cannot tell when the agent reads it; the agent has it and gets it at its next step";

/// What a prompt's turn says when nobody can tell whether the agent got
/// it ([`SendFailure::Unknown`]).
pub const UNKNOWN_FATE: &str = "the daemon did not confirm your message: it may have reached the agent; check the transcript before sending it again";

/// A key no other send of this client's shares ([`protocol::SEND_KEY`]).
fn send_key() -> String {
    static SENDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SENDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let random = crate::hex::random::<8>().unwrap_or_default();
    format!("{}-{n}-{random}", std::process::id())
}

/// Whether a call that failed with `err` may have reached the daemon all
/// the same (its answer was lost): a timeout, a connection that broke, an
/// answer that could not be read. A refusal the daemon answered, or a
/// connection never made, did not.
fn may_have_arrived(err: &CallFailure) -> bool {
    !matches!(
        err,
        CallFailure::Failed(
            crate::client::ClientError::Rpc { .. } | crate::client::ClientError::Connect { .. }
        )
    )
}

/// The daemon's word on the send `key` whose call failed with `err`
/// ([`protocol::method::Sent`]): admitted (the agent has it, as the input
/// named), refused or never received (the agent does not), or unknown.
async fn ask_fate(
    connection: &Connection,
    key: &str,
    err: &CallFailure,
) -> Result<Option<InputId>, SendFailure> {
    let asked = bounded(
        CALL_TIMEOUT,
        remote::call::<protocol::method::Sent>(
            connection,
            &protocol::SentParams {
                key: key.to_string(),
            },
        ),
    )
    .await;
    match asked {
        Ok(protocol::SentFate::Admitted { input_id }) => {
            tracing::info!(%err, "a send's answer was lost; the daemon says the agent has it");
            Ok(input_id)
        }
        Ok(protocol::SentFate::Refused { why }) => Err(SendFailure::NotSent(why)),
        Ok(protocol::SentFate::NotReceived) => Err(SendFailure::NotSent(format!(
            "{err}, and the daemon did not receive it"
        ))),
        Ok(protocol::SentFate::OnItsWay) => Err(SendFailure::Unknown(format!(
            "{err}; the daemon has not had the agent's answer yet"
        ))),
        Err(asked) => Err(SendFailure::Unknown(format!(
            "{err}; asking the daemon what became of it failed too: {asked}"
        ))),
    }
}

/// Send one prompt or steer ([`Outbound`]) and report how it went. A
/// prompt that repeats the failed turn's is sent as [`RETRY_INPUT`] when
/// that prompt is still its conversation's last message (a failed run
/// keeps it committed, so the text again would be in the transcript
/// twice). A send whose answer may have been lost (a timeout, a broken
/// connection) is not told as refused: the daemon is asked what it did
/// with it, by the key the send carried ([`ask_fate`]).
pub(super) async fn send_outbound(outbound: Outbound) {
    let Outbound {
        connection,
        text,
        steer,
        queue,
        retry,
        turn,
        reports,
        ui_tx,
    } = outbound;
    let mut sent: &str = &text;
    if let Some(session) = retry
        && repeats_the_last_prompt(&connection, &session, &text).await
    {
        let _ = ui_tx.send(Msg::Notice {
            level: NoticeLevel::Info,
            text: "your message is already in the transcript from the failed turn; retrying it"
                .to_string(),
        });
        sent = RETRY_INPUT;
    }
    let key = send_key();
    let result = bounded(CALL_TIMEOUT, remote::send(&connection, sent, !queue, &key)).await;
    // The daemon reloaded the agent after a storage failure; the input was
    // not taken and goes once more when the agent's stream is open again.
    if let Err(CallFailure::Failed(crate::client::ClientError::Rpc { code, .. })) = &result
        && *code == protocol::AGENT_RELOADED
    {
        reports.send(Report::Sent {
            turn,
            steer,
            result: Err(SendFailure::NotSent(String::new())),
            again: Some(sent.to_string()),
        });
        return;
    }
    let result = match result {
        Ok(input) => Ok(input),
        Err(err) if may_have_arrived(&err) => ask_fate(&connection, &key, &err).await,
        // The daemon's plain words for an agent its storage failure left
        // unable to take input (`crate::daemon::storage`).
        Err(CallFailure::Failed(crate::client::ClientError::Rpc { code, message, .. }))
            if code == crate::daemon::storage::reload_required_code() =>
        {
            Err(SendFailure::NotSent(message))
        }
        Err(other) => Err(SendFailure::NotSent(other.to_string())),
    };
    reports.send(Report::Sent {
        turn,
        steer,
        result,
        again: None,
    });
}

/// Whether `text` is the last message of `session` as the user's prompt.
pub(super) async fn repeats_the_last_prompt(
    connection: &Connection,
    session: &SessionId,
    text: &str,
) -> bool {
    // Only the last message travels, not the whole transcript on every
    // retry.
    let Ok(last) = bounded(CALL_TIMEOUT, remote::last_message(connection, session)).await else {
        return false;
    };
    matches!(
        last,
        Some(Message::User(user))
            if user.transcript_role == meerkat_core::TranscriptUserRole::Conversational
                && user.text_content().trim() == text.trim()
    )
}

/// The run a run's own event names (its resumption or its end).
fn run_of(event: &AgentEvent) -> Option<&RunId> {
    match event {
        AgentEvent::RunResumed { run_id, .. } => Some(run_id),
        AgentEvent::RunCompleted { identity, .. } | AgentEvent::RunFailed { identity, .. } => {
            identity.run_id.as_ref()
        }
        _ => None,
    }
}

/// Events only a running run publishes (its start aside).
pub(super) fn joins_a_run(event: &AgentEvent) -> bool {
    matches!(
        event,
        AgentEvent::TurnStarted { .. }
            | AgentEvent::RunResumed { .. }
            | AgentEvent::TextDelta { .. }
            | AgentEvent::TextComplete { .. }
            | AgentEvent::ToolExecutionStarted { .. }
            | AgentEvent::ToolExecutionCompleted { .. }
            | AgentEvent::TurnCompleted { .. }
            | AgentEvent::RunCompleted { .. }
            | AgentEvent::RunFailed { .. }
    )
}

/// `/model <reference>`: resolved against this process's
/// endpoints and their fetched lists ([`parse_model_ref`]); a configured endpoint's switch carries its auth
/// binding, a built-in one's clears it.
///
/// # Errors
///
/// [`ModelRefError`] when the reference picks no single endpoint.
pub fn model_switch(
    reference: &str,
    endpoints: &EndpointRegistry,
    models: Option<&ModelCache>,
    current: Option<&str>,
) -> Result<ModelSwitch, ModelRefError> {
    let index = models.map(ModelCache::index).unwrap_or_default();
    let target = parse_model_ref(reference, endpoints.as_slice(), &index, current)?;
    let endpoint =
        endpoints
            .get(&target.endpoint)
            .ok_or_else(|| ModelRefError::UnknownEndpoint {
                name: target.endpoint.clone(),
                known: endpoints.names(),
            })?;
    let listed = models
        .and_then(|models| models.entry(&endpoint.name, &target.model))
        .map(|entry| crate::daemon::protocol::ListedModel {
            context_window: entry.context_window,
            max_output_tokens: entry.max_output_tokens,
            reasoning: entry.capabilities.reasoning,
            vision: entry.capabilities.vision,
        });
    Ok(ModelSwitch {
        model: target.model,
        provider: Some(endpoint.dialect.as_str().to_string()),
        auth_binding: endpoint.binding(),
        endpoint: endpoint.name.clone(),
        listed,
    })
}
