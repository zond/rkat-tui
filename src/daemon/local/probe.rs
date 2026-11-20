//! What tests observe and hold in the local methods: one [`Probe`] in
//! [`Local`], one call at each point it observes, and the methods the
//! tests call.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::Local;
use crate::daemon::protocol::Interrupted;

/// The local methods' test observations and holds.
#[derive(Debug, Default)]
pub(super) struct Probe {
    /// How many times each engine method was forwarded.
    forwarded: parking_lot::Mutex<HashMap<String, usize>>,
    /// A mode change waits on it before its note.
    mode_hold: Arc<tokio::sync::RwLock<()>>,
    /// Every `rkat/interrupt`'s answer, in order.
    interrupts: parking_lot::Mutex<Vec<Interrupted>>,
    /// How many prompts waited for a mode change.
    mode_waits: AtomicU64,
}

impl Probe {
    /// The proxy forwards a request of the engine method `method`.
    pub(super) fn forwarding(&self, method: &str) {
        *self.forwarded.lock().entry(method.to_string()).or_insert(0) += 1;
    }

    /// A mode change is about to note itself: held while a test holds
    /// [`Local::hold_mode_notes`].
    pub(super) async fn mode_note(&self) -> tokio::sync::RwLockReadGuard<'_, ()> {
        self.mode_hold.read().await
    }

    /// A prompt waits for a mode change to end.
    pub(super) fn mode_wait(&self) {
        self.mode_waits.fetch_add(1, Ordering::SeqCst);
    }

    /// An `rkat/interrupt` answered `answer`.
    pub(super) fn interrupted(&self, answer: &Interrupted) {
        self.interrupts.lock().push(answer.clone());
    }
}

impl Local {
    /// Tests: hold every mode change before its note until the guard is
    /// dropped.
    pub async fn hold_mode_notes(&self) -> tokio::sync::OwnedRwLockWriteGuard<()> {
        Arc::clone(&self.probe.mode_hold).write_owned().await
    }

    /// Tests: how many prompts waited for a mode change to end.
    #[must_use]
    pub fn mode_waits(&self) -> u64 {
        self.probe.mode_waits.load(Ordering::SeqCst)
    }

    /// Tests: every interrupt's answer, in order.
    #[must_use]
    pub fn interrupts(&self) -> Vec<Interrupted> {
        self.probe.interrupts.lock().clone()
    }

    /// How many requests of the engine method `method` the clients sent
    /// through the proxy (tests: what a client polls).
    #[must_use]
    pub fn forwarded(&self, method: &str) -> usize {
        self.probe
            .forwarded
            .lock()
            .get(method)
            .copied()
            .unwrap_or(0)
    }

    /// Tests: leave `agent`'s session as a failed durable commit does
    /// (the engine's own test hook: its registration needs a reload, so
    /// the next send is refused with `MemberReloadRequired`). Whether it
    /// did. Meant for an agent with no run behind it: applied while a
    /// run's terminal is still being written, it fails that write too
    /// (`tests/it/storage.rs`).
    pub async fn degrade_for_test(&self, agent: &str) -> bool {
        let Some(session) = self.core.conversations.current(agent) else {
            return false;
        };
        self.parts
            .machine
            .force_session_durability_reload_required_for_test(&session)
            .await
    }

    /// Tests: the next `count` member reloads fail
    /// ([`crate::daemon::storage::reload`]).
    pub fn fail_reloads_for_test(&self, count: u32) {
        self.core.probe.fail_reloads(count);
    }

    /// Tests: a message of the daemon's own to `agent`, as its notes go
    /// (held for an open run, [`crate::daemon::agents::hold_or_steer`]);
    /// answered once the engine took it.
    ///
    /// # Errors
    ///
    /// The engine's refusal, as text.
    pub async fn steer_for_test(&self, agent: &str, text: &str) -> Result<(), String> {
        crate::daemon::agents::hold_or_steer(&self.core, agent, text.to_string())
            .await
            .outcome()
            .await
    }

    /// Tests: how many of the daemon's notes are held for `agent`'s open
    /// run ([`crate::daemon::activity::HeldNote`]).
    #[must_use]
    pub fn held_notes_for_test(&self, agent: &str) -> usize {
        self.core.activity.held_notes(agent)
    }
}
