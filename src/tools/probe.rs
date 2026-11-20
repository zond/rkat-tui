//! What tests observe in the file tools' searches: one [`Probe`] that each
//! dispatcher carries into every call's scan, where it counts (and paces)
//! the entries the walk examines. Outside tests it is empty and does
//! nothing.

/// The probe a dispatcher's calls carry (empty outside tests).
#[derive(Debug, Clone, Default)]
pub(crate) struct Probe {
    /// The test's view into the walk, when one was installed.
    #[cfg(any(test, feature = "test-support"))]
    walk: Option<std::sync::Arc<WalkProbe>>,
}

#[cfg(not(any(test, feature = "test-support")))]
impl Probe {
    /// One entry examined: outside tests there is nothing to count.
    #[inline]
    pub(crate) fn entry(&self) {
        let Self {} = self;
    }
}

#[cfg(any(test, feature = "test-support"))]
pub use hooks::WalkProbe;

#[cfg(any(test, feature = "test-support"))]
mod hooks {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use super::Probe;
    use crate::tools::FileToolDispatcher;

    /// A test's view into the walk: every entry examined is counted, and
    /// each may be slowed down so a test can drop the call mid-walk.
    #[derive(Debug, Default)]
    pub struct WalkProbe {
        /// Entries examined by every scan of the dispatcher.
        pub entries: AtomicU64,
        /// How long each entry takes.
        pub delay: Duration,
    }

    impl Probe {
        /// One entry examined: counted, then paced.
        pub(crate) fn entry(&self) {
            if let Some(walk) = &self.walk {
                walk.entries.fetch_add(1, Ordering::Relaxed);
                std::thread::sleep(walk.delay);
            }
        }
    }

    impl FileToolDispatcher {
        /// The same dispatcher with its searches counted (and paced) by
        /// `probe`.
        #[must_use]
        pub fn with_walk_probe(mut self, probe: Arc<WalkProbe>) -> Self {
            self.probe = Probe { walk: Some(probe) };
            self
        }
    }
}
