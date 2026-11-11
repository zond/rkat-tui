//! Moving the client: another conversation (`/new`, `/resume`, another
//! client's switch), another agent (`/agents <name>`) and another
//! workspace (`/rehome`).

use super::commands::daemon_status;
use super::depart::daemon_ended;
use super::*;

impl Runner {
    /// `/new` (`target: None`) or `/resume <session>`: ask the daemon to
    /// switch the orchestrator's conversation, then show the new one.
    pub(super) fn switch_conversation(&mut self, target: Option<SessionId>) {
        if !self.may_switch() {
            return;
        }
        let Some(link) = &self.link else {
            return;
        };
        self.switching = true;
        let connection = Arc::clone(&link.connection);
        let stream = link.stream.clone();
        let reports = self.reporter();
        let fresh = target.is_none();
        tokio::spawn(async move {
            reports.send(switch_and_adopt(&connection, &stream, target.as_ref(), fresh).await);
        });
    }

    /// Whether a conversation switch may start now (else says why).
    pub(super) fn may_switch(&self) -> bool {
        if self.turn.active {
            self.notice(
                NoticeLevel::Warn,
                "a turn is running; interrupt it first (Esc)",
            );
            return false;
        }
        if self.switching {
            self.notice(NoticeLevel::Info, "a conversation switch is under way");
            return false;
        }
        if self
            .link_or_say("the conversation was not switched")
            .is_none()
        {
            return false;
        }
        true
    }

    /// Show the conversation the orchestrator moved to (another client
    /// switched it).
    pub(super) fn adopt_switch(
        &mut self,
        session: SessionId,
        own: Option<bool>,
        warning: Option<String>,
    ) {
        let Some(link) = &self.link else {
            return;
        };
        self.switching = true;
        let connection = Arc::clone(&link.connection);
        let stream = link.stream.clone();
        let reports = self.reporter();
        tokio::spawn(async move {
            let result = adopt(&connection, &stream, session).await;
            reports.send(Report::Switched {
                result,
                own,
                warning,
            });
        });
    }

    /// Show the conversation `session` from its start again (`again`
    /// says why): the stream reopened live, the committed transcript.
    pub(super) fn rerender(&mut self, session: SessionId, again: Again) {
        let Some(link) = &self.link else {
            return;
        };
        self.switching = true;
        let connection = Arc::clone(&link.connection);
        let stream = link.stream.clone();
        let reports = self.reporter();
        tokio::spawn(async move {
            let result = adopt(&connection, &stream, session).await;
            reports.send(Report::Rerendered(result, again));
        });
    }

    /// The orchestrator runs as `adopted.session` now: follow it, and show
    /// its transcript, model and cost.
    pub(super) fn adopted(
        &mut self,
        adopted: Adopted,
        own: Option<bool>,
        warning: Option<String>,
        again: Option<Again>,
    ) {
        let Adopted {
            session,
            stream,
            blocks,
            last_seq,
            identity,
            usage,
            context,
        } = adopted;
        let Some(link) = &mut self.link else {
            return;
        };
        link.session = session.clone();
        link.stream = stream;
        // The old conversation's turn and dialogs are over (the same
        // conversation, seated again, keeps them).
        if again != Some(Again::Reseated) {
            if self.turn.active {
                self.turn.record(Err(TurnFailure::Interrupted));
                self.finish_turn();
            }
            for id in self.approvals.clear() {
                self.send(Msg::EscalationResolved(id));
            }
        }
        let id = session.to_string();
        self.mark = Some(Mark::rendered(&id, last_seq));
        self.save_cursor();
        self.followers.reset(session.clone());
        self.orphans.clear();
        self.send(Msg::Status(StatusUpdate::Session(id.clone())));
        self.send(Msg::ResetTranscript(blocks));
        // The conversation's own totals and gauge, read from the daemon.
        let (input, output) = usage.unwrap_or_default();
        self.send(Msg::Status(StatusUpdate::Tokens { input, output }));
        self.send(Msg::Status(StatusUpdate::Context {
            used: context,
            approx: false,
        }));
        self.load_cost(&session);
        self.refresh_tasks();
        if let Some((model, provider, endpoint)) = identity {
            self.followers.set_default_model(&model);
            self.show_model(
                endpoint.as_deref().unwrap_or(&provider),
                WireDialect::parse(&provider),
                &model,
            );
            // A fresh conversation starts on the daemon's start-up model:
            // this client's last `/model` carries over.
            if own == Some(true)
                && let Some(switch) = self.last_switch.clone()
                && switch.model != model
            {
                self.switch_model(switch);
            }
        }
        let short = id.get(..8).unwrap_or(&id).to_string();
        let text = match own {
            _ if again == Some(Again::LostEvents) => format!(
                "the stream lost events the daemon no longer keeps; conversation {short} is shown from its start"
            ),
            _ if again == Some(Again::Reseated) => format!(
                "the daemon rebuilt the agent on conversation {short} (a model it had no profile for); it is shown from its start"
            ),
            Some(true) => {
                format!("a new conversation ({short}); the previous one stays in /resume")
            }
            Some(false) => format!("resumed conversation {short}"),
            None => format!("another client switched the agent to conversation {short}"),
        };
        self.notice(NoticeLevel::Info, text);
        if let Some(warning) = warning {
            self.notice(NoticeLevel::Warn, warning);
        }
        self.discover_at = Some(Instant::now());
    }

    /// `/resume <reference>`: resolve it against the conversations, then
    /// switch.
    pub(super) fn resume_session(&mut self, reference: String) {
        if !self.may_switch() {
            return;
        }
        let Some(link) = &self.link else {
            return;
        };
        self.switching = true;
        let connection = Arc::clone(&link.connection);
        let stream = link.stream.clone();
        let current = link.session.clone();
        let reports = self.reporter();
        tokio::spawn(async move {
            let listed = match remote::conversations(&connection).await {
                Ok(listed) => listed,
                Err(err) => {
                    reports.send(Report::SwitchRefused(
                        NoticeLevel::Error,
                        format!("could not list the conversations: {err}"),
                    ));
                    return;
                }
            };
            let target = match sessions::resolve(&reference, &listed, Some(connection.agent())) {
                Ok(target) if target == current => {
                    reports.send(Report::SwitchRefused(
                        NoticeLevel::Info,
                        "already in this conversation".to_string(),
                    ));
                    return;
                }
                Ok(target) => target,
                Err(why) => {
                    reports.send(Report::SwitchRefused(NoticeLevel::Warn, why));
                    return;
                }
            };
            reports.send(switch_and_adopt(&connection, &stream, Some(&target), false).await);
        });
    }

    /// `/resume` alone: the picker over the conversations.
    pub(super) fn show_resume_picker(&self) {
        let Some(connection) = self.link_or_say("the conversations were not listed") else {
            return;
        };
        let reports = self.reporter();
        tokio::spawn(async move {
            let listed = bounded(CALL_TIMEOUT, remote::conversations(&connection))
                .await
                .map_err(|err| err.to_string());
            let mine = connection.agent().to_string();
            reports.send(Report::Picker(
                listed.map(|listed| sessions::picker_items(&listed, Some(&mine))),
            ));
        });
    }

    /// Show `session`'s stored cost totals (none: zero).
    pub(super) fn load_cost(&self, session: &SessionId) {
        let cost = self
            .bench
            .recorded_cost(&session.to_string())
            .unwrap_or_default();
        self.send(Msg::Status(StatusUpdate::CostTotals(cost)));
    }

    /// `/agents <name>`: work with the agent `name` from now on. The agent
    /// left keeps running (detached when this was its last client); this
    /// client attaches again, bound to `name`, and shows its conversation
    /// from the start.
    pub(super) fn switch_agent(&mut self, name: String) {
        if self.agent.as_deref() == Some(name.as_str()) {
            self.notice(
                NoticeLevel::Info,
                format!("already working with agent {name}"),
            );
            return;
        }
        if self.turn.active || self.switching {
            self.notice(
                NoticeLevel::Warn,
                "a turn is running or a switch is under way; switch agents once it ends (or interrupt it)",
            );
            return;
        }
        let Some(connection) = self.link_or_say("the agent was not switched") else {
            return;
        };
        let reports = self.reporter();
        tokio::spawn(async move {
            let status = daemon_status(&connection).await;
            reports.send(Report::AgentsRead { name, status });
        });
    }

    /// The agents as read for `/agents <name>`: switch to `name` when it
    /// works here and nothing runs now.
    pub(super) fn agents_read(&mut self, name: String, status: Option<protocol::Status>) {
        let Some(status) = status else {
            self.notice(
                NoticeLevel::Warn,
                "the daemon did not list its agents; the agent was not switched",
            );
            return;
        };
        if !status.agents.iter().any(|row| row.name == name) {
            let names: Vec<&str> = status.agents.iter().map(|row| row.name.as_str()).collect();
            self.notice(
                NoticeLevel::Warn,
                format!("no agent {name} works here (agents: {})", names.join(", ")),
            );
            return;
        }
        if self.turn.active || self.switching || self.agent.as_deref() == Some(name.as_str()) {
            // Something started meanwhile; the user asks again.
            return;
        }
        self.unlink();
        self.mark = None;
        self.agent = Some(name);
        self.agent_switch = true;
        self.show(LinkState::Reconnecting);
        self.connect();
    }

    /// `/rehome`: without an argument, what this workspace could adopt;
    /// with one, re-home (a pull or a push, [`realm_home::read_reference`]),
    /// then re-root this client on the workspace it serves now and
    /// reattach ([`Runner::reroot`]).
    ///
    /// [`realm_home::read_reference`]: crate::bootstrap::realm_home::read_reference
    pub(super) fn rehome(&mut self, argument: Option<String>) {
        use super::super::rehome::{self as rh, Side};
        use crate::bootstrap::realm_home::{self, Reading};
        let scope = &self.config.scope;
        let Some(global) = &scope.global else {
            self.notice(
                NoticeLevel::Warn,
                realm_home::HomeError::NoConfigDir.to_string(),
            );
            return;
        };
        let root = realm_home::root(global);
        let Some(argument) = argument else {
            match realm_home::adoptable(&root, scope.home.as_ref()) {
                Ok(text) => self.notice(NoticeLevel::Info, text),
                Err(err) => self.notice(NoticeLevel::Warn, err.to_string()),
            }
            return;
        };
        let refused = if scope.home.is_none() {
            Some(
                "this client runs on a shared realm (--realm, --isolated, --state-root): /rehome moves a workspace's own state only",
            )
        } else if self.config.address.is_some() {
            Some("/rehome works on this machine's workspace only; run it there")
        } else if self.config.reroot.is_none() {
            Some("this client cannot be re-rooted; use rkat-tui --rehome from the shell")
        } else if self.turn.active || self.switching {
            Some(
                "a turn is running or a switch is under way; re-home once it ends (or interrupt it)",
            )
        } else {
            None
        };
        if let Some(why) = refused {
            self.notice(NoticeLevel::Warn, why);
            return;
        }
        let (Some(link), Some(reroot)) = (&self.link, self.config.reroot.clone()) else {
            self.notice(
                NoticeLevel::Warn,
                format!("{}: /rehome needs it", self.unattached()),
            );
            return;
        };
        let connection = Arc::clone(&link.connection);
        let endpoint = link.endpoint.clone();
        let session = link.session.clone();
        let workspace = scope.project_root.as_path().to_path_buf();
        let read = realm_home::scan(&root).and_then(|homes| {
            let (way, reference) = realm_home::parse_way(&argument);
            let reading =
                realm_home::read_reference(&homes, reference, way, &workspace, &workspace)?;
            let current = realm_home::find_in(&homes, &workspace)?.cloned();
            Ok((homes, reading, current))
        });
        let (homes, reading, current) = match read {
            Ok(read) => read,
            Err(err) => {
                self.notice(NoticeLevel::Warn, err.to_string());
                return;
            }
        };
        let Some(current) = current else {
            self.notice(
                NoticeLevel::Warn,
                format!("no realm serves {} any more", workspace.display()),
            );
            return;
        };
        let (adopt, destination, replace, side, agent) = match reading {
            Reading::Push(path) => {
                let replace = match realm_home::find_in(&homes, &path) {
                    Ok(found) => found.cloned(),
                    Err(err) => {
                        self.notice(NoticeLevel::Warn, err.to_string());
                        return;
                    }
                };
                (
                    current,
                    path,
                    replace,
                    Side::Adopt,
                    AgentChoice::Conversation {
                        reference: session.to_string(),
                    },
                )
            }
            Reading::Pull(home) => (
                home,
                workspace,
                Some(current),
                Side::Replace,
                AgentChoice::Recent,
            ),
        };
        let say = self.say();
        let reports = self.reporter();
        self.rehoming = true;
        tokio::spawn(async move {
            let stopped = async {
                let plan = rh::prepare(
                    &root,
                    adopt,
                    destination,
                    replace,
                    &reroot.endpoint_files,
                    Some((&connection, side)),
                )
                .await
                .map_err(|err| err.to_string())?;
                say.notice(
                    NoticeLevel::Info,
                    format!(
                        "re-homing realm {} to {}: stopping the daemon",
                        plan.adopt().id,
                        plan.workspace().display()
                    ),
                );
                rh::stop(&connection).await.map_err(|why| {
                    format!("the re-home did not start: the daemon was not stopped ({why})")
                })?;
                Ok(Box::new(RehomeStopped {
                    plan,
                    endpoint,
                    reroot,
                    agent,
                }))
            }
            .await;
            reports.send(Report::RehomeStopped(stopped));
        });
    }

    /// The re-home's daemon stopped (or the re-home did not start): carry
    /// it out, then work on the workspace it serves now
    /// ([`Report::Rehomed`]).
    pub(super) fn rehome_stopped(&mut self, stopped: Result<Box<RehomeStopped>, String>) {
        let stopped = match stopped {
            Ok(stopped) => stopped,
            Err(why) => {
                self.rehoming = false;
                self.notice(NoticeLevel::Warn, why);
                return;
            }
        };
        let RehomeStopped {
            plan,
            endpoint,
            reroot,
            agent,
        } = *stopped;
        // This client's daemon stopped; the link is dropped here, so its
        // end is not taken for a lost connection.
        self.unlink();
        self.show(LinkState::Starting);
        let files = self.config.endpoint_files.clone();
        let say = self.say();
        let reports = self.reporter();
        tokio::spawn(async move {
            use super::super::rehome as rh;
            daemon_ended(&files, &endpoint, true, rh::STOP_WAIT).await;
            let rehomed = match rh::carry_out(plan, rh::STOP_WAIT).await {
                // Nothing moved: this workspace's daemon starts again.
                Err(err) => {
                    say.notice(
                        NoticeLevel::Error,
                        format!("the re-home failed: {err}; restarting this workspace's daemon"),
                    );
                    Rehomed::Failed
                }
                Ok(done) => {
                    let mut args = reroot.args.clone();
                    args.context_root = Some(done.home.manifest.path.clone());
                    match (reroot.configure)(args.clone()).await {
                        Ok(config) => {
                            say.notice(
                                NoticeLevel::Info,
                                format!(
                                    "{done}; this rkat-tui works in {} now, starting its daemon there",
                                    done.home.manifest.path.display()
                                ),
                            );
                            Rehomed::Moved {
                                config,
                                reroot: Reroot { args, ..reroot },
                                agent,
                            }
                        }
                        Err(err) => {
                            say.notice(
                                NoticeLevel::Error,
                                format!(
                                    "{done}, but this rkat-tui cannot run there: {err}; start rkat-tui in {}",
                                    done.home.manifest.path.display()
                                ),
                            );
                            Rehomed::Stranded
                        }
                    }
                }
            };
            reports.send(Report::Rehomed(Box::new(rehomed)));
        });
    }

    /// How the re-home ended ([`Self::rehome_stopped`]).
    pub(super) fn rehomed(&mut self, rehomed: Rehomed) {
        self.rehoming = false;
        match rehomed {
            Rehomed::Failed => {
                self.first_connect = true;
                self.connect();
            }
            Rehomed::Stranded => {
                self.show(LinkState::Stopped);
                self.stopping = true;
            }
            Rehomed::Moved {
                config,
                reroot,
                agent,
            } => self.reroot(config, reroot, agent),
        }
    }

    /// Work on `config` from now on (a re-home's workspace), asking for
    /// `agent` on the first attach, which renders its conversation from the
    /// start: the workbench, the UI and the log follow the workspace and
    /// its realm.
    pub(super) fn reroot(&mut self, mut config: ClientConfig, reroot: Reroot, agent: AgentChoice) {
        config.reroot = Some(reroot);
        config.agent = agent;
        config.start = super::super::StartAt::Current;
        let realm = config.scope.realm_root();
        if realm != self.config.scope.realm_root() {
            crate::workbench::app::reroute_logging(&realm);
        }
        self.bench.cancel_shell();
        self.bench.reroot(
            config.scope.clone(),
            config.user_shell.clone(),
            config.endpoints.clone(),
        );
        self.send(Msg::Rerooted {
            workspace: config.scope.project_root.clone(),
            realm,
            shell_confined: super::super::app_setup(&config).shell_confined,
        });
        self.config = config;
        self.agent = None;
        self.mark = None;
        self.daemon_id_seen = None;
        self.stopping = false;
        self.retries = 0;
        self.first_connect = true;
        self.show(LinkState::Connecting);
        self.connect();
    }
}

/// Every durable event of `session` after `after`, and the last sequence.
pub(super) async fn replay_to_end(
    connection: &Connection,
    session: &SessionId,
    after: u64,
) -> Result<(Vec<EventEnvelope<AgentEvent>>, u64), ClientError> {
    let mut events = Vec::new();
    let mut after = after;
    loop {
        let (page, more) = remote::replay(connection, session, after).await?;
        after = page.last().map_or(after, |envelope| envelope.seq);
        let empty = page.is_empty();
        events.extend(page);
        if !more || empty {
            return Ok((events, after));
        }
    }
}

/// The context of the last model call `events` report (the gauge's
/// reading), `None` when no turn reported its usage.
pub(super) fn last_context(events: &[EventEnvelope<AgentEvent>]) -> Option<u64> {
    events
        .iter()
        .rev()
        .find_map(|envelope| match &envelope.payload {
            AgentEvent::TurnCompleted {
                usage: Some(usage), ..
            } => Some(crate::ui::context_tokens(usage)),
            _ => None,
        })
}

/// How many events before the mark the first look for the last turn end
/// reads, and how many it reads at most (the window doubles each time).
pub(super) const CONTEXT_WINDOW: u64 = 256;

pub(super) const CONTEXT_SEARCH_MAX: u64 = 4096;

/// The context of the last model call among `session`'s events up to
/// `before` (the gauge's seed on attach), looking back in doubling windows,
/// [`CONTEXT_SEARCH_MAX`] events at most. `None` when there is none, or it
/// cannot be read.
pub(super) async fn last_context_before(
    connection: &Connection,
    session: &SessionId,
    before: u64,
) -> Option<u64> {
    let mut end = before;
    let mut window = CONTEXT_WINDOW;
    let mut searched = 0;
    while end > 0 && searched < CONTEXT_SEARCH_MAX {
        let start = end.saturating_sub(window);
        let mut after = start;
        let mut events = Vec::new();
        loop {
            let (page, more) = remote::replay(connection, session, after).await.ok()?;
            after = page.last().map_or(after, |envelope| envelope.seq);
            let empty = page.is_empty();
            events.extend(page.into_iter().filter(|envelope| envelope.seq <= end));
            if !more || empty || after >= end {
                break;
            }
        }
        if let Some(context) = last_context(&events) {
            return Some(context);
        }
        searched += end - start;
        end = start;
        window = window.saturating_mul(2);
    }
    None
}

/// This client's `/new` (`target` `None`, `fresh`) or `/resume`: ask the
/// daemon to switch, then read the conversation it switched to.
pub(super) async fn switch_and_adopt(
    connection: &Connection,
    stream: &str,
    target: Option<&SessionId>,
    fresh: bool,
) -> Report {
    let switched = bounded(SWITCH_TIMEOUT, remote::switch(connection, target)).await;
    let (result, warning) = match switched {
        Ok((session, warning)) => (adopt(connection, stream, session).await, warning),
        Err(err) => (Err(err.to_string()), None),
    };
    Report::Switched {
        result,
        own: Some(fresh),
        warning,
    }
}

/// Follow the orchestrator's conversation `session` on `connection`: its
/// stream opened again (the old one follows the retired session), its last
/// sequence (what is shown starts after it) and its committed transcript.
pub(super) async fn adopt(
    connection: &Connection,
    old_stream: &str,
    session: SessionId,
) -> Result<Adopted, String> {
    tokio::time::timeout(SWITCH_TIMEOUT, async {
        let stream = remote::reopen_stream(connection, old_stream).await?;
        let snapshot = remote::snapshot(connection, &session).await?;
        let messages = remote::transcript(connection, &session).await?;
        let usage = remote::usage(connection, &session).await.ok();
        let endpoint = remote::session_endpoint(connection, &session)
            .await
            .ok()
            .flatten();
        let context = last_context_before(connection, &session, snapshot.latest).await;
        Ok::<_, ClientError>(Adopted {
            blocks: crate::ui::transcript::blocks_from_messages(&messages),
            session,
            stream,
            last_seq: snapshot.latest,
            identity: Some((snapshot.model, snapshot.provider, endpoint)),
            usage,
            context,
        })
    })
    .await
    .map_err(|_| "timed out reading the new conversation".to_string())?
    .map_err(|err| err.to_string())
}
