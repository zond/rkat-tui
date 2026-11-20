//! Unattended turns (the owner's decision after the security review).
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
//!   [`super::ApprovalEngine::new_run_started`]): the user started it and may
//!   have detached since (a detached agent keeps running, D2);
//! - a client's input is waiting for its run and the call's run is not
//!   known to have started yet (the event watcher has not seen it): that
//!   run is the user's.
//!
//! The daemon installs an [`Attendance`]
//! ([`super::ApprovalEngine::set_attendance`]) before any agent runs; an
//! engine without one counts nobody as attached (fail closed), so only the
//! user's own runs put questions to the user there.

use std::collections::VecDeque;

use meerkat_core::{RunId, SessionId};

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
    /// The sending helper's parent: the agent that started it (`None`), or
    /// the helper that did, by its name.
    Parent(Option<String>),
    /// Another helper working for the same agent as the sending helper
    /// (by its name): a sibling their parent connected it with
    /// (`connect_helpers`), or a relative in that agent's tree.
    SameTree(String),
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
            Self::Parent(None) => "a message to your parent agent".to_string(),
            Self::Parent(Some(name)) => {
                format!("a message to `{name}`, the helper that started you")
            }
            Self::SameTree(name) => format!(
                "a message to `{name}`, another helper of the same agent (connected by their parent)"
            ),
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
    /// it is wired to; `None`: none of them. A helper's call names the
    /// helper (`helper`, its member name) beside the session it works for:
    /// its peers are its own (its parent, its helpers, its relatives).
    async fn agent_peer(
        &self,
        session: &SessionId,
        helper: Option<&str>,
        peer_id: &str,
    ) -> Option<PeerKind>;

    /// Why a message from `session` to `peer_id` cannot arrive now, in
    /// words for the agent (an agent of another workspace whose daemon is
    /// not running); `None`: it can, or `peer_id` is no such agent.
    async fn unreachable(&self, _session: &SessionId, _peer_id: &str) -> Option<String> {
        None
    }
}

/// How many run ids a session remembers to recognise a replayed start. A
/// replay arrives within a few runs of the original; the stopped run itself
/// never clears its own Stop, remembered or not.
pub(super) const RUNS_REMEMBERED: usize = 64;

/// A session's runs as the engine saw them start, and which of them a
/// user started: the evidence of attendance the engine keeps itself.
#[derive(Debug, Default)]
pub(super) struct Runs {
    /// The last [`RUNS_REMEMBERED`] run ids seen starting, oldest first,
    /// so a replayed start is recognised.
    pub(super) seen: VecDeque<RunId>,
    /// The run that started last.
    pub(super) current: Option<RunId>,
    /// A client's input to the session waits for the run that takes it
    /// (`crate::approval::ApprovalEngine::human_input`).
    pub(super) human_pending: bool,
    /// The last [`RUNS_REMEMBERED`] runs that started with a client's input
    /// waiting: runs a user started.
    pub(super) human: VecDeque<RunId>,
}

impl Runs {
    /// Run `run` started: `false` when its start was seen before (a
    /// replay). A new run is the current one, and the user's when their
    /// input waited for it.
    pub(super) fn started(&mut self, run: &RunId) -> bool {
        if self.seen.contains(run) {
            return false;
        }
        if self.seen.len() == RUNS_REMEMBERED {
            self.seen.pop_front();
        }
        self.seen.push_back(run.clone());
        self.current = Some(run.clone());
        if std::mem::take(&mut self.human_pending) {
            if self.human.len() == RUNS_REMEMBERED {
                self.human.pop_front();
            }
            self.human.push_back(run.clone());
        }
        true
    }

    /// Whether a call in `run` (`None`: the current run) belongs to a run
    /// the user started, as far as the runs tell.
    pub(super) fn users(&self, run: Option<&RunId>) -> bool {
        match run.or(self.current.as_ref()) {
            Some(run) if self.human.contains(run) => true,
            // A run the engine saw start without a client's input waiting.
            Some(run) if self.seen.contains(run) => false,
            // Its start is not seen yet: the user's when their input waits.
            _ => self.human_pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A helper's parent is named as its parent: the agent, or the helper
    /// that started it by its name.
    #[test]
    fn a_parent_is_named_as_the_parent() {
        assert_eq!(
            PeerKind::Parent(None).words(),
            "a message to your parent agent"
        );
        assert_eq!(
            PeerKind::Parent(Some("scout".into())).words(),
            "a message to `scout`, the helper that started you"
        );
    }
}
