//! Whom every helper and every agent-created mob works for, persisted under
//! the realm (`<realm>/tui_helpers.json`), so a member the mob runtime
//! restores after a restart is gated for the session that spawned it.
//!
//! The mob runtime owns the members and their durable sessions; this is the
//! rkat-tui's own fact next to it (upstream records no host metadata on a
//! member): each helper's [`HelperTag`] (its owner session, label path and
//! model) and, once known, the session it runs as; and the owner bridge
//! session of each mob rkat-tui's sessions created (from the mob host's
//! create hook).
//!
//! Every write goes to memory and to the file (read, changed, written
//! atomically). [`HelperBindings::forget_owner`] drops a session's entries
//! from memory only: lookups that miss memory read the file, so a session
//! resumed later still finds its helpers, and a session forgotten from this
//! process holds no memory.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use meerkat_core::SessionId;
use meerkat_mob::{AgentIdentity, MobId};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use super::registry::{HelperKey, HelperTag};
use crate::paths::{AtomicWriteError, FileMode, write_atomic};

/// The bindings file, under the realm directory.
pub const BINDINGS_FILE: &str = "tui_helpers.json";

/// The file format's version.
const FORMAT_VERSION: u32 = 1;

/// Failures reading or writing the bindings file.
#[derive(Debug, thiserror::Error)]
pub enum BindingsError {
    /// The file exists but cannot be read.
    #[error("cannot read {path}: {source}")]
    Read {
        /// The bindings file.
        path: PathBuf,
        source: std::io::Error,
    },
    /// The file is not a bindings file this version understands.
    #[error("cannot parse {path}: {source}")]
    Parse {
        /// The bindings file.
        path: PathBuf,
        source: serde_json::Error,
    },
    /// The file's version is not [`FORMAT_VERSION`].
    #[error("{path} has format version {found}, expected {FORMAT_VERSION}")]
    Version {
        /// The bindings file.
        path: PathBuf,
        /// The version it names.
        found: u32,
    },
    #[error("cannot encode the helper bindings: {0}")]
    Encode(serde_json::Error),
    /// The file cannot be written.
    #[error(transparent)]
    Write(#[from] AtomicWriteError),
}

/// One helper's binding: what it is, whom it works for and (once known) the
/// session it runs as.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberBinding {
    /// Owner, label path and model.
    #[serde(flatten)]
    pub tag: HelperTag,
    /// The helper's own session, once the mob names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
    /// It is done and its retire was decided
    /// ([`HelperBindings::owe_retire`]): a start that finds it still seated
    /// retires it (absent in the files an earlier build wrote: not owed).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub retire_owed: bool,
    /// The first line of the task its `assign` gave it, shortened
    /// ([`HelperBindings::set_task`]; absent in the files an earlier build
    /// wrote, and for a helper no `assign` started).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
}

/// A member binding as the file stores it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct MemberRecord {
    mob: MobId,
    member: AgentIdentity,
    #[serde(flatten)]
    binding: MemberBinding,
}

/// The file's contents.
#[derive(Debug, Serialize, Deserialize)]
struct FileFormat {
    version: u32,
    #[serde(default)]
    mobs: BTreeMap<MobId, SessionId>,
    #[serde(default)]
    members: Vec<MemberRecord>,
}

/// Bindings in memory (or as read from the file).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Bindings {
    /// Mob → the bridge session that created it.
    mobs: BTreeMap<MobId, SessionId>,
    /// Helper → its binding.
    members: BTreeMap<HelperKey, MemberBinding>,
}

impl Bindings {
    fn member_by_session(&self, session: &SessionId) -> Option<(HelperKey, MemberBinding)> {
        self.members
            .iter()
            .find(|(_, binding)| binding.session.as_ref() == Some(session))
            .map(|(key, binding)| (key.clone(), binding.clone()))
    }

    /// Drop `key` and the mobs its session created.
    fn remove_member(&mut self, key: &HelperKey) {
        if let Some(MemberBinding {
            session: Some(session),
            ..
        }) = self.members.remove(key)
        {
            self.mobs.retain(|_, owner| owner != &session);
        }
    }

    /// Drop `mob` and its members (and, transitively, the mobs they made).
    fn remove_mob(&mut self, mob: &MobId) {
        self.mobs.remove(mob);
        let members: Vec<HelperKey> = self
            .members
            .keys()
            .filter(|key| &key.mob == mob)
            .cloned()
            .collect();
        for key in &members {
            self.remove_member(key);
        }
    }

    /// Drop every entry `owner` holds: its helpers (and their sessions'
    /// mobs) and the mobs it created.
    fn forget_owner(&mut self, owner: &SessionId) {
        let helpers: Vec<HelperKey> = self
            .members
            .iter()
            .filter(|(_, binding)| &binding.tag.owner == owner)
            .map(|(key, _)| key.clone())
            .collect();
        for key in &helpers {
            self.remove_member(key);
        }
        self.mobs.retain(|_, creator| creator != owner);
    }

    fn decode(path: &Path, bytes: &[u8]) -> Result<Self, BindingsError> {
        let file: FileFormat =
            serde_json::from_slice(bytes).map_err(|source| BindingsError::Parse {
                path: path.to_path_buf(),
                source,
            })?;
        if file.version != FORMAT_VERSION {
            return Err(BindingsError::Version {
                path: path.to_path_buf(),
                found: file.version,
            });
        }
        Ok(Self {
            mobs: file.mobs,
            members: file
                .members
                .into_iter()
                .map(|record| (HelperKey::new(record.mob, record.member), record.binding))
                .collect(),
        })
    }

    fn encode(&self) -> Result<Vec<u8>, BindingsError> {
        let file = FileFormat {
            version: FORMAT_VERSION,
            mobs: self.mobs.clone(),
            members: self
                .members
                .iter()
                .map(|(key, binding)| MemberRecord {
                    mob: key.mob.clone(),
                    member: key.member.clone(),
                    binding: binding.clone(),
                })
                .collect(),
        };
        serde_json::to_vec_pretty(&file).map_err(BindingsError::Encode)
    }
}

/// Where the bindings persist.
#[derive(Debug)]
struct Store {
    realm_root: PathBuf,
    path: PathBuf,
    /// Fail this many writes next as a full disk does (tests).
    #[cfg(any(test, feature = "test-support"))]
    fail_writes: std::sync::atomic::AtomicUsize,
}

impl Store {
    fn new(realm_root: &Path) -> Self {
        Self {
            realm_root: realm_root.to_path_buf(),
            path: realm_root.join(BINDINGS_FILE),
            #[cfg(any(test, feature = "test-support"))]
            fail_writes: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// The file's bindings; empty when there is no file.
    fn read(&self) -> Result<Bindings, BindingsError> {
        match std::fs::read(&self.path) {
            Ok(bytes) => Bindings::decode(&self.path, &bytes),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Bindings::default()),
            Err(source) => Err(BindingsError::Read {
                path: self.path.clone(),
                source,
            }),
        }
    }

    fn write(&self, bindings: &Bindings) -> Result<(), BindingsError> {
        #[cfg(any(test, feature = "test-support"))]
        if self
            .fail_writes
            .fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |left| left.checked_sub(1),
            )
            .is_ok()
        {
            return Err(BindingsError::Write(AtomicWriteError::Io {
                op: "write",
                path: self.path.clone(),
                source: std::io::Error::from_raw_os_error(28),
            }));
        }
        write_atomic(
            &self.path,
            &bindings.encode()?,
            FileMode::Private,
            Some(&self.realm_root),
        )?;
        Ok(())
    }
}

/// One change of the bindings, kept to be applied to the file again while
/// a write of it failed.
type Change = std::sync::Arc<dyn Fn(&mut Bindings) + Send + Sync>;

/// What the bindings tell of their file ([`HelperBindings::on_saving`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Saving {
    /// A write failed while the file was up to date: the error. The
    /// changes are kept and written again at the next change and at
    /// [`HelperBindings::flush`]; nothing more is told until it succeeds.
    Unsaved(String),
    /// A write succeeded after a failure: the file is up to date again.
    SavedAgain,
}

/// Told of each [`Saving`].
pub type SavingOutlet = std::sync::Arc<dyn Fn(Saving) + Send + Sync>;

/// The helper and mob bindings, in memory and (when opened over a realm)
/// in the realm's bindings file.
///
/// A write that fails (a full or failing disk) is kept as the usage
/// records keep theirs (`crate::daemon::usage`): memory has the change, the
/// file is behind ("dirty"), and the changes it misses are applied to it
/// again at the next change and at [`Self::flush`], until a write
/// succeeds. They are kept as changes, not as memory's state, because
/// memory does not hold everything the file does
/// ([`Self::forget_owner`]); a lookup that reads the file applies them
/// too. The first failure and the recovery are told ([`Self::on_saving`]).
/// [`Self::bind`] and [`Self::set_mob_owner`] are refused instead: a
/// helper must not be seated, nor a mob attributed, on a binding a restart
/// would not find.
pub struct HelperBindings {
    store: Option<Store>,
    memory: Mutex<Bindings>,
    /// The changes the file misses; locked after `memory`.
    owed: Mutex<Vec<Change>>,
    outlet: std::sync::OnceLock<SavingOutlet>,
    /// Whether memory dropped entries the file still holds
    /// ([`Self::forget_owner`]): only then can a lookup that misses in
    /// memory find anything in the file.
    partial: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for HelperBindings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HelperBindings")
            .field("store", &self.store)
            .field("owed", &self.owed.lock().len())
            .finish_non_exhaustive()
    }
}

impl HelperBindings {
    /// Bindings kept in memory only (tests, and hosts without a realm).
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn in_memory() -> Self {
        Self::with(None, Bindings::default())
    }

    fn with(store: Option<Store>, memory: Bindings) -> Self {
        Self {
            store,
            memory: Mutex::new(memory),
            owed: Mutex::new(Vec::new()),
            outlet: std::sync::OnceLock::new(),
            partial: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// The bindings persisted under `realm_root`.
    ///
    /// # Errors
    ///
    /// [`BindingsError`] when the file exists but cannot be read or parsed.
    pub fn open(realm_root: &Path) -> Result<Self, BindingsError> {
        let store = Store::new(realm_root);
        let memory = store.read()?;
        Ok(Self::with(Some(store), memory))
    }

    /// Tell `outlet` when a write fails and when the file is saved again.
    /// Set once; returns whether this call set it.
    pub fn on_saving(&self, outlet: SavingOutlet) -> bool {
        self.outlet.set(outlet).is_ok()
    }

    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.store.as_ref().map(|store| store.path.as_path())
    }

    /// Fail the next `n` writes of the file as a full disk does (tests).
    #[cfg(any(test, feature = "test-support"))]
    pub fn fail_writes(&self, n: usize) {
        if let Some(store) = &self.store {
            store
                .fail_writes
                .store(n, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Whether the file misses changes a write failed to save.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn dirty(&self) -> bool {
        !self.owed.lock().is_empty()
    }

    /// Write the changes the file misses (a retry after a failure; nothing
    /// when it is up to date). Blocking file I/O.
    ///
    /// # Errors
    ///
    /// [`BindingsError`] when the write fails again.
    pub fn flush(&self) -> Result<(), BindingsError> {
        let memory = self.memory.lock();
        let mut owed = self.owed.lock();
        let Some(store) = self.store.as_ref() else {
            return Ok(());
        };
        if owed.is_empty() {
            return Ok(());
        }
        self.save(store, &memory, &mut owed, None)
    }

    fn tell(&self, saving: Saving) {
        if let Some(outlet) = self.outlet.get() {
            outlet(saving);
        }
    }

    /// Write the file with the changes it misses (`owed`) and `extra`
    /// applied; on success nothing is owed any more. Tells the first
    /// failure while the file was up to date, and the success after one.
    fn save(
        &self,
        store: &Store,
        memory: &Bindings,
        owed: &mut Vec<Change>,
        extra: Option<&Change>,
    ) -> Result<(), BindingsError> {
        let was_dirty = !owed.is_empty();
        let mut on_disk = match store.read() {
            Ok(bindings) => bindings,
            Err(err) => {
                tracing::warn!(error = %err, "helper bindings unreadable; rewriting them");
                memory.clone()
            }
        };
        for change in owed.iter().chain(extra) {
            change(&mut on_disk);
        }
        match store.write(&on_disk) {
            Ok(()) => {
                owed.clear();
                if was_dirty {
                    tracing::info!(path = %store.path.display(), "the helper bindings are saved again");
                    self.tell(Saving::SavedAgain);
                }
                Ok(())
            }
            Err(err) => {
                if !was_dirty {
                    self.tell(Saving::Unsaved(err.to_string()));
                }
                Err(err)
            }
        }
    }

    /// [`Self::open`], or empty bindings that still persist under
    /// `realm_root` when the file cannot be read (logged): members bound
    /// only by the unreadable file are refused, never attributed elsewhere.
    #[must_use]
    pub fn open_or_empty(realm_root: &Path) -> Self {
        Self::open(realm_root).unwrap_or_else(|err| {
            tracing::warn!(error = %err, "helper bindings unreadable; starting empty");
            Self::with(Some(Store::new(realm_root)), Bindings::default())
        })
    }

    /// Read `f` from memory, else from the file (with the changes it
    /// misses applied).
    fn lookup<T>(&self, f: impl Fn(&Bindings) -> Option<T>) -> Option<T> {
        if let Some(found) = f(&self.memory.lock()) {
            return Some(found);
        }
        // Memory holds all the file does until an owner is forgotten:
        // a miss then is a miss (no blocking read on every one).
        if !self.partial.load(std::sync::atomic::Ordering::Acquire) {
            return None;
        }
        let store = self.store.as_ref()?;
        match store.read() {
            Ok(mut bindings) => {
                for change in self.owed.lock().iter() {
                    change(&mut bindings);
                }
                f(&bindings)
            }
            Err(err) => {
                tracing::warn!(error = %err, "cannot read helper bindings");
                None
            }
        }
    }

    /// Apply `change` to memory and to the file; a failed write keeps it
    /// for the next one (the type's doc). The memory lock is held across
    /// the file update, so writes are serialized.
    fn change(
        &self,
        change: impl Fn(&mut Bindings) + Send + Sync + 'static,
    ) -> Result<(), BindingsError> {
        let change: Change = std::sync::Arc::new(change);
        let mut memory = self.memory.lock();
        change(&mut memory);
        let Some(store) = self.store.as_ref() else {
            return Ok(());
        };
        let mut owed = self.owed.lock();
        self.save(store, &memory, &mut owed, Some(&change))
            .inspect_err(|_| owed.push(change))
    }

    /// Apply `change` to the file and then to memory, or to neither when
    /// the write fails (a refusal).
    fn change_or_refuse(
        &self,
        change: impl Fn(&mut Bindings) + Send + Sync + 'static,
    ) -> Result<(), BindingsError> {
        let change: Change = std::sync::Arc::new(change);
        let mut memory = self.memory.lock();
        if let Some(store) = self.store.as_ref() {
            let mut owed = self.owed.lock();
            self.save(store, &memory, &mut owed, Some(&change))?;
        }
        change(&mut memory);
        Ok(())
    }

    /// The bridge session that created `mob`.
    #[must_use]
    pub fn mob_owner(&self, mob: &MobId) -> Option<SessionId> {
        self.lookup(|b| b.mobs.get(mob).cloned())
    }

    /// `mob` was created for `owner`.
    ///
    /// # Errors
    ///
    /// [`BindingsError`] when the file cannot be updated: refused, nothing
    /// is recorded (a mob with no recorded owner makes its later members
    /// unattributable).
    pub fn set_mob_owner(&self, mob: &MobId, owner: &SessionId) -> Result<(), BindingsError> {
        let (mob, owner) = (mob.clone(), owner.clone());
        self.change_or_refuse(move |b| {
            b.mobs.insert(mob.clone(), owner.clone());
        })
    }

    #[must_use]
    pub fn member(&self, key: &HelperKey) -> Option<MemberBinding> {
        self.lookup(|b| b.members.get(key).cloned())
    }

    /// The helper running as `session`.
    #[must_use]
    pub fn member_by_session(&self, session: &SessionId) -> Option<(HelperKey, MemberBinding)> {
        self.lookup(|b| b.member_by_session(session))
    }

    /// Bind `key` to `tag`, keeping the session already known for it when
    /// the tag is unchanged.
    ///
    /// # Errors
    ///
    /// [`BindingsError`] when the file cannot be updated: refused, nothing
    /// is recorded (the spawn is refused with it).
    pub fn bind(&self, key: &HelperKey, tag: &HelperTag) -> Result<(), BindingsError> {
        let (key, tag) = (key.clone(), tag.clone());
        self.change_or_refuse(move |b| {
            let same = b.members.get(&key).filter(|binding| binding.tag == tag);
            let session = same.and_then(|binding| binding.session.clone());
            let retire_owed = same.is_some_and(|binding| binding.retire_owed);
            let task = same.and_then(|binding| binding.task.clone());
            b.members.insert(
                key.clone(),
                MemberBinding {
                    tag: tag.clone(),
                    session,
                    retire_owed,
                    task,
                },
            );
        })
    }

    /// The task `key` was given is `task` (its first line, shortened).
    /// Nothing when it is not bound.
    pub fn set_task(&self, key: &HelperKey, task: &str) -> Result<(), BindingsError> {
        if self
            .lookup(|b| {
                b.members
                    .get(key)
                    .map(|binding| binding.task.as_deref() == Some(task))
            })
            .unwrap_or(true)
        {
            return Ok(());
        }
        let (key, task) = (key.clone(), task.to_string());
        self.change(move |b| {
            if let Some(binding) = b.members.get_mut(&key) {
                binding.task = Some(task.clone());
            }
        })
    }

    /// `key` is done and is being retired: a start that finds it still
    /// seated (the retire did not reach the engine before the daemon
    /// stopped) retires it. Nothing when it is not bound.
    pub fn owe_retire(&self, key: &HelperKey) -> Result<(), BindingsError> {
        if self
            .lookup(|b| b.members.get(key).map(|binding| binding.retire_owed))
            .unwrap_or(true)
        {
            return Ok(());
        }
        let key = key.clone();
        self.change(move |b| {
            if let Some(binding) = b.members.get_mut(&key) {
                binding.retire_owed = true;
            }
        })
    }

    /// `key` runs as `session`.
    pub fn set_member_session(
        &self,
        key: &HelperKey,
        tag: &HelperTag,
        session: &SessionId,
    ) -> Result<(), BindingsError> {
        if self
            .memory
            .lock()
            .members
            .get(key)
            .is_some_and(|b| b.session.as_ref() == Some(session) && &b.tag == tag)
        {
            return Ok(());
        }
        let (key, tag, session) = (key.clone(), tag.clone(), session.clone());
        self.change(move |b| {
            let same = b.members.get(&key).filter(|binding| binding.tag == tag);
            let retire_owed = same.is_some_and(|binding| binding.retire_owed);
            let task = same.and_then(|binding| binding.task.clone());
            b.members.insert(
                key.clone(),
                MemberBinding {
                    tag: tag.clone(),
                    session: Some(session.clone()),
                    retire_owed,
                    task,
                },
            );
        })
    }

    /// `key` retired: drop it (and the mobs its session created).
    /// Idempotent.
    pub fn remove_member(&self, key: &HelperKey) -> Result<(), BindingsError> {
        let key = key.clone();
        self.change(move |b| b.remove_member(&key))
    }

    /// `mob` was destroyed: drop it and its members. Idempotent.
    pub fn remove_mob(&self, mob: &MobId) -> Result<(), BindingsError> {
        let mob = mob.clone();
        self.change(move |b| b.remove_mob(&mob))
    }

    /// Every member bound, with its tag, as the bindings hold them now.
    #[must_use]
    pub fn members(&self) -> Vec<(HelperKey, HelperTag)> {
        self.memory
            .lock()
            .members
            .iter()
            .map(|(key, binding)| (key.clone(), binding.tag.clone()))
            .collect()
    }

    /// Drop what `owner` holds from memory (the file keeps it). Idempotent.
    pub fn forget_owner(&self, owner: &SessionId) {
        let mut memory = self.memory.lock();
        memory.forget_owner(owner);
        self.partial
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Bindings held in memory (tests and diagnostics): mobs, members.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn in_memory_counts(&self) -> (usize, usize) {
        let memory = self.memory.lock();
        (memory.mobs.len(), memory.members.len())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn tag(owner: &SessionId, label: &str) -> HelperTag {
        HelperTag {
            owner: owner.clone(),
            path: vec![label.to_string()],
            model: "m".into(),
            kind: crate::mobs::HelperKind::Other,
        }
    }

    #[test]
    fn bindings_round_trip_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let owner = SessionId::new();
        let helper_session = SessionId::new();
        let key = HelperKey::new(MobId::from("team"), AgentIdentity::from("scout"));
        {
            let bindings = HelperBindings::open(dir.path()).unwrap();
            bindings
                .set_mob_owner(&MobId::from("team"), &owner)
                .unwrap();
            bindings.bind(&key, &tag(&owner, "scout")).unwrap();
            bindings
                .set_member_session(&key, &tag(&owner, "scout"), &helper_session)
                .unwrap();
            bindings
                .set_mob_owner(&MobId::from("nested"), &helper_session)
                .unwrap();
        }
        let reopened = HelperBindings::open(dir.path()).unwrap();
        assert_eq!(
            reopened.mob_owner(&MobId::from("team")),
            Some(owner.clone())
        );
        let binding = reopened.member(&key).unwrap();
        assert_eq!(binding.tag, tag(&owner, "scout"));
        assert_eq!(binding.session, Some(helper_session.clone()));
        assert_eq!(
            reopened.member_by_session(&helper_session).map(|(k, _)| k),
            Some(key.clone())
        );

        // Forgetting drops memory only; lookups still find the file's.
        reopened.forget_owner(&owner);
        reopened.forget_owner(&owner);
        assert_eq!(reopened.in_memory_counts(), (0, 0));
        assert!(reopened.member(&key).is_some());

        // Retiring drops the member and the mob its session created.
        reopened.remove_member(&key).unwrap();
        reopened.remove_member(&key).unwrap();
        assert!(reopened.member(&key).is_none());
        assert!(reopened.mob_owner(&MobId::from("nested")).is_none());
        assert_eq!(reopened.mob_owner(&MobId::from("team")), Some(owner));
        reopened.remove_mob(&MobId::from("team")).unwrap();
        assert!(reopened.mob_owner(&MobId::from("team")).is_none());
    }

    /// A retire owed survives a reopen and the rebuild's bind of the same
    /// tag; a binding written without it (an earlier build's) owes none.
    #[test]
    fn a_retire_owed_is_kept_across_a_reopen_and_a_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let owner = SessionId::new();
        let key = HelperKey::new(MobId::from("tui"), AgentIdentity::from("scout"));
        {
            let bindings = HelperBindings::open(dir.path()).unwrap();
            bindings.bind(&key, &tag(&owner, "scout")).unwrap();
            assert!(!bindings.member(&key).unwrap().retire_owed);
            let text = std::fs::read_to_string(dir.path().join(BINDINGS_FILE)).unwrap();
            assert!(!text.contains("retire_owed"), "{text}");
            bindings.owe_retire(&key).unwrap();
        }
        let reopened = HelperBindings::open(dir.path()).unwrap();
        assert!(reopened.member(&key).unwrap().retire_owed);
        reopened.bind(&key, &tag(&owner, "scout")).unwrap();
        reopened
            .set_member_session(&key, &tag(&owner, "scout"), &SessionId::new())
            .unwrap();
        assert!(reopened.member(&key).unwrap().retire_owed);
        // Owing for a helper not bound records nothing.
        let other = HelperKey::new(MobId::from("tui"), AgentIdentity::from("gone"));
        reopened.owe_retire(&other).unwrap();
        assert!(reopened.member(&other).is_none());
    }

    #[test]
    fn an_unreadable_file_is_refused_or_started_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(BINDINGS_FILE), b"not json").unwrap();
        assert!(matches!(
            HelperBindings::open(dir.path()),
            Err(BindingsError::Parse { .. })
        ));
        let bindings = HelperBindings::open_or_empty(dir.path());
        assert!(bindings.mob_owner(&MobId::from("team")).is_none());
    }

    /// On a full disk a change is kept, not lost. Two writes
    /// fail (ENOSPC): the owed retire and the task are in memory, not in
    /// the file, the failure is told once; a lookup that reads the file
    /// (its owner forgotten from memory) still sees them; a flush once the
    /// disk has room writes both and tells "saved again" once; the file
    /// reopened has them. A bind while writes fail is refused and leaves
    /// nothing behind, in memory or in the file.
    #[test]
    fn changes_a_full_disk_refused_are_written_once_it_has_room() {
        let dir = tempfile::tempdir().unwrap();
        let owner = SessionId::new();
        let key = HelperKey::new(MobId::from("tui"), AgentIdentity::from("scout"));
        let bindings = HelperBindings::open(dir.path()).unwrap();
        let told = std::sync::Arc::new(Mutex::new(Vec::new()));
        assert!(bindings.on_saving({
            let told = std::sync::Arc::clone(&told);
            std::sync::Arc::new(move |saving| told.lock().push(saving))
        }));
        bindings.bind(&key, &tag(&owner, "scout")).unwrap();
        assert!(!bindings.dirty());

        bindings.fail_writes(2);
        let err = bindings.owe_retire(&key).unwrap_err().to_string();
        assert!(err.contains("No space left on device"), "{err}");
        assert!(bindings.set_task(&key, "find the parser").is_err());
        assert!(bindings.dirty());
        assert!(
            matches!(told.lock().as_slice(), [Saving::Unsaved(why)] if why.contains("No space"))
        );
        let file = HelperBindings::open(dir.path()).unwrap();
        assert!(!file.member(&key).unwrap().retire_owed);
        // Memory forgot the owner: the lookup reads the file and applies
        // what it misses.
        bindings.forget_owner(&owner);
        let seen = bindings.member(&key).unwrap();
        assert!(seen.retire_owed, "{seen:?}");
        assert_eq!(seen.task.as_deref(), Some("find the parser"));

        // A bind while the disk is full is refused, and nothing of it stays.
        bindings.fail_writes(1);
        let other = HelperKey::new(MobId::from("tui"), AgentIdentity::from("tester"));
        assert!(bindings.bind(&other, &tag(&owner, "tester")).is_err());
        assert!(bindings.member(&other).is_none());
        assert_eq!(told.lock().len(), 1, "one notice per failure period");

        bindings.flush().unwrap();
        assert!(!bindings.dirty());
        assert_eq!(
            told.lock().as_slice(),
            [Saving::Unsaved(err), Saving::SavedAgain]
        );
        let reopened = HelperBindings::open(dir.path()).unwrap();
        let stored = reopened.member(&key).unwrap();
        assert!(stored.retire_owed);
        assert_eq!(stored.task.as_deref(), Some("find the parser"));
        assert!(reopened.member(&other).is_none());
        // Nothing owed: a flush writes nothing and tells nothing.
        bindings.flush().unwrap();
        assert_eq!(told.lock().len(), 2);
    }
}
