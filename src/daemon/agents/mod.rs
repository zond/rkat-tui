//! The workspace's agents: several
//! agent members of the daemon's mob `tui`, one per agent a user
//! works with.
//!
//! An agent is named `main-<id>` ([`new_name`], a short random id; stable
//! for the member's life, shown in the status bar, `--list`, `/agents` and
//! `rkat/status`; [`is_agent`]).
//!
//! Each connection is bound to one agent when it attaches
//! ([`super::protocol::AgentChoice`]: a new agent, the most recently
//! active one, an agent by name, or the agent owning a conversation) and
//! every member-scoped method it calls acts on that agent: the `rkat/*`
//! methods act on it by construction, and the proxy refuses an engine
//! member action on another agent or on another agent's helper
//! ([`super::local::Local::observe`], [`super::protocol::NOT_YOURS`]). [`Agents`]
//! keeps the live ones: how many clients each has, whether it was left
//! running without one (detached), and when it was last active; it seats
//! new ones, retires them, and follows each one's events for the gate
//! ([`super::approvals::watch`]).
//!
//! A conversation belongs to the agent that ran it: the engine seats a
//! session only under the identity that created it
//! (`ResumeSessionBoundToAnotherMember`), so a conversation whose agent
//! quit is seated again under that agent's name ([`Agents::bind`]).
//!
//! The table of live agents and its seating rules are here; the names
//! ([`names`]), what the agents are told of each other ([`notes`]) and the
//! runs no client started ([`triggers`]) have files of their own.

mod names;
mod notes;
mod triggers;

pub use names::is_agent;
use names::new_name;
pub use notes::Leave;
pub(crate) use notes::append_note;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use meerkat_core::SessionId;
use meerkat_mob::{AgentIdentity, MobId};

use super::conversations::{RunState, SwitchError, Target};
use super::core::Core;
use super::protocol::{AgentChoice, AgentRow};
use super::{AGENT_PROFILE, MOB_ID};

/// Why a connection could not be bound to an agent.
#[derive(Debug, thiserror::Error)]
pub enum BindError {
    /// The named agent is not live.
    #[error("no agent `{0}` works in this workspace (rkat-tui --list shows the agents)")]
    NoSuchAgent(String),
    /// A connection that is no probe named no agent choice.
    #[error("an attach that is no probe names the agent it works with")]
    Unnamed,
    /// Seating the agent failed.
    #[error("cannot seat the agent: {0}")]
    Seat(#[from] SwitchError),
}

/// What a connection was bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bound {
    /// The agent.
    pub name: String,
    /// It was seated for this connection (a new agent, or an agent seated
    /// again on its conversation).
    pub seated: bool,
}

impl Bound {
    /// A live agent, bound as it is.
    fn existing(name: String) -> Self {
        Self {
            name,
            seated: false,
        }
    }
}

/// One live agent.
#[derive(Debug, Default)]
struct Agent {
    /// The connections bound to it.
    clients: usize,
    /// It was left running without a client (`/detach`, a hangup, or the
    /// daemon restored it); stays set while it lives.
    detached: bool,
    /// The daemon seated it at start for the first client, which has not
    /// come yet.
    spare: bool,
    /// When it was last active (a bind, a prompt, a run), in [`Agents`]'
    /// own count.
    active: u64,
    /// Its event watcher for the gate.
    watcher: Option<tokio::task::JoinHandle<()>>,
    /// What its unattended turns were denied, for its next client
    /// ([`Agents::take_missed`]); at most [`MISSED_KEPT`].
    missed: Vec<String>,
    /// How many more were denied than `missed` keeps.
    missed_dropped: usize,
}

impl Drop for Agent {
    fn drop(&mut self) {
        if let Some(watcher) = self.watcher.take() {
            watcher.abort();
        }
    }
}

/// How long [`Agents::stop_watching`] waits for a watcher to count what
/// its stream holds.
const WATCHER_DRAIN: std::time::Duration = std::time::Duration::from_secs(2);

/// How many unattended denials an agent keeps for its next client.
const MISSED_KEPT: usize = 32;

/// The live agents.
pub struct Agents {
    /// What the agents share with the local methods.
    core: Arc<Core>,
    /// The last watch event sent to each agent: a run it starts is the
    /// watch's.
    watch_sent: parking_lot::Mutex<std::collections::HashMap<String, String>>,
    table: parking_lot::Mutex<BTreeMap<String, Agent>>,
    /// One seat or retire at a time: two `new` binds seat two agents, a
    /// `recent` bind never sees half of one.
    seating: tokio::sync::Mutex<()>,
    /// The activity count ([`Agent::active`]).
    clock: AtomicU64,
    /// Tells the event watchers the daemon stops ([`Self::stop_watching`]).
    draining: tokio::sync::watch::Sender<bool>,
}

impl std::fmt::Debug for Agents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agents")
            .field("agents", &self.names())
            .finish_non_exhaustive()
    }
}

impl Agents {
    /// No agents yet ([`Self::start`] restores or seats them).
    #[must_use]
    pub fn new(core: Arc<Core>) -> Arc<Self> {
        Arc::new(Self {
            core,
            watch_sent: parking_lot::Mutex::new(std::collections::HashMap::new()),
            table: parking_lot::Mutex::new(BTreeMap::new()),
            seating: tokio::sync::Mutex::new(()),
            clock: AtomicU64::new(0),
            draining: tokio::sync::watch::channel(false).0,
        })
    }

    /// What the agents share with the local methods.
    #[must_use]
    pub fn core(&self) -> &Arc<Core> {
        &self.core
    }

    /// Whether `peer_id` is the comms peer id of a live agent other than
    /// the one running `session` (the mob's record of each agent's
    /// endpoint names it).
    pub async fn is_agent_peer(&self, session: &SessionId, peer_id: &str) -> bool {
        let caller = self.core.conversations.agent_of(session);
        // An agent of another workspace the caller is wired to.
        if caller
            .as_deref()
            .is_some_and(|caller| self.core.wires.is_peer(caller, peer_id))
        {
            return true;
        }
        let names: Vec<String> = self
            .table
            .lock()
            .keys()
            .filter(|name| caller.as_deref() != Some(name.as_str()))
            .cloned()
            .collect();
        let Ok(handle) = self.core.state.handle_for(&MobId::from(MOB_ID)).await else {
            return false;
        };
        for name in names {
            if let Ok(Some(endpoint)) = handle
                .member_peer_endpoint(&AgentIdentity::from(name.as_str()))
                .await
                && endpoint.descriptor.peer_id.to_string() == peer_id
            {
                return true;
            }
        }
        // One of the caller's own helpers (wired to it): a message to a
        // child is a peer message like one to another agent.
        for key in self.core.registry.live_for(session) {
            let Ok(handle) = self.core.state.handle_for(&key.mob).await else {
                continue;
            };
            if let Ok(Some(endpoint)) = handle.member_peer_endpoint(&key.member).await
                && endpoint.descriptor.peer_id.to_string() == peer_id
            {
                return true;
            }
        }
        false
    }

    /// `name`'s run ended: its watches that live for a turn go.
    pub fn run_ended(&self, name: &str) {
        self.core.watches.end_turn(name);
    }

    /// The conversation `name` runs as now.
    #[must_use]
    pub fn session_of(&self, name: &str) -> Option<SessionId> {
        self.core.conversations.current(name)
    }

    fn tick(&self) -> u64 {
        self.clock.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// The agents restored from the mob (left running before the daemon
    /// stopped: detached, most recently used conversation the most recent
    /// one), else one fresh agent, seated as a spare for the first client.
    /// Returns the most recent agent and its session, and whether it was
    /// seated now.
    ///
    /// # Errors
    ///
    /// [`super::DaemonError::Mob`] when the members cannot be listed or
    /// the first agent cannot be seated.
    pub async fn start(self: &Arc<Self>) -> Result<(String, SessionId, bool), super::DaemonError> {
        let mob = MobId::from(MOB_ID);
        let members = self
            .core
            .state
            .mob_list_members(&mob)
            .await
            .map_err(|err| super::DaemonError::mob(&mob, err))?;
        let mut restored: Vec<(String, SessionId)> = Vec::new();
        for member in members {
            let name = member.agent_identity.to_string();
            if !is_agent(&name) {
                continue;
            }
            // A restored agent registered itself in its customizer's
            // resume; one that did not is registered here, before its
            // first turn can delegate.
            let session = match self.core.conversations.current(&name) {
                Some(session) => Some(session),
                None => {
                    let session = self
                        .core
                        .state
                        .mob_member_status(&mob, &member.agent_identity)
                        .await
                        .ok()
                        .and_then(|status| status.current_session_id);
                    if let Some(session) = &session {
                        self.core.conversations.register(&name, session);
                    }
                    session
                }
            };
            match session {
                Some(session) => restored.push((name, session)),
                None => {
                    tracing::warn!(agent = %name, "an agent restored without a session is left out");
                }
            }
        }
        // The log's order: the conversation used last is the most recent.
        let order: Vec<SessionId> = self
            .core
            .conversations
            .entries()
            .into_iter()
            .rev()
            .map(|entry| entry.session_id)
            .collect();
        restored.sort_by_key(|(_, session)| order.iter().position(|s| s == session));
        if restored.is_empty() {
            let _one = self.seating.lock().await;
            let name = new_name(|name| self.taken(name));
            let session = self
                .core
                .conversations
                .seat_new(&name, &Target::Fresh)
                .await
                .map_err(|err| super::DaemonError::FirstAgent(err.to_string()))?;
            self.admit(&name, true, false);
            tracing::info!(agent = %name, %session, "seated the workspace's first agent");
            return Ok((name, session, true));
        }
        for (name, session) in &restored {
            self.core.conversations.record(session, name);
            self.admit(name, false, true);
            self.core
                .conversations
                .seed(&self.core.engine, session)
                .await;
        }
        let names: Vec<String> = restored.iter().map(|(name, _)| name.clone()).collect();
        // The mob keeps its wiring across a restart; an edge it lost is
        // made again now.
        self.core.wires.keep_only(&names);
        self.wire_up(&names).await;
        // No client came back with them: what each was last told of the
        // others' clients is stale.
        self.note_rosters(&names).await;
        let Some((name, session)) = restored.pop() else {
            return Err(super::DaemonError::NoSession(AGENT_PROFILE.to_string()));
        };
        tracing::info!(agents = ?names, "restored the workspace's agents");
        Ok((name, session, false))
    }

    /// Whether `name` is taken: a live agent's, or the log's (a conversation
    /// of an agent that quit keeps its name for a later re-seat).
    fn taken(&self, name: &str) -> bool {
        self.table.lock().contains_key(name)
            || self
                .core
                .conversations
                .entries()
                .iter()
                .any(|entry| entry.agent == name)
    }

    /// Put `name` in the table (seated now: `spare` until a client binds;
    /// restored: `detached`) and follow its events.
    fn admit(self: &Arc<Self>, name: &str, spare: bool, detached: bool) {
        let watcher = super::approvals::watch(super::approvals::Watched {
            state: Arc::clone(&self.core.state),
            mob: MobId::from(MOB_ID),
            identity: AgentIdentity::from(name),
            engine: Arc::clone(&self.core.engine),
            origins: Arc::clone(&self.core.origins),
            current: Arc::clone(&self.core.conversations),
            ledger: Arc::clone(&self.core.ledger),
            agents: Arc::downgrade(self),
            activity: Arc::clone(&self.core.activity),
            draining: self.draining.subscribe(),
        });
        let active = self.tick();
        self.table.lock().insert(
            name.to_string(),
            Agent {
                clients: 0,
                detached,
                spare,
                active,
                watcher: Some(watcher),
                missed: Vec::new(),
                missed_dropped: 0,
            },
        );
        // Its socket for agents of other workspaces (a failure is kept for
        // its card: `/wire` names it).
        let _ = self.core.wires.bind(name);
    }

    /// Wire every agent of `names` to every other live agent, and make its
    /// cross-workspace wires again where its member lost them (a start, a
    /// seat, a switch).
    async fn wire_up(&self, names: &[String]) {
        self.rewire(names).await;
        for name in names {
            self.core.wires.restore(name).await;
        }
    }

    /// Wire every agent of `names` to every other live agent (an edge is
    /// both directions, so each pair once, in name order; an existing one
    /// stays). Best effort: a failure is logged, and the agents then cannot
    /// reach each other.
    async fn rewire(&self, names: &[String]) {
        let live = self.names();
        let edges: std::collections::BTreeSet<(&String, &String)> = names
            .iter()
            .flat_map(|name| {
                live.iter()
                    .filter(move |other| *other != name)
                    .map(move |other| (name.min(other), name.max(other)))
            })
            .collect();
        if edges.is_empty() {
            return;
        }
        let edges: Vec<(AgentIdentity, AgentIdentity)> = edges
            .into_iter()
            .map(|(a, b)| {
                (
                    AgentIdentity::from(a.as_str()),
                    AgentIdentity::from(b.as_str()),
                )
            })
            .collect();
        match self
            .core
            .state
            .mob_wire_members_batch(&MobId::from(MOB_ID), edges)
            .await
        {
            Ok(report) => tracing::debug!(
                wired = report.wired.len(),
                already = report.already_wired.len(),
                "wired the agents"
            ),
            Err(err) => tracing::warn!(error = %err, "cannot wire the agents to each other"),
        }
    }

    /// The live agents' names.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.table.lock().keys().cloned().collect()
    }

    /// Whether `name` is a live agent.
    #[must_use]
    pub fn is_live(&self, name: &str) -> bool {
        self.table.lock().contains_key(name)
    }

    /// Every live agent and the session it runs as.
    #[must_use]
    pub fn live(&self) -> Vec<(String, SessionId)> {
        self.names()
            .into_iter()
            .filter_map(|name| {
                let session = self.core.conversations.current(&name)?;
                Some((name, session))
            })
            .collect()
    }

    /// The live agent whose current conversation is `session`.
    #[must_use]
    pub fn agent_running_as(&self, session: &SessionId) -> Option<String> {
        self.names()
            .into_iter()
            .find(|name| self.core.conversations.current(name).as_ref() == Some(session))
    }

    /// Whether a client is bound to the live agent that runs as `session`.
    #[must_use]
    pub fn attended(&self, session: &SessionId) -> bool {
        let Some(name) = self.agent_running_as(session) else {
            return false;
        };
        self.table
            .lock()
            .get(&name)
            .is_some_and(|agent| agent.clients > 0)
    }

    /// Keep `notice` of an unattended denial for the next client of the
    /// agent that runs as `session` (dropped when no live agent does).
    pub fn missed(&self, session: &SessionId, notice: String) {
        let Some(name) = self.agent_running_as(session) else {
            tracing::warn!(%session, %notice, "an unattended denial of no live agent");
            return;
        };
        tracing::info!(agent = %name, %notice, "denied in an unattended turn");
        if let Some(agent) = self.table.lock().get_mut(&name) {
            if agent.missed.len() < MISSED_KEPT {
                agent.missed.push(notice);
            } else {
                agent.missed_dropped += 1;
            }
        }
    }

    /// The unattended denials `name` kept for its client, emptied.
    pub fn take_missed(&self, name: &str) -> Vec<String> {
        let mut table = self.table.lock();
        let Some(agent) = table.get_mut(name) else {
            return Vec::new();
        };
        let mut missed = std::mem::take(&mut agent.missed);
        let dropped = std::mem::take(&mut agent.missed_dropped);
        if dropped > 0 {
            missed.push(format!(
                "and {dropped} more actions denied while no client was attached (the daemon's log names them)"
            ));
        }
        missed
    }

    /// The most recently active live agent (spares last).
    #[must_use]
    pub fn recent(&self) -> Option<String> {
        self.recent_used().or_else(|| self.spare())
    }

    /// `name` was active now (a prompt, a run).
    pub fn touch(&self, name: &str) {
        let active = self.tick();
        if let Some(agent) = self.table.lock().get_mut(name) {
            agent.active = active;
        }
    }

    /// Bind a connection to the agent `choice` names, seating one when it
    /// asks for a new agent (the spare, if one waits) or for a
    /// conversation no live agent runs. The returned guard holds the
    /// binding: dropped without [`Self::retire`] it leaves the agent
    /// detached when it was its last client.
    ///
    /// # Errors
    ///
    /// [`BindError`].
    pub async fn bind(
        self: &Arc<Self>,
        choice: &AgentChoice,
    ) -> Result<(Binding, Bound), BindError> {
        let _one = self.seating.lock().await;
        let bound = match choice {
            AgentChoice::New => match self.spare() {
                Some(name) => Bound::existing(name),
                None => self.seat(Target::Fresh, None).await?,
            },
            AgentChoice::Recent => self.bind_recent().await?,
            AgentChoice::Agent { name } => {
                if !self.is_live(name) {
                    return Err(BindError::NoSuchAgent(name.clone()));
                }
                Bound::existing(name.clone())
            }
            AgentChoice::Conversation { reference } => match self.resolve(reference).await {
                // No match (the client's `/resume` says why): the most
                // recent agent.
                None => self.bind_recent().await?,
                Some((_, owner)) if self.is_live(&owner) => Bound::existing(owner),
                Some((session, owner)) => self.seat(Target::Resume(session), Some(owner)).await?,
            },
        };
        let active = self.tick();
        // An agent left detached stays marked until it retires: `/quit`
        // asks once a detach happened ("nobody detached since").
        let mut fresh = bound.seated;
        let mut first = false;
        if let Some(agent) = self.table.lock().get_mut(&bound.name) {
            fresh |= agent.spare;
            first = agent.clients == 0;
            agent.clients += 1;
            agent.spare = false;
            agent.active = active;
        }
        tracing::info!(agent = %bound.name, seated = bound.seated, "a client bound to an agent");
        // The others learn it joined, or that it has a client again; a
        // second client changes nothing they are told.
        if bound.seated || first {
            self.note_roster_to_others(&bound.name, bound.seated).await;
        }
        Ok((
            Binding {
                agents: Arc::clone(self),
                name: bound.name.clone(),
                fresh,
                withdrawn: false,
                leaving: None,
            },
            bound,
        ))
    }

    /// The agent a `recent` bind takes: the most recently active one; with
    /// none but the spare, the conversation used last (not the spare's
    /// own), seated again under its agent; else the spare, or a new one.
    /// Called under the seating lock.
    async fn bind_recent(self: &Arc<Self>) -> Result<Bound, BindError> {
        if let Some(name) = self.recent_used() {
            return Ok(Bound::existing(name));
        }
        let spare = self.spare();
        let last = self
            .core
            .conversations
            .entries()
            .into_iter()
            .find(|entry| Some(&entry.agent) != spare.as_ref());
        Ok(match (last, spare) {
            (Some(entry), _) if !self.is_live(&entry.agent) => {
                let target = Target::Resume(entry.session_id.clone());
                self.seat(target, Some(entry.agent)).await?
            }
            (Some(entry), _) => Bound::existing(entry.agent),
            (None, Some(name)) => Bound::existing(name),
            (None, None) => self.seat(Target::Fresh, None).await?,
        })
    }

    /// The spare agent, when one waits for its first client.
    fn spare(&self) -> Option<String> {
        self.table
            .lock()
            .iter()
            .find(|(_, agent)| agent.spare)
            .map(|(name, _)| name.clone())
    }

    /// The most recently active agent that is not a spare.
    fn recent_used(&self) -> Option<String> {
        self.table
            .lock()
            .iter()
            .filter(|(_, agent)| !agent.spare)
            .max_by_key(|(_, agent)| agent.active)
            .map(|(name, _)| name.clone())
    }

    /// The conversation `reference` names (a session id or a unique
    /// prefix, as `/resume` takes it) and the agent it belongs to; `None`
    /// when nothing (or more than one) matches.
    async fn resolve(&self, reference: &str) -> Option<(SessionId, String)> {
        let listed = self.core.conversations.list(None, &self.live()).await;
        match super::conversations::resolve(reference, &listed, None) {
            Ok(session) => {
                let owner = self.core.conversations.agent_of(&session)?;
                Some((session, owner))
            }
            Err(why) => {
                tracing::debug!(%reference, %why, "an attach named no conversation");
                None
            }
        }
    }

    /// Seat an agent on `target`: `name` (an agent that quit, seated again
    /// on its conversation) or a new name. A spare nobody bound is retired
    /// first, and its unused conversation forgotten (the client asked for
    /// another one). Called under the seating lock.
    async fn seat(
        self: &Arc<Self>,
        target: Target,
        name: Option<String>,
    ) -> Result<Bound, BindError> {
        if let Some(spare) = self.spare() {
            match self.core.conversations.retire(&spare).await {
                Ok(session) => {
                    self.drop_agent(&spare, session.as_ref());
                    if let Some(session) = session {
                        self.core.conversations.forget(&session);
                    }
                }
                Err(err) => {
                    tracing::warn!(agent = %spare, error = %err, "the unused agent was not retired");
                }
            }
        }
        let name = name.unwrap_or_else(|| new_name(|name| self.taken(name)));
        let session = self.core.conversations.seat_new(&name, &target).await?;
        if matches!(target, Target::Resume(_)) {
            self.core
                .conversations
                .seed(&self.core.engine, &session)
                .await;
        }
        self.admit(&name, false, false);
        self.wire_up(std::slice::from_ref(&name)).await;
        // The others are told it joined once its client is bound
        // ([`Self::bind`]), so the one note says it has one.
        self.note_roster(&name).await;
        tracing::info!(agent = %name, %session, "seated an agent");
        Ok(Bound { name, seated: true })
    }

    /// Switch `name`'s conversation ([`super::conversations::Conversations::switch`]).
    ///
    /// # Errors
    ///
    /// [`SwitchError`].
    pub async fn switch(&self, name: &str, target: Target) -> Result<SessionId, SwitchError> {
        // The member left the mob for the switch (and was seated back on
        // its conversation when the switch failed): its wiring went with
        // it, and the conversation it runs as now may know none of the
        // agents working here.
        let switched = self.core.conversations.switch(name, target).await;
        if self.lost(name, &switched).await {
            return switched;
        }
        // Every client of the agent follows it there (the one that asked
        // too: it reads the answer).
        if let Ok(session) = &switched {
            self.core.decisions.broadcast(
                super::protocol::SWITCHED,
                &super::protocol::SwitchedNote {
                    agent: name.to_string(),
                    session_id: session.clone(),
                },
            );
        }
        self.wire_up(&[name.to_string()]).await;
        if switched.is_ok() {
            self.note_roster(name).await;
        }
        switched
    }

    /// When `result` says the agent `name` was lost (out of the mob, its
    /// conversation not seated again), drop it from the live agents and
    /// tell the others it left; returns whether it was. A lost agent is no
    /// ghost: `/agents`, `--list` and the proxy no longer count it live.
    async fn lost<T>(&self, name: &str, result: &Result<T, SwitchError>) -> bool {
        let Err(SwitchError::Lost { .. }) = result else {
            return false;
        };
        tracing::error!(agent = %name, "the agent was lost in a failed switch");
        let session = self.core.conversations.current(name);
        self.drop_agent(name, session.as_ref());
        self.note_others(name, &Leave::Closed).await;
        true
    }

    /// [`super::conversations::Conversations::reseat`].
    ///
    /// # Errors
    ///
    /// [`SwitchError`].
    pub async fn reseat(&self, name: &str) -> Result<SessionId, SwitchError> {
        let reseated = self.core.conversations.reseat(name).await;
        if self.lost(name, &reseated).await {
            return reseated;
        }
        self.wire_up(&[name.to_string()]).await;
        reseated
    }

    /// Retire `name`: a running turn is cancelled (also when whether it
    /// runs cannot be told: a cancel of an idle member is harmless), then
    /// the member leaves
    /// the mob ([`super::conversations::Conversations::retire`]: its helpers stop, the engine
    /// archives its session, which stays in the log). Nothing reaches it
    /// afterwards: it is no member, so a peer's send to it is refused.
    /// Returns the session it ran as.
    ///
    /// # Errors
    ///
    /// [`SwitchError`] when the engine refuses the retire.
    pub async fn retire(&self, name: &str, why: &Leave) -> Result<Option<SessionId>, SwitchError> {
        let seated = self.seating().await;
        self.retire_seated(name, why, &seated).await
    }

    /// The seating lock: no agent is seated, bound or retired while it is
    /// held (`rkat/quit` decides and acts under it).
    pub async fn seating(&self) -> Seated<'_> {
        Seated {
            _held: self.seating.lock().await,
        }
    }

    /// [`Self::retire`] under the seating lock `_seated` holds.
    ///
    /// # Errors
    ///
    /// [`SwitchError`] when the engine refuses the retire.
    pub async fn retire_seated(
        &self,
        name: &str,
        why: &Leave,
        _seated: &Seated<'_>,
    ) -> Result<Option<SessionId>, SwitchError> {
        let mob = MobId::from(MOB_ID);
        let identity = AgentIdentity::from(name);
        if self.core.conversations.running(name).await != RunState::Idle
            && let Err(err) = self.core.state.mob_force_cancel(&mob, identity).await
        {
            tracing::warn!(agent = %name, error = %err, "the running turn could not be cancelled before the retire");
        }
        let session = self.core.conversations.retire(name).await?;
        self.drop_agent(name, session.as_ref());
        self.note_others(name, why).await;
        tracing::info!(agent = %name, ?session, "the agent retired");
        Ok(session)
    }

    /// `name` is no live agent any more (retired, lost in a switch, or an
    /// unused spare): its row, activity record, file watches, last watch
    /// event and wires go, and the gate forgets `session`, the one it ran
    /// as. The one place an agent leaves the table.
    fn drop_agent(&self, name: &str, session: Option<&SessionId>) {
        self.table.lock().remove(name);
        self.core.activity.forget(name);
        self.core.watches.forget(name);
        self.watch_sent.lock().remove(name);
        self.core.wires.forget(name);
        if let Some(session) = session {
            self.core.engine.forget_session(session);
        }
    }

    /// Every live agent as `rkat/status` lists it, names in order.
    pub async fn rows(&self) -> Vec<AgentRow> {
        let snapshot: Vec<(String, usize, bool)> = self
            .table
            .lock()
            .iter()
            .map(|(name, agent)| (name.clone(), agent.clients, agent.detached))
            .collect();
        let mut rows = Vec::with_capacity(snapshot.len());
        let schedules = self.core.schedules.counts().await;
        for (name, clients, detached) in snapshot {
            rows.push(AgentRow {
                session_id: self.core.conversations.current(&name),
                running: self.core.conversations.running(&name).await.reported(),
                schedules: schedules.get(&name).copied().unwrap_or(0),
                wires: self.core.wires.rows(&name),
                name,
                clients,
                detached,
            });
        }
        rows
    }

    /// Whether any live agent was left running without a client.
    #[must_use]
    pub fn any_detached(&self) -> bool {
        self.table.lock().values().any(|agent| agent.detached)
    }

    /// A connection bound to `name` ends: one client fewer; leaving it
    /// without one (and without a shutdown) leaves it detached. A
    /// `withdrawn` binding's client never learned it attached: an agent
    /// seated (or taken as the spare) for it waits for the next client
    /// (`fresh`), any other is left as it was. Either way, an agent left
    /// with no client is news to the others ([`Self::attachment_changed`]).
    fn release(self: &Arc<Self>, name: &str, withdrawn: bool, fresh: bool) {
        let terminating = self.core.lifecycle.is_terminating();
        {
            let mut table = self.table.lock();
            let Some(agent) = table.get_mut(name) else {
                return;
            };
            agent.clients = agent.clients.saturating_sub(1);
            if agent.clients > 0 {
                return;
            }
            if withdrawn {
                agent.spare |= fresh;
            } else if !terminating && !agent.detached {
                agent.detached = true;
                tracing::info!(agent = %name, "the agent's last client left without retiring it: detached, it keeps running");
            }
        }
        if !terminating {
            self.attachment_changed(name);
        }
    }

    /// A client of `name` left by its `/quit` while other clients work with
    /// it: one client fewer, now.
    pub fn left(self: &Arc<Self>, name: &str) {
        self.release(name, false, false);
    }

    /// `rkat/detach`: the agent `name` keeps running without its client.
    pub fn detach(&self, name: &str) {
        if let Some(agent) = self.table.lock().get_mut(name) {
            agent.detached = true;
        }
    }

    /// End every agent's watcher (the daemon shuts down): each counts the
    /// model calls its stream already reported, so the ledger's flush after
    /// this has them, then ends; one that does not end within
    /// [`WATCHER_DRAIN`] is aborted.
    pub async fn stop_watching(&self) {
        let _ = self.draining.send(true);
        let watchers: Vec<_> = self
            .table
            .lock()
            .values_mut()
            .filter_map(|agent| agent.watcher.take())
            .collect();
        for mut watcher in watchers {
            // Each counts what its stream holds already, then ends; one
            // that does not within the bound is stopped.
            if tokio::time::timeout(WATCHER_DRAIN, &mut watcher)
                .await
                .is_err()
            {
                watcher.abort();
            }
        }
    }
}

/// The seating lock held ([`Agents::seating`]).
pub struct Seated<'a> {
    _held: tokio::sync::MutexGuard<'a, ()>,
}

/// [`crate::approval::Attendance`] over the live agents: the daemon's gate
/// asks it whether a client is with an agent.
pub struct AgentAttendance(std::sync::Weak<Agents>);

impl AgentAttendance {
    /// Attendance as `agents` keeps it (nothing is attended once they are
    /// gone).
    #[must_use]
    pub fn new(agents: &Arc<Agents>) -> Self {
        Self(Arc::downgrade(agents))
    }
}

#[async_trait::async_trait]
impl crate::approval::AgentPeers for AgentAttendance {
    async fn is_agent_peer(&self, session: &SessionId, peer_id: &str) -> bool {
        let Some(agents) = self.0.upgrade() else {
            return false;
        };
        agents.is_agent_peer(session, peer_id).await
    }

    async fn unreachable(&self, session: &SessionId, peer_id: &str) -> Option<String> {
        let agents = self.0.upgrade()?;
        let caller = agents.core.conversations.agent_of(session)?;
        agents.core.wires.unreachable(&caller, peer_id)
    }
}

impl crate::approval::Attendance for AgentAttendance {
    fn attached(&self, session: &SessionId) -> bool {
        self.0
            .upgrade()
            .is_some_and(|agents| agents.attended(session))
    }

    fn missed(&self, session: &SessionId, notice: String) {
        if let Some(agents) = self.0.upgrade() {
            agents.missed(session, notice);
        }
    }
}

/// A connection's binding to an agent ([`Agents::bind`]): released when
/// the connection ends.
#[derive(Debug)]
pub struct Binding {
    agents: Arc<Agents>,
    name: String,
    /// The agent was seated, or taken as the spare, for this binding.
    fresh: bool,
    withdrawn: bool,
    /// The connection's `/quit` decision, which released it already.
    leaving: Option<super::lifecycle::Leaving>,
}

impl Binding {
    /// The agent.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The connection's `/quit` decision (`leaving`) releases the binding
    /// when it is made ([`Agents::left`]); its end then releases nothing.
    pub fn leaving(&mut self, leaving: super::lifecycle::Leaving) {
        self.leaving = Some(leaving);
    }

    /// Give the binding up before the client learned it attached: the
    /// agent is no client's, and not detached.
    pub fn withdraw(mut self) {
        self.withdrawn = true;
    }
}

impl Drop for Binding {
    fn drop(&mut self) {
        if self
            .leaving
            .as_ref()
            .is_some_and(super::lifecycle::Leaving::left)
        {
            return;
        }
        self.agents.release(&self.name, self.withdrawn, self.fresh);
    }
}
