//! The faults tests inject into what the daemon's shared handles do: one
//! [`CoreProbe`] in [`super::Core`], read at each point it fails.

/// How one steer of a helper's note fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteFault {
    /// Refused before it reaches the engine: the agent never reads it.
    Refused,
    /// Sent, and then answered with an error (the engine took it).
    Late,
}

/// The faults tests queue for the daemon's member reloads and notes.
#[derive(Debug, Default)]
pub struct CoreProbe {
    /// How many of the next member reloads fail
    /// ([`crate::daemon::storage::reload`]).
    reload_faults: std::sync::atomic::AtomicU32,
    /// How the next steers of a helper's note fail
    /// ([`crate::daemon::agents::Agents::steer`]).
    pub note_faults: parking_lot::Mutex<Vec<NoteFault>>,
}

impl CoreProbe {
    /// The next `count` member reloads fail.
    pub fn fail_reloads(&self, count: u32) {
        self.reload_faults
            .store(count, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether this member reload fails (one fewer is left to fail).
    pub(crate) fn reload_fails(&self) -> bool {
        self.reload_faults
            .fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |left| left.checked_sub(1),
            )
            .is_ok()
    }

    /// A steer of a note (of a helper's ending: `helper_ended`) is about
    /// to reach the engine: the refusal of a queued [`NoteFault::Refused`],
    /// else whether its answer is to come back late ([`Self::answer`]).
    pub(crate) fn steering(&self, helper_ended: bool) -> Result<bool, String> {
        let fault = if helper_ended {
            let mut faults = self.note_faults.lock();
            (!faults.is_empty()).then(|| faults.remove(0))
        } else {
            None
        };
        match fault {
            Some(NoteFault::Refused) => Err("the note was refused on purpose (test)".to_string()),
            Some(NoteFault::Late) => Ok(true),
            None => Ok(false),
        }
    }

    /// The engine's answer `steered` to a steer, failed when [`Self::steering`]
    /// said it comes back `late`.
    pub(crate) fn answer(steered: Result<(), String>, late: bool) -> Result<(), String> {
        match steered {
            Ok(()) if late => Err("the note was answered late on purpose (test)".to_string()),
            steered => steered,
        }
    }
}
