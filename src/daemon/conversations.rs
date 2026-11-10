//! Conversations: `/new` and `/resume` over the mob (P4 of
//! `docs/mob-split.md`), one agent at a time.
//!
//! An agent is one member identity of the daemon's mob (`tui/main-<id>`,
//! [`super::agents`]) whatever it talks about; a conversation is the
//! session that identity runs as. `/new` retires the member (the engine
//! archives its session) and spawns the identity again, fresh; `/resume`
//! retires it and spawns it on an earlier session of its own
//! (`MemberLaunchMode::Resume`, which revives the archived session). The
//! identity, its approval gate, the `rkat/*` methods, the catch-up and
//! every client's addressing stay as they were: only the member's
//! `current_session_id` moves, which clients read with `mob/member_status`.
//!
//! A session belongs to the identity that ran it: the engine refuses to
//! seat another identity on it (`ResumeSessionBoundToAnotherMember`, its
//! durable `comms_name` names the first), so `/resume` offers another
//! agent's conversation only through that agent, and a conversation whose
//! agent quit is seated again under that agent's name.
//!
//! The engine keeps no list of a member's earlier sessions, so the daemon
//! records each session an agent ran as in
//! `<realm>/conversations.json` ([`Log`], with the agent's
//! name; a log written before P5 names none, and its sessions are the
//! single orchestrator `main`'s); `/resume` offers those, newest first,
//! with their first message read from the session's committed history
//! (`SessionServiceHistoryExt::read_history`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use meerkat_core::{Message, SessionHistoryQuery, SessionId, TranscriptUserRole};
use meerkat_mob::{AgentIdentity, MemberLaunchMode, MobId, MobRuntimeMode, SpawnMemberSpec};
use meerkat_mob_mcp::MobMcpState;
use serde::{Deserialize, Serialize};

use super::{MOB_ID, ORCHESTRATOR};
use crate::mobs::{HelperKey, HelperRegistry};

/// The agent of a log entry written before P5 (the single orchestrator).
fn legacy_agent() -> String {
    ORCHESTRATOR.to_string()
}

/// The conversation log's file name in the realm.
pub const LOG_NAME: &str = "conversations.json";

/// How long a conversation's title may be.
const TITLE_CHARS: usize = 72;

/// How many leading messages [`Conversations::list`] reads per
/// conversation for its title (the count comes from the page's metadata).
pub const TITLE_PAGE: usize = 16;

/// How many pages of [`TITLE_PAGE`] messages [`Conversations::list`]
/// reads at most looking for a conversation's first user message.
pub const TITLE_PAGES: usize = 8;

/// How many conversations [`Conversations::list`] reads at once.
const LIST_READS: usize = 8;

/// The conversation log cannot be read.
#[derive(Debug, thiserror::Error)]
#[error("cannot read the conversation log {}: {why}", file.display())]
pub struct LogError {
    /// The log.
    pub file: PathBuf,
    /// Why.
    pub why: String,
}

/// Whether `messages` hold a user turn.
#[must_use]
pub fn has_user_turn(messages: &[Message]) -> bool {
    messages
        .iter()
        .any(|message| matches!(message, Message::User(_)))
}

/// One conversation an agent ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The session.
    pub session_id: SessionId,
    /// The agent (member identity) that ran it; a session belongs to it.
    #[serde(default = "legacy_agent")]
    pub agent: String,
    /// When it was first seated (Unix seconds).
    pub started_at: u64,
    /// When it was last made current (Unix seconds).
    pub last_used: u64,
}

/// The conversation log (`conversations.json`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Log {
    /// Every conversation, oldest touched first (see [`Log::touch`]).
    #[serde(default)]
    pub sessions: Vec<Entry>,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

impl Log {
    /// The log in the realm at `realm_root`.
    #[must_use]
    pub fn path(realm_root: &Path) -> PathBuf {
        realm_root.join(LOG_NAME)
    }

    /// Read `path` strictly: `None` when there is no file.
    ///
    /// # Errors
    ///
    /// [`LogError`] naming the file when it cannot be read or parsed.
    pub fn read(path: &Path) -> Result<Option<Self>, LogError> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(LogError {
                    file: path.to_path_buf(),
                    why: err.to_string(),
                });
            }
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|err| LogError {
                file: path.to_path_buf(),
                why: err.to_string(),
            })
    }

    /// Read `path`; a missing or unreadable file is an empty log (an
    /// unreadable one is logged).
    #[must_use]
    pub fn load(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|err| {
                tracing::warn!(file = %path.display(), error = %err, "unreadable conversation log; starting a new one");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// Write the log to `path` (0600, by rename).
    ///
    /// # Errors
    ///
    /// The write's error.
    pub fn save(&self, path: &Path) -> Result<(), crate::paths::AtomicWriteError> {
        let bytes = serde_json::to_vec_pretty(self).unwrap_or_default();
        crate::paths::write_atomic(path, &bytes, crate::paths::FileMode::Private, None)
    }

    /// `session` is `agent`'s current one now, or was just used: recorded
    /// when new, else its `last_used` moved and the entry moved last (the
    /// log's order breaks a tie of seconds).
    pub fn touch(&mut self, session: &SessionId, agent: &str) {
        let at = now();
        match self.sessions.iter().position(|e| &e.session_id == session) {
            Some(index) => {
                let mut entry = self.sessions.remove(index);
                entry.last_used = at;
                self.sessions.push(entry);
            }
            None => self.sessions.push(Entry {
                session_id: session.clone(),
                agent: agent.to_string(),
                started_at: at,
                last_used: at,
            }),
        }
    }

    /// `session` is `agent`'s: recorded when new (a restored agent's), its
    /// `last_used` left as it is otherwise.
    pub fn record(&mut self, session: &SessionId, agent: &str) {
        if !self.has(session) {
            self.touch(session, agent);
        }
    }

    /// Drop `session` from the log (an agent's first conversation that
    /// nobody used).
    pub fn forget(&mut self, session: &SessionId) {
        self.sessions.retain(|e| &e.session_id != session);
    }

    /// The agent `session` belongs to, when it is one of the log's.
    #[must_use]
    pub fn agent_of(&self, session: &SessionId) -> Option<&str> {
        self.sessions
            .iter()
            .find(|e| &e.session_id == session)
            .map(|e| e.agent.as_str())
    }

    /// Whether `session` is one of the log's.
    #[must_use]
    pub fn has(&self, session: &SessionId) -> bool {
        self.sessions.iter().any(|e| &e.session_id == session)
    }

    /// The entries, last used first (of two used in the same second, the
    /// one touched later).
    #[must_use]
    pub fn newest_first(&self) -> Vec<Entry> {
        let mut entries: Vec<Entry> = self.sessions.iter().rev().cloned().collect();
        entries.sort_by_key(|e| std::cmp::Reverse(e.last_used));
        entries
    }
}

/// One row of `rkat/conversations`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listed {
    /// The session.
    pub session_id: SessionId,
    /// The agent that ran it (`None`: a view-only session, or an older
    /// daemon).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Another live agent runs as it now (not the caller's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_use: Option<String>,
    /// When it was last current (Unix seconds).
    pub last_used: u64,
    /// How many messages its history holds (`None`: unreadable).
    pub messages: Option<usize>,
    /// Its first user message, cut short.
    pub title: Option<String>,
    /// Whether the caller's agent runs as it now.
    pub current: bool,
    /// A session no member of the daemon ran (one the removed in-process
    /// runner created before P4): its transcript can be shown, but the engine
    /// will not seat the orchestrator on it ([`Conversations::earlier`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub view_only: bool,
}

/// Why a switch was refused or failed.
#[derive(Debug, thiserror::Error)]
pub enum SwitchError {
    /// `/resume` named a session no agent of this workspace ran as.
    #[error("session {0} is not one of this workspace's conversations")]
    Unknown(SessionId),
    /// `/resume` named another agent's conversation: the engine seats a
    /// session only under the identity that ran it.
    #[error("conversation {session} is agent {agent}'s")]
    OtherAgent {
        /// The conversation.
        session: SessionId,
        /// Its agent.
        agent: String,
    },
    /// `/resume` named the current conversation.
    #[error("already in this conversation")]
    Current,
    /// The orchestrator's run is open (a turn is running): a switch would
    /// retire the member mid-turn.
    #[error("a turn is running; interrupt it first (Esc)")]
    Running,
    /// The engine refused a step.
    #[error("{step}: {source}")]
    Mob {
        /// What was being done.
        step: &'static str,
        /// The engine's error.
        #[source]
        source: Box<meerkat_mob::MobError>,
    },
    /// The member has no session after the switch.
    #[error("the agent has no session after the switch")]
    NoSession,
    /// The switch failed after the member left the mob, and seating its
    /// previous conversation again failed too: the agent is gone (its
    /// conversations stay in the log).
    #[error("agent {agent} was lost: {cause}, and its conversation could not be seated again")]
    Lost {
        /// The agent.
        agent: String,
        /// Why the switch failed.
        #[source]
        cause: Box<SwitchError>,
    },
}

impl SwitchError {
    fn mob(step: &'static str, source: meerkat_mob::MobError) -> Self {
        Self::Mob {
            step,
            source: Box::new(source),
        }
    }
}

/// What a switch spawns the orchestrator on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A fresh session (`/new`).
    Fresh,
    /// An earlier session (`/resume`).
    Resume(SessionId),
}

/// The agents' conversations.
pub struct Conversations {
    state: Arc<MobMcpState>,
    registry: Arc<HelperRegistry>,
    log_file: PathBuf,
    log: parking_lot::Mutex<Log>,
    /// One switch at a time per agent.
    switching: parking_lot::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// The start-up model and binding (a fresh conversation's, until the
    /// client switches it).
    model: String,
    auth_binding: Option<meerkat_core::AuthBindingRef>,
    /// Tests: how many of the next spawns fail ([`Self::spawn`]).
    #[cfg(any(test, feature = "test-support"))]
    spawn_faults: Option<Arc<std::sync::atomic::AtomicUsize>>,
}

impl std::fmt::Debug for Conversations {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Conversations")
            .field("log_file", &self.log_file)
            .finish_non_exhaustive()
    }
}

impl Conversations {
    /// The conversations of the realm at `realm_root`.
    #[must_use]
    pub fn open(
        state: Arc<MobMcpState>,
        registry: Arc<HelperRegistry>,
        realm_root: &Path,
        model: String,
        auth_binding: Option<meerkat_core::AuthBindingRef>,
    ) -> Self {
        let log_file = Log::path(realm_root);
        let log = Log::load(&log_file);
        Self {
            state,
            registry,
            log_file,
            log: parking_lot::Mutex::new(log),
            switching: parking_lot::Mutex::default(),
            model,
            auth_binding,
            #[cfg(any(test, feature = "test-support"))]
            spawn_faults: None,
        }
    }

    /// Fail as many of the next spawns as `faults` holds (each failure
    /// takes one), with [`SwitchError::NoSession`]: a switch, and seating
    /// its previous conversation again, can then be made to fail.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_spawn_faults(
        mut self,
        faults: Option<Arc<std::sync::atomic::AtomicUsize>>,
    ) -> Self {
        self.spawn_faults = faults;
        self
    }

    /// The helper key of `agent`.
    #[must_use]
    pub fn key(agent: &str) -> HelperKey {
        HelperKey::new(MobId::from(MOB_ID), AgentIdentity::from(agent))
    }

    fn lock_of(&self, agent: &str) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(self.switching.lock().entry(agent.to_string()).or_default())
    }

    /// The session `agent` runs as now.
    #[must_use]
    pub fn current(&self, agent: &str) -> Option<SessionId> {
        self.registry.root_session(&Self::key(agent))
    }

    /// The agent the log says ran `session`.
    #[must_use]
    pub fn agent_of(&self, session: &SessionId) -> Option<String> {
        self.log.lock().agent_of(session).map(str::to_string)
    }

    /// The log's entries, last used first.
    #[must_use]
    pub fn entries(&self) -> Vec<Entry> {
        self.log.lock().newest_first()
    }

    /// Record `session` as `agent`'s current conversation.
    pub fn touch(&self, session: &SessionId, agent: &str) {
        let mut log = self.log.lock();
        log.touch(session, agent);
        if let Err(err) = log.save(&self.log_file) {
            tracing::warn!(error = %err, "cannot record the conversation");
        }
    }

    /// Record `session` as `agent`'s when the log does not have it yet.
    pub fn record(&self, session: &SessionId, agent: &str) {
        let mut log = self.log.lock();
        if log.has(session) {
            return;
        }
        log.record(session, agent);
        if let Err(err) = log.save(&self.log_file) {
            tracing::warn!(error = %err, "cannot record the conversation");
        }
    }

    /// Drop `session` from the log.
    pub fn forget(&self, session: &SessionId) {
        let mut log = self.log.lock();
        log.forget(session);
        if let Err(err) = log.save(&self.log_file) {
            tracing::warn!(error = %err, "cannot record the conversation");
        }
    }

    /// Whether `agent`'s run is open now: [`RunState::Unknown`] when its
    /// status cannot be read or says nothing of its run (not idle).
    pub async fn running(&self, agent: &str) -> RunState {
        self.state
            .mob_member_status(&MobId::from(MOB_ID), &AgentIdentity::from(agent))
            .await
            .map_or(RunState::Unknown, |status| run_state(&status))
    }

    /// The conversations, last used first, then the realm's sessions
    /// created before the daemon ([`Self::earlier`], view only), with their size and first
    /// message: each conversation's history is read [`TITLE_PAGE`]
    /// messages at a time until its first user message (at most
    /// [`TITLE_PAGES`] pages; the size is the first page's message count),
    /// [`LIST_READS`] conversations at a time. `viewer` is the caller's
    /// agent (its current one is marked current); `live` names every live
    /// agent and the session it runs as (another agent's is marked in use).
    pub async fn list(&self, viewer: Option<&str>, live: &[(String, SessionId)]) -> Vec<Listed> {
        use futures::StreamExt as _;
        let entries = self.log.lock().newest_first();
        let service = self.state.session_service();
        let conversations = futures::stream::iter(entries)
            .map(|entry| {
                let service = Arc::clone(&service);
                let holder = live
                    .iter()
                    .find(|(_, session)| session == &entry.session_id)
                    .map(|(agent, _)| agent.clone());
                let current = holder.is_some() && holder.as_deref() == viewer;
                let in_use = holder.filter(|_| !current);
                async move {
                    let session = entry.session_id.clone();
                    let read = |offset| {
                        let service = Arc::clone(&service);
                        let session = session.clone();
                        async move {
                            service
                                .read_history(
                                    &session,
                                    SessionHistoryQuery {
                                        offset,
                                        limit: Some(TITLE_PAGE),
                                    },
                                )
                                .await
                                .ok()
                        }
                    };
                    let (count, title) = count_and_title(read).await;
                    Listed {
                        current,
                        title,
                        messages: count,
                        session_id: entry.session_id,
                        agent: Some(entry.agent),
                        in_use,
                        last_used: entry.last_used,
                        view_only: false,
                    }
                }
            })
            .buffered(LIST_READS)
            .collect::<Vec<_>>()
            .await;
        let known: Vec<SessionId> = conversations
            .iter()
            .map(|row| row.session_id.clone())
            .collect();
        let mut listed = conversations;
        listed.extend(self.earlier(&known).await);
        listed
    }

    /// The realm's sessions that are no member's (the engine records no
    /// durable `comms_name` for them) and not in `known`: the ones the
    /// removed in-process runner created before P4. The engine refuses to
    /// seat the orchestrator on such a session ("missing durable
    /// comms_name for resumed mob member") and a mob-owned transcript
    /// cannot be copied into a fresh one, so they are listed view only,
    /// most recently updated first. A session whose metadata cannot be
    /// read is left out.
    async fn earlier(&self, known: &[SessionId]) -> Vec<Listed> {
        use futures::StreamExt as _;
        let service = self.state.session_service();
        let mut sessions = match service
            .list(meerkat_core::service::SessionQuery::default())
            .await
        {
            Ok(sessions) => sessions,
            Err(err) => {
                tracing::warn!(error = %err, "cannot list the realm's sessions");
                return Vec::new();
            }
        };
        sessions.retain(|summary| !known.contains(&summary.session_id));
        sessions.sort_by_key(|summary| std::cmp::Reverse(summary.updated_at));
        futures::stream::iter(sessions)
            .map(|summary| {
                let service = Arc::clone(&service);
                async move {
                    let session = summary.session_id.clone();
                    let member = match service.load_persisted_session_metadata(&session).await {
                        Ok(Some(view)) => view
                            .session_metadata
                            .is_none_or(|metadata| metadata.comms_name.is_some()),
                        _ => true,
                    };
                    if member {
                        return None;
                    }
                    let read = |offset| {
                        let service = Arc::clone(&service);
                        let session = session.clone();
                        async move {
                            service
                                .read_history(
                                    &session,
                                    SessionHistoryQuery {
                                        offset,
                                        limit: Some(TITLE_PAGE),
                                    },
                                )
                                .await
                                .ok()
                        }
                    };
                    let (count, title) = count_and_title(read).await;
                    Some(Listed {
                        session_id: summary.session_id,
                        agent: None,
                        in_use: None,
                        last_used: summary
                            .updated_at
                            .duration_since(std::time::UNIX_EPOCH)
                            .map_or(0, |elapsed| elapsed.as_secs()),
                        messages: count,
                        title,
                        current: false,
                        view_only: true,
                    })
                }
            })
            .buffered(LIST_READS)
            .filter_map(std::future::ready)
            .collect()
            .await
    }

    /// What the realm holds, for a re-home that would replace it: each
    /// conversation of the log with a user turn, or whose history cannot
    /// be read, in words (empty: provably empty).
    pub async fn holding(&self) -> Vec<String> {
        const PAGE: usize = 64;
        let entries = self.log.lock().newest_first();
        let service = self.state.session_service();
        let mut held = Vec::new();
        for entry in entries {
            let mut offset = 0;
            loop {
                let page = service
                    .read_history(
                        &entry.session_id,
                        SessionHistoryQuery {
                            offset,
                            limit: Some(PAGE),
                        },
                    )
                    .await;
                match page {
                    Ok(page) if has_user_turn(&page.messages) => {
                        held.push(format!("conversation {} has user turns", entry.session_id));
                        break;
                    }
                    Ok(page) if page.has_more && !page.messages.is_empty() => {
                        offset += page.messages.len();
                    }
                    Ok(_) => break,
                    Err(err) => {
                        held.push(format!(
                            "conversation {} cannot be read ({err})",
                            entry.session_id
                        ));
                        break;
                    }
                }
            }
        }
        held
    }

    /// `session`'s committed messages.
    ///
    /// # Errors
    ///
    /// The engine's error reading the history.
    pub async fn messages(
        &self,
        session: &SessionId,
    ) -> Result<Vec<Message>, meerkat_core::service::SessionError> {
        let service = self.state.session_service();
        service
            .read_history(
                session,
                SessionHistoryQuery {
                    offset: 0,
                    limit: None,
                },
            )
            .await
            .map(|page| page.messages)
    }

    /// Seed the approval `engine`'s classifier transcript of `session` from
    /// its committed history (the human's words and tool calls): the
    /// classifier of a restored or resumed
    /// conversation then sees what the user asked before. A session that
    /// cannot be read is left unseeded.
    pub async fn seed(&self, engine: &crate::approval::ApprovalEngine, session: &SessionId) {
        match self
            .state
            .session_service()
            .load_persisted_session(session)
            .await
        {
            Ok(Some(stored)) => engine.seed_transcript(&stored),
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(%session, error = %err, "cannot seed the classifier transcript");
            }
        }
    }

    /// The configured endpoint `session`'s durable auth binding names
    /// (`None`: a built-in endpoint, or unreadable).
    pub async fn endpoint_of(&self, session: &SessionId) -> Option<String> {
        let view = self
            .state
            .session_service()
            .load_persisted_session_metadata(session)
            .await
            .ok()??;
        let binding = view.session_metadata?.auth_binding?;
        (binding.realm.as_str() == crate::endpoints::ENDPOINT_REALM)
            .then(|| binding.binding.as_str().to_string())
    }

    /// A conversation's total token usage, as the engine keeps it with the
    /// session (`SessionService::read`'s billing; the wire's
    /// `events/snapshot` and `session/read` carry no usage).
    ///
    /// # Errors
    ///
    /// The engine's refusal.
    pub async fn usage(
        &self,
        session: &SessionId,
    ) -> Result<meerkat_core::Usage, meerkat_core::service::SessionError> {
        self.state
            .session_service()
            .read(session)
            .await
            .map(|view| view.billing.usage)
    }

    /// Make `target` `agent`'s conversation: stop the current one's
    /// helpers, retire the member and spawn it again on `target`. When the
    /// spawn fails the member is spawned back on the session it had.
    /// Returns the session it runs as now. The member leaves its mob for
    /// the switch: its wiring goes with it, which the caller restores
    /// ([`super::agents::Agents::switch`]).
    ///
    /// # Errors
    ///
    /// [`SwitchError`].
    pub async fn switch(&self, agent: &str, target: Target) -> Result<SessionId, SwitchError> {
        self.seat(agent, target, false).await
    }

    /// Seat `agent` again on the session it runs as, so its agent is built
    /// anew from the realm config (a model profile written since is in a
    /// rebuilt agent's model registry only: the engine captures the
    /// registry at build). Refused while a turn runs.
    ///
    /// # Errors
    ///
    /// [`SwitchError`]; [`SwitchError::NoSession`] without a session.
    pub async fn reseat(&self, agent: &str) -> Result<SessionId, SwitchError> {
        let current = self.current(agent).ok_or(SwitchError::NoSession)?;
        self.seat(agent, Target::Resume(current), true).await
    }

    /// Seat `agent`, which is no member now, on `target` (a new agent, or
    /// one whose earlier conversation is seated again after it quit), and
    /// record it. Returns the session it runs as.
    ///
    /// # Errors
    ///
    /// [`SwitchError`]: the engine refused the spawn, or `target` is
    /// another agent's conversation.
    pub async fn seat_new(&self, agent: &str, target: &Target) -> Result<SessionId, SwitchError> {
        let lock = self.lock_of(agent);
        let _one = lock.lock().await;
        if let Target::Resume(session) = target {
            self.check_own(agent, session)?;
        }
        let mob = MobId::from(MOB_ID);
        self.spawn(&mob, agent, target).await?;
        self.seated(&mob, agent).await
    }

    /// Retire `agent` (it leaves the mob; the engine archives its session,
    /// which stays in the log and can be seated again under its name): its
    /// helpers are stopped first. Returns the session it ran as.
    ///
    /// # Errors
    ///
    /// [`SwitchError::Mob`] when the engine refuses the retire.
    pub async fn retire(&self, agent: &str) -> Result<Option<SessionId>, SwitchError> {
        let lock = self.lock_of(agent);
        let _one = lock.lock().await;
        let previous = self.current(agent);
        if let Some(previous) = &previous {
            crate::mobs::cancel_helpers(&self.state, &self.registry, previous).await;
        }
        self.state
            .mob_retire(&MobId::from(MOB_ID), AgentIdentity::from(agent))
            .await
            .map_err(|err| SwitchError::mob("retiring the agent", err))?;
        self.registry.unregister_root(&Self::key(agent));
        self.switching.lock().remove(agent);
        Ok(previous)
    }

    /// [`SwitchError::Unknown`] or [`SwitchError::OtherAgent`] unless
    /// `session` is one of `agent`'s conversations.
    fn check_own(&self, agent: &str, session: &SessionId) -> Result<(), SwitchError> {
        match self.log.lock().agent_of(session) {
            None => Err(SwitchError::Unknown(session.clone())),
            Some(owner) if owner != agent => Err(SwitchError::OtherAgent {
                session: session.clone(),
                agent: owner.to_string(),
            }),
            Some(_) => Ok(()),
        }
    }

    /// [`Self::switch`]; `again`: the current session is a valid target
    /// ([`Self::reseat`]).
    async fn seat(
        &self,
        agent: &str,
        target: Target,
        again: bool,
    ) -> Result<SessionId, SwitchError> {
        let lock = self.lock_of(agent);
        let _one = lock.lock().await;
        let previous = self.current(agent);
        if let Target::Resume(session) = &target {
            if previous.as_ref() == Some(session) && !again {
                return Err(SwitchError::Current);
            }
            self.check_own(agent, session)?;
        }
        let mob = MobId::from(MOB_ID);
        let identity = AgentIdentity::from(agent);
        // Another client may be running a turn: its own guard is in
        // another process. The engine's run phase decides.
        self.refuse_running(&mob, &identity).await?;
        if let Some(previous) = &previous {
            crate::mobs::cancel_helpers(&self.state, &self.registry, previous).await;
        }
        // Once more right before the retire: a turn another client started
        // while the helpers were being cancelled is not retired mid-run.
        // The engine offers no way to hold turns off across the read and
        // the retire, so a turn admitted in between still loses its member
        // (see `docs/evidence.md`); the window is that of one status read.
        self.refuse_running(&mob, &identity).await?;
        self.state
            .mob_retire(&mob, identity.clone())
            .await
            .map_err(|err| SwitchError::mob("retiring the agent", err))?;
        // Seated again on the same session: its retired actor must be gone
        // first, or the new one shares runtime state the old one's exit
        // drops (an auth lease rotation then finds its store gone).
        if again && let Some(previous) = &previous {
            self.state
                .session_service()
                .await_removed_actor_exit(previous)
                .await;
        }
        let spawned = self.spawn(&mob, agent, &target).await;
        if let Err(err) = spawned {
            tracing::warn!(error = %err, "the switch failed; seating the previous conversation again");
            let restored = match &previous {
                Some(previous) => match self
                    .spawn(&mob, agent, &Target::Resume(previous.clone()))
                    .await
                {
                    Ok(()) => true,
                    Err(again) => {
                        tracing::error!(error = %again, "the previous conversation could not be seated again");
                        false
                    }
                },
                None => false,
            };
            if !restored {
                // Out of the mob: no owner any more.
                self.registry.unregister_root(&Self::key(agent));
                return Err(SwitchError::Lost {
                    agent: agent.to_string(),
                    cause: Box::new(err),
                });
            }
            return Err(err);
        }
        let session = self.seated(&mob, agent).await?;
        tracing::info!(%session, ?previous, agent, "the agent switched conversations");
        Ok(session)
    }

    /// The session the just spawned `agent` runs as, registered as an
    /// owner (on its session's model, else the start-up one) and recorded
    /// in the log.
    async fn seated(&self, mob: &MobId, agent: &str) -> Result<SessionId, SwitchError> {
        let session = self
            .state
            .mob_member_status(mob, &AgentIdentity::from(agent))
            .await
            .map_err(|err| SwitchError::mob("reading the agent", err))?
            .current_session_id
            .ok_or(SwitchError::NoSession)?;
        self.register(agent, &session);
        self.touch(&session, agent);
        Ok(session)
    }

    /// Register `agent` as an owner running as `session`, on its session's
    /// model when the registry knows it, else the start-up one.
    pub fn register(&self, agent: &str, session: &SessionId) {
        let (model, binding) = match self.registry.model_of(session) {
            Some(model) => (model, self.registry.auth_binding_of(session)),
            None => (self.model.clone(), self.auth_binding.clone()),
        };
        self.registry
            .register_root(Self::key(agent), session, &model, binding.as_ref());
    }

    /// Run `apply` on `agent`'s current session while it is idle: no
    /// conversation switch runs meanwhile (the switch lock is held) and no
    /// run is open when it starts (a model switch, `rkat/set_model`).
    ///
    /// # Errors
    ///
    /// [`SwitchError::Running`] when a turn runs, [`SwitchError::NoSession`]
    /// when the agent has none, [`SwitchError::Mob`] when its status
    /// cannot be read.
    pub async fn while_idle<T, F>(
        &self,
        agent: &str,
        apply: impl FnOnce(SessionId) -> F,
    ) -> Result<T, SwitchError>
    where
        F: std::future::Future<Output = T>,
    {
        let lock = self.lock_of(agent);
        let _one = lock.lock().await;
        let session = self.current(agent).ok_or(SwitchError::NoSession)?;
        self.refuse_running(&MobId::from(MOB_ID), &AgentIdentity::from(agent))
            .await?;
        Ok(apply(session).await)
    }

    /// [`SwitchError::Running`] when the member's run is open now.
    async fn refuse_running(
        &self,
        mob: &MobId,
        identity: &AgentIdentity,
    ) -> Result<(), SwitchError> {
        let status = self
            .state
            .mob_member_status(mob, identity)
            .await
            .map_err(|err| SwitchError::mob("reading the agent", err))?;
        if run_open(&status) {
            return Err(SwitchError::Running);
        }
        Ok(())
    }

    async fn spawn(&self, mob: &MobId, agent: &str, target: &Target) -> Result<(), SwitchError> {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(faults) = &self.spawn_faults
            && faults
                .fetch_update(
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                    |n| n.checked_sub(1),
                )
                .is_ok()
        {
            return Err(SwitchError::NoSession);
        }
        let mut spec = SpawnMemberSpec::host_root(ORCHESTRATOR, agent);
        spec.runtime_mode = Some(MobRuntimeMode::TurnDriven);
        if let Target::Resume(session) = target {
            spec.launch_mode = MemberLaunchMode::Resume {
                bridge_session_id: session.clone(),
                resume_from_role: None,
            };
        }
        self.state
            .mob_spawn_spec(mob, spec)
            .await
            .map(drop)
            .map_err(|err| SwitchError::mob("seating the agent", err))
    }
}

/// A conversation's message count (from its first page) and title (its
/// first user message), reading pages of [`TITLE_PAGE`] with `read(offset)`
/// until a title is found, the history ends, or [`TITLE_PAGES`] pages were
/// read. `(None, None)` when the first page is unreadable.
pub async fn count_and_title<R, F>(read: R) -> (Option<usize>, Option<String>)
where
    R: Fn(usize) -> F,
    F: std::future::Future<Output = Option<meerkat_core::SessionHistoryPage>>,
{
    let Some(first) = read(0).await else {
        return (None, None);
    };
    let count = Some(first.message_count);
    let mut page = first;
    let mut offset = 0;
    for _ in 1..TITLE_PAGES {
        if let Some(title) = title(&page.messages) {
            return (count, Some(title));
        }
        offset += page.messages.len();
        if !page.has_more || page.messages.is_empty() {
            return (count, None);
        }
        match read(offset).await {
            Some(next) => page = next,
            None => return (count, None),
        }
    }
    (count, title(&page.messages))
}

/// What a member's run is doing, as far as its status says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    /// Its run is open.
    Running,
    /// Its run is closed.
    Idle,
    /// The status could not be read, or carries no run state: neither
    /// running nor idle is known (a retire cancels anyway).
    Unknown,
}

impl RunState {
    /// As `rkat/status` reports it: `None` when unknown.
    #[must_use]
    pub fn reported(self) -> Option<bool> {
        match self {
            Self::Running => Some(true),
            Self::Idle => Some(false),
            Self::Unknown => None,
        }
    }
}

/// What the member's snapshot says of its run.
#[must_use]
pub fn run_state(status: &meerkat_mob::MobMemberSnapshot) -> RunState {
    run_state_of(status.progress.as_ref().map(|progress| progress.run_state))
}

/// [`run_state`] of a snapshot whose progress says `state` (`None`: it
/// carries no progress).
fn run_state_of(state: Option<meerkat_mob::MemberRunState>) -> RunState {
    match state {
        Some(meerkat_mob::MemberRunState::RunOpen) => RunState::Running,
        Some(meerkat_mob::MemberRunState::Idle) => RunState::Idle,
        Some(meerkat_mob::MemberRunState::Unknown) | None => RunState::Unknown,
    }
}

/// Whether the member's snapshot says its run is open.
#[must_use]
pub fn run_open(status: &meerkat_mob::MobMemberSnapshot) -> bool {
    run_state(status) == RunState::Running
}

/// The first conversational user message of `messages`, cut to
/// [`TITLE_CHARS`] on one line.
#[must_use]
pub fn title(messages: &[Message]) -> Option<String> {
    messages.iter().find_map(|message| match message {
        Message::User(user) if user.transcript_role == TranscriptUserRole::Conversational => {
            let text = user.text_content();
            let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
            if line.is_empty() {
                return None;
            }
            Some(if line.chars().count() > TITLE_CHARS {
                format!(
                    "{}…",
                    line.chars().take(TITLE_CHARS - 1).collect::<String>()
                )
            } else {
                line
            })
        }
        _ => None,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {

    /// The finding this guards (second review, item 9): a status without
    /// a run state read as idle, so a retire skipped the cancel and
    /// `/agents` said idle.
    #[test]
    fn a_status_without_a_run_state_is_unknown_not_idle() {
        use meerkat_mob::MemberRunState;
        assert_eq!(
            run_state_of(Some(MemberRunState::RunOpen)),
            RunState::Running
        );
        assert_eq!(run_state_of(Some(MemberRunState::Idle)), RunState::Idle);
        assert_eq!(
            run_state_of(Some(MemberRunState::Unknown)),
            RunState::Unknown
        );
        assert_eq!(run_state_of(None), RunState::Unknown);
        assert_eq!(RunState::Unknown.reported(), None);
        assert_eq!(RunState::Idle.reported(), Some(false));
    }

    use super::*;

    #[test]
    fn the_log_records_each_session_once_and_lists_the_last_used_first() {
        let dir = tempfile::tempdir().unwrap();
        let file = Log::path(dir.path());
        let (a, b) = (SessionId::new(), SessionId::new());
        let mut log = Log::load(&file);
        assert!(log.sessions.is_empty());
        log.touch(&a, "main-a");
        log.touch(&b, "main-b");
        log.sessions[1].last_used = 1;
        log.touch(&a, "main-a");
        log.save(&file).unwrap();
        let back = Log::load(&file);
        assert_eq!(back.sessions.len(), 2);
        assert!(back.has(&a) && back.has(&b));
        assert_eq!(back.newest_first()[0].session_id, a);
        assert_eq!(back.agent_of(&b), Some("main-b"));
        // Used in the same second, the one touched later is the newer:
        // a turn on `b` after `a` was seated (the CI failure this guards:
        // `-c` after a restart joined the idle agent seated last).
        let mut tie = back.clone();
        for entry in &mut tie.sessions {
            entry.last_used = 7;
        }
        tie.touch(&b, "main-b");
        for entry in &mut tie.sessions {
            entry.last_used = 7;
        }
        assert_eq!(tie.newest_first()[0].session_id, b);
        tie.touch(&a, "main-a");
        for entry in &mut tie.sessions {
            entry.last_used = 7;
        }
        assert_eq!(tie.newest_first()[0].session_id, a);
        let mut forgot = back;
        forgot.forget(&b);
        assert!(!forgot.has(&b) && forgot.has(&a));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::write(&file, "garbage").unwrap();
        assert!(Log::load(&file).sessions.is_empty());
    }

    /// A log written before P5 names no agent: its conversations are the
    /// single orchestrator `main`'s.
    #[test]
    fn a_log_from_before_p5_belongs_to_main() {
        let dir = tempfile::tempdir().unwrap();
        let file = Log::path(dir.path());
        let session = SessionId::new();
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(
            &file,
            serde_json::json!({"sessions": [{"session_id": session, "started_at": 1, "last_used": 2}]})
                .to_string(),
        )
        .unwrap();
        let log = Log::load(&file);
        assert_eq!(log.agent_of(&session), Some(ORCHESTRATOR));
    }

    /// A conversation that opens with notes before the first prompt is
    /// titled from a later page; the paging is bounded.
    #[tokio::test]
    async fn the_title_is_looked_for_past_the_first_page() {
        use meerkat_core::{SessionHistoryPage, SystemMessage};
        let session = SessionId::new();
        let history = |prompt_at: usize, len: usize| {
            let session = session.clone();
            move |offset: usize| {
                let messages: Vec<Message> = (offset..len.min(offset + TITLE_PAGE))
                    .map(|i| {
                        if i == prompt_at {
                            Message::User(meerkat_core::UserMessage::text("the real prompt"))
                        } else {
                            Message::System(SystemMessage::new(format!("note {i}")))
                        }
                    })
                    .collect();
                let page = SessionHistoryPage {
                    session_id: session.clone(),
                    message_count: len,
                    offset,
                    limit: Some(TITLE_PAGE),
                    has_more: offset + messages.len() < len,
                    messages,
                };
                async move { Some(page) }
            }
        };
        let late = TITLE_PAGE * 2 + 3;
        assert_eq!(
            count_and_title(history(late, 200)).await,
            (Some(200), Some("the real prompt".to_string()))
        );
        // Beyond the bound: counted, untitled.
        assert_eq!(
            count_and_title(history(TITLE_PAGE * TITLE_PAGES + 1, 500)).await,
            (Some(500), None)
        );
        // A short history without a prompt ends at its end.
        assert_eq!(count_and_title(history(99, 5)).await, (Some(5), None));
        let unreadable = |_offset: usize| async { None };
        assert_eq!(count_and_title(unreadable).await, (None, None));
    }

    #[test]
    fn a_title_is_the_first_user_line_cut_short() {
        let user = |text: &str| Message::User(meerkat_core::UserMessage::text(text.to_string()));
        assert_eq!(title(&[]), None);
        assert_eq!(
            title(&[user("  fix\n the   bug "), user("later")]).as_deref(),
            Some("fix the bug")
        );
        let long = "x".repeat(200);
        let cut = title(&[user(&long)]).unwrap();
        assert_eq!(cut.chars().count(), TITLE_CHARS);
        assert!(cut.ends_with('…'));
    }
}
