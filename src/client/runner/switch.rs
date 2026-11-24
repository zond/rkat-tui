//! Moving the client: another conversation (`/new`, `/resume`, another
//! client's switch), another agent (`/agents <name>`) and another
//! workspace (`/rehome`).

use super::commands::daemon_status;
use super::depart::daemon_ended;
use super::*;

/// The switches under way: of the agent's conversation (with what the
/// daemon pushed meanwhile), of the agent itself, and the reopen of the
/// agent's stream after its reload, which runs as a conversation switch.
#[derive(Default)]
pub(super) struct Switches {
    /// A conversation switch (this client's, or adopting another's) runs.
    pub(super) running: bool,
    /// The conversation the daemon last said the agent moved to
    /// ([`protocol::SWITCHED`]) while a switch of this client's ran: shown
    /// once that switch ends, unless it ended there.
    pub(super) pushed: Option<SessionId>,
    /// An agent switch on its way.
    pub(super) agent: AgentSwitch,
    /// The daemon said it reloaded the agent while a reopen of its stream
    /// was under way (the engine may end the old stream first, which
    /// reads as [`Shown::Reseated`]): that reopen is the reload's.
    pub(super) reloaded: bool,
    /// An input to send once more once the agent's stream is open again
    /// after its reload ([`Shown::Reloaded`]): its turn, steer and text.
    pub(super) again: Option<(u64, Option<PendingId>, String)>,
}

/// An agent switch on its way: the next link renders the agent's
/// conversation from its start.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum AgentSwitch {
    /// None runs.
    #[default]
    None,
    /// `/agents <name>`: to the agent [`Runner::agent`] names.
    Named,
    /// The agent picker's "new agent" row: connects ask for a new agent
    /// until one is bound.
    New,
}

impl AgentSwitch {
    /// A switch to the agent [`Runner::agent`] names; a new agent asked
    /// for and not bound yet stays asked for, as the connect asks.
    fn name_one(&mut self) {
        if *self == Self::None {
            *self = Self::Named;
        }
    }
}

impl Runner {
    /// `/new` (`target: None`) or `/resume <session>`: ask the daemon to
    /// switch the agent's conversation, then show the new one.
    pub(super) fn switch_conversation(&mut self, target: Option<SessionId>) {
        if !self.may_switch() {
            return;
        }
        let Some(link) = self.conn.link() else {
            return;
        };
        self.switches.running = true;
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
        if self.turn.is_open() {
            self.notice(
                NoticeLevel::Warn,
                "a turn is running; interrupt it first (Esc)",
            );
            return false;
        }
        if self.switches.running {
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

    /// Show the conversation `session` from its start (`why` says why):
    /// the agent moved to it, or the stream must be read again; the
    /// stream reopened live, the committed transcript.
    pub(super) fn adopt_switch(&mut self, session: SessionId, why: Shown, warning: Option<String>) {
        let Some(link) = self.conn.link() else {
            return;
        };
        self.switches.running = true;
        let connection = Arc::clone(&link.connection);
        let stream = link.stream.clone();
        let reports = self.reporter();
        tokio::spawn(async move {
            let result = adopt(&connection, &stream, session).await;
            reports.send(Report::Switched {
                result,
                why,
                warning,
            });
        });
    }

    /// Show the current conversation `session` from its start again.
    pub(super) fn rerender(&mut self, session: SessionId, why: Shown) {
        self.adopt_switch(session, why, None);
    }

    /// The agent runs as `adopted.session` now: follow it, and show
    /// its transcript, model and cost.
    pub(super) fn adopted(&mut self, adopted: Adopted, why: Shown, warning: Option<String>) {
        let Adopted {
            session,
            stream,
            blocks,
            last_seq,
            identity,
            usage,
        } = adopted;
        let why = if std::mem::take(&mut self.switches.reloaded) && why == Shown::Reseated {
            Shown::Reloaded
        } else {
            why
        };
        let Some(link) = self.conn.link_mut() else {
            return;
        };
        link.session = session.clone();
        link.stream = stream;
        // The old conversation's turn and dialogs are over (the same
        // conversation, seated again, keeps them).
        if !matches!(why, Shown::Reseated | Shown::Reloaded) {
            if self.turn.is_open() {
                self.turn.record(Err(TurnFailure::Interrupted));
                self.finish_turn();
            }
            for id in self.approvals.clear() {
                self.send(Msg::EscalationResolved(id));
            }
        }
        let id = session.to_string();
        self.seen.mark = Some(Mark::rendered(&id, last_seq));
        self.save_cursor();
        self.helpers.followers.reset(session);
        self.helpers.orphans.clear();
        self.send(Msg::Status(StatusUpdate::Session(id.clone())));
        self.send(Msg::ResetTranscript(blocks));
        // The conversation's own totals and gauge, read from the daemon.
        let usage = usage.unwrap_or_default();
        self.send(Msg::Status(StatusUpdate::Tokens {
            input: usage.input_tokens,
            output: usage.output_tokens,
        }));
        self.send(Msg::Status(StatusUpdate::Context {
            used: usage.context.map(|context| context.tokens),
            approx: usage.context.is_some_and(|context| context.approx),
        }));
        self.send(Msg::Status(StatusUpdate::CostTotals(usage.cost)));
        self.refresh_tasks();
        if let Some(identity) = identity {
            let model = identity.model.clone();
            self.helpers.followers.set_default_model(&model);
            self.show_model(
                identity.endpoint_name(),
                WireDialect::parse(&identity.provider),
                &model,
            );
            // A fresh conversation starts on the daemon's start-up model:
            // this client's last `/model` carries over.
            if why == Shown::New
                && let Some(switch) = self.models.last_switch.clone()
                && switch.model != model
            {
                self.switch_model(switch);
            }
        }
        let short = crate::text::short_id(&id).to_string();
        let text = match why {
            Shown::LostEvents => format!(
                "the stream lost events the daemon no longer keeps; conversation {short} is shown from its start"
            ),
            Shown::Reseated => format!(
                "the daemon rebuilt the agent on conversation {short} (a model it had no profile for); it is shown from its start"
            ),
            Shown::New => {
                format!("a new conversation ({short}); the previous one stays in /resume")
            }
            Shown::Resumed => format!("resumed conversation {short}"),
            Shown::Reloaded => crate::daemon::storage::RELOADED_LINE.to_string(),
            Shown::Others => {
                format!("another client switched the agent to conversation {short}")
            }
        };
        self.notice(NoticeLevel::Info, text);
        if let Some(warning) = warning {
            self.notice(NoticeLevel::Warn, warning);
        }
        self.list_mobs();
        // This client moved the agent: the agents of other workspaces it
        // is wired to learn its new conversation's key.
        if matches!(why, Shown::New | Shown::Resumed) {
            self.refresh_wires();
        }
    }

    /// `/resume <reference>`: resolve it against the conversations, then
    /// switch.
    pub(super) fn resume_session(&mut self, reference: String) {
        if !self.may_switch() {
            return;
        }
        let Some(link) = self.conn.link() else {
            return;
        };
        self.switches.running = true;
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
        if self.turn.is_open() || self.switches.running {
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

    /// The agent picker's "new agent" row: this client attaches again,
    /// bound to an agent seated for it, as a plain `rkat-tui` start is; the
    /// agent left keeps running (detached when this was its last client).
    pub(super) fn new_agent(&mut self) {
        if self.turn.is_open() || self.switches.running {
            self.notice(
                NoticeLevel::Warn,
                "a turn is running or a switch is under way; start a new agent once it ends (or interrupt it)",
            );
            return;
        }
        if self.link_or_say("no new agent was started").is_none() {
            return;
        }
        self.unlink();
        self.seen.mark = None;
        self.switches.agent = AgentSwitch::New;
        // A prompt sent meanwhile goes to the new agent once it is bound.
        self.held.get_or_insert_with(Vec::new);
        self.show(LinkState::Reconnecting);
        self.connect(false, 0);
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
        if self.turn.is_open()
            || self.switches.running
            || self.agent.as_deref() == Some(name.as_str())
        {
            // Something started meanwhile; the user asks again.
            return;
        }
        self.unlink();
        self.seen.mark = None;
        self.agent = Some(name);
        self.switches.agent.name_one();
        // A prompt sent meanwhile goes to this agent once it is bound.
        self.held.get_or_insert_with(Vec::new);
        self.show(LinkState::Reconnecting);
        self.connect(false, 0);
    }

    /// Why this client cannot re-home at all (attached to another
    /// machine's daemon, or not able to re-root itself).
    fn rehome_impossible(&self) -> Option<&'static str> {
        if self.config.address.is_some() {
            Some("/rehome works on this machine's workspace only; run it there")
        } else if self.config.reroot.is_none() {
            Some("this client cannot be re-rooted; use rkat-tui --rehome from the shell")
        } else {
            None
        }
    }

    /// The `/rehome` picker over the realms under this workspace's config
    /// directory ([`super::super::rehome::picker`]).
    fn rehome_picker(&mut self) {
        use crate::bootstrap::realm_home;
        let scope = &self.config.scope;
        let root = realm_home::root(&scope.global);
        match realm_home::scan(&root) {
            Ok(homes) => {
                let picker =
                    super::super::rehome::picker(&homes, &scope.home, scope.project_root.as_path());
                self.send(Msg::ShowPicker {
                    picker,
                    refresh: false,
                });
            }
            Err(err) => self.notice(NoticeLevel::Warn, err.to_string()),
        }
    }

    /// The `/rehome` picker's directory `typed`, read as `/rehome to`
    /// reads it: answered with the question to ask, or the refusal
    /// ([`Msg::RehomeChecked`]). Nothing changes.
    pub(super) fn rehome_check(&mut self, typed: String) {
        use crate::bootstrap::realm_home::{self, Reading, Way};
        let answer = (|| {
            if let Some(why) = self.rehome_impossible() {
                return Err(why.to_string());
            }
            let scope = &self.config.scope;
            let root = realm_home::root(&scope.global);
            let workspace = scope.project_root.as_path();
            let homes = realm_home::scan(&root).map_err(|err| err.to_string())?;
            let at = realm_home::Context::new(&root, workspace, workspace);
            match realm_home::read_reference(&homes, &typed, Way::Push, &at) {
                Ok(Reading::Push(path)) => {
                    let taken = realm_home::find_in(&homes, &path)
                        .map_err(|err| err.to_string())?
                        .cloned();
                    let question =
                        super::super::rehome::move_question(&scope.home, &path, taken.as_ref());
                    Ok((question, path))
                }
                Ok(Reading::Pull(home)) => Err(format!("`{typed}` names realm {}", home.id)),
                Err(err) => Err(err.to_string()),
            }
        })();
        self.send(Msg::RehomeChecked { typed, answer });
    }

    /// The `/rehome` picker's confirmed `d`: delete realm `id` as
    /// `rkat-tui --remove-state <id> --yes` does (never this workspace's
    /// own), then the picker again with the outcome under its rows.
    pub(super) fn remove_state(&mut self, id: String) {
        use crate::bootstrap::realm_home;
        let scope = &self.config.scope;
        let root = realm_home::root(&scope.global);
        let current = scope.home.clone();
        let workspace = scope.project_root.as_path().to_path_buf();
        let files = match &self.config.reroot {
            Some(reroot) => Arc::clone(&reroot.endpoint_files),
            None => super::super::rehome::default_endpoint_files(),
        };
        let say = self.say();
        tokio::spawn(async move {
            use crate::ui::PickerKind;
            let removed = async {
                if id == current.id {
                    return Err(format!(
                        "realm {id} is this workspace's own state: it is not deleted from here"
                    ));
                }
                let homes = realm_home::scan(&root).map_err(|err| err.to_string())?;
                let home = homes
                    .iter()
                    .find(|home| home.id == id)
                    .ok_or_else(|| format!("realm {id} is gone already"))?;
                super::super::rehome::remove_home(&root, home, &files, true, None)
                    .await
                    .map_err(|err| err.to_string())
            }
            .await;
            if let Ok(homes) = realm_home::scan(&root) {
                let _ = say.ui_tx.send(Msg::ShowPicker {
                    picker: super::super::rehome::picker(&homes, &current, &workspace),
                    refresh: true,
                });
            }
            match removed {
                Ok(done) => {
                    say.notice(NoticeLevel::Info, done.to_string());
                    let _ = say.ui_tx.send(Msg::PickerDone {
                        kind: PickerKind::Rehome,
                        text: done.to_string(),
                    });
                }
                Err(why) => {
                    let _ = say.ui_tx.send(Msg::PickerRefused {
                        kind: PickerKind::Rehome,
                        why,
                    });
                }
            }
        });
    }

    /// `/rehome`: without an argument, the picker of this workspace's
    /// state ([`Self::rehome_picker`]); with one, re-home (a pull or a
    /// push, [`realm_home::read_reference`]), then re-root this client on
    /// the workspace it serves now and reattach ([`Runner::reroot`]). A
    /// directory read either way, to which this workspace would move a
    /// state that holds no conversation, is refused
    /// ([`RehomeError::NothingToMove`]): `to` says it is meant. State the
    /// re-home would replace that holds conversations refuses it, and a
    /// dialog asks whether to delete it ([`rehome::discard_picker`]);
    /// `discard` is the realm id the user confirmed deleting there.
    ///
    /// [`realm_home::read_reference`]: crate::bootstrap::realm_home::read_reference
    /// [`RehomeError::NothingToMove`]: super::super::rehome::RehomeError::NothingToMove
    /// [`rehome::discard_picker`]: super::super::rehome::discard_picker
    pub(super) fn rehome(&mut self, argument: Option<String>, discard: Option<String>) {
        use super::super::rehome::{self as rh, RehomeError, Side};
        use crate::bootstrap::realm_home::{self, HomeError, Reading, Way};
        if let Some(why) = self.rehome_impossible() {
            self.notice(NoticeLevel::Warn, why);
            return;
        }
        let Some(argument) = argument else {
            self.rehome_picker();
            return;
        };
        let scope = &self.config.scope;
        let root = realm_home::root(&scope.global);
        let refused = if self.turn.is_open() || self.switches.running {
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
        let (Some(link), Some(reroot)) = (self.conn.link(), self.config.reroot.clone()) else {
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
        let (way, reference) = realm_home::parse_way(&argument);
        let read = realm_home::scan(&root).and_then(|homes| {
            let at = realm_home::Context::new(&root, &workspace, &workspace);
            let reading = realm_home::read_reference(&homes, reference, way, &at)?;
            let current = realm_home::find_in(&homes, &workspace)?.cloned();
            Ok((homes, reading, current))
        });
        // Read either way, a move of a state that holds nothing is
        // refused below: more likely an adoption that matched no realm.
        let either_push = match (&read, way) {
            (Ok((_, Reading::Push(path), _)), Way::Either) => Some(path.clone()),
            _ => None,
        };
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
        // The argument as resolved, for the question if the state it would
        // replace holds something.
        let again = match side {
            Side::Adopt => format!("to {}", destination.display()),
            Side::Replace => format!("from {}", adopt.id),
        };
        let taken = replace.clone();
        let say = self.say();
        let reports = self.reporter();
        if let Conn::Up { leaving, .. } = &mut self.conn {
            *leaving = Some(Leaving::Rehome);
        }
        tokio::spawn(async move {
            let stopped = async {
                let plan = rh::prepare(
                    &root,
                    adopt,
                    destination,
                    replace,
                    &reroot.endpoint_files,
                    Some((&connection, side)),
                    discard.as_deref(),
                )
                .await
                .map_err(|err| match (&err, &taken) {
                    (RehomeError::Home(HomeError::Holds { id, .. }), Some(taken))
                        if *id == taken.id =>
                    {
                        let _ = say.ui_tx.send(Msg::ShowPicker {
                            picker: rh::discard_picker(taken, &again),
                            refresh: false,
                        });
                        format!("{err}; the dialog asks whether to delete it and re-home (y), or keep it (n)")
                    }
                    _ => err.to_string(),
                })?;
                if let Some(path) = either_push {
                    let held = rh::held_by(&connection, &path)
                        .await
                        .map_err(|err| err.to_string())?;
                    if held.is_empty() {
                        return Err(RehomeError::NothingToMove { path }.to_string());
                    }
                }
                say.notice(
                    NoticeLevel::Info,
                    format!("{}: stopping the daemon", plan.describe()),
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
                match &mut self.conn {
                    Conn::Up { leaving, .. } => *leaving = None,
                    Conn::Rehoming => self.conn = Conn::Idle,
                    _ => {}
                }
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
        self.conn = Conn::Rehoming;
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
                                    "{done}; this rkat-tui now works in {}, starting its daemon there",
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
        self.conn = Conn::Idle;
        match rehomed {
            Rehomed::Failed => self.connect(true, 0),
            Rehomed::Stranded => {
                self.show(LinkState::Stopped);
                self.conn = Conn::Stopped;
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
        });
        self.config = config;
        self.agent = None;
        self.seen.mark = None;
        self.seen.daemon_id = None;
        self.show(LinkState::Connecting);
        self.connect(true, 0);
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
        why: if fresh { Shown::New } else { Shown::Resumed },
        warning,
    }
}

/// Follow the agent's conversation `session` on `connection`: its
/// stream opened again (the old one follows the retired session), its last
/// sequence (what is shown starts after it) and its committed transcript.
/// How long a stream's open waits for the agent's session to be live
/// again ([`reopen_settled`]), and how often it asks.
const SETTLE_WAIT: Duration = Duration::from_secs(10);
const SETTLE_EVERY: Duration = Duration::from_millis(250);

/// The engine's refusal of a stream on a member whose session actor is
/// being stood down or brought back (right after an interrupt, a model
/// switch's reseat): it passes once the actor is live again.
fn actor_settling(err: &ClientError) -> bool {
    matches!(err, ClientError::Rpc { message, .. } if message.contains("no live session actor"))
}

/// [`remote::reopen_stream`], asked again while the engine says the
/// agent's session is not live yet, for at most [`SETTLE_WAIT`]: only a
/// refusal that persists is an error, and then in plain words.
async fn reopen_settled(connection: &Connection, old: &str) -> Result<String, String> {
    let give_up = tokio::time::Instant::now() + SETTLE_WAIT;
    let mut old = Some(old);
    loop {
        let opened = match old.take() {
            Some(old) => remote::reopen_stream(connection, old).await,
            None => remote::open_stream(connection, None).await,
        };
        match opened {
            Err(err) if actor_settling(&err) => {
                if tokio::time::Instant::now() >= give_up {
                    tracing::warn!(%err, "the agent's session did not come back");
                    return Err(format!(
                        "the agent's session was not ready again within {}s; /resume shows the conversation once it is",
                        SETTLE_WAIT.as_secs()
                    ));
                }
                tokio::time::sleep(SETTLE_EVERY).await;
            }
            other => return other.map_err(|err| err.to_string()),
        }
    }
}

pub(super) async fn adopt(
    connection: &Connection,
    old_stream: &str,
    session: SessionId,
) -> Result<Adopted, String> {
    tokio::time::timeout(SWITCH_TIMEOUT, async {
        let stream = reopen_settled(connection, old_stream).await?;
        let read = |err: ClientError| err.to_string();
        let snapshot = remote::snapshot(connection, &session).await.map_err(read)?;
        let messages = remote::transcript(connection, &session)
            .await
            .map_err(read)?;
        let usage = remote::usage(connection, &session).await.ok();
        let endpoint = remote::session_endpoint(connection, &session)
            .await
            .ok()
            .flatten();
        Ok::<_, String>(Adopted {
            blocks: crate::ui::transcript::blocks_from_messages(&messages),
            session,
            stream,
            last_seq: snapshot.latest,
            identity: Some(SessionModel {
                model: snapshot.model,
                provider: snapshot.provider,
                endpoint,
            }),
            usage,
        })
    })
    .await
    .map_err(|_| "timed out reading the new conversation".to_string())?
}
