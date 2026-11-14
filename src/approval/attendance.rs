//! Unattended turns (A6 of the security review, the user's decision of
//! 2026-11-10, changed by the owner on 2026-11-14).
//!
//! A turn no human started (another agent's message woke the agent, a
//! schedule or a file watch) runs unattended unless a client is attached
//! to the agent at the moment of a call. In an unattended turn a call auto
//! mode's classifier reviews is still reviewed: an allow runs it, a block
//! denies it. A call that would put a question to the human (a protected
//! path, a question of manual or accept-edits mode, a paused auto mode, an
//! action too large to review, a review that could not be completed) is
//! denied with [`super::DenyReason::Unattended`]; the agent reads in the
//! tool result that the action needs a user attached to it, and a notice
//! is queued ([`Attendance::missed`]) that the agent's next attached client
//! shows. Reads, Tier-1 shell commands and the in-project writes the mode
//! allows without review run as before; plan mode stays plan mode.
//!
//! What counts as attended, all of it positive evidence (a check that
//! cannot tell counts as unattended):
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
//! user's own runs put questions to the user there.

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

/// Which kind of agent a comms send goes to ([`AgentPeers::agent_peer`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerKind {
    /// Another agent of this workspace (its permission mode is this one's).
    Workspace,
    /// One of the sender's own helpers.
    OwnHelper,
    /// An agent of another workspace the sender is wired to (`/wire`),
    /// by its label (`<workspace>/<agent>`): its workspace has its own
    /// permission mode.
    Wired(String),
}

impl PeerKind {
    /// The recipient in words, as an allow's detail and the audit log say
    /// it.
    #[must_use]
    pub fn words(&self) -> String {
        match self {
            Self::Workspace => "a message to another agent of this workspace".to_string(),
            Self::OwnHelper => "a message to your own helper".to_string(),
            Self::Wired(label) => {
                format!("a message to {label}, an agent of another workspace")
            }
        }
    }
}

/// The workspace's agents as comms peers: a message one agent sends
/// another changes nothing in the tree (the recipient's own calls pass the
/// gate), so the gate lets it through without review.
#[async_trait::async_trait]
pub trait AgentPeers: Send + Sync {
    /// Which kind of agent peer `peer_id` (as the comms tools name a peer)
    /// is to the agent running `session`: another live agent of this
    /// workspace, one of its own helpers, or an agent of another workspace
    /// it is wired to; `None`: none of them.
    async fn agent_peer(&self, session: &SessionId, peer_id: &str) -> Option<PeerKind>;

    /// Why a message from `session` to `peer_id` cannot arrive now, in
    /// words for the agent (an agent of another workspace whose daemon is
    /// not running); `None`: it can, or `peer_id` is no such agent.
    async fn unreachable(&self, _session: &SessionId, _peer_id: &str) -> Option<String> {
        None
    }
}
