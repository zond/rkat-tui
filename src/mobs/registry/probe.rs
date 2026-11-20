//! What tests set and observe of the helper registry: one
//! [`RegistryProbe`] in [`HelperRegistry`], and the methods tests call.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::{HelperBindings, HelperRegistry, HelperStatus};

/// The registry's test switches and counts.
#[derive(Debug, Default)]
pub(super) struct RegistryProbe {
    /// Forks are wired as their spawn is announced
    /// ([`HelperRegistry::wire_forks_early`]).
    forks_early: AtomicBool,
    /// How often a fork's wire found it not seated yet.
    forks_not_seated: AtomicUsize,
}

impl HelperRegistry {
    /// An empty registry, its bindings in memory only.
    #[must_use]
    pub fn in_memory() -> Self {
        Self::with_bindings(HelperBindings::in_memory())
    }

    /// Tests: wire every fork as its spawn is announced, before the
    /// follower opens its stream (so before its spawn is built and it is
    /// seated): what a slow spawn does to the order.
    pub fn wire_forks_early(&self) {
        self.probe.forks_early.store(true, Ordering::SeqCst);
    }

    /// Tests: whether forks are wired early ([`Self::wire_forks_early`]).
    #[must_use]
    pub fn wires_forks_early(&self) -> bool {
        self.probe.forks_early.load(Ordering::SeqCst)
    }

    /// Tests: a fork's wire found it not seated yet.
    pub fn fork_not_seated(&self) {
        self.probe.forks_not_seated.fetch_add(1, Ordering::Relaxed);
    }

    /// Tests: how often a fork's wire found it not seated yet.
    #[must_use]
    pub fn forks_not_seated(&self) -> usize {
        self.probe.forks_not_seated.load(Ordering::Relaxed)
    }

    /// Running helpers.
    #[must_use]
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
}
