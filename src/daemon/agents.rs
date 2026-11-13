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

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use meerkat_core::SessionId;
use meerkat_mob::{AgentIdentity, MobId};
use meerkat_mob_mcp::MobMcpState;

use super::approvals::DaemonToolOrigins;
use super::conversations::{Conversations, RunState, SwitchError, Target};
use super::lifecycle::Lifecycle;
use super::protocol::{AgentChoice, AgentRow};
use super::{AGENT_PROFILE, MOB_ID};
use crate::approval::ApprovalEngine;

/// What every agent's name starts with (its id follows).
pub const NAME_PREFIX: &str = "main-";

/// How many characters an agent's id has.
pub const ID_CHARS: usize = 4;

/// The characters an agent's id is drawn from.
const ID_ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";

/// Whether `name` is an agent's member identity: `main-<id>` (lower-case
/// letters and digits).
#[must_use]
pub fn is_agent(name: &str) -> bool {
    name.strip_prefix(NAME_PREFIX).is_some_and(|id| {
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
    static FALLBACK: AtomicU64 = AtomicU64::new(0);
    let read = crate::hex::random_bytes::<ID_CHARS>();
    let mut bytes = read.as_ref().copied().unwrap_or([0; ID_CHARS]);
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

/// What another agent was doing when a roster note was sent: whether a
/// run of it was open, and whether a client was attached to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerState {
    /// Its run, as its status said.
    pub run: RunState,
    /// At least one client was bound to it.
    pub attached: bool,
}

impl PeerState {
    /// The state as a roster note words it. An unreadable run state claims
    /// neither working nor idle.
    fn describe(self) -> &'static str {
        match (self.run, self.attached) {
            (RunState::Running, _) => "working",
            (RunState::Idle, true) => "idle, a client attached",
            (RunState::Idle, false) => "idle, no client attached",
            (RunState::Unknown, true) => "a client attached",
            (RunState::Unknown, false) => "no client attached",
        }
    }
}

/// The note telling an agent which other agents work in its workspace
/// and what each was doing when it was sent (`None`: none do). It is sent
/// when the roster or an agent's attachment changes, never for a run's
/// start or end, so the note says its states are a snapshot.
#[must_use]
pub fn roster_note(others: &[(String, PeerState)]) -> Option<String> {
    if others.is_empty() {
        return None;
    }
    let names = others
        .iter()
        .map(|(name, state)| format!("`{name}` ({})", state.describe()))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "Other agents in this workspace now: {names}. You are wired to each of them as a peer. \
         These states are as of this note: a new one replaces it when an agent joins, or a client \
         attaches to an agent or leaves it; none comes when an agent starts or ends a run."
    ))
}

/// `roster` (the note about this workspace's agents, when there are
/// others) with a line naming the agents of other workspaces `wired`
/// lists; `None` when both are empty.
#[must_use]
pub fn with_wired(roster: Option<String>, wired: &[String]) -> Option<String> {
    if wired.is_empty() {
        return roster;
    }
    let names = wired
        .iter()
        .map(|label| format!("`{label}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let line = format!(
        "Agents of other workspaces you are wired to: {names} (each another project's agent; its state is not known here, and a message starts a model turn there when it is idle)."
    );
    Some(match roster {
        Some(roster) => format!("{roster} {line}"),
        None => line,
    })
}

/// Why an agent leaves the workspace, as the others are told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Leave {
    /// Its user ran `/quit` (or quit the TUI) as its last client.
    Quit,
    /// Another agent's user retired it from `/agents` (`by`: that agent).
    Retired {
        /// The agent whose user retired it, when known.
        by: Option<String>,
    },
    /// Its session was closed otherwise (the daemon stopped, it was lost).
    Closed,
}

/// The note telling an agent that `name` joined (`None`) or left the
/// workspace, and why.
#[must_use]
pub fn membership_note(name: &str, left: Option<&Leave>) -> String {
    let why = match left {
        None => {
            return format!("Agent `{name}` joined this workspace; you are wired to it as a peer.");
        }
        Some(Leave::Quit) => "its user quit".to_string(),
        Some(Leave::Retired { by: Some(by) }) => {
            format!("the user of `{by}` retired it from /agents")
        }
        Some(Leave::Retired { by: None }) => "it was retired from /agents".to_string(),
        Some(Leave::Closed) => "its session was closed".to_string(),
    };
    format!("Agent `{name}` left this workspace ({why}); it takes no more messages.")
}

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

/// How long a seat or retire waits for its membership note to an idle
/// agent ([`Agents::note_each`]) before leaving it to land on its own.
const NOTE_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// How many unattended denials an agent keeps for its next client.
const MISSED_KEPT: usize = 32;

/// Append `text` to `name`'s system context under `key`. Best effort: a
/// failure is logged.
async fn append_note(state: Arc<MobMcpState>, name: String, text: String, key: String) {
    let mut request = meerkat_core::AppendSystemContextRequest::from_text(text);
    request.source = Some(NOTE_SOURCE.to_string());
    request.idempotency_key = Some(key);
    if let Err(err) = state
        .mob_append_system_context(
            &MobId::from(MOB_ID),
            &AgentIdentity::from(name.as_str()),
            request,
        )
        .await
    {
        tracing::warn!(agent = %name, error = %err, "the agent could not be told about the other agents");
    }
}

/// What an agent is told when it makes a profile read-only with comms.
pub const READ_ONLY_COMMS_NOTE: &str = "A mob profile you just wrote has `read_only: true` and `comms: true`: its members can read messages, but they cannot send any (`read_only` blocks every mutating tool, `send_message` and the other comms sends included). Drop `read_only` (and narrow the tools another way) if they must answer.";

/// Whether `args` hold a profile's tool settings with both `read_only`
/// and `comms` true (anywhere: a definition's profiles, or one profile).
#[must_use]
pub fn read_only_with_comms(args: &serde_json::Value) -> bool {
    match args {
        serde_json::Value::Object(map) => {
            let set = |key: &str| map.get(key).and_then(serde_json::Value::as_bool) == Some(true);
            (set("read_only") && set("comms")) || map.values().any(read_only_with_comms)
        }
        serde_json::Value::Array(items) => items.iter().any(read_only_with_comms),
        _ => false,
    }
}

/// The live agents.
pub struct Agents {
    /// When the agents are idle.
    activity: Arc<super::activity::Activity>,
    state: Arc<MobMcpState>,
    conversations: Arc<Conversations>,
    engine: Arc<ApprovalEngine>,
    origins: Arc<DaemonToolOrigins>,
    lifecycle: Arc<Lifecycle>,
    /// The agents' schedules (a run one fired is attributed to it).
    schedules: Arc<super::schedules::Schedules>,
    /// Where a fired run is announced to the clients.
    decisions: Arc<super::decisions::Decisions>,
    /// The audit log, when it could be opened.
    audit: Option<Arc<super::audit::AuditLog>>,
    /// The agents' file watches.
    watches: Arc<super::watches::Watches>,
    /// What every agent's and helper's model calls used.
    ledger: Arc<super::usage::Ledger>,
    /// The helpers and whom they work for (a message to one's own helper
    /// is a peer message like one to another agent).
    registry: Arc<crate::mobs::HelperRegistry>,
    /// The agents' sockets and wires to other workspaces.
    wires: Arc<super::wires::Wires>,
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

/// What [`Agents`] is made of.
pub struct AgentsParts {
    /// When the agents are idle.
    pub activity: Arc<super::activity::Activity>,
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
    /// The agents' schedules.
    pub schedules: Arc<super::schedules::Schedules>,
    /// The consent surface (its broadcast announces fired runs).
    pub decisions: Arc<super::decisions::Decisions>,
    /// The audit log, when it could be opened.
    pub audit: Option<Arc<super::audit::AuditLog>>,
    /// The agents' file watches.
    pub watches: Arc<super::watches::Watches>,
    /// The usage records.
    pub ledger: Arc<super::usage::Ledger>,
    /// The helpers and their owners.
    pub registry: Arc<crate::mobs::HelperRegistry>,
    /// The agents' sockets and wires to other workspaces.
    pub wires: Arc<super::wires::Wires>,
}

impl Agents {
    /// No agents yet ([`Self::start`] restores or seats them).
    #[must_use]
    pub fn new(parts: AgentsParts) -> Arc<Self> {
        let AgentsParts {
            activity,
            state,
            conversations,
            engine,
            origins,
            lifecycle,
            schedules,
            decisions,
            audit,
            watches,
            ledger,
            registry,
            wires,
        } = parts;
        Arc::new(Self {
            activity,
            state,
            conversations,
            engine,
            origins,
            lifecycle,
            schedules,
            decisions,
            audit,
            watches,
            ledger,
            registry,
            wires,
            watch_sent: parking_lot::Mutex::new(std::collections::HashMap::new()),
            table: parking_lot::Mutex::new(BTreeMap::new()),
            seating: tokio::sync::Mutex::new(()),
            clock: AtomicU64::new(0),
            draining: tokio::sync::watch::channel(false).0,
        })
    }

    /// A run of `name` that no client's input started began with input
    /// `prompt`: when a schedule of the agent fired it (on the evidence
    /// [`super::schedules::Schedules::attribute`] takes), the audit log
    /// records the trigger and the agent's clients are told
    /// ([`super::protocol::TRIGGERED`]).
    pub async fn unprompted_run(
        &self,
        name: &str,
        session: &SessionId,
        run: &meerkat_core::RunId,
        prompt: &str,
    ) {
        // The watch event the daemon sent it: positive by construction.
        let from_watch = {
            let mut sent = self.watch_sent.lock();
            if sent.get(name).is_some_and(|text| text == prompt) {
                sent.remove(name);
                true
            } else {
                false
            }
        };
        if from_watch {
            self.audit_trigger(name, session, run, &format!("watch ({prompt})"));
            return;
        }
        let Some(fired) = self.schedules.attribute(name, run, prompt).await else {
            return;
        };
        let label = format!("scheduled: {}", fired.name);
        tracing::info!(agent = %name, %run, schedule = %fired.schedule_id, "a schedule fired the agent's run");
        self.announce(
            name,
            session,
            run,
            label,
            prompt,
            &format!("schedule {} ({})", fired.name, fired.schedule_id),
        );
    }

    /// Record and announce a run `trigger` started (`label` as the clients
    /// show it, `detail` as the audit log names it).
    pub fn announce(
        &self,
        name: &str,
        session: &SessionId,
        run: &meerkat_core::RunId,
        label: String,
        prompt: &str,
        detail: &str,
    ) {
        if let Some(audit) = &self.audit {
            let mut line =
                super::audit::Line::event("run_triggered", detail.to_string()).of_agent(name);
            line.session = Some(session.to_string());
            line.run = Some(run.to_string());
            audit.write(&line);
        }
        self.decisions.broadcast(
            super::protocol::TRIGGERED,
            &super::protocol::TriggeredNote {
                agent: name.to_string(),
                session_id: session.clone(),
                label,
                prompt: prompt.to_string(),
            },
        );
    }

    /// `name` called `tool` with `args`: a profile made read-only with
    /// comms on (`mob_create`, `mob_profile_create`, `mob_profile_update`)
    /// gets the agent a note that its members cannot send messages (the
    /// engine's `read_only` blocks every mutating tool, `send_message`
    /// included). The note reaches it at its next turn boundary.
    pub fn tool_requested(&self, name: &str, tool: &str, args: &serde_json::Value) {
        if !matches!(
            tool,
            "mob_create" | "mob_profile_create" | "mob_profile_update"
        ) || !read_only_with_comms(args)
        {
            return;
        }
        let key = format!("read-only-comms-{}", self.tick());
        tokio::spawn(append_note(
            Arc::clone(&self.state),
            name.to_string(),
            READ_ONLY_COMMS_NOTE.to_string(),
            key,
        ));
    }

    /// Whether `peer_id` is the comms peer id of a live agent other than
    /// the one running `session` (the mob's record of each agent's
    /// endpoint names it).
    pub async fn is_agent_peer(&self, session: &SessionId, peer_id: &str) -> bool {
        let caller = self.conversations.agent_of(session);
        // An agent of another workspace the caller is wired to.
        if caller
            .as_deref()
            .is_some_and(|caller| self.wires.is_peer(caller, peer_id))
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
        let Ok(handle) = self.state.handle_for(&MobId::from(MOB_ID)).await else {
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
        for key in self.registry.live_for(session) {
            let Ok(handle) = self.state.handle_for(&key.mob).await else {
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

    /// The audit log's `run_triggered` line for `name`'s `run`.
    fn audit_trigger(
        &self,
        name: &str,
        session: &SessionId,
        run: &meerkat_core::RunId,
        detail: &str,
    ) {
        if let Some(audit) = &self.audit {
            let mut line =
                super::audit::Line::event("run_triggered", detail.to_string()).of_agent(name);
            line.session = Some(session.to_string());
            line.run = Some(run.to_string());
            audit.write(&line);
        }
    }

    /// Deliver a watch's `batch` to its agent: a message into its
    /// session (into a running turn at its next step, else it starts one,
    /// which is then a turn no user started), announced to its clients
    /// and written to the audit log.
    pub async fn watch_event(&self, batch: super::watches::Batch) {
        let name = batch.agent.clone();
        let Some(session) = self.conversations.current(&name) else {
            return;
        };
        if !self.table.lock().contains_key(&name) {
            return;
        }
        let text = batch.text();
        self.watch_sent.lock().insert(name.clone(), text.clone());
        let _sending = self.activity.sending(&name);
        let sent = self
            .state
            .mob_member_send(
                &MobId::from(MOB_ID),
                AgentIdentity::from(name.as_str()),
                meerkat_core::types::ContentInput::from(text.clone()),
                meerkat_core::types::HandlingMode::Steer,
                None,
            )
            .await;
        if let Err(err) = sent {
            tracing::warn!(agent = %name, error = %err, "a watch event did not reach the agent");
            self.watch_sent.lock().remove(&name);
            return;
        }
        if let Some(audit) = &self.audit {
            audit.write(&super::audit::Line::event("watch_event", text.clone()).of_agent(&name));
        }
        self.decisions.broadcast(
            super::protocol::TRIGGERED,
            &super::protocol::TriggeredNote {
                agent: name,
                session_id: session,
                label: "files changed".to_string(),
                prompt: text,
            },
        );
    }

    /// When the agents are idle.
    #[must_use]
    pub fn activity(&self) -> &Arc<super::activity::Activity> {
        &self.activity
    }

    /// `name`'s run ended: its watches that live for a turn go.
    pub fn run_ended(&self, name: &str) {
        self.watches.end_turn(name);
    }

    /// The conversation `name` runs as now.
    #[must_use]
    pub fn session_of(&self, name: &str) -> Option<SessionId> {
        self.conversations.current(name)
    }

    /// The agents' file watches.
    #[must_use]
    pub fn watches(&self) -> &Arc<super::watches::Watches> {
        &self.watches
    }

    /// What every agent's and helper's model calls used.
    #[must_use]
    pub fn ledger(&self) -> &Arc<super::usage::Ledger> {
        &self.ledger
    }

    /// The agents' sockets and wires to other workspaces.
    #[must_use]
    pub fn wires(&self) -> &Arc<super::wires::Wires> {
        &self.wires
    }

    /// The agents' schedules.
    #[must_use]
    pub fn schedules(&self) -> &Arc<super::schedules::Schedules> {
        &self.schedules
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
        // The mob keeps its wiring across a restart; an edge it lost is
        // made again now.
        self.rewire(&names).await;
        self.wires.keep_only(&names);
        self.restore_wires(&names).await;
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
                .conversations
                .entries()
                .iter()
                .any(|entry| entry.agent == name)
    }

    /// Put `name` in the table (seated now: `spare` until a client binds;
    /// restored: `detached`) and follow its events.
    fn admit(self: &Arc<Self>, name: &str, spare: bool, detached: bool) {
        let watcher = super::approvals::watch(super::approvals::Watched {
            state: Arc::clone(&self.state),
            mob: MobId::from(MOB_ID),
            identity: AgentIdentity::from(name),
            engine: Arc::clone(&self.engine),
            origins: Arc::clone(&self.origins),
            current: Arc::clone(&self.conversations),
            ledger: Arc::clone(&self.ledger),
            agents: Arc::downgrade(self),
            activity: Arc::clone(&self.activity),
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
        let _ = self.wires.bind(name);
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

    /// Make the cross-workspace wires of every agent of `names` again
    /// where its member lost them (a start, a seat, a switch).
    async fn restore_wires(&self, names: &[String]) {
        for name in names {
            self.wires.restore(name).await;
        }
    }

    /// Append `text` to `name`'s system context (it reads it at its next
    /// turn; nothing wakes it). Best effort: a failure is logged.
    async fn note(&self, name: &str, text: String) {
        let key = format!("rkat-agents:{name}:{}", self.tick());
        append_note(Arc::clone(&self.state), name.to_string(), text, key).await;
    }

    /// Every live agent and what it is doing now (whether a run is open,
    /// whether a client is attached), names in order.
    async fn states(&self) -> Vec<(String, PeerState)> {
        let attached: Vec<(String, bool)> = self
            .table
            .lock()
            .iter()
            .map(|(name, agent)| (name.clone(), agent.clients > 0))
            .collect();
        let mut states = Vec::with_capacity(attached.len());
        for (name, attached) in attached {
            let run = self.conversations.running(&name).await;
            states.push((name, PeerState { run, attached }));
        }
        states
    }

    /// The roster note for `name`: every other agent of `states`, then
    /// the agents of other workspaces it is wired to.
    fn roster_for(&self, name: &str, states: &[(String, PeerState)]) -> Option<String> {
        let others: Vec<(String, PeerState)> = states
            .iter()
            .filter(|(other, _)| other != name)
            .cloned()
            .collect();
        with_wired(roster_note(&others), &self.wires.labels(name))
    }

    /// Tell `name` which other agents work here now and what each is
    /// doing (its fresh conversation knows none).
    async fn note_roster(&self, name: &str) {
        if let Some(text) = self.roster_for(name, &self.states().await) {
            self.note(name, text).await;
        }
    }

    /// Tell every agent of `names` the roster as it is now, each its own
    /// ([`Self::note_each`]): the daemon restored them, and what their
    /// conversations were last told of the others' clients is gone.
    async fn note_rosters(&self, names: &[String]) {
        let states = self.states().await;
        let notes = names
            .iter()
            .filter_map(|name| Some((name.clone(), self.roster_for(name, &states)?)))
            .collect();
        self.note_each(notes).await;
    }

    /// Tell every other live agent the roster as it is now, after `name`
    /// joined (`joined`: led by the line saying so) or a client attached
    /// to it or its last one left.
    async fn note_roster_to_others(&self, name: &str, joined: bool) {
        let states = self.states().await;
        let notes = states
            .iter()
            .filter(|(other, _)| other != name)
            .filter_map(|(other, _)| {
                let roster = self.roster_for(other, &states)?;
                let text = if joined {
                    format!("{} {roster}", membership_note(name, None))
                } else {
                    roster
                };
                Some((other.clone(), text))
            })
            .collect();
        self.note_each(notes).await;
    }

    /// `name`'s attachment changed outside the seating lock (its last
    /// client left): the others get the roster as it is then, under the
    /// lock, so it is ordered with the seats and retires around it. An
    /// agent retired meanwhile, or a daemon shutting down, tells no one.
    fn attachment_changed(self: &Arc<Self>, name: &str) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let agents = Arc::clone(self);
        let name = name.to_string();
        runtime.spawn(async move {
            let _one = agents.seating.lock().await;
            if agents.is_live(&name) && !agents.lifecycle.is_terminating() {
                agents.note_roster_to_others(&name, false).await;
            }
        });
    }

    /// Tell every other live agent that `name` left.
    async fn note_others(&self, name: &str, left: &Leave) {
        let notes = self
            .names()
            .into_iter()
            .filter(|other| other != name)
            .map(|other| (other, membership_note(name, Some(left))))
            .collect();
        self.note_each(notes).await;
    }

    /// Append each note of `notes` (agent, text) to its agent.
    ///
    /// The engine applies an append to a running member's session only
    /// when its turn ends, so a note to a busy agent would hold the
    /// seating lock (and the attach waiting on it) for that agent's whole
    /// turn, its helpers' included. Each note is its own task: one to an
    /// idle agent is waited for (bounded by [`NOTE_WAIT`]), so the agent
    /// reads it at its next turn; one to a running agent, or one whose
    /// state cannot be read, is left to land when that turn ends.
    async fn note_each(&self, notes: Vec<(String, String)>) {
        let mut waited = Vec::new();
        for (other, text) in notes {
            let idle = self.conversations.running(&other).await == RunState::Idle;
            let key = format!("rkat-agents:{other}:{}", self.tick());
            let task = tokio::spawn(append_note(
                Arc::clone(&self.state),
                other.clone(),
                text,
                key,
            ));
            if idle {
                waited.push((other, task));
            } else {
                tracing::debug!(agent = %other, "the agent is busy: its note about the other agents lands after its turn");
            }
        }
        for (other, task) in waited {
            if tokio::time::timeout(NOTE_WAIT, task).await.is_err() {
                tracing::debug!(agent = %other, "the note about the other agents was not applied in time: it lands later");
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

    /// The live agent whose current conversation is `session`.
    #[must_use]
    pub fn agent_running_as(&self, session: &SessionId) -> Option<String> {
        self.names()
            .into_iter()
            .find(|name| self.conversations.current(name).as_ref() == Some(session))
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
                // No match (the client's `/resume` says why): the most
                // recent agent.
                None => self.bind_recent().await?,
                Some((_, owner)) if self.is_live(&owner) => Bound {
                    name: owner,
                    seated: false,
                },
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
    /// prefix, as `/resume` takes it) and the agent it belongs to; `None`
    /// when nothing (or more than one) matches.
    async fn resolve(&self, reference: &str) -> Option<(SessionId, String)> {
        let listed = self.conversations.list(None, &self.live()).await;
        match super::conversations::resolve(reference, &listed, None) {
            Ok(session) => {
                let owner = self.conversations.agent_of(&session)?;
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
                    self.wires.forget(&spare);
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
        self.restore_wires(std::slice::from_ref(&name)).await;
        // The others are told it joined once its client is bound
        // ([`Self::bind`]), so the one note says it has one.
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
        if self.lost(name, &switched).await {
            return switched;
        }
        // Every client of the agent follows it there (the one that asked
        // too: it reads the answer).
        if let Ok(session) = &switched {
            self.decisions.broadcast(
                super::protocol::SWITCHED,
                &super::protocol::SwitchedNote {
                    agent: name.to_string(),
                    session_id: session.clone(),
                },
            );
        }
        self.rewire(&[name.to_string()]).await;
        self.restore_wires(&[name.to_string()]).await;
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
        self.table.lock().remove(name);
        self.wires.forget(name);
        self.note_others(name, &Leave::Closed).await;
        true
    }

    /// [`Conversations::reseat`].
    ///
    /// # Errors
    ///
    /// [`SwitchError`].
    pub async fn reseat(&self, name: &str) -> Result<SessionId, SwitchError> {
        let reseated = self.conversations.reseat(name).await;
        if self.lost(name, &reseated).await {
            return reseated;
        }
        self.rewire(&[name.to_string()]).await;
        self.restore_wires(&[name.to_string()]).await;
        reseated
    }

    /// Retire `name`: a running turn is cancelled (also when whether it
    /// runs cannot be told: a cancel of an idle member is harmless), then
    /// the member leaves
    /// the mob ([`Conversations::retire`]: its helpers stop, the engine
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
        if self.conversations.running(name).await != RunState::Idle
            && let Err(err) = self.state.mob_force_cancel(&mob, identity).await
        {
            tracing::warn!(agent = %name, error = %err, "the running turn could not be cancelled before the retire");
        }
        let session = self.conversations.retire(name).await?;
        self.table.lock().remove(name);
        self.activity.forget(name);
        self.watches.forget(name);
        self.watch_sent.lock().remove(name);
        self.wires.forget(name);
        if let Some(session) = &session {
            self.engine.forget_session(session);
        }
        self.note_others(name, why).await;
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
        let schedules = self.schedules.counts().await;
        for (name, clients, detached) in snapshot {
            rows.push(AgentRow {
                session_id: self.conversations.current(&name),
                running: self.conversations.running(&name).await.reported(),
                schedules: schedules.get(&name).copied().unwrap_or(0),
                wires: self.wires.rows(&name),
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
        let terminating = self.lifecycle.is_terminating();
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
        let caller = agents.conversations.agent_of(session)?;
        agents.wires.unreachable(&caller, peer_id)
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_read_only_profile_with_comms_is_found_wherever_it_is() {
        let definition = serde_json::json!({"definition": {"profiles": {
            "worker": {"tools": {"read_only": true, "comms": true, "shell": false}},
            "lead": {"tools": {"comms": true}}
        }}});
        assert!(read_only_with_comms(&definition));
        let profile = serde_json::json!({"name": "w", "profile": {"tools": {"read_only": true, "comms": false}}});
        assert!(!read_only_with_comms(&profile));
        assert!(!read_only_with_comms(
            &serde_json::json!({"tools": {"comms": true}})
        ));
    }

    #[test]
    fn the_notes_name_the_agents() {
        assert_eq!(roster_note(&[]), None);
        let state = |run, attached| PeerState { run, attached };
        let roster = roster_note(&[
            ("main-aaaa".into(), state(RunState::Running, false)),
            ("main-bbbb".into(), state(RunState::Idle, true)),
            ("main-cccc".into(), state(RunState::Idle, false)),
            ("main-dddd".into(), state(RunState::Unknown, false)),
        ])
        .unwrap();
        assert!(
            roster.contains(
                "`main-aaaa` (working), `main-bbbb` (idle, a client attached), \
                 `main-cccc` (idle, no client attached), `main-dddd` (no client attached)."
            ) && roster.contains("wired"),
            "{roster}"
        );
        // A snapshot, and the note says so.
        assert!(
            roster.contains("as of this note") && roster.contains("starts or ends a run"),
            "{roster}"
        );
        assert!(membership_note("main-cccc", None).contains("`main-cccc` joined"));
        // Why it left: its user quit only on /quit.
        let quit = membership_note("main-cccc", Some(&Leave::Quit));
        assert!(
            quit.contains("`main-cccc` left") && quit.contains("its user quit"),
            "{quit}"
        );
        let retired = membership_note(
            "main-cccc",
            Some(&Leave::Retired {
                by: Some("main-aaaa".into()),
            }),
        );
        assert!(
            retired.contains("the user of `main-aaaa` retired it from /agents")
                && !retired.contains("quit"),
            "{retired}"
        );
        let closed = membership_note("main-cccc", Some(&Leave::Closed));
        assert!(
            closed.contains("its session was closed") && !closed.contains("quit"),
            "{closed}"
        );
    }

    #[test]
    fn the_roster_names_the_agents_of_other_workspaces_it_is_wired_to() {
        assert_eq!(with_wired(None, &[]), None);
        assert_eq!(with_wired(Some("r".into()), &[]).as_deref(), Some("r"));
        let alone = with_wired(None, &["api/main-ab12".into()]).unwrap();
        assert!(
            alone.starts_with("Agents of other workspaces you are wired to: `api/main-ab12`")
                && alone.contains("not known here"),
            "{alone}"
        );
        let both = with_wired(Some("Other agents.".into()), &["api/main-ab12".into()]).unwrap();
        assert!(
            both.starts_with("Other agents. Agents of other workspaces"),
            "{both}"
        );
    }

    #[test]
    fn agent_names_are_main_and_a_short_id() {
        for _ in 0..64 {
            let name = new_name(|_| false);
            assert!(is_agent(&name), "{name}");
            assert_eq!(name.len(), NAME_PREFIX.len() + ID_CHARS, "{name}");
        }
        assert!(!is_agent("main"), "the profile, not an agent");
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
