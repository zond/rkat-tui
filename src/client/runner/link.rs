//! The connection: connecting (and starting the daemon), the attach's
//! catch-up, losing and re-making the link, the notifications it carries,
//! the helper mobs it follows and the agent's current session.

use super::commands::others_notice;
use super::switch::replay_to_end;
use super::*;

impl Runner {
    /// Try the failed connect again, as the first one (a daemon may be
    /// started).
    pub(super) fn retry(&mut self) {
        let tries = match self.conn {
            Conn::Retrying { tries, .. } => tries,
            _ => 0,
        };
        self.show(LinkState::Connecting);
        self.connect(true, tries);
    }

    /// `/daemon start`: connect again after the connect failed for good
    /// (or the daemon was stopped), starting the daemon when this client
    /// may; attached or connecting, there is nothing to do.
    pub(super) fn daemon_start(&mut self) {
        if let Some(link) = self.conn.link() {
            self.notice(
                NoticeLevel::Info,
                format!(
                    "already attached to the daemon at {}",
                    link.endpoint.address
                ),
            );
            return;
        }
        if matches!(self.conn, Conn::Connecting { .. }) {
            self.notice(NoticeLevel::Info, "already connecting to the daemon");
            return;
        }
        self.notice(NoticeLevel::Info, "connecting to the workspace's daemon");
        self.show(LinkState::Connecting);
        self.connect(true, 0);
    }

    /// A connect failed with `reason`: try again after a backoff while
    /// retries are left (none for an agent the daemon no longer has),
    /// else say so for good.
    pub(super) fn connect_failed(
        &mut self,
        tries: u32,
        reason: String,
        log: Option<PathBuf>,
        retry: bool,
    ) {
        let retry_at = (retry && tries < START_RETRIES)
            .then(|| Instant::now() + START_RETRY_FIRST * 2u32.pow(tries));
        self.conn = match retry_at {
            Some(at) => Conn::Retrying {
                at,
                tries: tries + 1,
            },
            None => Conn::Failed,
        };
        if retry_at.is_none() {
            self.notice(
                NoticeLevel::Error,
                format!(
                    "cannot attach to the workspace daemon: {reason}; /daemon start tries again"
                ),
            );
        } else {
            tracing::warn!(%reason, retries = tries + 1, "attach failed, retrying");
        }
        self.show(LinkState::Failed {
            reason,
            log,
            retry_at: retry_at.map(Instant::into_std),
        });
    }

    /// Start connecting in the background: `first` (the first connect, a
    /// retry of it, a `/daemon start`) with start-on-demand, otherwise as a
    /// reconnect with backoff; `tries` connects failed before this one.
    pub(super) fn connect(&mut self, first: bool, tries: u32) {
        let files = self.config.endpoint_files.clone();
        // The agent's saved cursor is read once it is known (at attach).
        let saved = self
            .mark
            .as_ref()
            .map(|mark| mark.cursor(self.daemon_id_seen.as_deref()));
        let attach = Attach {
            agent: match &self.agent {
                Some(name) => AgentChoice::Agent { name: name.clone() },
                None => self.config.agent.clone(),
            },
            cursor: (!self.agent_switch).then(|| self.config.cursor.clone()),
            start_token: self.config.start_token.clone(),
        };
        let plan = ConnectPlan {
            address: self.config.address.clone(),
            files,
            spawn: self.config.spawn.clone(),
            first,
            saved,
            attach,
            ui_tx: self.ui_tx.clone(),
        };
        let task = tokio::spawn(plan.run());
        self.conn = Conn::Connecting { task, tries };
    }

    pub(super) fn connected(
        &mut self,
        joined: Result<Result<Established, ClientError>, tokio::task::JoinError>,
    ) {
        let tries = match self.conn {
            Conn::Connecting { tries, .. } => tries,
            _ => 0,
        };
        let established = match joined {
            Ok(Ok(established)) => established,
            Ok(Err(err)) => {
                let (reason, log) = failure(&err);
                self.connect_failed(tries, reason, log, !gone(&err));
                return;
            }
            Err(err) => {
                tracing::warn!(error = %err, "the connect task failed");
                self.connect_failed(tries, format!("the connect task failed: {err}"), None, true);
                return;
            }
        };
        let Established {
            link,
            mark,
            reset,
            identity,
            usage,
            queued,
        } = established;
        let address = link.endpoint.address;
        let agent = link.connection.agent().to_string();
        let switched_agent = std::mem::take(&mut self.agent_switch);
        let seated = link.connection.seated();
        self.agent = Some(agent.clone());
        self.send(Msg::Status(StatusUpdate::Agent(Some(agent.clone()))));
        let daemon_id = super::super::catchup::daemon_id(&link.endpoint.token);
        // A daemon this client has not shown yet (the first attach, or one
        // that restarted): what its start-up had to say.
        let new_daemon = self.daemon_id_seen.as_deref() != Some(daemon_id.as_str());
        self.daemon_id_seen = Some(daemon_id);
        let session = link.session.to_string();
        let fresh = self.mark.is_none();
        // A render from the start replaces what was shown; a resumed
        // stream goes on from the mark the cursor was saved from.
        if reset.is_some()
            || self
                .mark
                .as_ref()
                .is_none_or(|current| current.session_id() != mark.session_id())
        {
            self.mark = Some(mark);
        }
        self.send(Msg::Status(StatusUpdate::Session(session.clone())));
        if switched_agent {
            self.notice(
                NoticeLevel::Info,
                format!("switched to agent {agent}; the agent you left keeps running"),
            );
        } else if fresh {
            self.notice(NoticeLevel::Info, ATTACH_NOTICE);
            self.notice(
                NoticeLevel::Info,
                if seated {
                    format!("working with agent {agent} (seated for this rkat-tui)")
                } else {
                    format!("working with agent {agent}")
                },
            );
        } else {
            self.notice(
                NoticeLevel::Info,
                format!("reattached to the daemon at {address}"),
            );
        }
        if let Some(identity) = identity {
            self.followers.set_default_model(&identity.model);
            self.show_model(
                identity.endpoint_name(),
                WireDialect::parse(&identity.provider),
                &identity.model,
            );
        }
        let daemon_build = link.connection.daemon_build().map(str::to_string);
        if new_daemon && daemon_build.as_deref() != Some(crate::BUILD) {
            self.notice(
                NoticeLevel::Warn,
                format!(
                    "the daemon runs build {}, this rkat-tui is build {}: /daemon stop and attach again to run this build",
                    daemon_build.as_deref().unwrap_or("unknown"),
                    crate::BUILD
                ),
            );
        }
        self.show(LinkState::Attached {
            address: address.to_string(),
            daemon_build,
        });
        // The helpers of this conversation: what was shown before a
        // reconnect stays shown.
        if fresh || self.followers.owner() != Some(&link.session) {
            self.followers.reset(link.session.clone());
        } else {
            self.followers.streams_lost();
        }
        self.orphans.clear();
        self.conn = Conn::Up {
            link,
            leaving: None,
        };
        self.link_epoch += 1;
        if let Some(blocks) = reset {
            self.send(Msg::ResetTranscript(blocks));
        }
        // The gauge, as a switch seeds it: the daemon's reading of the last
        // model call (the replay since the cursor may hold no turn end).
        if let Some(context) = usage.as_ref().and_then(|usage| usage.context) {
            self.send(Msg::Status(StatusUpdate::Context {
                used: Some(context.tokens),
                approx: context.approx,
            }));
        }
        // What the live stream delivered before the usage answer: its turns
        // are in the totals.
        for note in queued {
            self.on_notification(note);
        }
        // The counters are the session's totals; after the
        // replay and the live events queued before the usage read, whose
        // turns they already hold. Live turn ends after this add to them,
        // and the daemon pushes its later totals by model (`rkat/cost`).
        if let Some(usage) = usage {
            self.send(Msg::Status(StatusUpdate::Tokens {
                input: usage.input_tokens,
                output: usage.output_tokens,
            }));
            self.send(Msg::Status(StatusUpdate::CostTotals(usage.cost)));
        }
        // The mobs there are now; the daemon pushes the ones to come.
        self.list_mobs();
        self.refresh_tasks();
        if new_daemon {
            self.startup_notices();
        }
        // Every attach: what the agent was denied while nobody was with it.
        self.missed_notices();
        if fresh && !switched_agent {
            self.other_agents();
            match std::mem::replace(&mut self.config.start, super::super::StartAt::Current) {
                super::super::StartAt::Current => {}
                super::super::StartAt::Pick => self.show_resume_picker(),
                // The daemon bound this client to the agent that owns the
                // conversation (seated again on it when it had quit): a
                // switch is needed only when that agent moved on since.
                super::super::StartAt::Resume(reference)
                    if session.starts_with(reference.trim()) => {}
                super::super::StartAt::Resume(reference) => self.resume_session(reference),
            }
        }
    }

    /// The start-up notice naming the other agents working here
    /// (`rkat/status`), when there are any.
    pub(super) fn other_agents(&self) {
        let Some(mine) = self.connection().map(|c| c.agent().to_string()) else {
            return;
        };
        self.ask_if_attached::<protocol::method::Status>(protocol::Empty {}, move |read| {
            read.ok()
                .and_then(|status| others_notice(&status.agents, &mine))
                .map(|text| (NoticeLevel::Info, text))
                .into_iter()
                .collect()
        });
    }

    /// Show what this client's agent was denied in turns no user started
    /// while no client was attached (`rkat/missed`).
    pub(super) fn missed_notices(&self) {
        self.ask_if_attached::<protocol::method::Missed>(protocol::Empty {}, |read| match read {
            Ok(read) => read
                .notices
                .into_iter()
                .map(|notice| (NoticeLevel::Warn, notice.text))
                .collect(),
            Err(err) => {
                tracing::debug!(error = %err, "cannot read what the agent was denied unattended");
                Vec::new()
            }
        });
    }

    /// Show the daemon's start-up notices (`rkat/notices`).
    pub(super) fn startup_notices(&self) {
        self.ask_if_attached::<protocol::method::Notices>(protocol::Empty {}, |read| match read {
            Ok(read) => read
                .notices
                .into_iter()
                .map(|notice| {
                    let level = if notice.warn {
                        NoticeLevel::Warn
                    } else {
                        NoticeLevel::Info
                    };
                    (level, notice.text)
                })
                .collect(),
            Err(err) => {
                tracing::debug!(error = %err, "cannot read the daemon's start-up notices");
                Vec::new()
            }
        });
    }

    pub(super) fn push_mode(&self, status: ModeStatus) {
        self.send(Msg::Status(StatusUpdate::Mode {
            mode: status.mode,
            paused: status.paused,
            denials: status.denials,
        }));
    }

    /// The connection is gone: re-make it (unless the daemon was told to
    /// stop). A running turn goes on in the daemon; the catch-up shows it.
    pub(super) fn lost(&mut self) {
        let leaving = match std::mem::replace(&mut self.conn, Conn::Idle) {
            Conn::Up { leaving, .. } => leaving,
            _ => None,
        };
        self.save_cursor();
        self.followers.streams_lost();
        self.orphans.clear();
        self.known_mobs.clear();
        self.pushed_session = None;
        self.switching = false;
        // What is still pending shows again after the reattach.
        for id in self.approvals.clear() {
            self.send(Msg::EscalationResolved(id));
        }
        if leaving == Some(Leaving::Rehome) {
            // The re-home stopped this daemon; it reattaches when done.
            self.conn = Conn::Rehoming;
            return;
        }
        if leaving == Some(Leaving::Stop) {
            self.conn = Conn::Stopped;
            self.show(LinkState::Stopped);
            self.notice(
                NoticeLevel::Info,
                "the daemon has shut down; /quit to leave",
            );
            if self.turn.active {
                self.turn.record(Err(TurnFailure::Interrupted));
                self.finish_turn();
            }
            return;
        }
        self.show(LinkState::Reconnecting);
        self.connect(false, 0);
    }

    /// Drop the link and everything shown from it (a re-home or an agent
    /// switch makes a new one).
    pub(super) fn unlink(&mut self) {
        self.save_cursor();
        self.conn = Conn::Idle;
        self.followers.streams_lost();
        self.orphans.clear();
        self.known_mobs.clear();
        self.pushed_session = None;
        for id in self.approvals.clear() {
            self.send(Msg::EscalationResolved(id));
        }
    }

    pub(super) fn on_notification(&mut self, note: Notification) {
        if self.conn.link().is_none() {
            return;
        }
        if let Some(push) = Push::decode(note) {
            self.on_push(push);
        }
    }

    fn on_push(&mut self, push: Push) {
        let Some(link) = self.conn.link() else {
            return;
        };
        match push {
            Push::Decision(notice) => {
                let session = link.session.clone();
                if let Some(change) = self.approvals.notice(&session, *notice) {
                    for msg in change.messages() {
                        self.send(msg);
                    }
                }
            }
            Push::DecisionEnded(ended) => {
                if let Some(change) = self.approvals.decision_ended(&ended) {
                    for msg in change.messages() {
                        self.send(msg);
                    }
                }
            }
            // Another agent's call (its helpers' are its session's).
            Push::Approval(protocol::ApprovalNote::Decision { session_id, .. })
                if session_id != link.session => {}
            Push::Approval(approval) => {
                for msg in super::super::approvals::note_messages(approval) {
                    self.send(msg);
                }
            }
            // Another agent's change.
            Push::FileDiff(diff) if diff.agent != link.connection.agent() => {}
            Push::FileDiff(diff) => self.send(Msg::Diff {
                tool_use_id: diff.tool_use_id,
                path: diff.path,
                unified: diff.unified,
                added: diff.added,
                removed: diff.removed,
            }),
            Push::Triggered(fired)
                if fired.agent == link.connection.agent() && fired.session_id == link.session =>
            {
                self.send(Msg::Triggered {
                    label: fired.label,
                    prompt: fired.prompt,
                });
            }
            Push::Cost(totals) if totals.session_id == link.session => {
                self.send(Msg::Status(StatusUpdate::CostTotals(totals.cost)));
                // The gauge is the daemon's reading of the last model call
                // (after a compaction its estimate).
                if let Some(context) = totals.context {
                    self.send(Msg::Status(StatusUpdate::Context {
                        used: Some(context.tokens),
                        approx: context.approx,
                    }));
                }
            }
            Push::Switched(moved) if moved.agent == link.connection.agent() => {
                if moved.session_id == link.session {
                    self.pushed_session = None;
                } else if self.switching {
                    // Shown once the switch under way ends.
                    self.pushed_session = Some(moved.session_id);
                } else {
                    self.adopt_switch(moved.session_id, Shown::Others, None);
                }
            }
            Push::Mode(status) => self.push_mode(status),
            Push::Idle(note)
                if note.agent == link.connection.agent() && note.session_id == link.session =>
            {
                self.idle_reported(note);
            }
            Push::Triggered(_) | Push::Cost(_) | Push::Switched(_) | Push::Idle(_) => {}
            Push::Mob(protocol::MobNote {
                mob_id,
                change: protocol::MobChange::Hosts,
            }) => {
                self.known_mobs.insert(mob_id.to_string());
                self.open_known_mobs();
            }
            Push::Mob(protocol::MobNote {
                mob_id,
                change: protocol::MobChange::Destroyed,
            }) => {
                self.known_mobs.remove(mob_id.as_str());
            }
            Push::Stream {
                stream,
                event,
                epoch,
            } if stream == link.stream => match serde_json::from_value(event) {
                Ok(envelope) => self.on_event(envelope, epoch.as_deref()),
                Err(err) => tracing::warn!(error = %err, "a stream event this version cannot read"),
            },
            Push::Stream { stream, event, .. } => match serde_json::from_value(event) {
                Ok(attributed) => self.on_mob_event(stream, attributed),
                Err(err) => tracing::debug!(error = %err, "not an attributed event"),
            },
            // The conversation's own stream: the daemon seated the agent
            // again on it (a model switch rebuilt its agent).
            Push::StreamEnd(stream) if stream == link.stream && !self.switching => {
                let session = link.session.clone();
                self.rerender(session, Shown::Reseated);
            }
            Push::StreamEnd(stream) => self.followers.ended(&stream),
        }
    }

    /// An event of a mob's stream: a helper's, or one of a stream whose
    /// open has not answered yet.
    fn on_mob_event(&mut self, stream: String, attributed: meerkat_mob::AttributedEvent) {
        if let Some(by) = self.followers.helper_of(&stream, &attributed) {
            self.followers
                .convening(&attributed.envelope.payload, Some(&by));
        }
        if self.followers.owns(&stream) {
            let actions = self.followers.event(&stream, attributed);
            self.follow(actions);
            // A helper's session or a council may make a mob this agent's.
            self.open_known_mobs();
        } else {
            self.orphans.hold(stream, attributed);
        }
    }

    /// Carry out what the followers ask.
    pub(super) fn follow(&self, actions: Vec<FollowerAction>) {
        for action in actions {
            match action {
                FollowerAction::Ui(msg) => self.send(msg),
                FollowerAction::Backfill {
                    helper,
                    session,
                    after,
                } => {
                    let Some(connection) = self.connection() else {
                        continue;
                    };
                    let reports = self.reporter();
                    tokio::spawn(async move {
                        let events =
                            bounded(CALL_TIMEOUT, replay_to_end(&connection, &session, after))
                                .await
                                .ok()
                                .map(|(events, _)| events)
                                .unwrap_or_default();
                        reports.send(Report::Backfilled { helper, events });
                    });
                }
                FollowerAction::Snapshot { helper, session } => {
                    let Some(connection) = self.connection() else {
                        continue;
                    };
                    let reports = self.reporter();
                    tokio::spawn(async move {
                        if let Ok((model, _)) =
                            bounded(CALL_TIMEOUT, remote::identity(&connection, &session)).await
                        {
                            reports.send(Report::HelperModel { helper, model });
                        }
                    });
                }
            }
        }
    }

    /// List the daemon's mobs once (an attach, another conversation): the
    /// daemon pushes the ones that host helpers later ([`protocol::MOB`]).
    pub(super) fn list_mobs(&self) {
        let Some(connection) = self.connection() else {
            return;
        };
        let reports = self.reporter();
        tokio::spawn(async move {
            let mobs = bounded(CALL_TIMEOUT, remote::mobs(&connection))
                .await
                .unwrap_or_default();
            reports.send(Report::Mobs(mobs));
        });
    }

    /// Open every known mob that is this agent's and not open yet.
    pub(super) fn open_known_mobs(&mut self) {
        let mobs: Vec<String> = self
            .known_mobs
            .iter()
            .filter(|mob| self.followers.should_open(mob))
            .cloned()
            .collect();
        for mob in mobs {
            self.open_mob(mob);
        }
    }

    /// The daemon moved the agent to another conversation while a switch
    /// of this client's ran: show it now, unless the switch ended there.
    pub(super) fn follow_pushed_session(&mut self) {
        let Some(session) = self.pushed_session.take() else {
            return;
        };
        if self.conn.link().is_some_and(|link| link.session != session) {
            self.adopt_switch(session, Shown::Others, None);
        }
    }

    /// Open `mob`'s mob-wide stream and read its members' sessions.
    pub(super) fn open_mob(&self, mob: String) {
        let Some(connection) = self.connection() else {
            return;
        };
        let reports = self.reporter();
        tokio::spawn(async move {
            let opened = bounded(CALL_TIMEOUT, async {
                let stream = remote::open_mob_stream(&connection, &mob).await?;
                let mut members = Vec::new();
                for label in remote::members(&connection, &mob).await.unwrap_or_default() {
                    let session = remote::member_session(&connection, &mob, &label).await.ok();
                    members.push((label, session));
                }
                Ok::<_, ClientError>((stream, members))
            })
            .await
            .map_err(|err| err.to_string());
            reports.send(Report::MobOpened { mob, opened });
        });
    }

    pub(super) fn save_cursor(&self) {
        if let Some(mark) = &self.mark
            && let Some(agent) = &self.agent
            && mark.sequence() > 0
        {
            self.config
                .cursor
                .for_agent(agent)
                .save(&mark.cursor(self.daemon_id_seen.as_deref()));
        }
    }
}

/// The endpoint the first connect attaches to: a live one, or (auto mode)
/// the one a started daemon writes.
pub(super) async fn first_endpoint(
    files: &[PathBuf],
    spawn: Option<&SpawnPlan>,
    start_token: Option<&str>,
    ui_tx: &mpsc::UnboundedSender<Msg>,
) -> Result<Endpoint, ClientError> {
    let mode = if spawn.is_some() {
        AttachMode::Auto
    } else {
        AttachMode::Existing
    };
    let decision = launch::decide(launch::probe_any(files).await, mode);
    match (decision, spawn) {
        (Decision::Attach(endpoint), _) => Ok(endpoint),
        (Decision::Start, Some(plan)) => {
            let _ = ui_tx.send(Msg::Status(StatusUpdate::Connection(LinkState::Starting)));
            let (pid, mut exited) = launch::spawn_daemon(
                &plan.program,
                &launch::argv_for(&plan.argv, files),
                &plan.log,
                start_token,
            )?;
            let _ = ui_tx.send(Msg::Notice {
                level: NoticeLevel::Info,
                text: format!(
                    "started rkat-tui-mob (pid {pid}; output in {})",
                    plan.log.display()
                ),
            });
            launch::wait_for_endpoint(files, launch::START_TIMEOUT, &mut exited, &plan.log).await
        }
        (Decision::Start | Decision::Refuse, _) => Err(ClientError::NoDaemon {
            file: files.first().cloned().unwrap_or_default(),
        }),
    }
}

/// Start the daemon again after it died (its endpoint file is stale) and
/// wait for its endpoint.
pub(super) async fn restart(
    files: &[PathBuf],
    plan: &SpawnPlan,
    ui_tx: &mpsc::UnboundedSender<Msg>,
) -> Result<Endpoint, ClientError> {
    let _ = ui_tx.send(Msg::Status(StatusUpdate::Connection(LinkState::Starting)));
    // No start token: a daemon that died lost what it knew about its
    // clients, so the restarted one has no owner and `/quit` asks.
    let (pid, mut exited) = launch::spawn_daemon(
        &plan.program,
        &launch::argv_for(&plan.argv, files),
        &plan.log,
        None,
    )?;
    let _ = ui_tx.send(Msg::Notice {
        level: NoticeLevel::Warn,
        text: format!(
            "the daemon died (its endpoint file is stale); restarted rkat-tui-mob (pid {pid}; output in {})",
            plan.log.display()
        ),
    });
    launch::wait_for_endpoint(files, launch::START_TIMEOUT, &mut exited, &plan.log).await
}

/// How a connect attaches: the agent it asks for, where the agent's
/// saved cursor is (`None`: render its conversation from the start), and
/// the start token it presents.
#[derive(Debug, Clone)]
pub(super) struct Attach {
    agent: AgentChoice,
    cursor: Option<CursorFile>,
    start_token: Option<String>,
}

/// One connect: where, and whether it is the first (which may start the
/// daemon and is not retried here: the runner's own retries back off and
/// say so).
pub(super) struct ConnectPlan {
    /// The daemon's address when reached by address (`--attach=HOST:PORT`).
    address: Option<Endpoint>,
    /// The endpoint files of the workspace's daemon, otherwise.
    files: Vec<PathBuf>,
    /// How to start the daemon (auto mode).
    spawn: Option<SpawnPlan>,
    first: bool,
    saved: Option<Cursor>,
    attach: Attach,
    ui_tx: mpsc::UnboundedSender<Msg>,
}

impl ConnectPlan {
    /// Attach, trying again with backoff after a failure that may pass (a
    /// reconnect); the first connect and a refusal that will not pass
    /// ([`gone`]) end at their first failure.
    async fn run(self) -> Result<Established, ClientError> {
        let mut wait = BACKOFF_FIRST;
        let mut restarted: Option<Instant> = None;
        loop {
            match self.attempt(&mut restarted).await {
                Ok(established) => return Ok(established),
                Err(err) if self.first || gone(&err) => return Err(err),
                Err(err) => tracing::debug!(error = %err, "reconnect failed"),
            }
            tokio::time::sleep(wait).await;
            wait = (wait * 2).min(BACKOFF_MAX);
        }
    }

    /// One try: the address again; the first connect's endpoint (started
    /// when none is live, in auto mode); or a live endpoint, else (auto
    /// mode) the daemon started again when it died and left its endpoint
    /// file, at most every [`RESTART_EVERY`]. A daemon that shut down
    /// removed its file and is waited for.
    async fn attempt(&self, restarted: &mut Option<Instant>) -> Result<Established, ClientError> {
        let saved = self.saved.as_ref();
        if let Some(endpoint) = &self.address {
            return establish(endpoint.clone(), saved, &self.attach).await;
        }
        if self.first {
            let endpoint = first_endpoint(
                &self.files,
                self.spawn.as_ref(),
                self.attach.start_token.as_deref(),
                &self.ui_tx,
            )
            .await?;
            return establish(endpoint, saved, &self.attach).await;
        }
        let none = || ClientError::NoDaemon {
            file: self.files.first().cloned().unwrap_or_default(),
        };
        match launch::probe_any(&self.files).await {
            launch::Liveness::Live(endpoint) => establish(endpoint, saved, &self.attach).await,
            launch::Liveness::Stale(_)
                if restarted.is_none_or(|at| at.elapsed() >= RESTART_EVERY) =>
            {
                let Some(plan) = &self.spawn else {
                    return Err(none());
                };
                *restarted = Some(Instant::now());
                match restart(&self.files, plan, &self.ui_tx).await {
                    Ok(endpoint) => establish(endpoint, saved, &self.attach).await,
                    Err(err) => {
                        let _ = self.ui_tx.send(Msg::Notice {
                            level: NoticeLevel::Error,
                            text: format!("the daemon could not be restarted: {err}"),
                        });
                        Err(none())
                    }
                }
            }
            launch::Liveness::Stale(_) | launch::Liveness::Absent => Err(none()),
        }
    }
}

/// What the status bar says of a failed connect: the reason and the
/// daemon's log, when there is one to read.
pub(super) fn failure(err: &ClientError) -> (String, Option<PathBuf>) {
    match err {
        ClientError::DaemonExited { status, log } => {
            (format!("daemon exited ({status})"), Some(log.clone()))
        }
        ClientError::NeverCameUp { waited, log } => (
            format!("daemon did not come up within {}s", waited.as_secs()),
            Some(log.clone()),
        ),
        ClientError::NoDaemon { .. } => ("no daemon answers".to_string(), None),
        other => (other.to_string(), None),
    }
}

/// Whether a failed attach will fail again: the daemon refused to bind
/// this client to its agent (it is no longer live).
pub(super) fn gone(err: &ClientError) -> bool {
    matches!(err, ClientError::Refused { code, .. } if *code == protocol::NO_AGENT)
}

/// Attach (bound to the agent `attach` asks for), find the agent's
/// session and open its stream: resumed from `saved` or the agent's saved
/// cursor when it names this daemon's stream of the session (the events
/// since come on the stream), else live after rendering the conversation
/// from its start.
pub(super) async fn establish(
    endpoint: Endpoint,
    saved: Option<&Cursor>,
    attach: &Attach,
) -> Result<Established, ClientError> {
    let (connection, mut notifications) = Connection::attach_to(
        &endpoint,
        ATTACH_TIMEOUT,
        attach.start_token.as_deref(),
        false,
        Some(attach.agent.clone()),
    )
    .await?;
    // An agent seated for this client (a new one, or one seated again on
    // its conversation) has a new event stream: its conversation is
    // rendered from the start, whatever an earlier client saved.
    let from_file = match saved {
        _ if connection.seated() => None,
        Some(_) => None,
        None => attach
            .cursor
            .as_ref()
            .and_then(|cursor| cursor.for_agent(connection.agent()).load()),
    };
    let saved = if connection.seated() {
        None
    } else {
        saved.or(from_file.as_ref())
    };
    tokio::time::timeout(CALL_TIMEOUT, async {
        connection
            .call(protocol::engine::INITIALIZE, serde_json::json!({}))
            .await?;
        let session = connection
            .session()
            .cloned()
            .ok_or(ClientError::NoSession)?;
        let id = session.to_string();
        let resume = saved.and_then(|cursor| {
            cursor.resumable(&id, &super::super::catchup::daemon_id(&endpoint.token))
        });
        let (stream, mark, reset) = match resume {
            Some((epoch, seq)) => {
                let stream = remote::open_stream(&connection, Some((&epoch, seq))).await?;
                (stream, Mark::resume(&id, saved), None)
            }
            None => {
                // Live first, so nothing falls between the stream and the
                // render; the mark drops what both deliver. The snapshot's
                // cursor is where the render ends.
                let stream = remote::open_stream(&connection, None).await?;
                let latest = remote::snapshot(&connection, &session).await?.latest;
                let messages = remote::transcript(&connection, &session).await?;
                (
                    stream,
                    Mark::rendered(&id, latest),
                    Some(crate::ui::transcript::blocks_from_messages(&messages)),
                )
            }
        };
        let identity = match remote::identity(&connection, &session).await {
            Ok((model, provider)) => {
                let endpoint = remote::session_endpoint(&connection, &session)
                    .await
                    .ok()
                    .flatten();
                Some(SessionModel {
                    model,
                    provider,
                    endpoint,
                })
            }
            Err(_) => None,
        };
        let usage = remote::usage(&connection, &session).await.ok();
        // Every notification read before the usage answer: a turn that
        // ended before the usage read is in the totals; its live end must
        // not be added on top.
        let mut queued = Vec::new();
        while let Ok(note) = notifications.try_recv() {
            queued.push(note);
        }
        Ok(Established {
            link: Link {
                connection: Arc::new(connection),
                notifications,
                stream,
                session,
                endpoint,
            },
            mark,
            reset,
            identity,
            usage,
            queued,
        })
    })
    .await
    .map_err(|_| ClientError::Timeout("reading the agent's state"))?
}
