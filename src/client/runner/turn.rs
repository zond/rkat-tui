//! The UI's turn: prompts and steers (sent in order by one task), Esc,
//! the agent's events and when the turn ends, and the model the turn runs
//! on.

use super::*;

impl Runner {
    pub(super) fn start_turn(&mut self, text: &str) {
        let Some(connection) = self.connection() else {
            self.send(Msg::TurnFinished(TurnOutcome {
                result: Err(TurnFailure::Failed(format!(
                    "{}; try again once it is",
                    self.unattached()
                ))),
            }));
            return;
        };
        let text = self.expand_mentions(text);
        // The prompt of the turn that just failed, typed again in its
        // conversation: the task checks whether it is still the last
        // message there.
        let retry = self.failed_prompt.take().and_then(|(session, failed)| {
            (self.conn.link().is_some_and(|link| link.session == session)
                && failed.trim() == text.trim())
            .then_some(session)
        });
        self.turns += 1;
        self.turn = Turn {
            active: true,
            prompt: Some(text.clone()),
            id: self.turns,
            ..Turn::default()
        };
        self.outbound(Outbound {
            connection,
            text,
            steer: None,
            retry,
            turn: self.turns,
            reports: self.reporter(),
            ui_tx: self.ui_tx.clone(),
        });
    }

    pub(super) fn steer(&mut self, id: PendingId, text: &str) {
        if !self.turn.active {
            self.send(Msg::SteerAck {
                id,
                result: Err(SteerFailure::NotRunning),
            });
            return;
        }
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
        self.turn.steers.sending(id);
        self.outbound(Outbound {
            connection,
            text,
            steer: Some(id),
            retry: None,
            turn: self.turn.id,
            reports: self.reporter(),
            ui_tx: self.ui_tx.clone(),
        });
    }

    /// A prompt or a steer of turn `turn` was sent (`result`): a steer is
    /// acknowledged (taken back from the turn when it failed); a prompt
    /// that failed ends its turn, if that turn is still the UI's.
    ///
    /// A steer's answer names the input the runtime admitted it as, which
    /// the agent's `boundary_append_applied` names when it joins the run
    /// ([`Steers`]). An answer that names none ([`UNNAMED_INPUT`]) is
    /// acknowledged (the runtime has the message) and shown delivered at
    /// once with a notice: no other client's delivery is taken for it.
    pub(super) fn sent(
        &mut self,
        turn: u64,
        steer: Option<PendingId>,
        result: Result<Option<InputId>, String>,
    ) {
        let current = self.turn.active && self.turn.id == turn;
        match (steer, result) {
            (Some(id), Ok(input)) => {
                if current {
                    self.turn.steered = true;
                }
                self.admitted.push(id);
                self.send(Msg::SteerAck { id, result: Ok(()) });
                let delivered = match input {
                    Some(input) => self.turn.steers.admitted(id, input),
                    None => {
                        self.turn.steers.forget(id);
                        self.notice(NoticeLevel::Warn, UNNAMED_INPUT);
                        true
                    }
                };
                if delivered {
                    self.send(Msg::SteerDelivered(id));
                }
            }
            (Some(id), Err(why)) => {
                self.turn.steers.forget(id);
                self.send(Msg::SteerAck {
                    id,
                    result: Err(SteerFailure::Rejected(why)),
                });
            }
            (None, Ok(_)) => {}
            (None, Err(why)) if current => {
                self.turn.record(Err(TurnFailure::Failed(format!(
                    "the prompt was not sent: {why}"
                ))));
                self.finish_turn();
            }
            (None, Err(why)) => {
                tracing::info!(%why, "a prompt of an earlier turn was not sent");
            }
        }
    }

    /// ↑ on an empty editor while the agent holds this client's steers:
    /// ask the daemon to take back the ones it still holds queued
    /// (`rkat/recall`, by the inputs their sends admitted); answered with
    /// [`Msg::Recalled`] when the daemon has (or with nothing to ask, at
    /// once).
    pub(super) fn recall(&mut self) {
        let outstanding = self.turn.steers.outstanding();
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
            Ok(answer) => (self.turn.steers.recalled(&answer.recalled), answer.errors),
            Err(why) => (Vec::new(), vec![why]),
        };
        self.admitted.retain(|id| !recalled.contains(id));
        self.send(Msg::Recalled { recalled, errors });
    }

    /// Hand `outbound` to the task that sends the prompts and steers, one
    /// after another in the order they were made (a steer must not reach
    /// the agent before the prompt it follows); the task is started on
    /// first use.
    pub(super) fn outbound(&mut self, outbound: Outbound) {
        let sender = self.outbound.get_or_insert_with(|| {
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
                result: Err("the client could not send it".to_string()),
            });
        }
    }

    /// Esc: stop the run the stream reported; a second Esc after
    /// [`STOP_GRACE`] (or a first one before any run was reported)
    /// force-cancels the member.
    pub(super) fn interrupt(&mut self) {
        if !self.turn.active {
            return;
        }
        let now = Instant::now();
        let Some(connection) = self.connection() else {
            self.turn.record(Err(TurnFailure::Interrupted));
            self.finish_turn();
            return;
        };
        let reports = self.reporter();
        let stop = match (&self.turn.interrupted_at, &self.turn.current_run) {
            (None, Some(run)) if self.turn.open_runs.contains(run) => Some(run.to_string()),
            (None, _) => None,
            (Some(first), _)
                if now.duration_since(*first) >= STOP_GRACE && !self.turn.force_cancelled =>
            {
                None
            }
            (Some(_), _) => return,
        };
        if self.turn.interrupted_at.is_none() {
            self.turn.interrupted_at = Some(now);
            self.turn.give_up_at = Some(now + INTERRUPT_DEADLINE);
        }
        if stop.is_none() {
            self.turn.force_cancelled = true;
        }
        tokio::spawn(async move {
            let result = match stop {
                Some(run) => bounded(
                    CALL_TIMEOUT,
                    remote::stop_run(&connection, &run, INTERRUPT_REASON),
                )
                .await
                .map(drop),
                None => bounded(CALL_TIMEOUT, remote::force_cancel(&connection)).await,
            };
            if let Err(failure) = result {
                reports.send(Report::Interrupt(failure.to_string()));
            }
        });
    }

    /// One of the member's events (live, or replayed after the cursor; of
    /// the space `epoch`): shown once ([`Mark`]), and what it means for the
    /// turn. A stream that reports it lost events the member no longer
    /// retains (the cursor was older than its window) is rendered again
    /// from the conversation's start.
    pub(super) fn on_event(&mut self, envelope: EventEnvelope<AgentEvent>, epoch: Option<&str>) {
        if matches!(envelope.payload, AgentEvent::StreamTruncated { .. }) {
            if let Some(link) = self.conn.link()
                && !self.switching
            {
                let session = link.session.clone();
                self.rerender(session, Again::LostEvents);
            }
            return;
        }
        if let Some(mark) = &mut self.mark
            && !mark.admit(epoch, envelope.seq)
        {
            return;
        }
        self.followers.convening(&envelope.payload, None);
        let mut terminal = false;
        // A run already going when this client attached (its start was shown
        // before it detached): the UI is busy until its end.
        if !self.turn.active && joins_a_run(&envelope.payload) {
            self.turn = Turn {
                active: true,
                saw_run: true,
                ..Turn::default()
            };
            self.send(Msg::RemoteTurn { prompt: None });
        }
        match &envelope.payload {
            // Another agent's message (P5). Into a running turn it is
            // delivered at the next boundary, and the events carry no text:
            // the block names the sender. An idle agent wakes for it: the
            // run's input carries the text (below).
            AgentEvent::PeerContentIngested {
                kind: meerkat_core::CommsNoticeKind::Message,
                peer: Some(peer),
                ..
            } => {
                if self.turn.active {
                    let from = peer.display_name.as_deref().unwrap_or("another agent");
                    self.send(Msg::Peer {
                        from: crate::ui::peer::short_name(from).to_string(),
                        text: String::new(),
                    });
                }
                return;
            }
            AgentEvent::RunStarted {
                identity, input, ..
            } => {
                if !self.turn.active {
                    self.turn = Turn {
                        active: true,
                        ..Turn::default()
                    };
                    let prompt = input.prompt_text();
                    match prompt.as_deref().and_then(crate::ui::peer::of_input) {
                        Some((from, text)) => {
                            self.send(Msg::Peer {
                                from: crate::ui::peer::short_name(&from).to_string(),
                                text,
                            });
                            self.send(Msg::RemoteTurn { prompt: None });
                        }
                        None => self.send(Msg::RemoteTurn { prompt }),
                    }
                } else if self.turn.saw_run {
                    // A follow-up run: it carries the steers that missed
                    // the boundary.
                    let own = self.turn.steers.take_all();
                    if own.is_empty() {
                        // Another client's steer: its text joins the turn.
                        if let Some(prompt) = input.prompt_text() {
                            self.send(Msg::RemoteTurn {
                                prompt: Some(prompt),
                            });
                        }
                    }
                    for id in own {
                        self.send(Msg::SteerDelivered(id));
                    }
                }
                self.turn.saw_run = true;
                self.turn.finish_by = None;
                self.turn.recheck_at = None;
                self.turn.settle += 1;
                if let Some(run) = &identity.run_id {
                    self.turn.open_runs.insert(run.clone());
                    self.turn.current_run = Some(run.clone());
                }
            }
            AgentEvent::ToolExecutionCompleted { name, .. }
                if crate::daemon::tasks::changes_tasks(name) =>
            {
                self.refresh_tasks();
            }
            AgentEvent::BoundaryAppendApplied { input_id, .. } => {
                // This client's steer only: another client's input marks
                // nothing here.
                if let Some(id) = self.turn.steers.applied(input_id) {
                    self.send(Msg::SteerDelivered(id));
                }
            }
            AgentEvent::RunCompleted {
                identity,
                result,
                usage,
                ..
            } => {
                if let Some(run) = &identity.run_id {
                    self.turn.open_runs.remove(run);
                }
                let usage = usage.as_usage();
                self.turn.record(Ok(TurnSummary {
                    text: result.clone(),
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                }));
                terminal = true;
            }
            AgentEvent::RunFailed {
                identity,
                error_report,
                ..
            } => {
                if let Some(run) = &identity.run_id {
                    self.turn.open_runs.remove(run);
                }
                let interrupted = self.turn.interrupted_at.is_some()
                    || error_report.class == AgentErrorClass::Cancelled;
                self.turn.record(Err(if interrupted {
                    TurnFailure::Interrupted
                } else {
                    TurnFailure::Failed(error_report.message.clone())
                }));
                terminal = true;
            }
            _ => {}
        }
        self.send(Msg::Agent(envelope));
        if terminal {
            self.save_cursor();
            if self.turn.active && self.turn.open_runs.is_empty() {
                self.runs_ended();
            }
        }
    }

    /// Every run of the turn ended. Input the runtime still holds (a steer
    /// that missed the boundary, whichever client sent it) runs as a
    /// follow-up run of this turn, so the turn ends only when the daemon
    /// reports none, within [`FOLLOW_UP_WAIT`]. Without a daemon to ask it
    /// ends [`RUN_GRACE`] later, or waits the bound for a steer of its own.
    fn runs_ended(&mut self) {
        let now = Instant::now();
        let Some(connection) = self.connection() else {
            let wait = if self.turn.steers.following() {
                FOLLOW_UP_WAIT
            } else {
                RUN_GRACE
            };
            self.turn.finish_by = Some(now + wait);
            return;
        };
        self.turn.finish_by = Some(now + FOLLOW_UP_WAIT);
        self.ask_active_inputs(connection);
    }

    /// Ask the daemon how much unfinished input the conversation holds.
    fn ask_active_inputs(&mut self, connection: Arc<Connection>) {
        self.turn.recheck_at = None;
        self.turn.settle += 1;
        let check = self.turn.settle;
        let reports = self.reporter();
        tokio::spawn(async move {
            let result = bounded(
                CALL_TIMEOUT,
                remote::call::<protocol::method::ActiveInputs>(&connection, &protocol::Empty {}),
            )
            .await
            .map(|answer| answer.count)
            .map_err(|err| err.to_string());
            reports.send(Report::ActiveInputs { check, result });
        });
    }

    /// The daemon's answer to [`Self::ask_active_inputs`]: none held and no
    /// steer of this client outstanding ends the turn; otherwise it asks
    /// again shortly (a run starting meanwhile cancels that). A failed
    /// question falls back to the short grace.
    pub(super) fn active_inputs(&mut self, check: u64, result: Result<usize, String>) {
        if !self.turn.active || check != self.turn.settle || !self.turn.open_runs.is_empty() {
            return;
        }
        match result {
            Ok(0) if !self.turn.steers.following() => self.finish_turn(),
            Ok(held) => {
                self.turn.held_input = held > 0;
                self.turn.recheck_at = Some(Instant::now() + INPUT_RECHECK);
            }
            Err(err) => {
                tracing::debug!(error = %err, "cannot ask the daemon for unfinished input");
                self.turn.finish_by = Some(Instant::now() + RUN_GRACE);
            }
        }
    }

    pub(super) fn on_deadline(&mut self) {
        let now = Instant::now();
        if self.turn.finish_by.is_some_and(|at| at <= now) {
            if self.turn.held_input || self.turn.steers.following() {
                self.notice(
                    NoticeLevel::Warn,
                    "the agent still holds a message for this turn; it runs when the agent takes it",
                );
            }
            self.finish_turn();
        } else if self.turn.recheck_at.is_some_and(|at| at <= now) {
            self.turn.recheck_at = None;
            match self.connection() {
                Some(connection) => self.ask_active_inputs(connection),
                None => self.turn.finish_by = Some(now + RUN_GRACE),
            }
        } else if self.turn.give_up_at.is_some_and(|at| at <= now) {
            self.notice(
                NoticeLevel::Warn,
                "the daemon did not confirm the interrupt; the run may still be going (it shows here when it reports)",
            );
            self.turn.record(Err(TurnFailure::Interrupted));
            self.finish_turn();
        }
    }

    pub(super) fn finish_turn(&mut self) {
        let turn = std::mem::take(&mut self.turn);
        if !turn.active {
            return;
        }
        // A failed turn's prompt stays committed: the same text typed again
        // is a retry, not a second copy ([`Runner::retry_input`]).
        self.failed_prompt = match (&turn.result, &turn.prompt, self.conn.link()) {
            (Some(Err(TurnFailure::Failed(_))), Some(prompt), Some(link)) if !turn.steered => {
                Some((link.session.clone(), prompt.clone()))
            }
            _ => None,
        };
        // Steers still held went to the runtime; it owns them now.
        let mut steers = turn.steers;
        for id in steers.take_all() {
            self.send(Msg::SteerDelivered(id));
        }
        self.send(Msg::TurnFinished(TurnOutcome {
            result: turn.result.unwrap_or_else(|| Ok(TurnSummary::default())),
        }));
        self.save_cursor();
        // The denial counters may have moved.
        self.refresh_mode();
    }

    /// The status bar names `model` on `endpoint` (of `dialect`).
    pub(super) fn show_model(&mut self, endpoint: &str, dialect: Option<WireDialect>, model: &str) {
        self.model = Some(ShownModel {
            endpoint: endpoint.to_string(),
            model: model.to_string(),
        });
        self.send(Msg::Status(StatusUpdate::Model {
            endpoint: endpoint.to_string(),
            model: model.to_string(),
        }));
        self.current_dialect = dialect;
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
        let Some(ShownModel { endpoint, model }) = &self.model else {
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
            let provider = self.current_dialect?.into();
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
            self.model.as_ref().map(|shown| shown.endpoint.as_str()),
        ) {
            Ok(switch) => self.switch_model(switch),
            Err(err) => {
                if !self.bench.defer_until_listed(reference) {
                    self.notice(NoticeLevel::Warn, format!("model not changed: {err}"));
                }
            }
        }
    }

    /// Switch the orchestrator's session to `switch` in place
    /// (`rkat/set_model`): refused while a
    /// turn runs; the next prompt is an ordinary send.
    pub(super) fn switch_model(&self, switch: ModelSwitch) {
        if self.turn.active {
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

    /// The model lists arrived ([`Workbench::background`]): a `/model
    /// <ref>` that waited for them switches now.
    pub(super) fn model_lists(&mut self, joined: Result<FetchResults, tokio::task::JoinError>) {
        let current = current(self.model.as_ref());
        let reference = self.bench.lists_arrived(joined, current);
        // A configured endpoint's window is its listing's.
        if self
            .model
            .as_ref()
            .is_some_and(|shown| self.bench.is_configured_endpoint(&shown.endpoint))
        {
            self.send_context_window();
        }
        if let Some(reference) = reference {
            self.set_model(&reference);
        }
    }

    /// `/compact now`: ask the orchestrator's next compaction check to
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
        let ui_tx = self.ui_tx.clone();
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
            let _ = ui_tx.send(Msg::CompactRequested(
                asked.map_err(CompactFailure::Request),
            ));
        });
    }
}

/// What the UI is told when the daemon's answer to a steer names no
/// input: its delivery cannot be told from another client's.
pub const UNNAMED_INPUT: &str = "the daemon did not name the input your message became, so this client cannot tell when the agent reads it; the agent has it and gets it at its next step";

/// Send one prompt or steer ([`Outbound`]) and report how it went. A
/// prompt that repeats the failed turn's is sent as [`RETRY_INPUT`] when
/// that prompt is still its conversation's last message (a failed run
/// keeps it committed, so the text again would be in the transcript
/// twice).
pub(super) async fn send_outbound(outbound: Outbound) {
    let Outbound {
        connection,
        text,
        steer,
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
    let result = bounded(
        CALL_TIMEOUT,
        remote::send(&connection, sent, steer.is_some()),
    )
    .await
    .map_err(|err| err.to_string());
    reports.send(Report::Sent {
        turn,
        steer,
        result,
    });
}

/// Whether `text` is the last message of `session` as the user's prompt.
pub(super) async fn repeats_the_last_prompt(
    connection: &Connection,
    session: &SessionId,
    text: &str,
) -> bool {
    let Ok(messages) = bounded(CALL_TIMEOUT, remote::transcript(connection, session)).await else {
        return false;
    };
    let last = messages
        .iter()
        .rev()
        .find(|message| !matches!(message, Message::System(_) | Message::SystemNotice(_)));
    matches!(
        last,
        Some(Message::User(user))
            if user.transcript_role == meerkat_core::TranscriptUserRole::Conversational
                && user.text_content().trim() == text.trim()
    )
}

/// Events only a running run publishes (its start aside).
pub(super) fn joins_a_run(event: &AgentEvent) -> bool {
    matches!(
        event,
        AgentEvent::TurnStarted { .. }
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
