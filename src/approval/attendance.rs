//! Unattended turns (A6 of the security review, the user's decision of
//! 2026-11-10).
//!
//! A turn no human started (another agent's message woke the agent; later a
//! schedule) runs unattended unless a client is attached to the agent at
//! the moment of a call. In an unattended turn a call that would need review
//! (the classifier's, or a question to the human: anything above Tier 1 that
//! no allow rule covers, and an edit the mode does not allow on its own) is
//! denied with [`super::DenyReason::Unattended`], without a classifier
//! request; the agent reads in the tool result that the action needs a user
//! attached to it, and a notice is queued ([`Attendance::missed`]) that the
//! agent's next attached client shows. Reads, Tier-1 shell commands and the
//! in-project writes the mode allows without review run as before.
//!
//! What counts as attended, all of it positive evidence (a check that
//! cannot tell denies):
//!
//! - a client is attached to the agent now ([`Attendance::attached`]);
//!   attaching mid-turn makes the turn's later calls attended;
//! - the call belongs to a run the engine saw start after a client's input
//!   to the session ([`super::ApprovalEngine::human_input`] and
//!   [`super::ApprovalEngine::run_started`]): the user started it and may
//!   have detached since (a detached agent keeps running, D2);
//! - a client's input is waiting for its run and the call's run is not
//!   known to have started yet (the event watcher has not seen it): that
//!   run is the user's.
//!
//! The daemon installs an [`Attendance`]
//! ([`super::ApprovalEngine::set_attendance`]) before any agent runs; an
//! engine without one counts nobody as attached (fail closed), so only the
//! user's own runs are reviewed there.

use meerkat_core::SessionId;

/// Who is with an agent, as the daemon knows it.
pub trait Attendance: Send + Sync {
    /// Whether a client is attached to the agent whose conversation is
    /// `session` now. A session no live agent runs as is not attended.
    fn attached(&self, session: &SessionId) -> bool;

    /// An unattended call of `session` was denied: queue `notice` for the
    /// agent's next attached client.
    fn missed(&self, session: &SessionId, notice: String);
}

/// The workspace's agents as comms peers: a message one agent sends
/// another changes nothing in the tree (the recipient's own calls pass the
/// gate), so the gate lets it through without review.
#[async_trait::async_trait]
pub trait AgentPeers: Send + Sync {
    /// Whether `peer_id` (as the comms tools name a peer) is a live agent
    /// of this workspace other than the one running `session`.
    async fn is_agent_peer(&self, session: &SessionId, peer_id: &str) -> bool;
}
