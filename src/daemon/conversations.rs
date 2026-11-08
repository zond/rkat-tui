//! Conversations: `/new` and `/resume` over the mob (P4 of
//! `docs/mob-split.md`).
//!
//! The orchestrator is one member identity (`tui/main`) whatever it talks
//! about; a conversation is the session that identity runs as. `/new`
//! retires the member (the engine archives its session) and spawns the
//! identity again, fresh; `/resume` retires it and spawns it on an earlier
//! session (`MemberLaunchMode::Resume`, which revives the archived
//! session). The identity, its approval gate, the `rkat/*` methods, the
//! catch-up and every client's addressing stay as they were: only the
//! member's `current_session_id` moves, which clients read with
//! `mob/member_status`.
//!
//! The engine keeps no list of a member's earlier sessions, so the daemon
//! records each session its orchestrator ran as in
//! `<workspace>/.rkat/tui/conversations.json` ([`Log`]); `/resume` offers
//! those, newest first, with their first message read from the session's
//! committed history (`SessionServiceHistoryExt::read_history`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use meerkat_core::{Message, SessionHistoryQuery, SessionId, TranscriptUserRole};
use meerkat_mob::{AgentIdentity, MemberLaunchMode, MobId, MobRuntimeMode, SpawnMemberSpec};
use meerkat_mob_mcp::MobMcpState;
use serde::{Deserialize, Serialize};

use super::{MOB_ID, ORCHESTRATOR};
use crate::mobs::{HelperKey, HelperRegistry};

/// The conversation log's file name under `<workspace>/.rkat/tui/`.
pub const LOG_NAME: &str = "conversations.json";

/// How long a conversation's title may be.
const TITLE_CHARS: usize = 72;

/// How many leading messages [`Conversations::list`] reads per
/// conversation for its title (the count comes from the page's metadata).
pub const TITLE_PAGE: usize = 16;

/// How many conversations [`Conversations::list`] reads at once.
const LIST_READS: usize = 8;

/// One conversation the orchestrator ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The session.
    pub session_id: SessionId,
    /// When it was first seated (Unix seconds).
    pub started_at: u64,
    /// When it was last made current (Unix seconds).
    pub last_used: u64,
}

/// The conversation log (`conversations.json`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Log {
    /// Every conversation, in the order they were first seated.
    #[serde(default)]
    pub sessions: Vec<Entry>,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

impl Log {
    /// The log of the workspace at `context_root`.
    #[must_use]
    pub fn path(context_root: &Path) -> PathBuf {
        context_root.join(".rkat").join("tui").join(LOG_NAME)
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

    /// `session` is current now: recorded when new, its `last_used`
    /// moved otherwise.
    pub fn touch(&mut self, session: &SessionId) {
        let at = now();
        match self.sessions.iter_mut().find(|e| &e.session_id == session) {
            Some(entry) => entry.last_used = at,
            None => self.sessions.push(Entry {
                session_id: session.clone(),
                started_at: at,
                last_used: at,
            }),
        }
    }

    /// Whether `session` is one of the log's.
    #[must_use]
    pub fn has(&self, session: &SessionId) -> bool {
        self.sessions.iter().any(|e| &e.session_id == session)
    }

    /// The entries, last used first.
    #[must_use]
    pub fn newest_first(&self) -> Vec<Entry> {
        let mut entries = self.sessions.clone();
        entries.sort_by_key(|e| std::cmp::Reverse((e.last_used, e.started_at)));
        entries
    }
}

/// One row of `rkat/conversations`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listed {
    /// The session.
    pub session_id: SessionId,
    /// When it was last current (Unix seconds).
    pub last_used: u64,
    /// How many messages its history holds (`None`: unreadable).
    pub messages: Option<usize>,
    /// Its first user message, cut short.
    pub title: Option<String>,
    /// Whether the orchestrator runs as it now.
    pub current: bool,
}

/// Why a switch was refused or failed.
#[derive(Debug, thiserror::Error)]
pub enum SwitchError {
    /// `/resume` named a session this workspace's orchestrator never ran as.
    #[error("session {0} is not one of this workspace's conversations")]
    Unknown(SessionId),
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
    #[error("the orchestrator has no session after the switch")]
    NoSession,
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

/// The orchestrator's conversations.
pub struct Conversations {
    state: Arc<MobMcpState>,
    registry: Arc<HelperRegistry>,
    log_file: PathBuf,
    log: parking_lot::Mutex<Log>,
    /// One switch at a time.
    switching: tokio::sync::Mutex<()>,
    /// The start-up model and binding (a fresh conversation's, until the
    /// client switches it).
    model: String,
    auth_binding: Option<meerkat_core::AuthBindingRef>,
}

impl std::fmt::Debug for Conversations {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Conversations")
            .field("log_file", &self.log_file)
            .finish_non_exhaustive()
    }
}

impl Conversations {
    /// The conversations of the workspace at `context_root`, the current
    /// one being `current` (recorded in the log).
    #[must_use]
    pub fn open(
        state: Arc<MobMcpState>,
        registry: Arc<HelperRegistry>,
        context_root: &Path,
        current: &SessionId,
        model: String,
        auth_binding: Option<meerkat_core::AuthBindingRef>,
    ) -> Self {
        let log_file = Log::path(context_root);
        let mut log = Log::load(&log_file);
        log.touch(current);
        if let Err(err) = log.save(&log_file) {
            tracing::warn!(error = %err, "cannot record the conversation");
        }
        Self {
            state,
            registry,
            log_file,
            log: parking_lot::Mutex::new(log),
            switching: tokio::sync::Mutex::new(()),
            model,
            auth_binding,
        }
    }

    fn key() -> HelperKey {
        HelperKey::new(MobId::from(MOB_ID), AgentIdentity::from(ORCHESTRATOR))
    }

    /// The session the orchestrator runs as now.
    #[must_use]
    pub fn current(&self) -> Option<SessionId> {
        self.registry.root_session(&Self::key())
    }

    /// The conversations, last used first, with their size and first
    /// message: each conversation's first [`TITLE_PAGE`] messages are read
    /// (the size is the page's message count), [`LIST_READS`] at a time.
    pub async fn list(&self) -> Vec<Listed> {
        use futures::StreamExt as _;
        let entries = self.log.lock().newest_first();
        let current = self.current();
        let service = self.state.session_service();
        futures::stream::iter(entries)
            .map(|entry| {
                let service = Arc::clone(&service);
                let current = current.as_ref() == Some(&entry.session_id);
                async move {
                    let page = service
                        .read_history(
                            &entry.session_id,
                            SessionHistoryQuery {
                                offset: 0,
                                limit: Some(TITLE_PAGE),
                            },
                        )
                        .await
                        .ok();
                    listed_row(entry, page.as_ref(), current)
                }
            })
            .buffered(LIST_READS)
            .collect()
            .await
    }

    /// `session`'s committed messages.
    ///
    /// # Errors
    ///
    /// The engine's error reading the history.
    pub async fn messages(&self, session: &SessionId) -> Result<Vec<Message>, String> {
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
            .map_err(|err| err.to_string())
    }

    /// A conversation's total token usage, as the engine keeps it with the
    /// session (`SessionService::read`'s billing; the wire's
    /// `events/snapshot` and `session/read` carry no usage).
    ///
    /// # Errors
    ///
    /// The engine's refusal, as text.
    pub async fn usage(&self, session: &SessionId) -> Result<meerkat_core::Usage, String> {
        self.state
            .session_service()
            .read(session)
            .await
            .map(|view| view.billing.usage)
            .map_err(|err| err.to_string())
    }

    /// Make `target` the orchestrator's conversation: stop the current
    /// one's helpers, retire the member and spawn it again on `target`.
    /// When the spawn fails the member is spawned back on the session it
    /// had. Returns the session it runs as now.
    ///
    /// # Errors
    ///
    /// [`SwitchError`].
    pub async fn switch(&self, target: Target) -> Result<SessionId, SwitchError> {
        let _one = self.switching.lock().await;
        let previous = self.current();
        if let Target::Resume(session) = &target {
            if previous.as_ref() == Some(session) {
                return Err(SwitchError::Current);
            }
            if !self.log.lock().has(session) {
                return Err(SwitchError::Unknown(session.clone()));
            }
        }
        let mob = MobId::from(MOB_ID);
        let identity = AgentIdentity::from(ORCHESTRATOR);
        // Another client may be running a turn: its own guard is in
        // another process. The engine's run phase decides.
        let status = self
            .state
            .mob_member_status(&mob, &identity)
            .await
            .map_err(|err| SwitchError::mob("reading the orchestrator", err))?;
        if run_open(&status) {
            return Err(SwitchError::Running);
        }
        if let Some(previous) = &previous {
            crate::mobs::cancel_helpers(&self.state, &self.registry, previous).await;
        }
        self.state
            .mob_retire(&mob, identity.clone())
            .await
            .map_err(|err| SwitchError::mob("retiring the orchestrator", err))?;
        let spawned = self.spawn(&mob, &target).await;
        if let Err(err) = spawned {
            tracing::warn!(error = %err, "the switch failed; seating the previous conversation again");
            if let Some(previous) = &previous
                && let Err(again) = self.spawn(&mob, &Target::Resume(previous.clone())).await
            {
                tracing::error!(error = %again, "the previous conversation could not be seated again");
            }
            return Err(err);
        }
        let session = self
            .state
            .mob_member_status(&mob, &identity)
            .await
            .map_err(|err| SwitchError::mob("reading the orchestrator", err))?
            .current_session_id
            .ok_or(SwitchError::NoSession)?;
        let (model, binding) = match self.registry.model_of(&session) {
            Some(model) => (model, self.registry.auth_binding_of(&session)),
            None => (self.model.clone(), self.auth_binding.clone()),
        };
        self.registry
            .register_root(Self::key(), &session, &model, binding.as_ref());
        {
            let mut log = self.log.lock();
            log.touch(&session);
            if let Err(err) = log.save(&self.log_file) {
                tracing::warn!(error = %err, "cannot record the conversation");
            }
        }
        tracing::info!(%session, ?previous, "the orchestrator switched conversations");
        Ok(session)
    }

    async fn spawn(&self, mob: &MobId, target: &Target) -> Result<(), SwitchError> {
        let mut spec = SpawnMemberSpec::host_root(ORCHESTRATOR, ORCHESTRATOR);
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
            .map_err(|err| SwitchError::mob("seating the orchestrator", err))
    }
}

/// The `rkat/conversations` row of `entry`, from the first page of its
/// history (`None`: unreadable).
#[must_use]
pub fn listed_row(
    entry: Entry,
    page: Option<&meerkat_core::SessionHistoryPage>,
    current: bool,
) -> Listed {
    Listed {
        current,
        title: page.and_then(|page| title(&page.messages)),
        messages: page.map(|page| page.message_count),
        session_id: entry.session_id,
        last_used: entry.last_used,
    }
}

/// Whether the member's snapshot says its run is open.
#[must_use]
pub fn run_open(status: &meerkat_mob::MobMemberSnapshot) -> bool {
    status
        .progress
        .as_ref()
        .is_some_and(|progress| progress.run_state == meerkat_mob::MemberRunState::RunOpen)
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
    use super::*;

    #[test]
    fn the_log_records_each_session_once_and_lists_the_last_used_first() {
        let dir = tempfile::tempdir().unwrap();
        let file = Log::path(dir.path());
        let (a, b) = (SessionId::new(), SessionId::new());
        let mut log = Log::load(&file);
        assert!(log.sessions.is_empty());
        log.touch(&a);
        log.touch(&b);
        log.sessions[1].last_used = 1;
        log.touch(&a);
        log.save(&file).unwrap();
        let back = Log::load(&file);
        assert_eq!(back.sessions.len(), 2);
        assert!(back.has(&a) && back.has(&b));
        assert_eq!(back.newest_first()[0].session_id, a);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::write(&file, "garbage").unwrap();
        assert!(Log::load(&file).sessions.is_empty());
    }

    #[test]
    fn a_listed_row_counts_from_the_page_metadata_and_titles_from_its_messages() {
        let user = |text: &str| Message::User(meerkat_core::UserMessage::text(text.to_string()));
        let session = SessionId::new();
        let entry = Entry {
            session_id: session.clone(),
            started_at: 1,
            last_used: 2,
        };
        // A long history: only its first page was read.
        let page = meerkat_core::SessionHistoryPage {
            session_id: session,
            message_count: 500,
            offset: 0,
            limit: Some(TITLE_PAGE),
            has_more: true,
            messages: vec![user("fix the bug")],
        };
        let row = listed_row(entry.clone(), Some(&page), true);
        assert_eq!(row.messages, Some(500));
        assert_eq!(row.title.as_deref(), Some("fix the bug"));
        assert!(row.current);
        assert_eq!(row.last_used, 2);
        let unreadable = listed_row(entry, None, false);
        assert_eq!((unreadable.messages, unreadable.title), (None, None));
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
