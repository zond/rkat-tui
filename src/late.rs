//! Handles a holder gets once, after it was made: the daemon's start-up
//! binds them (`daemon::engine`'s `bind_late`, `daemon`'s
//! `bind_agents`), since what they reach is made after their holder.
//!
//! An unbound handle does nothing (a hook not called, a lookup that
//! finds nothing), which a correctly started daemon never meets; a
//! binding forgotten there would be silent. So each [`Late`] says the
//! first time it is reached unbound, in one warning naming it, and the
//! daemon's own check (`daemon::local::probe`'s `unbound_for_test`) lists every
//! one that is unbound, which the daemon's start-up test holds empty.
//! Unit tests make holders unbound on purpose: the warning is all they
//! cause.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

/// A handle bound once at start-up ([module](self)).
pub(crate) struct Late<T> {
    cell: OnceLock<T>,
    warned: AtomicBool,
}

impl<T> Default for Late<T> {
    fn default() -> Self {
        Self {
            cell: OnceLock::new(),
            warned: AtomicBool::new(false),
        }
    }
}

impl<T> std::fmt::Debug for Late<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Late")
            .field("bound", &self.is_bound())
            .finish()
    }
}

impl<T> Late<T> {
    /// An unbound handle.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Bind it to `value`: whether this call did (`false`: it was bound
    /// already, and `value` is dropped).
    pub(crate) fn set(&self, value: T) -> bool {
        self.cell.set(value).is_ok()
    }

    pub(crate) fn is_bound(&self) -> bool {
        self.cell.get().is_some()
    }

    /// The handle; unbound, `None`, and the first time a warning that
    /// `what` was never bound.
    pub(crate) fn get(&self, what: &'static str) -> Option<&T> {
        let got = self.cell.get();
        if got.is_none() {
            warn_once(&self.warned, what);
        }
        got
    }
}

/// Warn, the first time `flag` sees it, that the handle `what` was never
/// bound at start-up.
pub(crate) fn warn_once(flag: &AtomicBool, what: &'static str) {
    if !flag.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            binding = what,
            "{what} was never bound at the daemon's start-up: what it does is skipped"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_late_handle_is_bound_once_and_says_so() {
        let late = Late::<u32>::new();
        assert!(!late.is_bound());
        assert_eq!(late.get("the number"), None);
        assert!(late.warned.load(Ordering::Relaxed), "the first miss warns");
        assert!(late.set(4));
        assert!(!late.set(5), "bound once");
        assert_eq!(late.get("the number"), Some(&4));
        assert!(late.is_bound());
    }
}
