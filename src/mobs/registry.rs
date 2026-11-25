//! Who the helpers work for, which of them are running, and what the UI
//! hears about them.
//!
//! The registry is rkat-tui's own bookkeeping next to the mob runtime (which
//! owns the helpers): every session rkat-tui opens registers its model and
//! auth binding; every helper spawn is tagged with the session it works for
//! (its *owner*: the session whose user's words its calls are judged
//! against), its path of helper labels (nesting) and the model it runs on.
//!
//! A helper's owner is resolved from typed facts only, never guessed
//! ([`HelperRegistry::owner_for_spawn`]): the member that spawned it (its
//! owner is that member's), or else the bridge session that created its mob
//! (recorded by the mob host's create hook), which is an agent's session or a
//! helper's own session. A spawn whose owner does not resolve to a session
//! open in this daemon is refused.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use meerkat_core::{AgentEvent, AuthBindingRef, ScopedAgentEvent, SessionId, StreamScopeFrame};
use meerkat_mob::{AgentIdentity, MobId};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};

use crate::mobs::bindings::{BindingsError, HelperBindings};

#[cfg(any(test, feature = "test-support"))]
mod probe;

/// What a helper is and whom it works for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelperTag {
    /// The agent's session the helper works for (directly, or through the
    /// helpers above it).
    pub owner: SessionId,
    /// Helper labels from the outermost helper to this one (its depth is the
    /// length).
    pub path: Vec<String>,
    /// The model the helper runs on.
    pub model: String,
    /// How it was started, which decides what its parent is told of it
    /// ([`super::endings`]). Absent in the bindings an earlier build wrote:
    /// [`HelperKind::Other`].
    #[serde(default, skip_serializing_if = "HelperKind::is_other")]
    pub kind: HelperKind,
}

/// What a helper starts from (`assign`'s `context`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HelperContext {
    /// The task text alone.
    Fresh,
    /// A copy of its parent's committed conversation (the engine's fork).
    Fork,
}

impl HelperContext {
    /// Its word, as the tool, the panel and `/helpers` say it.
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            Self::Fresh => "fresh",
            Self::Fork => "fork",
        }
    }
}

/// How long a helper stays (`assign`'s `keep`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HelperLife {
    /// Retired once its task's run ended and its answer reached its parent.
    Single,
    /// Seated and idle after each run until its parent retires it.
    Kept,
}

impl HelperLife {
    /// Its word, as the tool, the panel and `/helpers` say it.
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            Self::Single => "single-use",
            Self::Kept => "kept",
        }
    }
}

/// How a helper was started, as far as its parent's notes go
/// ([`super::endings`]): the two choices of `assign`
/// (`crate::daemon::assign`), or anything else. Stored in
/// `tui_helpers.json` as one word ([`StoredKind`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "StoredKind", into = "StoredKind")]
pub enum HelperKind {
    /// Started with `assign` (or, by an earlier build, with `spawn_member`,
    /// `mob_spawn_member` or `fork_off`). Single-use: its parent is told
    /// when it finishes, with its answer, and it is retired. Kept: its
    /// parent is told each time it goes idle. A fork's first run is told
    /// by the engine ("Background fork_off job … finished").
    Started(HelperContext, HelperLife),
    /// Anything else (a `delegate` helper, a council's participant, a
    /// member an earlier build bound without a kind).
    #[default]
    Other,
}

impl HelperKind {
    fn is_other(&self) -> bool {
        *self == Self::Other
    }

    /// Its life (`None`: not started by `assign`).
    #[must_use]
    pub fn life(self) -> Option<HelperLife> {
        match self {
            Self::Started(_, life) => Some(life),
            Self::Other => None,
        }
    }

    /// Whether it is a fork.
    #[must_use]
    pub fn is_fork(self) -> bool {
        matches!(self, Self::Started(HelperContext::Fork, _))
    }

    /// Its two words (`fresh · single-use`; `None`: not started by `assign`).
    #[must_use]
    pub fn text(self) -> Option<String> {
        match self {
            Self::Started(context, life) => Some(format!("{} · {}", context.word(), life.word())),
            Self::Other => None,
        }
    }
}

/// [`HelperKind`] as `tui_helpers.json` stores it. The words of earlier
/// builds still open and mean what they meant: `assigned` (`assign`) is a
/// fresh single-use helper, `member` (`spawn_member`, `mob_spawn_member`) a
/// fresh kept one, `fork` (`fork_off`, kept seated after its outcome) a
/// kept fork.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredKind {
    #[serde(alias = "assigned")]
    FreshSingle,
    #[serde(alias = "member")]
    FreshKept,
    ForkSingle,
    #[serde(alias = "fork")]
    ForkKept,
    Other,
}

impl From<StoredKind> for HelperKind {
    fn from(stored: StoredKind) -> Self {
        use HelperContext::{Fork, Fresh};
        use HelperLife::{Kept, Single};
        match stored {
            StoredKind::FreshSingle => Self::Started(Fresh, Single),
            StoredKind::FreshKept => Self::Started(Fresh, Kept),
            StoredKind::ForkSingle => Self::Started(Fork, Single),
            StoredKind::ForkKept => Self::Started(Fork, Kept),
            StoredKind::Other => Self::Other,
        }
    }
}

impl From<HelperKind> for StoredKind {
    fn from(kind: HelperKind) -> Self {
        use HelperContext::{Fork, Fresh};
        use HelperLife::{Kept, Single};
        match kind {
            HelperKind::Started(Fresh, Single) => Self::FreshSingle,
            HelperKind::Started(Fresh, Kept) => Self::FreshKept,
            HelperKind::Started(Fork, Single) => Self::ForkSingle,
            HelperKind::Started(Fork, Kept) => Self::ForkKept,
            HelperKind::Other => Self::Other,
        }
    }
}

impl HelperTag {
    /// The helper's own label (its member identity).
    #[must_use]
    pub fn label(&self) -> &str {
        self.path.last().map_or("", String::as_str)
    }

    /// Nesting depth (1 for a helper of the session's own agent).
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn depth(&self) -> usize {
        self.path.len()
    }

    /// The labels of the helpers above this one.
    #[must_use]
    pub fn parent_path(&self) -> &[String] {
        self.path.split_last().map_or(&[], |(_, parents)| parents)
    }

    /// `event` scoped to this helper: the owner's primary frame, then one
    /// mob-member frame per helper on the path.
    ///
    /// A helper runs no flow, but upstream's
    /// [`StreamScopeFrame::MobMember`] requires a `flow_run_id` string: it
    /// is left empty (upstream ask: make it `Option<String>`).
    #[must_use]
    pub fn scope(&self, event: AgentEvent) -> ScopedAgentEvent {
        let mut frames = Vec::with_capacity(self.path.len() + 1);
        frames.push(StreamScopeFrame::Primary {
            session_id: self.owner.to_string(),
        });
        frames.extend(self.path.iter().map(|label| StreamScopeFrame::MobMember {
            flow_run_id: String::new(),
            agent_identity: label.clone(),
            agent_runtime_id: None,
            fence_token: None,
            generation: None,
        }));
        ScopedAgentEvent::new(frames, event)
    }
}

/// A running helper, as the status bar lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelperStatus {
    /// Member identity.
    pub label: String,
    /// The model it runs on.
    pub model: String,
    /// Nesting depth.
    pub depth: usize,
}

/// A helper's key: its mob and its member identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct HelperKey {
    /// The mob the helper is a member of.
    pub mob: MobId,
    /// Its member identity in that mob.
    pub member: AgentIdentity,
}

impl HelperKey {
    /// The key of `member` in `mob`.
    #[must_use]
    pub fn new(mob: MobId, member: AgentIdentity) -> Self {
        Self { mob, member }
    }
}

impl std::fmt::Display for HelperKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.mob, self.member)
    }
}

/// A helper, or every helper of a mob, that left for good
/// ([`Outlets::gone`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gone {
    /// The helper retired (finished, stopped or retired by a tool).
    Member(HelperKey),
    /// The mob was destroyed.
    Mob(MobId),
}

/// The session a new helper works for, and what it inherits from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owner {
    /// The agent's session.
    pub session: SessionId,
    /// Labels of the helpers between the session and the new helper.
    pub path: Vec<String>,
    /// The model the new helper inherits (the session's, or its parent
    /// helper's).
    pub model: String,
    /// The session's explicit auth binding (a configured endpoint's).
    pub auth_binding: Option<AuthBindingRef>,
}

/// Why a helper's owner does not resolve: the spawn (or rebuild) is
/// refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OwnerError {
    /// The spawning member is no helper this daemon bound.
    #[error("the spawning member {0} works for no session of this daemon")]
    UnknownSpawner(HelperKey),
    /// The mob was not created by a session of this daemon.
    #[error("mob {0} was created by no session of this daemon")]
    UnknownMob(MobId),
    /// The mob's creator is neither an agent's session nor a helper.
    #[error("session {0} is neither a session of this daemon nor a helper")]
    UnknownSession(SessionId),
    /// The owner session is not open in this daemon.
    #[error("session {0} is not open")]
    OwnerNotOpen(SessionId),
    /// A restored member has no persisted owner.
    #[error("helper {0} has no recorded owner")]
    UnknownMember(HelperKey),
}

/// How many destroyed mobs a slow listener may fall behind by.
const DESTROYED_QUEUE: usize = 64;

/// A agent's session's model, as helpers inherit it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionModel {
    model: String,
    auth_binding: Option<AuthBindingRef>,
}

/// The longest first line of a helper's task its binding keeps
/// ([`HelperRegistry::note_task`]).
pub const TASK_LINE: usize = 80;

/// What [`Outlets::seated`] is told: the owner session whose
/// seated helpers changed.
pub struct SeatedHook(pub Box<dyn Fn(&SessionId) + Send + Sync>);

impl std::fmt::Debug for SeatedHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SeatedHook")
    }
}

/// Where a registry reports what happens to its helpers, fixed when it is
/// built ([`HelperRegistry::with_outlets`]): each one left out is not
/// reported (a test's registry).
#[derive(Debug, Default)]
pub struct Outlets {
    /// Each helper and mob that left for good (the daemon's connections
    /// of helpers in different mobs follow them, `crate::daemon::links`).
    pub gone: Option<tokio::sync::mpsc::UnboundedSender<Gone>>,
    /// The owner session whose seated helpers changed, after each change
    /// (the daemon pushes the list to the owner's clients,
    /// [`crate::daemon::protocol::SEATED`]).
    pub seated: Option<SeatedHook>,
    /// Each helper's ending to tell its parent ([`super::endings`]).
    pub ended: Option<tokio::sync::mpsc::UnboundedSender<super::endings::Ended>>,
    /// Each helper's run at its limit, for its checkpoint; left out, such
    /// a run is a failure.
    pub checkpoints: Option<tokio::sync::mpsc::UnboundedSender<super::endings::LimitReached>>,
}

/// How long a helper whose `assign` call returned keeps its slot while its
/// parent's mob does not list it yet (a fork whose spawn is still
/// building): well past the follower's wait for a fork's seat.
const LAUNCH_GRACE: std::time::Duration = std::time::Duration::from_mins(1);

/// One `assign` call's hold on a helper ([`HelperRegistry::reserve`]).
#[derive(Debug)]
struct Pending {
    /// The owner session of the parent's tree.
    owner: SessionId,
    /// The parent's own path in it (empty: the agent).
    path: Vec<String>,
    /// The kind the call asked for, until the helper's seat takes it.
    kind: Option<HelperKind>,
    /// When the call returned with the helper started (`None`: the call is
    /// under way).
    launched: Option<std::time::Instant>,
    /// The helper retired, or its mob or conversation went, while the call
    /// was under way: nothing is held once it returns.
    gone: bool,
}

/// The helpers held for `owner`'s parent at `path` beside its `seated`
/// ones: calls under way, and started helpers not seated yet. A started
/// one seen seated, or past [`LAUNCH_GRACE`], is let go.
fn starting_beside(
    pending: &mut HashMap<HelperKey, Pending>,
    owner: &SessionId,
    path: &[String],
    seated: &[HelperKey],
) -> Vec<HelperKey> {
    pending.retain(|key, held| {
        held.launched
            .is_none_or(|at| !(seated.contains(key) || at.elapsed() >= LAUNCH_GRACE))
    });
    let mut starting: Vec<HelperKey> = pending
        .iter()
        .filter(|(key, held)| &held.owner == owner && held.path == path && !seated.contains(key))
        .map(|(key, _)| key.clone())
        .collect();
    starting.sort();
    starting
}

/// Why [`HelperRegistry::reserve`] held nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// Another `assign` call is seating a helper of that name.
    NameInUse,
    /// The parent is at its limit: its seated helpers and `starting` (the
    /// ones being seated) make `limit` or more.
    Full {
        /// The helpers being seated for it, beside the seated ones.
        starting: Vec<HelperKey>,
        /// The limit in force.
        limit: usize,
    },
}

/// An `assign` call's hold on its helper's name and a slot under its
/// parent's limit ([`HelperRegistry::reserve`]). Dropped, it lets both go
/// (the spawn failed, or the call was cancelled mid-spawn), and so the kind
/// the call asked for, which no later seat of that name then takes; once
/// [`Self::launched`], the slot stays until the helper is seen seated (or
/// leaves).
#[derive(Debug)]
#[must_use]
pub struct Reservation<'a> {
    registry: &'a HelperRegistry,
    key: HelperKey,
    launched: bool,
}

impl Reservation<'_> {
    /// The helper was started: its slot stays until its mob lists it (it
    /// counts from then as a seated helper) or it leaves.
    pub fn launched(mut self) {
        self.launched = true;
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        let mut pending = self.registry.pending.lock();
        let keep = self.launched && pending.get(&self.key).is_some_and(|held| !held.gone);
        if keep {
            if let Some(held) = pending.get_mut(&self.key) {
                held.kind = None;
                held.launched = Some(std::time::Instant::now());
            }
        } else {
            pending.remove(&self.key);
        }
    }
}

/// rkat-tui's helper bookkeeping, shared by the spawn customizer, the
/// event followers, the session handles and the runner.
#[derive(Debug)]
pub struct HelperRegistry {
    /// agent's sessions open in this process.
    sessions: RwLock<HashMap<SessionId, SessionModel>>,
    /// Host-root members that are owners themselves (the mob daemon's
    /// agent), and the session each runs as: a member they spawn
    /// works for that session.
    roots: RwLock<HashMap<HelperKey, SessionId>>,
    /// Whom each helper and each agent-created mob works for.
    bindings: HelperBindings,
    /// Running helpers, in spawn order.
    live: Mutex<IndexMap<HelperKey, HelperTag>>,
    /// Each destroyed mob, as the host's after-destroy hook reports it
    /// (again before each retry of its storage cleanup): what still
    /// observes the mob lets go ([`Self::destroyed_mobs`]).
    destroyed: tokio::sync::broadcast::Sender<MobId>,
    /// The agents' private scratch directories
    /// ([`crate::approval::scratch`]): an agent's helpers share its own.
    scratch: crate::approval::scratch::ScratchDirs,
    /// How each helper ends, and when its parent is told.
    endings: std::sync::Arc<super::endings::HelperEndings>,
    /// Where each helper and mob that left for good is reported
    /// ([`Outlets::gone`]).
    gone: Option<tokio::sync::mpsc::UnboundedSender<Gone>>,
    /// The helpers `assign` calls are seating, by key: each holds its name
    /// and a slot under its parent's limit, and the kind the call asked for
    /// until its seat takes it ([`Self::reserve`], [`Self::take_intent`]):
    /// a fork's seat carries no label of the call (the engine's `fork_off`
    /// seats it).
    pending: Mutex<HashMap<HelperKey, Pending>>,
    /// How many helpers each agent and each helper may hold from `assign`
    /// (`helpers.max_live`).
    max_live: std::sync::atomic::AtomicUsize,
    /// Told the owner session whose seated helpers changed (one was
    /// seated or left), after the change ([`Outlets::seated`]).
    seated: Option<SeatedHook>,
    /// Whether each followed helper runs, and since when (its last run's
    /// start or end as the follower saw it).
    activity: Mutex<HashMap<HelperKey, (bool, std::time::Instant)>>,
    /// How many helpers each parent (an owner session and the parent's
    /// path) had retire since the daemon started.
    ended: Mutex<HashMap<(SessionId, Vec<String>), usize>>,
    /// Tests: their switches and counts ([`probe::RegistryProbe`]).
    #[cfg(any(test, feature = "test-support"))]
    probe: probe::RegistryProbe,
}

impl HelperRegistry {
    /// A registry whose bindings persist under `realm_root` (an
    /// unreadable bindings file is logged and replaced).
    #[must_use]
    pub fn persistent(realm_root: &Path) -> Self {
        let mut registry = Self::with_bindings(HelperBindings::open_or_empty(realm_root));
        registry.scratch =
            crate::approval::scratch::ScratchDirs::in_temp(realm_root.display().to_string());
        registry
    }

    fn with_bindings(bindings: HelperBindings) -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            roots: RwLock::new(HashMap::new()),
            bindings,
            live: Mutex::new(IndexMap::new()),
            destroyed: tokio::sync::broadcast::channel(DESTROYED_QUEUE).0,
            scratch: crate::approval::scratch::ScratchDirs::off(),
            endings: std::sync::Arc::default(),
            gone: None,
            pending: Mutex::new(HashMap::new()),
            max_live: std::sync::atomic::AtomicUsize::new(
                usize::try_from(crate::config::registry::DEFAULT_HELPERS_LIVE).unwrap_or(8),
            ),
            seated: None,
            activity: Mutex::new(HashMap::new()),
            ended: Mutex::new(HashMap::new()),
            #[cfg(any(test, feature = "test-support"))]
            probe: probe::RegistryProbe::default(),
        }
    }

    /// This registry reporting to `outlets` (before it is shared: they are
    /// fixed from then on).
    #[must_use]
    pub fn with_outlets(mut self, outlets: Outlets) -> Self {
        let Outlets {
            gone,
            seated,
            ended,
            checkpoints,
        } = outlets;
        self.gone = gone;
        self.seated = seated;
        self.endings = std::sync::Arc::new(super::endings::HelperEndings::delivering(
            ended,
            checkpoints,
        ));
        self
    }

    /// How each helper ends ([`super::endings`]).
    #[must_use]
    pub fn endings(&self) -> &std::sync::Arc<super::endings::HelperEndings> {
        &self.endings
    }

    /// How many helpers each agent and each helper may hold from `assign`
    /// now (`helpers.max_live`).
    #[must_use]
    pub fn max_live(&self) -> usize {
        self.max_live.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Hold the `assign` calls from now on to `limit` helpers each (at
    /// least one). Nothing seated is retired.
    pub fn set_max_live(&self, limit: usize) {
        self.max_live
            .store(limit.max(1), std::sync::atomic::Ordering::SeqCst);
    }

    /// An `assign` call of the parent at `path` of `owner`'s tree, which
    /// holds the helpers `seated` now (its own, seated in its mob), is
    /// about to seat `key` as `kind`: the name and a slot under the
    /// parent's limit are held, checked and taken in one step, so two
    /// calls of one model step cannot both take the last slot or one name.
    /// The slot is released when the [`Reservation`] drops (the spawn
    /// failed, or the call was cancelled), unless it was
    /// [`Reservation::launched`].
    ///
    /// # Errors
    ///
    /// [`Refused`]: another call seats a helper of that name, or the
    /// parent is at its limit.
    pub fn reserve(
        &self,
        key: HelperKey,
        kind: HelperKind,
        owner: &SessionId,
        path: &[String],
        seated: &[HelperKey],
    ) -> Result<Reservation<'_>, Refused> {
        let limit = self.max_live();
        let mut pending = self.pending.lock();
        if pending.contains_key(&key) {
            return Err(Refused::NameInUse);
        }
        let starting = starting_beside(&mut pending, owner, path, seated);
        if seated.len() + starting.len() >= limit {
            return Err(Refused::Full { starting, limit });
        }
        pending.insert(
            key.clone(),
            Pending {
                owner: owner.clone(),
                path: path.to_vec(),
                kind: Some(kind),
                launched: None,
                gone: false,
            },
        );
        Ok(Reservation {
            registry: self,
            key,
            launched: false,
        })
    }

    /// The helpers `assign` calls of the parent at `path` of `owner`'s tree
    /// are seating, or started and not yet seen among its `seated` ones:
    /// what counts against its limit beside them.
    #[must_use]
    pub fn starting(
        &self,
        owner: &SessionId,
        path: &[String],
        seated: &[HelperKey],
    ) -> Vec<HelperKey> {
        starting_beside(&mut self.pending.lock(), owner, path, seated)
    }

    /// The kind an `assign` call asked for `key`, taken (`None`: no call
    /// under way asked; a cancelled or failed call's is gone with its
    /// [`Reservation`]).
    #[must_use]
    pub fn take_intent(&self, key: &HelperKey) -> Option<HelperKind> {
        self.pending
            .lock()
            .get_mut(key)
            .and_then(|pending| pending.kind.take())
    }

    /// `assign`'s hold on `key` goes with the helper: a started one's at
    /// once, a call's under way when it returns.
    fn release(&self, gone: impl Fn(&HelperKey, &Pending) -> bool) {
        self.pending.lock().retain(|key, pending| {
            if !gone(key, pending) {
                return true;
            }
            pending.gone = true;
            pending.launched.is_none()
        });
    }

    /// The session that started the helper `key` (its parent): the session
    /// of a `delegate` helper's implicit mob, else the session that
    /// created its mob.
    #[must_use]
    pub fn parent_session(&self, key: &HelperKey) -> Option<SessionId> {
        key.mob
            .as_str()
            .strip_prefix("implicit-")
            .and_then(|session| SessionId::parse(session).ok())
            .or_else(|| self.bindings.mob_owner(&key.mob))
    }

    /// The running helper that runs as `session`, with its tag.
    #[must_use]
    pub fn live_by_session(&self, session: &SessionId) -> Option<(HelperKey, HelperTag)> {
        let (key, _) = self.bindings.member_by_session(session)?;
        let tag = self.live.lock().get(&key).cloned()?;
        Some((key, tag))
    }

    /// The running helper working for `owner` whose labels are `path`
    /// (outermost first).
    #[must_use]
    pub fn live_by_path(
        &self,
        owner: &SessionId,
        path: &[String],
    ) -> Option<(HelperKey, HelperTag)> {
        self.live
            .lock()
            .iter()
            .find(|(_, tag)| &tag.owner == owner && tag.path == path)
            .map(|(key, tag)| (key.clone(), tag.clone()))
    }

    /// The session the helper `key` runs as, once known.
    #[must_use]
    pub fn member_session(&self, key: &HelperKey) -> Option<SessionId> {
        self.bindings
            .member(key)
            .and_then(|binding| binding.session)
    }

    /// The helper bound as running as `session`, live or not.
    #[must_use]
    pub fn member_key_of_session(&self, session: &SessionId) -> Option<HelperKey> {
        self.bindings.member_by_session(session).map(|(key, _)| key)
    }

    /// The tag of the running helper `key`.
    #[must_use]
    pub fn live_tag(&self, key: &HelperKey) -> Option<HelperTag> {
        self.live.lock().get(key).cloned()
    }

    /// The helpers bound in the bindings file now, each with its tag (at
    /// a start: the ones the last daemon left).
    #[must_use]
    pub fn bound_members(&self) -> Vec<(HelperKey, HelperTag)> {
        self.bindings.members()
    }

    /// The helpers bound below `key`: its own helpers, theirs, and so on
    /// (the same owner, a longer path that begins with its own), deepest
    /// first. Empty when `key` is not bound.
    #[must_use]
    pub fn bound_below(&self, key: &HelperKey) -> Vec<(HelperKey, HelperTag)> {
        let bound = self.bound_members();
        let Some(tag) = bound
            .iter()
            .find_map(|(bound, tag)| (bound == key).then(|| tag.clone()))
        else {
            return Vec::new();
        };
        let mut below: Vec<(HelperKey, HelperTag)> = bound
            .into_iter()
            .filter(|(other, other_tag)| {
                other != key
                    && other_tag.owner == tag.owner
                    && other_tag.path.len() > tag.path.len()
                    && other_tag.path.starts_with(&tag.path)
            })
            .collect();
        below.sort_by_key(|(_, tag)| std::cmp::Reverse(tag.path.len()));
        below
    }

    /// The scratch directory of the agent `member`, made private when it
    /// is not there yet; `None` when the shell could not write it
    /// (`writable` says, from the confinement plan) or it cannot be made (a
    /// warning is logged).
    #[must_use]
    pub fn scratch_for_agent(
        &self,
        member: &str,
        writable: impl Fn(&Path) -> bool,
    ) -> Option<PathBuf> {
        if !self
            .scratch
            .path_for(member)
            .is_some_and(|path| writable(&path))
        {
            return None;
        }
        match self.scratch.ensure(member) {
            Ok(dir) => Some(dir),
            Err(crate::approval::scratch::ScratchError::NoBase) => None,
            Err(err) => {
                tracing::warn!(agent = member, error = %err, "the agent has no scratch directory");
                None
            }
        }
    }

    /// The scratch directory of the agent running as `session` (its
    /// helpers' calls are judged as that session's), when it has one.
    #[must_use]
    pub fn scratch_of_session(&self, session: &SessionId) -> Option<PathBuf> {
        let key = self.root_of_session(session)?;
        self.scratch.existing(key.member.as_str())
    }

    /// Remove the scratch directory of the agent `member` (it retired).
    pub fn remove_scratch(&self, member: &str) {
        self.scratch.remove(member);
    }

    /// The persisted bindings (their retry and their outlet, the daemon's;
    /// tests).
    #[must_use]
    pub fn bindings(&self) -> &HelperBindings {
        &self.bindings
    }

    /// `session` (re)opened or switched to `model` on `auth_binding`:
    /// its helpers inherit both.
    pub fn register_session(
        &self,
        session: &SessionId,
        model: &str,
        auth_binding: Option<&AuthBindingRef>,
    ) {
        self.sessions.write().insert(
            session.clone(),
            SessionModel {
                model: model.to_string(),
                auth_binding: auth_binding.cloned(),
            },
        );
    }

    /// The host-root member `key` runs as `session` on `model` through
    /// `auth_binding` and is an owner itself: helpers it spawns into its
    /// own mob, and helpers its session starts otherwise, work for
    /// `session` (see [`Self::owner_for_spawn`]).
    pub fn register_root(
        &self,
        key: HelperKey,
        session: &SessionId,
        model: &str,
        auth_binding: Option<&AuthBindingRef>,
    ) {
        self.register_session(session, model, auth_binding);
        self.roots.write().insert(key, session.clone());
    }

    /// The host-root member `key` left its mob (retired): it is no owner
    /// any more. Its session's bookkeeping goes with
    /// [`Self::forget_session`].
    pub fn unregister_root(&self, key: &HelperKey) {
        self.roots.write().remove(key);
    }

    /// The session the host-root member `key` runs as, when it is one.
    #[must_use]
    pub fn root_session(&self, key: &HelperKey) -> Option<SessionId> {
        self.roots.read().get(key).cloned()
    }

    /// The host-root member running as `session`, when one does.
    #[must_use]
    pub fn root_of_session(&self, session: &SessionId) -> Option<HelperKey> {
        self.roots
            .read()
            .iter()
            .find(|(_, running)| *running == session)
            .map(|(key, _)| key.clone())
    }

    /// `session` is no longer open here: drop it, its running helpers
    /// and its bindings from memory (the bindings file keeps them for a
    /// later resume). New spawns for it are refused until it registers
    /// again. Idempotent. Its helpers' mob members are not retired here
    /// ([`crate::mobs::cancel_helpers`] does that).
    pub fn forget_session(&self, session: &SessionId) {
        self.sessions.write().remove(session);
        let mut gone = Vec::new();
        self.live.lock().retain(|key, tag| {
            let stays = &tag.owner != session;
            if !stays {
                gone.push(key.clone());
            }
            stays
        });
        self.activity.lock().retain(|key, _| !gone.contains(key));
        self.release(|_, held| &held.owner == session);
        self.bindings.forget_owner(session);
    }

    /// `mob`'s helpers work for the session `owner` (the bridge session it
    /// was created for, or the one that convened its council,
    /// [`super::install_mob_hooks`]).
    ///
    /// # Errors
    ///
    /// [`BindingsError`] when the binding cannot be persisted.
    pub fn record_mob(&self, mob: &MobId, owner: &SessionId) -> Result<(), BindingsError> {
        self.bindings.set_mob_owner(mob, owner)
    }

    /// Whom a helper spawned into `mob` works for: the owner of
    /// `spawner` when a member of the mob spawned it, else the owner of
    /// the bridge session that created the mob.
    ///
    /// # Errors
    ///
    /// [`OwnerError`] when that does not resolve to a session open here.
    pub fn owner_for_spawn(
        &self,
        mob: &MobId,
        spawner: Option<&AgentIdentity>,
    ) -> Result<Owner, OwnerError> {
        if let Some(spawner) = spawner {
            let key = HelperKey::new(mob.clone(), spawner.clone());
            if let Some(session) = self.root_session(&key) {
                return self.owner_of_session(&session);
            }
            let binding = self
                .bindings
                .member(&key)
                .ok_or(OwnerError::UnknownSpawner(key))?;
            return self.inherit(&binding.tag);
        }
        let creator = self
            .bindings
            .mob_owner(mob)
            .ok_or_else(|| OwnerError::UnknownMob(mob.clone()))?;
        self.owner_of_session(&creator)
    }

    /// The owner of work a `session` starts: the agent's session itself, or
    /// the owner of the helper running as it.
    fn owner_of_session(&self, session: &SessionId) -> Result<Owner, OwnerError> {
        if let Some(model) = self.sessions.read().get(session) {
            return Ok(Owner {
                session: session.clone(),
                path: Vec::new(),
                model: model.model.clone(),
                auth_binding: model.auth_binding.clone(),
            });
        }
        let (_, binding) = self
            .bindings
            .member_by_session(session)
            .ok_or_else(|| OwnerError::UnknownSession(session.clone()))?;
        self.inherit(&binding.tag)
    }

    /// The owner a helper below the helper `parent` inherits.
    fn inherit(&self, parent: &HelperTag) -> Result<Owner, OwnerError> {
        let sessions = self.sessions.read();
        let model = sessions
            .get(&parent.owner)
            .ok_or_else(|| OwnerError::OwnerNotOpen(parent.owner.clone()))?;
        Ok(Owner {
            session: parent.owner.clone(),
            path: parent.path.clone(),
            model: parent.model.clone(),
            auth_binding: model.auth_binding.clone(),
        })
    }

    /// The persisted tag of a member being rebuilt (a restore after a
    /// restart, or a resume).
    ///
    /// # Errors
    ///
    /// [`OwnerError::UnknownMember`] when none was recorded.
    pub fn owner_for_resume(&self, key: &HelperKey) -> Result<HelperTag, OwnerError> {
        self.bindings
            .member(key)
            .map(|binding| binding.tag)
            .ok_or_else(|| OwnerError::UnknownMember(key.clone()))
    }

    /// The auth binding of `session`, when it is open here.
    #[must_use]
    pub fn auth_binding_of(&self, session: &SessionId) -> Option<AuthBindingRef> {
        self.sessions
            .read()
            .get(session)
            .and_then(|model| model.auth_binding.clone())
    }

    /// The model of `session`, when it is open here.
    #[must_use]
    pub fn model_of(&self, session: &SessionId) -> Option<String> {
        self.sessions
            .read()
            .get(session)
            .map(|model| model.model.clone())
    }

    /// The helper `key` is being spawned (or rebuilt) with `tag`: bind
    /// it durably, then list it as running.
    ///
    /// # Errors
    ///
    /// [`BindingsError`] when the binding cannot be persisted (the spawn
    /// is refused: a member whose owner would be lost on restart never
    /// runs).
    pub fn spawning(&self, key: HelperKey, tag: HelperTag) -> Result<(), BindingsError> {
        self.bindings.bind(&key, &tag)?;
        self.endings.spawned(&key, &tag);
        let owner = tag.owner.clone();
        self.live.lock().insert(key, tag);
        self.seated_changed(&owner);
        Ok(())
    }

    /// A run of the helper `key` started (`running`) or ended, now.
    pub fn ran(&self, key: &HelperKey, running: bool) {
        self.activity
            .lock()
            .insert(key.clone(), (running, std::time::Instant::now()));
    }

    /// Whether the helper `key` runs and since when, as its follower saw
    /// its runs (`None`: no run of it was seen since the daemon started).
    #[must_use]
    pub fn activity(&self, key: &HelperKey) -> Option<(bool, std::time::Instant)> {
        self.activity.lock().get(key).copied()
    }

    /// How many helpers of the parent at `parent_path` in `owner`'s tree
    /// retired since the daemon started.
    #[must_use]
    pub fn ended_count(&self, owner: &SessionId, parent_path: &[String]) -> usize {
        self.ended
            .lock()
            .get(&(owner.clone(), parent_path.to_vec()))
            .copied()
            .unwrap_or_default()
    }

    /// The helper `key` was given `task`: its first line, shortened to
    /// [`TASK_LINE`] characters, is kept with its binding.
    pub fn note_task(&self, key: &HelperKey, task: &str) {
        let line = task
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or_default();
        let line = crate::text::truncate_chars(line, TASK_LINE);
        if let Err(err) = self.bindings.set_task(key, &line) {
            tracing::warn!(helper = %key, error = %err, "cannot record the helper's task");
        }
    }

    /// The first line of the task the helper `key` was given, when kept.
    #[must_use]
    pub fn task_of(&self, key: &HelperKey) -> Option<String> {
        self.bindings.member(key).and_then(|binding| binding.task)
    }

    /// The helpers seated for `owner` may have changed (one was bound,
    /// joined its mob, or left): the hook is told.
    pub fn seated_changed(&self, owner: &SessionId) {
        if let Some(hook) = &self.seated {
            (hook.0)(owner);
        }
    }

    /// The helper `key` runs as `session`: helpers it starts inherit its
    /// owner.
    pub fn bind_session(&self, key: &HelperKey, tag: &HelperTag, session: &SessionId) {
        if let Err(err) = self.bindings.set_member_session(key, tag, session) {
            tracing::warn!(helper = %key, error = %err, "cannot record the helper's session");
        }
    }

    /// The helper `key` is not running here (its mob stopped, or it was
    /// never observable); its binding stays for a rebuild.
    pub fn finished(&self, key: &HelperKey) {
        let removed = self.live.lock().shift_remove(key);
        if let Some(tag) = removed {
            self.seated_changed(&tag.owner);
        }
    }

    /// The helper `key` is done and being retired: a start that finds it
    /// still seated retires it ([`HelperBindings::owe_retire`]).
    pub fn owe_retire(&self, key: &HelperKey) {
        if let Err(err) = self.bindings.owe_retire(key) {
            tracing::warn!(helper = %key, error = %err, "cannot record the helper's retire");
        }
    }

    /// Whether `key`'s retire was decided and is owed (it is done).
    #[must_use]
    pub fn retire_owed(&self, key: &HelperKey) -> bool {
        self.bindings
            .member(key)
            .is_some_and(|binding| binding.retire_owed)
    }

    /// The helper `key` left its mob for good: drop it everywhere.
    /// Idempotent.
    pub fn retired(&self, key: &HelperKey) {
        let removed = self.live.lock().shift_remove(key);
        let tag = removed.or_else(|| self.bindings.member(key).map(|binding| binding.tag));
        self.activity.lock().remove(key);
        self.release(|held, _| held == key);
        if let Some(tag) = &tag {
            *self
                .ended
                .lock()
                .entry((tag.owner.clone(), tag.parent_path().to_vec()))
                .or_default() += 1;
        }
        let owner = tag.map(|tag| tag.owner);
        if let Err(err) = self.bindings.remove_member(key) {
            tracing::warn!(helper = %key, error = %err, "cannot drop the helper's binding");
        }
        if let Some(owner) = owner {
            self.seated_changed(&owner);
        }
        self.report_gone(Gone::Member(key.clone()));
    }

    /// `mob` was destroyed: drop its helpers everywhere. Idempotent.
    pub fn mob_destroyed(&self, mob: &MobId) {
        let mut owners = Vec::new();
        self.live.lock().retain(|key, tag| {
            let stays = &key.mob != mob;
            if !stays && !owners.contains(&tag.owner) {
                owners.push(tag.owner.clone());
            }
            stays
        });
        self.activity.lock().retain(|key, _| &key.mob != mob);
        self.release(|key, _| &key.mob == mob);
        let _ = self.destroyed.send(mob.clone());
        if let Err(err) = self.bindings.remove_mob(mob) {
            tracing::warn!(mob = %mob, error = %err, "cannot drop the mob's bindings");
        }
        for owner in &owners {
            self.seated_changed(owner);
        }
        self.report_gone(Gone::Mob(mob.clone()));
    }

    fn report_gone(&self, gone: Gone) {
        if let Some(sink) = &self.gone {
            let _ = sink.send(gone);
        }
    }

    /// The mobs destroyed from now on ([`Self::mob_destroyed`]): a client
    /// connection's mob-wide streams on one are closed, since an open
    /// stream observes the mob's database and its storage cleanup refuses
    /// while anything does (`docs/evidence.md`, "`mob_destroy`").
    #[must_use]
    pub fn destroyed_mobs(&self) -> tokio::sync::broadcast::Receiver<MobId> {
        self.destroyed.subscribe()
    }

    /// The session the running helper `key` works for.
    #[must_use]
    pub fn owner_of(&self, key: &HelperKey) -> Option<SessionId> {
        self.live.lock().get(key).map(|tag| tag.owner.clone())
    }

    /// Running helpers that work for `owner`.
    #[must_use]
    pub fn live_for(&self, owner: &SessionId) -> Vec<HelperKey> {
        self.live
            .lock()
            .iter()
            .filter(|(_, tag)| &tag.owner == owner)
            .map(|(key, _)| key.clone())
            .collect()
    }

    /// A helper's events cannot be followed (clients follow it on its
    /// mob's own stream; the daemon reads its catalog at its next call).
    pub fn follow_failed(&self, tag: &HelperTag) {
        tracing::warn!(
            helper = tag.label(),
            "the daemon cannot follow a helper's events"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn binding() -> meerkat_core::AuthBindingRef {
        meerkat_core::AuthBindingRef {
            realm: meerkat_core::RealmId::parse(crate::endpoints::ENDPOINT_REALM).unwrap(),
            binding: meerkat_core::BindingId::parse("proxy").unwrap(),
            profile: None,
            origin: meerkat_core::BindingOrigin::Configured,
        }
    }

    #[test]
    fn a_host_root_member_owns_what_it_spawns() {
        use meerkat_mob::{AgentIdentity, MobId};

        let registry = HelperRegistry::in_memory();
        let mob = MobId::from("tui");
        let main = AgentIdentity::from("main");
        // Before it is registered, a spawn of `main` is an unknown spawner.
        assert!(matches!(
            registry.owner_for_spawn(&mob, Some(&main)),
            Err(OwnerError::UnknownSpawner(_))
        ));
        let session = SessionId::new();
        let key = HelperKey::new(mob.clone(), main.clone());
        registry.register_root(key.clone(), &session, "m-root", Some(&binding()));
        assert_eq!(registry.root_session(&key), Some(session.clone()));
        let owner = registry.owner_for_spawn(&mob, Some(&main)).unwrap();
        assert_eq!(owner.session, session);
        assert_eq!(owner.model, "m-root");
        assert_eq!(owner.auth_binding, Some(binding()));
        assert!(owner.path.is_empty());
        // `delegate` from its session: the implicit mob its session created.
        let implicit = MobId::from(format!("implicit-{session}"));
        registry.record_mob(&implicit, &session).unwrap();
        assert_eq!(
            registry.owner_for_spawn(&implicit, None).unwrap().session,
            session
        );
    }

    /// A council's mob is recorded for the session that convened it (the
    /// engine names it in the create request, `super::install_mob_hooks`);
    /// one recorded for a session not open here gets no participant, and a
    /// destroyed one's record goes.
    #[test]
    fn a_council_works_for_the_session_it_is_recorded_for_and_no_other() {
        use meerkat_mob::MobId;

        let registry = HelperRegistry::in_memory();
        let agent = SessionId::new();
        registry.register_session(&agent, "m", None);
        let convened = MobId::from("council--agent-1");
        registry.record_mob(&convened, &agent).unwrap();
        assert_eq!(
            registry.owner_for_spawn(&convened, None).unwrap().session,
            agent
        );
        let foreign = MobId::from("council--agent-2");
        let stranger = SessionId::new();
        registry.record_mob(&foreign, &stranger).unwrap();
        assert_eq!(
            registry.owner_for_spawn(&foreign, None),
            Err(OwnerError::UnknownSession(stranger))
        );
        registry.mob_destroyed(&convened);
        assert_eq!(
            registry.owner_for_spawn(&convened, None),
            Err(OwnerError::UnknownMob(convened))
        );
    }

    #[test]
    fn owners_follow_mob_creators_spawners_and_nesting() {
        use meerkat_mob::{AgentIdentity, MobId};

        let registry = HelperRegistry::in_memory();
        let a = SessionId::new();
        let b = SessionId::new();
        let implicit_a = MobId::from(format!("implicit-{a}"));
        // Nothing created the mob: refused, whoever is open.
        registry.register_session(&a, "m-a", Some(&binding()));
        registry.register_session(&b, "m-b", None);
        assert_eq!(
            registry.owner_for_spawn(&implicit_a, None),
            Err(OwnerError::UnknownMob(implicit_a.clone()))
        );
        assert!(
            registry
                .owner_for_spawn(&MobId::from("team"), None)
                .is_err()
        );
        // A session's own mob.
        registry.record_mob(&implicit_a, &a).unwrap();
        let owner = registry.owner_for_spawn(&implicit_a, None).unwrap();
        assert_eq!(owner.session, a);
        assert_eq!(owner.model, "m-a");
        assert_eq!(owner.auth_binding, Some(binding()));
        assert!(owner.path.is_empty());
        // A mob `b` created works for `b`, never for the other session.
        registry.record_mob(&MobId::from("team"), &b).unwrap();
        assert_eq!(
            registry
                .owner_for_spawn(&MobId::from("team"), None)
                .unwrap()
                .session,
            b
        );
        // An unknown spawner is refused, not attributed to the mob's creator.
        let lead = AgentIdentity::from("lead");
        assert!(matches!(
            registry.owner_for_spawn(&MobId::from("team"), Some(&lead)),
            Err(OwnerError::UnknownSpawner(_))
        ));

        // A helper of `a`, then a helper of that helper (in its implicit mob).
        let tag = HelperTag {
            owner: a.clone(),
            path: vec!["scout".into()],
            model: "m-a".into(),
            kind: HelperKind::Other,
        };
        let key = HelperKey::new(implicit_a.clone(), AgentIdentity::from("scout"));
        registry.spawning(key.clone(), tag.clone()).unwrap();
        let helper_session = SessionId::new();
        registry.bind_session(&key, &tag, &helper_session);
        let nested_mob = MobId::from(format!("implicit-{helper_session}"));
        registry.record_mob(&nested_mob, &helper_session).unwrap();
        let nested = registry.owner_for_spawn(&nested_mob, None).unwrap();
        assert_eq!(nested.session, a);
        assert_eq!(nested.path, vec!["scout".to_string()]);
        assert_eq!(nested.auth_binding, Some(binding()));
        // A member spawning into its own mob: its owner.
        let by_member = registry
            .owner_for_spawn(&implicit_a, Some(&AgentIdentity::from("scout")))
            .unwrap();
        assert_eq!(by_member.session, a);
        assert_eq!(registry.live_for(&a), vec![key.clone()]);
        assert!(registry.live_for(&b).is_empty());
        registry.finished(&key);
        assert!(registry.live().is_empty());
        // Finished is not retired: the binding stays for a rebuild.
        assert_eq!(registry.owner_for_resume(&key).unwrap(), tag);
        registry.retired(&key);
        registry.retired(&key);
        assert!(registry.owner_for_resume(&key).is_err());
        assert!(registry.owner_for_spawn(&nested_mob, None).is_err());
    }

    /// The kinds are stored as one word each, and the words earlier builds
    /// wrote open as what they meant: `assign`'s helper single-use, a
    /// `spawn_member` one kept, a `fork_off` child a kept fork.
    #[test]
    fn kinds_are_stored_as_words_and_earlier_words_still_open() {
        use HelperContext::{Fork, Fresh};
        use HelperLife::{Kept, Single};
        let word = |kind: HelperKind| serde_json::to_string(&kind).unwrap();
        assert_eq!(
            word(HelperKind::Started(Fresh, Single)),
            r#""fresh_single""#
        );
        assert_eq!(word(HelperKind::Started(Fork, Kept)), r#""fork_kept""#);
        for (stored, kind) in [
            ("assigned", HelperKind::Started(Fresh, Single)),
            ("member", HelperKind::Started(Fresh, Kept)),
            ("fork", HelperKind::Started(Fork, Kept)),
            ("other", HelperKind::Other),
            ("fresh_kept", HelperKind::Started(Fresh, Kept)),
            ("fork_single", HelperKind::Started(Fork, Single)),
        ] {
            let read: HelperKind = serde_json::from_str(&format!("\"{stored}\"")).unwrap();
            assert_eq!(read, kind, "{stored}");
            let again: HelperKind = serde_json::from_str(&word(kind)).unwrap();
            assert_eq!(again, kind);
        }
        assert_eq!(
            HelperKind::Started(Fork, Single).text().as_deref(),
            Some("fork · single-use")
        );
        assert_eq!(HelperKind::Other.text(), None);
    }

    fn tui(member: &str) -> HelperKey {
        HelperKey::new(MobId::from("tui"), AgentIdentity::from(member))
    }

    const FRESH: HelperKind = HelperKind::Started(HelperContext::Fresh, HelperLife::Single);

    /// The limit counts the seated helpers and the calls under way: at one
    /// below it, of two calls in one step the second is refused (and so is
    /// a second call for one name); a call whose spawn failed lets its slot
    /// and its name go; a started helper keeps its slot until its mob
    /// lists it, then counts as seated, and leaving lets it go.
    #[test]
    fn a_reservation_holds_a_name_and_a_slot_until_it_is_let_go() {
        let registry = HelperRegistry::in_memory();
        registry.set_max_live(3);
        let agent = SessionId::new();
        let seated = [tui("a")];
        let first = registry
            .reserve(tui("b"), FRESH, &agent, &[], &seated)
            .unwrap();
        // One name, one call.
        assert_eq!(
            registry
                .reserve(tui("b"), FRESH, &agent, &[], &seated)
                .unwrap_err(),
            Refused::NameInUse
        );
        let second = registry
            .reserve(tui("c"), FRESH, &agent, &[], &seated)
            .unwrap();
        // `a` seated, `b` and `c` under way: the third is refused, naming
        // the two being seated.
        assert_eq!(
            registry
                .reserve(tui("d"), FRESH, &agent, &[], &seated)
                .unwrap_err(),
            Refused::Full {
                starting: vec![tui("b"), tui("c")],
                limit: 3
            }
        );
        // Another parent, or a helper of the agent, has a limit of its own.
        let helper_path = ["a".to_string()];
        let other = registry
            .reserve(tui("e"), FRESH, &agent, &helper_path, &[])
            .unwrap();
        // `c`'s spawn failed: its slot and its kind go.
        drop(second);
        assert_eq!(registry.take_intent(&tui("c")), None);
        assert_eq!(registry.starting(&agent, &[], &seated), vec![tui("b")]);
        // `b` was started: its seat takes the kind, its slot stays until
        // its mob lists it.
        assert_eq!(registry.take_intent(&tui("b")), Some(FRESH));
        first.launched();
        assert_eq!(registry.take_intent(&tui("b")), None);
        assert_eq!(registry.starting(&agent, &[], &seated), vec![tui("b")]);
        let both = [tui("a"), tui("b")];
        assert!(registry.starting(&agent, &[], &both).is_empty());
        assert!(registry.starting(&agent, &[], &seated).is_empty());
        // Under way when it retired: nothing is held once the call returns.
        let quick = registry
            .reserve(tui("q"), FRESH, &agent, &[], &seated)
            .unwrap();
        registry.retired(&tui("q"));
        quick.launched();
        assert!(registry.starting(&agent, &[], &seated).is_empty());
        // A lowered limit refuses the next and leaves the held alone.
        registry.set_max_live(1);
        assert!(matches!(
            registry.reserve(tui("f"), FRESH, &agent, &[], &seated),
            Err(Refused::Full { limit: 1, .. })
        ));
        assert_eq!(registry.starting(&agent, &helper_path, &[]), vec![tui("e")]);
        other.launched();
        registry.forget_session(&agent);
        assert!(registry.starting(&agent, &helper_path, &[]).is_empty());
    }

    /// An `assign` call cancelled mid-spawn (its future dropped, as the
    /// user's interrupt does) leaves no intent and no slot: a member later
    /// seated under that name by another path takes no stale kind.
    #[test]
    fn a_cancelled_call_leaves_no_intent_and_no_slot() {
        use futures::FutureExt;
        let registry = HelperRegistry::in_memory();
        registry.set_max_live(1);
        let agent = SessionId::new();
        let call = async {
            let reservation = registry.reserve(tui("w"), FRESH, &agent, &[], &[]).unwrap();
            std::future::pending::<()>().await;
            reservation.launched();
        };
        let mut call = Box::pin(call);
        assert!(call.as_mut().now_or_never().is_none());
        assert_eq!(registry.starting(&agent, &[], &[]), vec![tui("w")]);
        drop(call);
        assert_eq!(registry.take_intent(&tui("w")), None);
        assert!(registry.starting(&agent, &[], &[]).is_empty());
        assert!(registry.reserve(tui("w"), FRESH, &agent, &[], &[]).is_ok());
    }

    /// Concurrent calls at one below the limit, from many threads: exactly
    /// one takes the last slot.
    #[test]
    fn concurrent_calls_take_the_last_slot_once() {
        let registry = std::sync::Arc::new(HelperRegistry::in_memory());
        registry.set_max_live(8);
        let agent = SessionId::new();
        let seated: Vec<HelperKey> = (0..7).map(|n| tui(&format!("s{n}"))).collect();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let taken: Vec<bool> = (0..8)
            .map(|n| {
                let (registry, agent, seated, barrier) = (
                    std::sync::Arc::clone(&registry),
                    agent.clone(),
                    seated.clone(),
                    std::sync::Arc::clone(&barrier),
                );
                std::thread::spawn(move || {
                    barrier.wait();
                    registry
                        .reserve(tui(&format!("n{n}")), FRESH, &agent, &[], &seated)
                        .map(Reservation::launched)
                        .is_ok()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(taken.iter().filter(|taken| **taken).count(), 1, "{taken:?}");
    }

    #[test]
    fn forgetting_a_session_prunes_its_helpers() {
        use meerkat_mob::{AgentIdentity, MobId};

        let registry = HelperRegistry::in_memory();
        let a = SessionId::new();
        let b = SessionId::new();
        registry.register_session(&a, "m", None);
        registry.register_session(&b, "m", None);
        let team = MobId::from("team");
        registry.record_mob(&team, &a).unwrap();
        let tag = HelperTag {
            owner: a.clone(),
            path: vec!["scout".into()],
            model: "m".into(),
            kind: HelperKind::Other,
        };
        let key = HelperKey::new(team.clone(), AgentIdentity::from("scout"));
        registry.spawning(key, tag).unwrap();
        registry.forget_session(&a);
        registry.forget_session(&a);
        assert!(registry.live().is_empty());
        assert_eq!(registry.bindings().in_memory_counts(), (0, 0));
        // `a` is gone: its mob's spawns are refused, never given to `b`.
        assert_eq!(
            registry.owner_for_spawn(&team, None),
            Err(OwnerError::UnknownMob(team))
        );
    }

    #[test]
    fn helper_events_are_scoped_by_their_path() {
        let tag = HelperTag {
            owner: SessionId::new(),
            path: vec!["outer".into(), "inner".into()],
            model: "m".into(),
            kind: HelperKind::Other,
        };
        assert_eq!(tag.label(), "inner");
        assert_eq!(tag.depth(), 2);
        assert_eq!(tag.parent_path(), ["outer".to_string()]);
        let scoped = tag.scope(AgentEvent::TextDelta {
            delta: "x".into(),
            assistant_message_id: None,
        });
        assert_eq!(scoped.scope_path.len(), 3);
        assert!(matches!(
            &scoped.scope_path[2],
            StreamScopeFrame::MobMember { agent_identity, .. } if agent_identity == "inner"
        ));
    }
}
