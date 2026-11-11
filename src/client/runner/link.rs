//! The connection: connecting (and starting the daemon), the attach's
//! catch-up, losing and re-making the link, the notifications it carries,
//! the helper mobs it follows and the agent's current session.

use super::commands::others_notice;
use super::switch::{last_context_before, replay_to_end};
use super::*;

impl Runner {
    /// Try the failed connect again, as the first one (a daemon may be
    /// started).
    pub(super) fn retry(&mut self) {
        self.retry_at = None;
        self.first_connect = true;
        self.show(LinkState::Connecting);
        self.connect();
    }

    /// `/daemon start`: connect again after the connect failed for good
    /// (or the daemon was stopped), starting the daemon when this client
    /// may; attached or connecting, there is nothing to do.
    pub(super) fn daemon_start(&mut self) {
        if let Some(link) = &self.link {
            self.notice(
                NoticeLevel::Info,
                format!(
                    "already attached to the daemon at {}",
                    link.endpoint.address
                ),
            );
            return;
        }
        if self.connecting.is_some() {
            self.notice(NoticeLevel::Info, "already connecting to the daemon");
            return;
        }
        self.stopping = false;
        self.retries = 0;
        self.notice(NoticeLevel::Info, "connecting to the workspace's daemon");
        self.retry();
    }

    /// A connect failed with `reason`: try again after a backoff while
    /// retries are left (none for an agent the daemon no longer has),
    /// else say so for good.
    pub(super) fn connect_failed(&mut self, reason: String, log: Option<PathBuf>, retry: bool) {
        let retry_at = (retry && self.retries < START_RETRIES).then(|| {
            let wait = START_RETRY_FIRST * 2u32.pow(self.retries);
            self.retries += 1;
            Instant::now() + wait
        });
        self.retry_at = retry_at;
        if retry_at.is_none() {
            self.first_failed = true;
            self.notice(
                NoticeLevel::Error,
                format!(
                    "cannot attach to the workspace daemon: {reason}; /daemon start tries again"
                ),
            );
        } else {
            tracing::warn!(%reason, retries = self.retries, "attach failed, retrying");
        }
        self.show(LinkState::Failed {
            reason,
            log,
            retry_at: retry_at.map(Instant::into_std),
        });
    }

    /// Start connecting in the background: the first time with
    /// start-on-demand, afterwards as a reconnect with backoff.
    pub(super) fn connect(&mut self) {
        let files = self.config.endpoint_files.clone();
        // The agent's saved cursor is read once it is known (at attach).
        let saved = self
            .mark
            .as_ref()
            .map(|mark| mark.cursor(self.daemon_id_seen.as_deref()));
        let ui_tx = self.ui_tx.clone();
        let start_token = self.config.start_token.clone();
        let attach = Attach {
            agent: match &self.agent {
                Some(name) => AgentChoice::Agent { name: name.clone() },
                None => self.config.agent.clone(),
            },
            cursor: (!self.agent_switch).then(|| self.config.cursor.clone()),
            start_token: start_token.clone(),
        };
        let task = if let Some(endpoint) = self.config.address.clone() {
            // Reached by address: the same address again, with backoff.
            let first = std::mem::take(&mut self.first_connect);
            tokio::spawn(async move {
                let mut wait = BACKOFF_FIRST;
                loop {
                    match establish(endpoint.clone(), saved.as_ref(), &attach).await {
                        Ok(established) => return Ok(established),
                        Err(err) if first || gone(&err) => return Err(err),
                        Err(err) => tracing::debug!(error = %err, "reconnect failed"),
                    }
                    tokio::time::sleep(wait).await;
                    wait = (wait * 2).min(BACKOFF_MAX);
                }
            })
        } else if std::mem::take(&mut self.first_connect) {
            let spawn = self.config.spawn.clone();
            tokio::spawn(async move {
                let endpoint =
                    first_endpoint(&files, spawn.as_ref(), start_token.as_deref(), &ui_tx).await?;
                establish(endpoint, saved.as_ref(), &attach).await
            })
        } else {
            let spawn = self.config.spawn.clone();
            tokio::spawn(async move {
                let mut wait = BACKOFF_FIRST;
                let mut restarted: Option<Instant> = None;
                loop {
                    match launch::probe_any(&files).await {
                        launch::Liveness::Live(endpoint) => {
                            match establish(endpoint, saved.as_ref(), &attach).await {
                                Ok(established) => return Ok(established),
                                Err(err) if gone(&err) => return Err(err),
                                Err(err) => tracing::debug!(error = %err, "reconnect failed"),
                            }
                        }
                        // The daemon died and left its endpoint file:
                        // start it again (auto mode), at most every
                        // RESTART_EVERY. A daemon that shut down removed
                        // its file and is waited for.
                        launch::Liveness::Stale(_)
                            if restarted.is_none_or(|at| at.elapsed() >= RESTART_EVERY) =>
                        {
                            if let Some(plan) = &spawn {
                                restarted = Some(Instant::now());
                                match restart(&files, plan, &ui_tx).await {
                                    Ok(endpoint) => {
                                        match establish(endpoint, saved.as_ref(), &attach).await {
                                            Ok(established) => return Ok(established),
                                            Err(err) => {
                                                tracing::debug!(error = %err, "reattach failed");
                                            }
                                        }
                                    }
                                    Err(err) => {
                                        let _ = ui_tx.send(Msg::Notice {
                                            level: NoticeLevel::Error,
                                            text: format!(
                                                "the daemon could not be restarted: {err}"
                                            ),
                                        });
                                    }
                                }
                            }
                        }
                        launch::Liveness::Stale(_) | launch::Liveness::Absent => {}
                    }
                    tokio::time::sleep(wait).await;
                    wait = (wait * 2).min(BACKOFF_MAX);
                }
            })
        };
        self.connecting = Some(task);
    }

    pub(super) fn connected(
        &mut self,
        joined: Result<Result<Established, ClientError>, tokio::task::JoinError>,
    ) {
        let established = match joined {
            Ok(Ok(established)) => established,
            Ok(Err(err)) => {
                let (reason, log) = failure(&err);
                self.connect_failed(reason, log, !gone(&err));
                return;
            }
            Err(err) => {
                tracing::warn!(error = %err, "the connect task failed");
                self.connect_failed(format!("the connect task failed: {err}"), None, true);
                return;
            }
        };
        self.retries = 0;
        let Established {
            link,
            mark,
            reset,
            identity,
            usage,
            queued,
            context,
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
        if let Some((model, provider, endpoint)) = identity {
            self.followers.set_default_model(&model);
            self.show_model(
                endpoint.as_deref().unwrap_or(&provider),
                WireDialect::parse(&provider),
                &model,
            );
        }
        if fresh {
            self.load_cost(&link.session);
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
        self.link = Some(link);
        self.link_epoch += 1;
        if let Some(blocks) = reset {
            self.send(Msg::ResetTranscript(blocks));
        }
        // The gauge, as a switch seeds it: the replay since the cursor may
        // hold no turn end although the session has a context.
        if context.is_some() {
            self.send(Msg::Status(StatusUpdate::Context {
                used: context,
                approx: false,
            }));
        }
        // What the live stream delivered before the usage answer: its turns
        // are in the totals.
        for note in queued {
            self.on_notification(&note);
        }
        // The counters are the session's totals; after the
        // replay and the live events queued before the usage read, whose
        // turns they already hold. Live turn ends after this add to them.
        if let Some((input, output)) = usage {
            self.send(Msg::Status(StatusUpdate::Tokens { input, output }));
        }
        let now = Instant::now();
        self.discover_at = Some(now);
        self.session_check_at = Some(now + SESSION_POLL);
        self.refresh_mode();
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
        let Some(connection) = self.connection() else {
            return;
        };
        let ui_tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let read = bounded(
                CALL_TIMEOUT,
                remote::call::<protocol::method::Status>(&connection, &protocol::Empty {}),
            )
            .await;
            if let Ok(status) = read
                && let Some(text) = others_notice(&status.agents, connection.agent())
            {
                let _ = ui_tx.send(Msg::Notice {
                    level: NoticeLevel::Info,
                    text,
                });
            }
        });
    }

    /// Show what this client's agent was denied in turns no user started
    /// while no client was attached (`rkat/missed`).
    pub(super) fn missed_notices(&self) {
        let Some(connection) = self.connection() else {
            return;
        };
        let ui_tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let read = bounded(
                CALL_TIMEOUT,
                remote::call::<protocol::method::Missed>(&connection, &protocol::Empty {}),
            )
            .await;
            match read {
                Ok(read) => {
                    for notice in read.notices {
                        let _ = ui_tx.send(Msg::Notice {
                            level: NoticeLevel::Warn,
                            text: notice.text,
                        });
                    }
                }
                Err(err) => {
                    tracing::debug!(error = %err, "cannot read what the agent was denied unattended");
                }
            }
        });
    }

    /// Show the daemon's start-up notices (`rkat/notices`).
    pub(super) fn startup_notices(&self) {
        let Some(connection) = self.connection() else {
            return;
        };
        let ui_tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let read = bounded(
                CALL_TIMEOUT,
                remote::call::<protocol::method::Notices>(&connection, &protocol::Empty {}),
            )
            .await;
            match read {
                Ok(read) => {
                    for notice in read.notices {
                        let level = if notice.warn {
                            NoticeLevel::Warn
                        } else {
                            NoticeLevel::Info
                        };
                        let _ = ui_tx.send(Msg::Notice {
                            level,
                            text: notice.text,
                        });
                    }
                }
                Err(err) => {
                    tracing::debug!(error = %err, "cannot read the daemon's start-up notices");
                }
            }
        });
    }

    /// Read the daemon's permission mode into the status bar.
    pub(super) fn refresh_mode(&self) {
        let Some(connection) = self.connection() else {
            return;
        };
        let reports = self.reporter();
        tokio::spawn(async move {
            let read = bounded(
                CALL_TIMEOUT,
                remote::call::<protocol::method::GetMode>(&connection, &protocol::Empty {}),
            )
            .await;
            if let Ok(status) = read {
                reports.send(Report::Mode(status));
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
        self.link = None;
        self.save_cursor();
        self.followers.streams_lost();
        self.orphans.clear();
        self.discover_at = None;
        self.session_check_at = None;
        self.switching = false;
        // What is still pending shows again after the reattach.
        for id in self.approvals.clear() {
            self.send(Msg::EscalationResolved(id));
        }
        if self.rehoming {
            // The re-home stopped this daemon; it reattaches when done.
            return;
        }
        if self.stopping {
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
        self.connect();
    }

    /// Drop the link and everything shown from it (a re-home or an agent
    /// switch makes a new one).
    pub(super) fn unlink(&mut self) {
        self.save_cursor();
        self.link = None;
        self.followers.streams_lost();
        self.orphans.clear();
        self.discover_at = None;
        self.session_check_at = None;
        for id in self.approvals.clear() {
            self.send(Msg::EscalationResolved(id));
        }
    }

    pub(super) fn on_notification(&mut self, note: &Notification) {
        let Some(link) = &self.link else {
            return;
        };
        if note.method == protocol::DECISION_NOTICE {
            let session = link.session.clone();
            if let Some(change) = self.approvals.notice(&session, &note.params) {
                for msg in change.messages() {
                    self.send(msg);
                }
            }
            return;
        }
        if note.method == protocol::APPROVAL_NOTE {
            match serde_json::from_value::<protocol::ApprovalNote>(note.params.clone()) {
                // Another agent's call (its helpers' are its session's).
                Ok(protocol::ApprovalNote::Decision { session_id, .. })
                    if session_id != link.session => {}
                Ok(approval) => {
                    let (msgs, mode) = super::super::approvals::note_messages(approval);
                    for msg in msgs {
                        self.send(msg);
                    }
                    if mode {
                        self.refresh_mode();
                    }
                }
                Err(err) => tracing::debug!(error = %err, "unreadable approval note"),
            }
            return;
        }
        if note.method == protocol::FILE_DIFF {
            match serde_json::from_value::<protocol::FileDiffNote>(note.params.clone()) {
                // Another agent's change.
                Ok(diff) if diff.agent != link.connection.agent() => {}
                Ok(diff) => self.send(Msg::Diff {
                    tool_use_id: diff.tool_use_id,
                    path: diff.path,
                    unified: diff.unified,
                    added: diff.added,
                    removed: diff.removed,
                }),
                Err(err) => tracing::debug!(error = %err, "unreadable file diff"),
            }
            return;
        }
        if note.method == protocol::TRIGGERED {
            match serde_json::from_value::<protocol::TriggeredNote>(note.params.clone()) {
                Ok(fired)
                    if fired.agent == link.connection.agent()
                        && fired.session_id == link.session =>
                {
                    self.send(Msg::Triggered {
                        label: fired.label,
                        prompt: fired.prompt,
                    });
                }
                Ok(_) => {}
                Err(err) => tracing::debug!(error = %err, "unreadable trigger note"),
            }
            return;
        }
        if note.method == protocol::DECISION_ENDED {
            if let Some(change) = self.approvals.decision_ended(&note.params) {
                for msg in change.messages() {
                    self.send(msg);
                }
            }
            return;
        }
        if let Some((envelope, epoch)) = remote::stream_event(note, &link.stream) {
            self.on_event(envelope, epoch.as_deref());
            return;
        }
        if let Some(stream) = remote::stream_end(note) {
            // The conversation's own stream: the daemon seated the agent
            // again on it (a model switch rebuilt its agent).
            if stream == link.stream && !self.switching {
                let session = link.session.clone();
                self.rerender(session, Again::Reseated);
                return;
            }
            self.followers.ended(stream);
            return;
        }
        let Some((stream, attributed)) = remote::attributed_event(note) else {
            return;
        };
        if helpers::starts_helpers(&attributed.envelope.payload) {
            self.start_discovery();
        }
        if let Some(by) = self.followers.helper_of(&stream, &attributed) {
            self.followers
                .convening(&attributed.envelope.payload, Some(&by));
        }
        if self.followers.owns(&stream) {
            let actions = self.followers.event(&stream, attributed);
            self.follow(actions);
        } else {
            // A stream whose open has not answered yet.
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

    /// A tool that seats helpers started: list the mobs for a while, so a
    /// new one (an implicit `delegate` mob) is followed.
    pub(super) fn start_discovery(&mut self) {
        let now = Instant::now();
        self.discover_until = Some(now + DISCOVER_FOR);
        if self.discover_at.is_none() {
            self.discover_at = Some(now);
        }
    }

    /// List the daemon's mobs and follow the new ones.
    pub(super) fn discover(&mut self) {
        self.discover_at = None;
        let Some(connection) = self.connection() else {
            return;
        };
        if self.discovering {
            return;
        }
        self.discovering = true;
        let reports = self.reporter();
        tokio::spawn(async move {
            let mobs = bounded(CALL_TIMEOUT, remote::mobs(&connection))
                .await
                .unwrap_or_default();
            reports.send(Report::Mobs(mobs));
        });
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

    /// Read the orchestrator's current session (another client may have
    /// switched it).
    pub(super) fn check_session(&mut self) {
        self.session_check_at = None;
        let Some(connection) = self.connection() else {
            return;
        };
        if self.checking_session || self.switching {
            self.session_check_at = Some(Instant::now() + SESSION_POLL);
            return;
        }
        self.checking_session = true;
        let reports = self.reporter();
        tokio::spawn(async move {
            reports.send(
                match bounded(CALL_TIMEOUT, remote::session_id(&connection)).await {
                    Ok(session) => Report::SessionNow(session),
                    Err(_) => Report::SessionUnread,
                },
            );
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
        let session = remote::session_id(&connection).await?;
        let id = session.to_string();
        let resume = saved.and_then(|cursor| {
            cursor.resumable(&id, &super::super::catchup::daemon_id(&endpoint.token))
        });
        let (stream, mark, reset, context) = match resume {
            Some((epoch, seq)) => {
                let stream = remote::open_stream(&connection, Some((&epoch, seq))).await?;
                let mark = Mark::resume(&id, saved);
                // The gauge's seed: the stream replays only what came after
                // the mark.
                let context = last_context_before(&connection, &session, seq).await;
                (stream, mark, None, context)
            }
            None => {
                // Live first, so nothing falls between the stream and the
                // render; the mark drops what both deliver. The snapshot's
                // cursor is where the render ends.
                let stream = remote::open_stream(&connection, None).await?;
                let latest = remote::snapshot(&connection, &session).await?.latest;
                let messages = remote::transcript(&connection, &session).await?;
                let context = last_context_before(&connection, &session, latest).await;
                (
                    stream,
                    Mark::rendered(&id, latest),
                    Some(crate::ui::transcript::blocks_from_messages(&messages)),
                    context,
                )
            }
        };
        let identity = match remote::identity(&connection, &session).await {
            Ok((model, provider)) => {
                let endpoint = remote::session_endpoint(&connection, &session)
                    .await
                    .ok()
                    .flatten();
                Some((model, provider, endpoint))
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
            context,
        })
    })
    .await
    .map_err(|_| ClientError::Timeout("reading the agent's state"))?
}

/// What a connect to a daemon's address says about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// Something accepted.
    Accepts,
    /// Refused outright (nothing listens): evidence the daemon stopped.
    Refused,
    /// Timed out or failed otherwise: no evidence either way.
    Unknown,
}

/// Probe the loopback `address` now (a connect there is refused at once
/// when nothing listens), within a second, without blocking the loop.
pub(super) async fn probe_address(address: std::net::SocketAddr) -> Probe {
    match tokio::time::timeout(
        Duration::from_secs(1),
        tokio::net::TcpStream::connect(address),
    )
    .await
    {
        Ok(Ok(_)) => Probe::Accepts,
        Ok(Err(err)) => classify(&err),
        Err(_) => Probe::Unknown,
    }
}

/// A failed connect: a refusal (or a reset, which a loopback connect to a
/// port nobody listens on can report) is evidence of a stop; anything
/// else (a timeout, a local socket error) is not.
#[must_use]
pub fn classify(err: &std::io::Error) -> Probe {
    match err.kind() {
        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::ConnectionReset => {
            Probe::Refused
        }
        _ => Probe::Unknown,
    }
}

pub(super) async fn next_notification(link: &mut Option<Link>) -> Option<Notification> {
    match link {
        Some(link) => link.notifications.recv().await,
        None => std::future::pending().await,
    }
}
