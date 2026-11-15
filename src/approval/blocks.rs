//! The calls the gate recently blocked that the user can approve
//! (`/approve`, [`super::grants`]), and the approvals once: per session,
//! the newest [`MAX_RECENT_BLOCKS`] blocked calls, and the keys of the
//! calls approved once (the next identical call passes and uses the
//! approval up).
//!
//! Locks: the per-session map's, alone.

use std::collections::{HashMap, HashSet, VecDeque};

use meerkat_core::SessionId;
use parking_lot::Mutex;

use super::engine::CacheKey;
use super::grants::{BlockedCall, GrantScope, MAX_RECENT_BLOCKS};

/// A blocked call as the gate keeps it: what `/approve` shows, and the
/// key an approval once matches.
#[derive(Debug, Clone)]
pub(super) struct Blocked {
    pub(super) call: BlockedCall,
    pub(super) key: CacheKey,
}

#[derive(Debug, Default)]
struct SessionBlocks {
    /// Oldest first.
    recent: VecDeque<Blocked>,
    approved_once: HashSet<CacheKey>,
}

/// The recent blocks and the approvals once (see the [module docs](self)).
#[derive(Debug, Default)]
pub struct Blocks {
    sessions: Mutex<HashMap<SessionId, SessionBlocks>>,
}

impl Blocks {
    /// Keep `blocked` for `session`; the oldest goes beyond
    /// [`MAX_RECENT_BLOCKS`].
    pub(super) fn remember(&self, session: &SessionId, blocked: Blocked) {
        let mut sessions = self.sessions.lock();
        let recent = &mut sessions.entry(session.clone()).or_default().recent;
        if recent.len() == MAX_RECENT_BLOCKS {
            recent.pop_front();
        }
        recent.push_back(blocked);
    }

    /// The calls of `session` the gate recently blocked that the user can
    /// approve, oldest first.
    #[must_use]
    pub fn list(&self, session: &SessionId) -> Vec<BlockedCall> {
        self.sessions
            .lock()
            .get(session)
            .map(|state| state.recent.iter().map(|b| b.call.clone()).collect())
            .unwrap_or_default()
    }

    /// Take blocked call `id` of `session` off the list to approve it for
    /// `scope`.
    ///
    /// # Errors
    ///
    /// No such call, or (for a scope other than once) no grant fits it:
    /// then it stays listed.
    pub(super) fn take(
        &self,
        session: &SessionId,
        id: u64,
        scope: GrantScope,
    ) -> Result<Blocked, String> {
        let mut sessions = self.sessions.lock();
        let listed = sessions.get_mut(session).and_then(|state| {
            let at = state
                .recent
                .iter()
                .position(|blocked| blocked.call.id == id)?;
            Some((state, at))
        });
        let Some((state, at)) = listed else {
            return Err(format!("no blocked call #{id} (`/approve` lists them)"));
        };
        if scope != GrantScope::Once && state.recent[at].call.grant.is_none() {
            return Err(format!(
                "no grant fits blocked call #{id}; approve it once instead"
            ));
        }
        state
            .recent
            .remove(at)
            .ok_or_else(|| format!("no blocked call #{id} (`/approve` lists them)"))
    }

    /// Whether `session` has blocked call `id` listed.
    pub(super) fn contains(&self, session: &SessionId, id: u64) -> bool {
        self.sessions
            .lock()
            .get(session)
            .is_some_and(|state| state.recent.iter().any(|b| b.call.id == id))
    }

    /// Take blocked call `id` of `session` off the list.
    pub(super) fn drop_call(&self, session: &SessionId, id: u64) {
        if let Some(state) = self.sessions.lock().get_mut(session) {
            state.recent.retain(|blocked| blocked.call.id != id);
        }
    }

    /// The next call of `session` with `key` passes once.
    pub(super) fn approve_once(&self, session: &SessionId, key: CacheKey) {
        self.sessions
            .lock()
            .entry(session.clone())
            .or_default()
            .approved_once
            .insert(key);
    }

    /// Whether the user approved the call with `key` once for `session`;
    /// the approval is used up.
    pub(super) fn take_approved(&self, session: &SessionId, key: &CacheKey) -> bool {
        self.sessions
            .lock()
            .get_mut(session)
            .is_some_and(|state| state.approved_once.remove(key))
    }

    /// Forget `session`'s blocked calls and approvals.
    pub(super) fn forget(&self, session: &SessionId) {
        self.sessions.lock().remove(session);
    }
}
