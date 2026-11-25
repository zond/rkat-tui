//! The calls the gate recently blocked that the user can approve
//! (`/permissions`, [`super::grants`]), and the approvals once: per session,
//! the newest [`MAX_RECENT_BLOCKS`] entries, each a blocked call or a call
//! approved once that waits for the agent to run it again. An approval
//! once stays on the list, marked ([`BlockedCall::approved_once`]), until
//! the next identical call of the session takes it (the call passes and
//! the entry goes), the user withdraws it (the entry is a blocked call
//! again) or the session is forgotten. The cap counts every entry: an
//! approval once that ten newer blocks push off the list lapses with it.
//!
//! Locks: the per-session map's, alone.

use std::collections::{HashMap, VecDeque};

use meerkat_core::SessionId;
use parking_lot::Mutex;

use super::engine::CacheKey;
use super::grants::{BlockedCall, GrantScope, MAX_RECENT_BLOCKS};

/// A blocked call as the gate keeps it: what `/permissions` shows, and the
/// key an approval once matches.
#[derive(Debug, Clone)]
pub(super) struct Blocked {
    pub(super) call: BlockedCall,
    pub(super) key: CacheKey,
}

#[derive(Debug, Default)]
struct SessionBlocks {
    /// Oldest first; an entry approved once has `call.approved_once`.
    recent: VecDeque<Blocked>,
}

/// The recent blocks and the approvals once (see the [module docs](self)).
#[derive(Debug, Default)]
pub struct Blocks {
    sessions: Mutex<HashMap<SessionId, SessionBlocks>>,
}

/// Why a listed call cannot be acted on.
fn not_listed(id: u64) -> String {
    format!("no blocked call #{id} (`/permissions` lists them)")
}

impl Blocks {
    /// Keep `blocked` for `session`, in place of an earlier block of the
    /// identical call (one row per call: a call blocked again after its
    /// approval was withdrawn is listed once); the oldest entry goes beyond
    /// [`MAX_RECENT_BLOCKS`], an approval once waiting there too.
    pub(super) fn remember(&self, session: &SessionId, blocked: Blocked) {
        let mut sessions = self.sessions.lock();
        let recent = &mut sessions.entry(session.clone()).or_default().recent;
        recent.retain(|known| known.call.approved_once || known.key != blocked.key);
        if recent.len() == MAX_RECENT_BLOCKS {
            recent.pop_front();
        }
        recent.push_back(blocked);
    }

    /// The entries of `session`, oldest first: the calls the gate recently
    /// blocked that the user can approve, and the calls approved once that
    /// wait for the agent ([`BlockedCall::approved_once`]).
    #[must_use]
    pub fn list(&self, session: &SessionId) -> Vec<BlockedCall> {
        self.sessions
            .lock()
            .get(session)
            .map(|state| state.recent.iter().map(|b| b.call.clone()).collect())
            .unwrap_or_default()
    }

    /// Approve blocked call `id` of `session` for `scope`: once, it stays
    /// listed, marked, until the identical call takes it
    /// ([`Self::take_approved`]); for a grant it leaves the list. Returns
    /// the call.
    ///
    /// # Errors
    ///
    /// No such call, one approved once already (withdraw that first), or
    /// (for a scope other than once) no grant fits it: then it stays as it
    /// was.
    pub(super) fn approve(
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
            return Err(not_listed(id));
        };
        if state.recent[at].call.approved_once {
            return Err(waiting(id));
        }
        if scope == GrantScope::Once {
            let entry = &mut state.recent[at];
            // Nobody could have read all of it: an approval once approves
            // the whole call.
            if let Some(bytes) = entry.call.call_bytes {
                return Err(too_long_to_approve(id, bytes));
            }
            entry.call.approved_once = true;
            return Ok(entry.clone());
        }
        if state.recent[at].call.grant.is_none() {
            return Err(format!(
                "no grant fits blocked call #{id}; approve it once instead"
            ));
        }
        state.recent.remove(at).ok_or_else(|| not_listed(id))
    }

    /// Blocked call `id` of `session`, listed and not approved once.
    ///
    /// # Errors
    ///
    /// No such call, or one approved once already.
    pub(super) fn blocked(&self, session: &SessionId, id: u64) -> Result<BlockedCall, String> {
        let sessions = self.sessions.lock();
        let entry = sessions
            .get(session)
            .and_then(|state| state.recent.iter().find(|b| b.call.id == id));
        match entry {
            None => Err(not_listed(id)),
            Some(blocked) if blocked.call.approved_once => Err(waiting(id)),
            Some(blocked) => Ok(blocked.call.clone()),
        }
    }

    /// Take blocked call `id` of `session` off the list.
    pub(super) fn drop_call(&self, session: &SessionId, id: u64) {
        if let Some(state) = self.sessions.lock().get_mut(session) {
            state.recent.retain(|blocked| blocked.call.id != id);
        }
    }

    /// Withdraw the approval once of call `id` of `session`: it is a
    /// blocked call again, which the user may approve otherwise. Returns
    /// the call.
    ///
    /// # Errors
    ///
    /// No such call, or one not approved once.
    pub(super) fn withdraw(&self, session: &SessionId, id: u64) -> Result<BlockedCall, String> {
        let mut sessions = self.sessions.lock();
        let entry = sessions
            .get_mut(session)
            .and_then(|state| state.recent.iter_mut().find(|b| b.call.id == id));
        match entry {
            None => Err(format!("no call #{id} in `/permissions`' Blocked list")),
            Some(blocked) if !blocked.call.approved_once => Err(format!(
                "call #{id} is not approved once; there is nothing to withdraw"
            )),
            Some(blocked) => {
                blocked.call.approved_once = false;
                Ok(blocked.call.clone())
            }
        }
    }

    /// Whether the user approved the call with `key` once for `session`;
    /// the approval is used up and its entry leaves the list.
    pub(super) fn take_approved(&self, session: &SessionId, key: &CacheKey) -> bool {
        let mut sessions = self.sessions.lock();
        let Some(state) = sessions.get_mut(session) else {
            return false;
        };
        let Some(at) = state
            .recent
            .iter()
            .position(|blocked| blocked.call.approved_once && &blocked.key == key)
        else {
            return false;
        };
        state.recent.remove(at);
        true
    }

    /// Forget `session`'s blocked calls and approvals.
    pub(super) fn forget(&self, session: &SessionId) {
        self.sessions.lock().remove(session);
    }
}

/// Why blocked call `id`, `bytes` long, is not approved once: more of it
/// than can be shown.
pub(crate) fn too_long_to_approve(id: u64, bytes: usize) -> String {
    format!(
        "call #{id} is {bytes} bytes, more than /permissions can show ({} at most), so nobody could have read all of it: it cannot be approved once; ask the agent for a smaller call, or allow a kind of effect (e) or a grant (s/a) you can judge",
        super::prompts::MAX_REVIEWED_ACTION_BYTES
    )
}

/// Why call `id`, approved once already, is not approved again.
fn waiting(id: u64) -> String {
    format!(
        "call #{id} is approved once already and waits for the agent to run it again; withdraw the approval (w) to approve it otherwise"
    )
}
