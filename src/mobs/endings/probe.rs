//! What tests hold and observe in how helpers end: one [`EndingsProbe`]
//! in [`HelperEndings`] and one [`RetirementsProbe`] in [`Retirements`],
//! one call at each point they hold, and the methods the tests call.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;

use super::{HelperEndings, HelperKey, Retirements};

/// Tests: what [`HelperEndings::before_deciding`] awaits for a helper.
pub type DecidingHookFn = Arc<
    dyn Fn(HelperKey) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

/// [`DecidingHookFn`], held.
struct DecidingHook(DecidingHookFn);

impl std::fmt::Debug for DecidingHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DecidingHook")
    }
}

/// Await `hook`, when there is one, with `key`.
pub(super) async fn run(hook: Option<DecidingHookFn>, key: &HelperKey) {
    if let Some(hook) = hook {
        hook(key.clone()).await;
    }
}

/// The holds tests put on the endings.
#[derive(Debug, Default)]
pub(super) struct EndingsProbe {
    /// Awaited before a helper's going idle is decided.
    before_deciding: OnceLock<DecidingHook>,
    /// Awaited before a finished `assign` helper is retired.
    before_retiring: OnceLock<DecidingHook>,
    /// The daemon's delivery holds every batch of endings open until the
    /// daemon stops ([`HelperEndings::hold_batches`]).
    batches_held: AtomicBool,
}

impl EndingsProbe {
    /// A helper's going idle is about to be decided.
    pub(super) async fn deciding(&self, key: &HelperKey) {
        run(
            self.before_deciding.get().map(|hook| Arc::clone(&hook.0)),
            key,
        )
        .await;
    }

    /// What a retire of a finished helper awaits first (taken before the
    /// retire's own task starts).
    pub(super) fn retiring(&self) -> Option<DecidingHookFn> {
        self.before_retiring.get().map(|hook| Arc::clone(&hook.0))
    }
}

/// The holds and refusals tests put on the retirements.
#[derive(Debug, Default)]
pub(super) struct RetirementsProbe {
    /// Awaited inside a retire, just before it reaches the engine.
    before_engine: OnceLock<DecidingHook>,
    /// The helpers whose retire is refused, with the refusal.
    refuse: Mutex<HashMap<HelperKey, String>>,
}

impl RetirementsProbe {
    /// A retire of `key` is about to reach the engine: its refusal, when
    /// a test set one.
    pub(super) async fn reaching_engine(&self, key: &HelperKey) -> Result<(), String> {
        run(
            self.before_engine.get().map(|hook| Arc::clone(&hook.0)),
            key,
        )
        .await;
        match self.refuse.lock().get(key) {
            Some(refusal) => Err(refusal.clone()),
            None => Ok(()),
        }
    }
}

impl Retirements {
    /// Tests: whether the daemon is stopping (no retire starts).
    #[must_use]
    pub fn closed(&self) -> bool {
        self.gate.borrow().0
    }

    /// Tests: refuse every retire of `key` with `refusal`, as an engine
    /// that keeps refusing would (`None`: let them through again).
    pub fn refuse(&self, key: &HelperKey, refusal: Option<&str>) {
        let mut refused = self.probe.refuse.lock();
        match refusal {
            Some(refusal) => {
                refused.insert(key.clone(), refusal.to_string());
            }
            None => {
                refused.remove(key);
            }
        }
    }

    /// Tests: await `hook` (with the helper's key) inside each retire, just
    /// before it reaches the engine (once).
    pub fn before_engine(&self, hook: DecidingHookFn) {
        let _ = self.probe.before_engine.set(DecidingHook(hook));
    }
}

impl HelperEndings {
    /// Tests: await `hook` (with the helper's key) before each decision
    /// of a helper's going idle (once).
    pub fn before_deciding(&self, hook: DecidingHookFn) {
        let _ = self.test_probe.before_deciding.set(DecidingHook(hook));
    }

    /// Tests: await `hook` (with the helper's key) before a finished
    /// `assign` helper is retired (once).
    pub fn before_retiring(&self, hook: DecidingHookFn) {
        let _ = self.test_probe.before_retiring.set(DecidingHook(hook));
    }

    /// Tests: the daemon's delivery holds every batch of endings open
    /// until the daemon stops, as if each stop came within its window.
    pub fn hold_batches(&self) {
        self.test_probe.batches_held.store(true, Ordering::SeqCst);
    }

    /// Tests: whether the delivery holds its batches
    /// ([`Self::hold_batches`]).
    #[must_use]
    pub fn batches_held(&self) -> bool {
        self.test_probe.batches_held.load(Ordering::SeqCst)
    }

    /// Tests: how many retires these endings started so far.
    #[must_use]
    pub fn retires(&self) -> usize {
        self.retires.load(Ordering::Relaxed)
    }

    /// Tests: whether `key` is followed and not ended yet.
    #[must_use]
    pub fn is_tracked(&self, key: &HelperKey) -> bool {
        self.is_live(key)
    }

    /// Tests: whether `key`'s ending was settled (told or not).
    #[must_use]
    pub fn settled(&self, key: &HelperKey) -> bool {
        self.is_settled(key)
    }

    /// Tests: how many fork outcomes on record were reported so far (each
    /// fork's own once: the follower's live event or its look at start).
    #[must_use]
    pub fn outcomes_reported(&self) -> usize {
        self.outcomes_reported.load(Ordering::Relaxed)
    }

    /// Tests: how many runs' ends were decided as a helper's going idle.
    #[must_use]
    pub fn idle_decided(&self) -> usize {
        self.idle_decided.load(Ordering::Relaxed)
    }

    /// Tests: how many endings were told so far.
    #[must_use]
    pub fn told(&self) -> usize {
        self.told.load(Ordering::Relaxed)
    }
}
