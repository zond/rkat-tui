//! The workspace's agents (P5 of `docs/mob-split.md`): several
//! orchestrator members of the daemon's mob `tui`, one per agent a user
//! works with.
//!
//! An agent is named `main-<id>` ([`new_name`], a short random id; stable
//! for the member's life, shown in the status bar, `--list`, `/agents` and
//! `rkat/status`). A daemon whose mob still has the single orchestrator
//! of before P5, `main`, keeps it as one agent among the others
//! ([`is_agent`]).
//!
//! Each connection is bound to one agent when it attaches
//! ([`super::protocol::AgentChoice`]: a new agent, the most recently
//! active one, an agent by name, or the agent owning a conversation) and
//! every member-scoped method it calls acts on that agent. [`Agents`]
//! keeps the live ones: how many clients each has, whether it was left
//! running without one (detached), and when it was last active; it seats
//! new ones, retires them, and follows each one's events for the gate
//! ([`super::approvals::watch`]).
//!
//! A conversation belongs to the agent that ran it: the engine seats a
//! session only under the identity that created it
//! (`ResumeSessionBoundToAnotherMember`), so a conversation whose agent
//! quit is seated again under that agent's name ([`Agents::bind`]).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use meerkat_core::SessionId;
use meerkat_mob::{AgentIdentity, MobId};
use meerkat_mob_mcp::MobMcpState;

use super::approvals::DaemonToolOrigins;
use super::conversations::{Conversations, SwitchError, Target};
use super::lifecycle::Lifecycle;
use super::protocol::{AgentChoice, AgentRow};
use super::{MOB_ID, ORCHESTRATOR};
use crate::approval::ApprovalEngine;

/// What every agent's name starts with (its id follows).
pub const NAME_PREFIX: &str = "main-";

/// How many characters an agent's id has.
pub const ID_CHARS: usize = 4;

/// The characters an agent's id is drawn from.
const ID_ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";

/// Whether `name` is an agent's member identity: `main-<id>` (lower-case
/// letters and digits), or the bare `main` of a daemon from before P5.
#[must_use]
pub fn is_agent(name: &str) -> bool {
    name == ORCHESTRATOR
        || name.strip_prefix(NAME_PREFIX).is_some_and(|id| {
            !id.is_empty()
                && id.len() <= 16
                && id
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

/// A fresh agent name no `taken` name has.
#[must_use]
pub fn new_name(taken: impl Fn(&str) -> bool) -> String {
    loop {
        let name = format!("{NAME_PREFIX}{}", random_id());
        if !taken(&name) {
            return name;
        }
    }
}

/// [`ID_CHARS`] characters of [`ID_ALPHABET`], from the system's random
/// source (else from the clock and a counter: a name only needs to differ
/// from the taken ones, which [`new_name`] checks).
fn random_id() -> String {
    use std::io::Read as _;
    static FALLBACK: AtomicU64 = AtomicU64::new(0);
    let mut bytes = [0u8; ID_CHARS];
    let read =
        std::fs::File::open("/dev/urandom").and_then(|mut random| random.read_exact(&mut bytes));
    if read.is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        let mixed = u64::from(nanos) ^ FALLBACK.fetch_add(0x9e37_79b9, Ordering::Relaxed);
        bytes = mixed.to_le_bytes()[..ID_CHARS]
            .try_into()
            .unwrap_or([0; ID_CHARS]);
    }
    bytes
        .iter()
        .map(|byte| char::from(ID_ALPHABET[usize::from(*byte) % ID_ALPHABET.len()]))
        .collect()
}

/// The source the daemon's notes about the other agents carry.
const NOTE_SOURCE: &str = "rkat-tui-agents";

/// The note telling an agent which other agents work in its workspace
/// (`None`: none do).
#[must_use]
pub fn roster_note(others: &[String]) -> Option<String> {
    if others.is_empty() {
        return None;
    }
    let names = others
        .iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "Other agents working in this workspace now: {names}. You are wired to each of them as a peer."
    ))
}

/// The note telling an agent that `name` joined (`joined`) or left the
/// workspace.
#[must_use]
pub fn membership_note(name: &str, joined: bool) -> String {
    if joined {
        format!("Agent `{name}` joined this workspace; you are wired to it as a peer.")
    } else {
        format!("Agent `{name}` left this workspace (its user quit); it takes no more messages.")
    }
}

/// Why a connection could not be bound to an agent.
#[derive(Debug, thiserror::Error)]
pub enum BindError {
    /// The named agent is not live.
    #[error("no agent `{0}` works in this workspace (rkat-tui --list shows the agents)")]
    NoSuchAgent(String),
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
}

impl Drop for Agent {
    fn drop(&mut self) {
        if let Some(watcher) = self.watcher.take() {
            watcher.abort();
        }
    }
}

/// The live agents.
pub struct Agents {
    state: Arc<MobMcpState>,
    conversations: Arc<Conversations>,
    engine: Arc<ApprovalEngine>,
    origins: Arc<DaemonToolOrigins>,
    lifecycle: Arc<Lifecycle>,
    table: parking_lot::Mutex<BTreeMap<String, Agent>>,
    /// One seat or retire at a time: two `new` binds seat two agents, a
    /// `recent` bind never sees half of one.
    seating: tokio::sync::Mutex<()>,
    /// The activity count ([`Agent::active`]).
    clock: AtomicU64,
}

impl std::fmt::Debug for Agents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agents")
            .field("agents", &self.names())
            .finish_non_exhaustive()
    }
}

/// What [`Agents`] is made of.
pub struct AgentsParts {
    /// The mob host.
    pub state: Arc<MobMcpState>,
    /// The agents' conversations.
    pub conversations: Arc<Conversations>,
    /// The approval engine every agent's gate shares.
    pub engine: Arc<ApprovalEngine>,
    /// The members' tool catalogs.
    pub origins: Arc<DaemonToolOrigins>,
    /// The daemon's lifecycle (a shutdown leaves no agent detached).
    pub lifecycle: Arc<Lifecycle>,
}

impl Agents {
    /// No agents yet ([`Self::start`] restores or seats them).
    #[must_use]
    pub fn new(parts: AgentsParts) -> Arc<Self> {
        let AgentsParts {
            state,
            conversations,
            engine,
            origins,
            lifecycle,
        } = parts;
        Arc::new(Self {
            state,
            conversations,
            engine,
            origins,
            lifecycle,
            table: parking_lot::Mutex::new(BTreeMap::new()),
            seating: tokio::sync::Mutex::new(()),
            clock: AtomicU64::new(0),
        })
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
            let session = match self.conversations.current(&name) {
                Some(session) => Some(session),
                None => {
                    let session = self
                        .state
                        .mob_member_status(&mob, &member.agent_identity)
                        .await
                        .ok()
                        .and_then(|status| status.current_session_id);
                    if let Some(session) = &session {
                        self.conversations.register(&name, session);
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
                .conversations
                .seat_new(&name, &Target::Fresh)
                .await
                .map_err(|err| super::DaemonError::FirstAgent(err.to_string()))?;
            self.admit(&name, true, false);
            tracing::info!(agent = %name, %session, "seated the workspace's first agent");
            return Ok((name, session, true));
        }
        for (name, session) in &restored {
            self.conversations.record(session, name);
            self.admit(name, false, true);
            self.conversations.seed(&self.engine, session).await;
        }
        let names: Vec<String> = restored.iter().map(|(name, _)| name.clone()).collect();
        // The mob keeps its wiring across a restart; an edge missing (an
        // agent seated by a daemon of before P5) is made now.
        self.rewire(&names).await;
        let Some((name, session)) = restored.pop() else {
            return Err(super::DaemonError::NoSession(ORCHESTRATOR.to_string()));
        };
        tracing::info!(agents = ?names, "restored the workspace's agents");
        Ok((name, session, false))
    }

    /// Whether `name` is taken: a live agent's, or the log's (a conversation
    /// of an agent that quit keeps its name for a later re-seat).
    fn taken(&self, name: &str) -> bool {
        self.table.lock().contains_key(name)
            || self
                .conversations
                .entries()
                .iter()
                .any(|entry| entry.agent == name)
    }

    /// Put `name` in the table (seated now: `spare` until a client binds;
    /// restored: `detached`) and follow its events.
    fn admit(self: &Arc<Self>, name: &str, spare: bool, detached: bool) {
        let watcher = super::approvals::watch(
            Arc::clone(&self.state),
            MobId::from(MOB_ID),
            AgentIdentity::from(name),
            Arc::clone(&self.engine),
            Arc::clone(&self.origins),
            Arc::clone(&self.conversations),
            Arc::downgrade(self),
        );
        let active = self.tick();
        self.table.lock().insert(
            name.to_string(),
            Agent {
                clients: 0,
                detached,
                spare,
                active,
                watcher: Some(watcher),
            },
        );
    }

    /// Wire every agent of `names` to every other live agent (an edge is
    /// both directions; an existing one stays). Best effort: a failure is
    /// logged, and the agents then cannot reach each other.
    async fn rewire(&self, names: &[String]) {
        let live = self.names();
        let mut edges: Vec<(String, String)> = Vec::new();
        for name in names {
            for other in &live {
                let pair = (name.clone(), other.clone());
                let back = (other.clone(), name.clone());
                if other != name && !edges.contains(&pair) && !edges.contains(&back) {
                    edges.push(pair);
                }
            }
        }
        if edges.is_empty() {
            return;
        }
        let edges: Vec<(AgentIdentity, AgentIdentity)> = edges
            .into_iter()
            .map(|(a, b)| (AgentIdentity::from(a), AgentIdentity::from(b)))
            .collect();
        match self
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

    /// Append `text` to `name`'s system context (it reads it at its next
    /// turn; nothing wakes it). Best effort: a failure is logged.
    async fn note(&self, name: &str, text: String) {
        let key = format!("rkat-agents:{name}:{}", self.tick());
        let mut request = meerkat_core::AppendSystemContextRequest::from_text(text);
        request.source = Some(NOTE_SOURCE.to_string());
        request.idempotency_key = Some(key);
        if let Err(err) = self
            .state
            .mob_append_system_context(&MobId::from(MOB_ID), &AgentIdentity::from(name), request)
            .await
        {
            tracing::warn!(agent = %name, error = %err, "the agent could not be told about the other agents");
        }
    }

    /// Tell `name` which other agents work here now (its fresh
    /// conversation knows none).
    async fn note_roster(&self, name: &str) {
        let others: Vec<String> = self
            .names()
            .into_iter()
            .filter(|other| other != name)
            .collect();
        if let Some(text) = roster_note(&others) {
            self.note(name, text).await;
        }
    }

    /// Tell every other live agent that `name` joined (`joined`) or left.
    async fn note_others(&self, name: &str, joined: bool) {
        for other in self.names() {
            if other != name {
                self.note(&other, membership_note(name, joined)).await;
            }
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
                let session = self.conversations.current(&name)?;
                Some((name, session))
            })
            .collect()
    }

    /// The most recently active live agent (spares last).
    #[must_use]
    pub fn recent(&self) -> Option<String> {
        self.table
            .lock()
            .iter()
            .max_by_key(|(_, agent)| (!agent.spare, agent.active))
            .map(|(name, _)| name.clone())
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
                Some(name) => Bound {
                    name,
                    seated: false,
                },
                None => self.seat(Target::Fresh, None).await?,
            },
            AgentChoice::Recent => self.bind_recent().await?,
            AgentChoice::Agent { name } => {
                if !self.is_live(name) {
                    return Err(BindError::NoSuchAgent(name.clone()));
                }
                Bound {
                    name: name.clone(),
                    seated: false,
                }
            }
            AgentChoice::Conversation { reference } => match self.resolve(reference).await {
                // No agent's conversation (a session created before the
                // daemon, which the client shows view only) or no match
                // (the client's `/resume` says why): the most recent agent.
                None => self.bind_recent().await?,
                Some((session, None)) => {
                    tracing::debug!(%session, "a view-only conversation: binding the most recent agent");
                    self.bind_recent().await?
                }
                Some((_, Some(owner))) if self.is_live(&owner) => Bound {
                    name: owner,
                    seated: false,
                },
                Some((session, Some(owner))) => {
                    self.seat(Target::Resume(session), Some(owner)).await?
                }
            },
        };
        let active = self.tick();
        // An agent left detached stays marked until it retires: `/quit`
        // asks once a detach happened ("nobody detached since").
        let mut fresh = bound.seated;
        if let Some(agent) = self.table.lock().get_mut(&bound.name) {
            fresh |= agent.spare;
            agent.clients += 1;
            agent.spare = false;
            agent.active = active;
        }
        tracing::info!(agent = %bound.name, seated = bound.seated, "a client bound to an agent");
        Ok((
            Binding {
                agents: Arc::clone(self),
                name: bound.name.clone(),
                fresh,
                withdrawn: false,
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
            return Ok(Bound {
                name,
                seated: false,
            });
        }
        let spare = self.spare();
        let last = self
            .conversations
            .entries()
            .into_iter()
            .find(|entry| Some(&entry.agent) != spare.as_ref());
        Ok(match (last, spare) {
            (Some(entry), _) if !self.is_live(&entry.agent) => {
                let target = Target::Resume(entry.session_id.clone());
                self.seat(target, Some(entry.agent)).await?
            }
            (Some(entry), _) => Bound {
                name: entry.agent,
                seated: false,
            },
            (None, Some(name)) => Bound {
                name,
                seated: false,
            },
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
    /// prefix, as `/resume` takes it) and the agent it belongs to (`None`:
    /// a view-only session); `None` when nothing (or more than one)
    /// matches.
    async fn resolve(&self, reference: &str) -> Option<(SessionId, Option<String>)> {
        let listed = self.conversations.list(None, &self.live()).await;
        match crate::client::sessions::resolve(reference, &listed, None) {
            Ok(session) => {
                let owner = self.conversations.agent_of(&session);
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
            match self.conversations.retire(&spare).await {
                Ok(session) => {
                    self.table.lock().remove(&spare);
                    if let Some(session) = session {
                        self.engine.forget_session(&session);
                        self.conversations.forget(&session);
                    }
                }
                Err(err) => {
                    tracing::warn!(agent = %spare, error = %err, "the unused agent was not retired");
                }
            }
        }
        let name = name.unwrap_or_else(|| new_name(|name| self.taken(name)));
        let session = self.conversations.seat_new(&name, &target).await?;
        if matches!(target, Target::Resume(_)) {
            self.conversations.seed(&self.engine, &session).await;
        }
        self.admit(&name, false, false);
        self.rewire(std::slice::from_ref(&name)).await;
        self.note_others(&name, true).await;
        self.note_roster(&name).await;
        tracing::info!(agent = %name, %session, "seated an agent");
        Ok(Bound { name, seated: true })
    }

    /// Switch `name`'s conversation ([`Conversations::switch`]).
    ///
    /// # Errors
    ///
    /// [`SwitchError`].
    pub async fn switch(&self, name: &str, target: Target) -> Result<SessionId, SwitchError> {
        // The member left the mob for the switch (and was seated back on
        // its conversation when the switch failed): its wiring went with
        // it, and the conversation it runs as now may know none of the
        // agents working here.
        let switched = self.conversations.switch(name, target).await;
        self.rewire(&[name.to_string()]).await;
        if switched.is_ok() {
            self.note_roster(name).await;
        }
        switched
    }

    /// [`Conversations::reseat`].
    ///
    /// # Errors
    ///
    /// [`SwitchError`].
    pub async fn reseat(&self, name: &str) -> Result<SessionId, SwitchError> {
        let reseated = self.conversations.reseat(name).await;
        self.rewire(&[name.to_string()]).await;
        reseated
    }

    /// Retire `name`: a running turn is cancelled, then the member leaves
    /// the mob ([`Conversations::retire`]: its helpers stop, the engine
    /// archives its session, which stays in the log). Nothing reaches it
    /// afterwards: it is no member, so a peer's send to it is refused.
    /// Returns the session it ran as.
    ///
    /// # Errors
    ///
    /// [`SwitchError`] when the engine refuses the retire.
    pub async fn retire(&self, name: &str) -> Result<Option<SessionId>, SwitchError> {
        let _one = self.seating.lock().await;
        let mob = MobId::from(MOB_ID);
        let identity = AgentIdentity::from(name);
        if self.conversations.running(name).await == Some(true)
            && let Err(err) = self.state.mob_force_cancel(&mob, identity).await
        {
            tracing::warn!(agent = %name, error = %err, "the running turn could not be cancelled before the retire");
        }
        let session = self.conversations.retire(name).await?;
        self.table.lock().remove(name);
        if let Some(session) = &session {
            self.engine.forget_session(session);
        }
        self.note_others(name, false).await;
        tracing::info!(agent = %name, ?session, "the agent retired");
        Ok(session)
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
        for (name, clients, detached) in snapshot {
            rows.push(AgentRow {
                session_id: self.conversations.current(&name),
                running: self.conversations.running(&name).await.unwrap_or(false),
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
    /// (`fresh`), any other is left as it was.
    fn release(&self, name: &str, withdrawn: bool, fresh: bool) {
        let terminating = self.lifecycle.is_terminating();
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

    /// `rkat/detach`: the agent `name` keeps running without its client.
    pub fn detach(&self, name: &str) {
        if let Some(agent) = self.table.lock().get_mut(name) {
            agent.detached = true;
        }
    }

    /// Abort every agent's watcher (the daemon shuts down).
    pub fn stop_watching(&self) {
        for agent in self.table.lock().values_mut() {
            if let Some(watcher) = agent.watcher.take() {
                watcher.abort();
            }
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
}

impl Binding {
    /// The agent.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Give the binding up before the client learned it attached: the
    /// agent is no client's, and not detached.
    pub fn withdraw(mut self) {
        self.withdrawn = true;
    }
}

impl Drop for Binding {
    fn drop(&mut self) {
        self.agents.release(&self.name, self.withdrawn, self.fresh);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_notes_name_the_agents() {
        assert_eq!(roster_note(&[]), None);
        let roster = roster_note(&["main-aaaa".into(), "main-bbbb".into()]).unwrap();
        assert!(
            roster.contains("`main-aaaa`, `main-bbbb`") && roster.contains("wired"),
            "{roster}"
        );
        assert!(membership_note("main-cccc", true).contains("`main-cccc` joined"));
        assert!(membership_note("main-cccc", false).contains("`main-cccc` left"));
    }

    #[test]
    fn agent_names_are_main_and_a_short_id() {
        for _ in 0..64 {
            let name = new_name(|_| false);
            assert!(is_agent(&name), "{name}");
            assert_eq!(name.len(), NAME_PREFIX.len() + ID_CHARS, "{name}");
        }
        assert!(is_agent("main"), "the orchestrator of before P5");
        for not in [
            "", "main-", "main-AB", "main-a b", "mainx", "helper", "main-a/b",
        ] {
            assert!(!is_agent(not), "{not}");
        }
        // A taken name is never drawn again.
        let first = new_name(|_| false);
        let other = new_name(|name| name == first);
        assert_ne!(first, other);
    }
}
