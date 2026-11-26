//! The profiles of the mobs the daemon hosts, as the spawn customizer reads
//! them: a member seated by a profile's name gets that profile inline with
//! its model, provider and server replaced
//! ([`super::customizer`]), as the engine lets a spawn replace only the
//! model of a profile it names (`model_override`), and the provider and
//! server a stored profile declares would stay.
//!
//! The customizer is synchronous and the engine hands it no profile, so the
//! definitions are kept here, with the mob set's epoch
//! (`MobMcpState::mob_set_changes`) they were read at. [`MobProfiles::follow`]
//! reads them again each time the set of mobs changes, so the common lookup
//! is a map read. A lookup that finds the epoch moved since (a mob the agent
//! created a moment ago, whose spawn came before the follower ran) reads the
//! definitions itself, synchronously ([`MobProfiles::read_now`]): a spawn
//! reaches the customizer only through the mob's handle, which the engine
//! hands out once the mob is in its managed set, and it bumps the epoch in
//! the same write, so that read always holds the mob. A mob's definition
//! does not change while it exists (lukacf/meerkat#2184); one destroyed and
//! created again under its id moves the epoch too.

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use meerkat_mob::{MobDefinition, MobHandle, MobId, Profile, ProfileName};
use meerkat_mob_mcp::MobMcpState;

/// How long a lookup waits for its own read of the definitions. The read
/// is the engine's in-memory snapshot of its mob handles (a read lock and
/// a clone of each handle); the bound only ends a read that a writer of
/// the mob set holds up, and the spawn is then refused as not known.
const READ_NOW: Duration = Duration::from_secs(5);

/// What a mob's definition says of one profile name.
#[derive(Debug, Clone, PartialEq)]
pub enum Stored {
    Profile(Box<Profile>),
    /// The definition has no inline profile of the name (the engine refuses
    /// the spawn, naming it).
    Missing,
    /// The mob is not known here.
    Unknown,
}

/// The definitions as read, and the mob set's epoch they were read at
/// (`None`: never read from the state).
#[derive(Debug, Default)]
struct Read {
    epoch: Option<u64>,
    definitions: HashMap<MobId, Arc<MobDefinition>>,
}

/// The definitions of the mobs the daemon hosts, by mob.
#[derive(Debug, Default)]
pub struct MobProfiles {
    read: parking_lot::RwLock<Read>,
    /// The state the definitions are read from ([`Self::bind`]).
    state: crate::late::Late<Weak<MobMcpState>>,
}

/// The name of the profiles' handle the daemon's start-up binds
/// ([`crate::late`]).
const STATE: &str = "the mob profiles' mob host";

impl MobProfiles {
    /// Read the definitions from `state` (once; a second state is
    /// ignored).
    pub fn bind(&self, state: &Arc<MobMcpState>) {
        let _ = self.state.set(Arc::downgrade(state));
    }

    /// Whether [`Self::bind`] was called ([`crate::late`]).
    #[cfg(any(test, feature = "test-support"))]
    pub fn is_bound(&self) -> bool {
        self.state.is_bound()
    }

    /// The profile `name` of the mob `mob`: from the definitions as read,
    /// read again now when the set of mobs changed since.
    #[must_use]
    pub fn lookup(&self, mob: &MobId, name: &str) -> Stored {
        if let Some(state) = self.state.get(STATE).and_then(Weak::upgrade) {
            let epoch = *state.mob_set_changes().borrow();
            if self.read.read().epoch != Some(epoch) {
                self.read_now(&state, epoch);
            }
        }
        let read = self.read.read();
        let Some(definition) = read.definitions.get(mob) else {
            return Stored::Unknown;
        };
        definition
            .resolve_inline_profile(&ProfileName::from(name))
            .map_or(Stored::Missing, |profile| {
                Stored::Profile(Box::new(profile.clone()))
            })
    }

    /// Know `definition` (its id's earlier one replaced).
    #[cfg(any(test, feature = "test-support"))]
    pub fn insert(&self, definition: MobDefinition) {
        self.read
            .write()
            .definitions
            .insert(definition.id.clone(), Arc::new(definition));
    }

    /// Know exactly the mobs of `handles`, read at `epoch` (unless a read
    /// of a later epoch is known already).
    fn replace(&self, epoch: u64, handles: &[(MobId, MobHandle)]) {
        let definitions = handles
            .iter()
            .map(|(id, handle)| (id.clone(), Arc::new(handle.definition().clone())))
            .collect();
        let mut read = self.read.write();
        if read.epoch.is_some_and(|known| known > epoch) {
            return;
        }
        *read = Read {
            epoch: Some(epoch),
            definitions,
        };
    }

    /// Read `state`'s definitions on the caller's thread, the mob set at
    /// `epoch` or later. The caller is the spawn customizer, which the
    /// engine calls synchronously on the mob's actor task, a worker of the
    /// multi-thread runtime: `block_in_place` hands that worker's other
    /// tasks to another thread while this one waits, so nothing else
    /// stalls, and the wait is bounded ([`READ_NOW`]). The read needs
    /// nothing of the actor that waits: it takes the state's lock of the
    /// mob set, which a spawn does not hold. Outside a multi-thread
    /// runtime nothing is read (a unit test's lookup sees what was
    /// inserted).
    fn read_now(&self, state: &MobMcpState, epoch: u64) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        if runtime.runtime_flavor() != tokio::runtime::RuntimeFlavor::MultiThread {
            return;
        }
        let read = tokio::task::block_in_place(|| {
            runtime.block_on(tokio::time::timeout(READ_NOW, state.mob_handles_snapshot()))
        });
        match read {
            Ok(Ok(handles)) => self.replace(epoch, &handles),
            Ok(Err(err)) => tracing::warn!(error = %err, "cannot read the mobs' profiles"),
            Err(_) => tracing::warn!(
                bound = ?READ_NOW,
                "the mobs' profiles did not read in time: a spawn by a profile's name may be refused"
            ),
        }
    }

    /// Read the definitions of `state`'s mobs now and each time its set of
    /// mobs changes, until the state is gone.
    pub async fn follow(self: Arc<Self>, state: Weak<MobMcpState>) {
        let Some(mut changes) = state.upgrade().map(|state| state.mob_set_changes()) else {
            return;
        };
        loop {
            let epoch = *changes.borrow_and_update();
            let Some(live) = state.upgrade() else {
                return;
            };
            match live.mob_handles_snapshot().await {
                Ok(handles) => self.replace(epoch, &handles),
                Err(err) => tracing::warn!(error = %err, "cannot read the mobs' profiles"),
            }
            drop(live);
            if changes.changed().await.is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_profile_is_found_missing_or_its_mob_unknown() {
        let profiles = MobProfiles::default();
        let team = MobId::from("team");
        assert_eq!(profiles.lookup(&team, "worker"), Stored::Unknown);
        let mut definition =
            MobDefinition::implicit_with_provider("team", "claude-haiku-4-5", None, None);
        definition.id = team.clone();
        let name = definition.profiles.keys().next().unwrap().to_string();
        profiles.insert(definition);
        let Stored::Profile(profile) = profiles.lookup(&team, &name) else {
            panic!("{:?}", profiles.lookup(&team, &name));
        };
        assert_eq!(profile.model, "claude-haiku-4-5");
        assert_eq!(profiles.lookup(&team, "nobody"), Stored::Missing);
    }

    #[test]
    fn an_older_read_does_not_replace_a_newer_one() {
        let profiles = MobProfiles::default();
        profiles.replace(3, &[]);
        let mut definition = MobDefinition::implicit_with_provider("team", "m", None, None);
        definition.id = MobId::from("team");
        let name = definition.profiles.keys().next().unwrap().to_string();
        profiles.insert(definition);
        profiles.replace(2, &[]);
        assert_ne!(
            profiles.lookup(&MobId::from("team"), &name),
            Stored::Unknown
        );
        profiles.replace(3, &[]);
        assert_eq!(
            profiles.lookup(&MobId::from("team"), &name),
            Stored::Unknown
        );
    }
}
