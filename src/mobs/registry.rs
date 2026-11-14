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
    /// The councils the gate admitted and the session that convened each,
    /// by the temporary mob the engine creates for it
    /// ([`super::council`]).
    councils: Mutex<HashMap<MobId, SessionId>>,
    /// Each destroyed mob, as the host's after-destroy hook reports it
    /// (again before each retry of its storage cleanup): what still
    /// observes the mob lets go ([`Self::destroyed_mobs`]).
    destroyed: tokio::sync::broadcast::Sender<MobId>,
    /// The agents' private scratch directories
    /// ([`crate::approval::scratch`]): an agent's helpers share its own.
    scratch: crate::approval::scratch::ScratchDirs,
}

impl HelperRegistry {
    /// An empty registry, its bindings in memory only.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn in_memory() -> Self {
        Self::with_bindings(HelperBindings::in_memory())
    }

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
            councils: Mutex::new(HashMap::new()),
            destroyed: tokio::sync::broadcast::channel(DESTROYED_QUEUE).0,
            scratch: crate::approval::scratch::ScratchDirs::off(),
        }
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

    /// The persisted bindings (tests and diagnostics).
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
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
    /// own mob, and helpers its session delegates to, work for
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
        self.live.lock().retain(|_, tag| &tag.owner != session);
        self.bindings.forget_owner(session);
    }

    /// `mob` was created for the bridge session `owner`; a council's
    /// temporary mob ([`Self::expect_council`]) is recorded for the session
    /// that convened it instead.
    ///
    /// # Errors
    ///
    /// [`BindingsError`] when the binding cannot be persisted.
    pub fn record_mob(&self, mob: &MobId, owner: &SessionId) -> Result<(), BindingsError> {
        let convener = self.councils.lock().get(mob).cloned();
        self.bindings
            .set_mob_owner(mob, convener.as_ref().unwrap_or(owner))
    }

    /// The gate admitted a council that `session` convened, for which the
    /// engine creates `mob`: its participants work for `session`.
    /// A council already expected for `mob` keeps its convener: a second
    /// call naming the same id is not recorded (its participants are then
    /// refused), so it cannot take the first one's participants.
    pub fn expect_council(&self, mob: MobId, session: SessionId) {
        self.councils.lock().entry(mob).or_insert(session);
    }

    /// The council call for `mob` that `session` convened failed: no mob
    /// comes of it, and its entry goes (a later council may use the id).
    pub fn council_failed(&self, mob: &MobId, session: &SessionId) {
        let mut councils = self.councils.lock();
        if councils.get(mob) == Some(session) {
            councils.remove(mob);
        }
    }

    /// The session expected to convene `mob`, when one is (tests).
    #[cfg(test)]
    fn convener(&self, mob: &MobId) -> Option<SessionId> {
        self.councils.lock().get(mob).cloned()
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
        self.live.lock().insert(key, tag);
        Ok(())
    }

    /// The helper `key` runs as `session`: helpers it delegates to
    /// inherit its owner.
    pub fn bind_session(&self, key: &HelperKey, tag: &HelperTag, session: &SessionId) {
        if let Err(err) = self.bindings.set_member_session(key, tag, session) {
            tracing::warn!(helper = %key, error = %err, "cannot record the helper's session");
        }
    }

    /// The helper `key` is not running here (its mob stopped, or it was
    /// never observable); its binding stays for a rebuild.
    pub fn finished(&self, key: &HelperKey) {
        self.live.lock().shift_remove(key);
    }

    /// The helper `key` left its mob for good: drop it everywhere.
    /// Idempotent.
    pub fn retired(&self, key: &HelperKey) {
        self.finished(key);
        if let Err(err) = self.bindings.remove_member(key) {
            tracing::warn!(helper = %key, error = %err, "cannot drop the helper's binding");
        }
    }

    /// `mob` was destroyed: drop its helpers everywhere. Idempotent.
    pub fn mob_destroyed(&self, mob: &MobId) {
        self.live.lock().retain(|key, _| &key.mob != mob);
        self.councils.lock().remove(mob);
        let _ = self.destroyed.send(mob.clone());
        if let Err(err) = self.bindings.remove_mob(mob) {
            tracing::warn!(mob = %mob, error = %err, "cannot drop the mob's bindings");
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

    /// Running helpers.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn live(&self) -> Vec<HelperStatus> {
        self.live
            .lock()
            .values()
            .map(|tag| HelperStatus {
                label: tag.label().to_string(),
                model: tag.model.clone(),
                depth: tag.depth(),
            })
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

    #[test]
    fn a_council_works_for_the_session_that_convened_it_and_no_other() {
        use meerkat_mob::MobId;

        let registry = HelperRegistry::in_memory();
        let agent = SessionId::new();
        registry.register_session(&agent, "m", None);
        // The engine creates a council's mob for a fresh session of its own.
        let convened = MobId::from("council--agent-1");
        registry.expect_council(convened.clone(), agent.clone());
        registry.record_mob(&convened, &SessionId::new()).unwrap();
        assert_eq!(
            registry.owner_for_spawn(&convened, None).unwrap().session,
            agent
        );
        // One the gate never admitted keeps that foreign session.
        let foreign = MobId::from("council--agent-2");
        let stranger = SessionId::new();
        registry.record_mob(&foreign, &stranger).unwrap();
        assert_eq!(
            registry.owner_for_spawn(&foreign, None),
            Err(OwnerError::UnknownSession(stranger))
        );
        // Destroyed, the expectation goes with it.
        registry.mob_destroyed(&convened);
        let again = SessionId::new();
        registry.record_mob(&convened, &again).unwrap();
        assert_eq!(
            registry.owner_for_spawn(&convened, None),
            Err(OwnerError::UnknownSession(again))
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

    /// A council expected for a mob keeps its convener against a second
    /// call naming the same id, and its entry goes when its call failed
    /// (only the convener's failure removes it).
    #[test]
    fn a_council_entry_is_not_taken_over_and_goes_when_its_call_failed() {
        let registry = HelperRegistry::in_memory();
        let (first, second) = (SessionId::new(), SessionId::new());
        let mob = MobId::from("council-x");
        registry.expect_council(mob.clone(), first.clone());
        registry.expect_council(mob.clone(), second.clone());
        assert_eq!(registry.convener(&mob), Some(first.clone()));
        registry.council_failed(&mob, &second);
        assert_eq!(registry.convener(&mob), Some(first.clone()));
        registry.council_failed(&mob, &first);
        assert_eq!(registry.convener(&mob), None);
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
