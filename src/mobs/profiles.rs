//! The profiles of the mobs the daemon hosts, as the spawn customizer reads
//! them: a member seated by a profile's name gets that profile inline with
//! its model, provider and server replaced
//! ([`super::customizer`]), as the engine lets a spawn replace only the
//! model of a profile it names (`model_override`), and the provider and
//! server a stored profile declares would stay.
//!
//! The customizer is synchronous and the engine hands it no profile, so the
//! definitions are kept here: [`MobProfiles::follow`] reads every mob's
//! definition again each time the set of mobs changes. A mob's definition
//! does not change while it exists (lukacf/meerkat#2184), and a member can
//! be spawned only into a mob that exists, whose create has changed the set
//! before its tool call returned.

use std::collections::HashMap;
use std::sync::{Arc, Weak};

use meerkat_mob::{MobDefinition, MobHandle, MobId, Profile, ProfileName};
use meerkat_mob_mcp::MobMcpState;

/// What a mob's definition says of one profile name.
#[derive(Debug, Clone, PartialEq)]
pub enum Stored {
    /// Its profile.
    Profile(Box<Profile>),
    /// The definition has no inline profile of the name (the engine refuses
    /// the spawn, naming it).
    Missing,
    /// The mob is not known here.
    Unknown,
}

/// The definitions of the mobs the daemon hosts, by mob.
#[derive(Debug, Default)]
pub struct MobProfiles {
    definitions: parking_lot::RwLock<HashMap<MobId, Arc<MobDefinition>>>,
}

impl MobProfiles {
    /// The profile `name` of the mob `mob`.
    #[must_use]
    pub fn lookup(&self, mob: &MobId, name: &str) -> Stored {
        let definitions = self.definitions.read();
        let Some(definition) = definitions.get(mob) else {
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
        self.definitions
            .write()
            .insert(definition.id.clone(), Arc::new(definition));
    }

    /// Know exactly the mobs of `handles`.
    fn replace(&self, handles: &[(MobId, MobHandle)]) {
        let next = handles
            .iter()
            .map(|(id, handle)| (id.clone(), Arc::new(handle.definition().clone())))
            .collect();
        *self.definitions.write() = next;
    }

    /// Read the definitions of `state`'s mobs now and each time its set of
    /// mobs changes, until the state is gone.
    pub async fn follow(self: Arc<Self>, state: Weak<MobMcpState>) {
        let Some(mut changes) = state.upgrade().map(|state| state.mob_set_changes()) else {
            return;
        };
        loop {
            changes.borrow_and_update();
            let Some(live) = state.upgrade() else {
                return;
            };
            match live.mob_handles_snapshot().await {
                Ok(handles) => self.replace(&handles),
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
}
