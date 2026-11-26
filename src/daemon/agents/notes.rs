//! What the daemon tells an agent about the others, and how every note
//! reaches an agent.
//!
//! An agent gets one note when an agent joins or leaves the workspace, an
//! event it cannot learn of otherwise. Who is here and what each is doing
//! is `contacts`' to give, always current, so nothing that restates it is
//! appended and nothing grows with attaches and detaches.
//!
//! A note is either appended to the agent's system context, read at its
//! next turn and waking nobody ([`append_note`], [`Agents::send_notes`]),
//! or steered into its run, which starts one when it is idle
//! ([`steer_member`], [`Agents::steer`]). Steered are a helper's ending
//! and an approval from `/permissions` ([`Agents::approved`]).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use meerkat_core::SessionId;
use meerkat_mob::{AgentIdentity, MobId};
use meerkat_mob_mcp::MobMcpState;

use super::super::MOB_ID;
use super::super::conversations::RunState;
use super::super::core::Core;
use super::Agents;
use super::sent::Kind;

/// The source the daemon's notes about the other agents carry.
const NOTE_SOURCE: &str = "rkat-tui-agents";

/// What another agent is doing now: whether a run of it is open, and
/// whether a client is attached to it (`contacts` gives it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerState {
    /// Its run, as its status said.
    pub run: RunState,
    /// At least one client was bound to it.
    pub attached: bool,
}

impl PeerState {
    /// The state as `contacts` words it. An unreadable run state claims
    /// neither working nor idle.
    fn describe(self) -> &'static str {
        match (self.run, self.attached) {
            (RunState::Running, _) => "working",
            (RunState::Idle, true) => "idle, a client attached",
            (RunState::Idle, false) => "idle, no client attached",
            (RunState::Unknown, true) => "a client attached",
            (RunState::Unknown, false) => "no client attached",
        }
    }
}

/// Why an agent leaves the workspace, as the others are told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Leave {
    /// Its user ran `/quit` (or quit the TUI) as its last client.
    Quit,
    /// Another agent's user retired it from `/agents` (`by`: that agent).
    Retired {
        /// The agent whose user retired it, when known.
        by: Option<String>,
    },
    /// Its session was closed otherwise (the daemon stopped, it was lost).
    Closed,
}

/// The note telling an agent that `name` joined (`None`) or left the
/// workspace, and why.
#[must_use]
pub fn membership_note(name: &str, left: Option<&Leave>) -> String {
    let why = match left {
        None => {
            return format!(
                "Agent `{name}` joined this workspace; you are wired to it as a peer. `contacts` shows who works here and what each is doing."
            );
        }
        Some(Leave::Quit) => "its user quit".to_string(),
        Some(Leave::Retired { by: Some(by) }) => {
            format!("the user of `{by}` retired it from /agents")
        }
        Some(Leave::Retired { by: None }) => "it was retired from /agents".to_string(),
        Some(Leave::Closed) => "its session was closed".to_string(),
    };
    format!("Agent `{name}` left this workspace ({why}); it takes no more messages.")
}

/// How long a seat or retire waits for its membership note to an idle
/// agent ([`Agents::send_notes`]) before leaving it to land on its own.
const NOTE_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Append `text` to `agent`'s system context (it reads it at its next
/// turn; nothing wakes it) as `source`'s note, under an idempotency key no
/// other note shares, across processes too: a session keeps the notes of
/// earlier daemons, and the engine refuses a key it already holds with
/// another text. Every note the daemon sends an agent goes through here.
pub(crate) async fn append_note(
    state: &MobMcpState,
    agent: &str,
    source: &str,
    text: String,
) -> Result<(), meerkat_mob_mcp::MobAppendSystemContextError> {
    append_note_to(
        state,
        &MobId::from(MOB_ID),
        &AgentIdentity::from(agent),
        source,
        text,
    )
    .await
}

/// [`append_note`] to `member` of `mob` (a helper: what its parent's
/// `connect_helpers` gave it, [`crate::daemon::connect`]).
pub(crate) async fn append_note_to(
    state: &MobMcpState,
    mob: &MobId,
    member: &AgentIdentity,
    source: &str,
    text: String,
) -> Result<(), meerkat_mob_mcp::MobAppendSystemContextError> {
    static NOTES: AtomicU64 = AtomicU64::new(0);
    let agent = member.as_str();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let mut request = meerkat_core::AppendSystemContextRequest::from_text(text);
    request.source = Some(source.to_string());
    request.idempotency_key = Some(format!(
        "{source}:{agent}:{}-{nanos}-{}",
        std::process::id(),
        NOTES.fetch_add(1, Ordering::Relaxed)
    ));
    state
        .mob_append_system_context(mob, member, request)
        .await
        .map(drop)
}

/// [`append_note`] of a note about the other agents; a failure is logged.
pub(super) async fn note_agent(state: Arc<MobMcpState>, agent: String, text: String) {
    if let Err(err) = append_note(&state, &agent, NOTE_SOURCE, text).await {
        tracing::warn!(%agent, error = %err, "the agent could not be told about the other agents");
    }
}

/// Steer `text` into member `member` of `mob`: into its open run, which
/// reads it at its next step, or as the input of a run that starts for
/// it when it is idle. Every message the daemon sends a member to act on
/// now goes through here (a helper's ending, an approval, a plan-mode
/// change, a watch's batch, the smart compaction's request). It returns
/// once the engine took the input, not when a run ends.
///
/// A member the engine refuses with `MemberReloadRequired` (a storage
/// write of it failed, [`crate::daemon::storage`]) is reloaded once and
/// sent to once more.
///
/// # Errors
///
/// The engine's refusal of the send (or of the reload it needed).
pub(crate) async fn steer_member(
    core: &Core,
    mob: &MobId,
    member: AgentIdentity,
    text: String,
) -> Result<(), meerkat_mob::MobError> {
    steer_member_as(core, mob, member, text).await.map(drop)
}

/// [`steer_member`], answering the input the member's runtime admitted
/// the steer as (`None`: it named none).
pub(crate) async fn steer_member_as(
    core: &Core,
    mob: &MobId,
    member: AgentIdentity,
    text: String,
) -> Result<Option<meerkat_core::lifecycle::InputId>, meerkat_mob::MobError> {
    let send = |text: String| {
        core.state.mob_member_send(
            mob,
            member.clone(),
            meerkat_core::types::ContentInput::from(text),
            meerkat_core::types::HandlingMode::Steer,
            None,
        )
    };
    let receipt = match send(text.clone()).await {
        Err(meerkat_mob::MobError::MemberReloadRequired { reason, .. }) => {
            crate::daemon::storage::reload(core, mob, &member, &reason, true).await?;
            send(text).await?
        }
        other => other?,
    };
    Ok(receipt.admission.map(|admission| admission.input_id))
}

/// A note of the daemon's own to the workspace's agent `agent`, steered
/// at once ([`steer_member`]): into its open run, whose next model request
/// takes it with every other steer waiting then (Meerkat 0.9.18), so it
/// never costs the user's message a request; or as a run of its own when
/// the agent is idle.
pub(crate) async fn steer_agent(core: &Core, agent: &str, text: String) -> Result<(), String> {
    steer_member(core, &MobId::from(MOB_ID), AgentIdentity::from(agent), text)
        .await
        .map_err(|err| err.to_string())
}

/// Why the daemon steers a message into one of its agents
/// ([`Agents::steer`]): who a run it starts belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wake {
    /// A helper of it ended ([`super::endings`]): a run it starts is no
    /// user's, attributed as "a helper ended".
    HelperEnded,
    /// The user approved a blocked call of it (`/permissions`): a run it
    /// starts is the user's (attended, [`crate::approval::attendance`]).
    Approved,
}

/// The line a client shows over the note of an approval from `/permissions`
/// (the run it starts, or the running turn it went into).
pub const APPROVED_LABEL: &str = "approved: the agent is told and goes on";

impl Agents {
    /// Steer `text` into `name`'s run, or start a run with it when it is
    /// idle ([`steer_member`]), recorded as `wake`'s so the run it starts
    /// is attributed: a helper's ending's is no user's; an approval's is
    /// the user's, marked so before the send when the agent is idle (the
    /// next run that starts takes it, [`ApprovalEngine::human_input`]) and
    /// again when it starts with this text (the agent's run ended between
    /// the look and the send, [`ApprovalEngine::user_started`]).
    ///
    /// [`ApprovalEngine::human_input`]: crate::approval::ApprovalEngine::human_input
    /// [`ApprovalEngine::user_started`]: crate::approval::ApprovalEngine::user_started
    pub(crate) async fn steer(&self, name: &str, text: String, wake: Wake) -> Result<(), String> {
        let kind = match wake {
            Wake::HelperEnded => Kind::HelperEnding,
            Wake::Approved => Kind::Approval,
        };
        self.sent.record(name, kind, text.clone());
        if wake == Wake::Approved
            && !self.core.activity.running(name)
            && let Some(session) = self.core.conversations.current(name)
        {
            self.core.engine.human_input(&session);
        }
        let _sending = self.core.activity.sending(name);
        #[cfg(any(test, feature = "test-support"))]
        let late = self.core.probe.steering(wake == Wake::HelperEnded)?;
        let steered = steer_agent(&self.core, name, text).await;
        #[cfg(any(test, feature = "test-support"))]
        let steered = crate::daemon::core::probe::CoreProbe::answer(steered, late);
        // A helper's note stays marked as sent after an error: should the
        // engine have taken it all the same, the run it starts is still
        // recognised, and the copy held meanwhile goes then
        // ([`super::endings::HeldNotes::delivered`]).
        steered.inspect_err(|_| {
            if wake == Wake::Approved {
                self.sent.forget(name, Kind::Approval);
            }
        })
    }

    /// A run of `name` started with input `prompt`: whether it is the run
    /// an approval's note started ([`Wake::Approved`]); it is then the
    /// user's run (in `session`, marked as such).
    pub(crate) fn approval_run(
        &self,
        name: &str,
        session: &SessionId,
        run: &meerkat_core::RunId,
        prompt: &str,
    ) -> bool {
        if !self
            .sent
            .take_if(name, Kind::Approval, |text| prompt.contains(text))
        {
            return false;
        }
        self.core.engine.user_started(session, run);
        true
    }

    /// The user approved a blocked call of `session` from `/permissions`:
    /// tell the agent that runs `session` at once, so it goes on.
    ///
    /// - **Who.** The agent, for its own calls and its helpers' alike (a
    ///   helper's is named as that helper's in the note). The approval
    ///   holds for the agent and its helpers, so the agent makes the call
    ///   again itself or through a helper.
    /// - **How.** Steered ([`Self::steer`]): into the agent's running
    ///   turn (it reads it at its next step), or as a run of its own when it
    ///   is idle. That run is the user's (the user's approval is the
    ///   user's act): attended, and its clients show it under
    ///   [`APPROVED_LABEL`].
    /// - **Not held after an interrupt.** A helper's ending to an agent
    ///   whose run the user just interrupted waits for the user's next
    ///   message ([`super::endings`]); an approval does not: the user is
    ///   acting on this agent right now, so the run starts. (The engine
    ///   refuses an append to the system context right after an
    ///   interrupted run, not a message.)
    ///
    /// A failure is a warning to the agent's clients.
    pub async fn approved(
        self: &Arc<Self>,
        session: &SessionId,
        approved: crate::approval::Approved,
    ) {
        let Some(name) = self.agent_running_as(session) else {
            tracing::info!("the approval's conversation is no agent's now: nobody is told");
            return;
        };
        let text = approved.note;
        match self.steer(&name, text.clone(), Wake::Approved).await {
            Ok(()) => {
                self.audit_run(&name, None, "approval_told", text.clone());
                self.tell_triggered(
                    &name,
                    session,
                    (
                        crate::daemon::protocol::TriggerCause::Approval,
                        APPROVED_LABEL.to_string(),
                    ),
                    text,
                );
            }
            Err(err) => self.approval_not_told(name, &err),
        }
    }

    /// The approval's note did not reach `name` (`err`): a warning to its
    /// clients.
    fn approval_not_told(&self, name: String, err: &str) {
        tracing::warn!(agent = %name, error = %err, "the agent was not told of the approval");
        self.core.decisions.agent_notice(
            name,
            true,
            format!(
                "the agent was not told of the approval (it runs the call when it tries it again): {err}"
            ),
        );
    }
}

impl Agents {
    /// Every live agent and what it is doing now (whether a run is open,
    /// whether a client is attached), names in order.
    async fn states(&self) -> Vec<(String, PeerState)> {
        let attached: Vec<(String, bool)> = self
            .table
            .lock()
            .iter()
            .map(|(name, agent)| (name.clone(), agent.clients > 0))
            .collect();
        let mut states = Vec::with_capacity(attached.len());
        for (name, attached) in attached {
            let run = self.core.conversations.running(&name).await;
            states.push((name, PeerState { run, attached }));
        }
        states
    }

    /// Every live agent and its state in `contacts`' words.
    pub(crate) async fn worded_states(&self) -> Vec<(String, &'static str)> {
        self.states()
            .await
            .into_iter()
            .map(|(name, state)| (name, state.describe()))
            .collect()
    }

    /// Tell every other live agent that `name` joined the workspace (its
    /// first client bound to it).
    pub(super) async fn note_joined(&self, name: &str) {
        let notes = self
            .names()
            .into_iter()
            .filter(|other| other != name)
            .map(|other| (other, membership_note(name, None)))
            .collect();
        self.send_notes(notes).await;
    }

    /// Tell every live agent `text` (a live change of a setting its
    /// instructions state, such as its helpers' models).
    pub(crate) async fn note_everyone(&self, text: &str) {
        let notes = self
            .names()
            .into_iter()
            .map(|name| (name, text.to_string()))
            .collect();
        self.send_notes(notes).await;
    }

    /// Tell every other live agent that `name` left.
    pub(super) async fn note_others(&self, name: &str, left: &Leave) {
        let notes = self
            .names()
            .into_iter()
            .filter(|other| other != name)
            .map(|other| (other, membership_note(name, Some(left))))
            .collect();
        self.send_notes(notes).await;
    }

    /// Append each note of `notes` (agent, text) to its agent.
    ///
    /// The engine applies an append to a running member's session only
    /// when its turn ends, so a note to a busy agent would hold the
    /// seating lock (and the attach waiting on it) for that agent's whole
    /// turn, its helpers' included. Each note is its own task: one to an
    /// idle agent is waited for (bounded by [`NOTE_WAIT`]), so the agent
    /// reads it at its next turn; one to a running agent, or one whose
    /// state cannot be read, is left to land when that turn ends.
    async fn send_notes(&self, notes: Vec<(String, String)>) {
        let mut waited = Vec::new();
        for (other, text) in notes {
            let idle = self.core.conversations.running(&other).await == RunState::Idle;
            let task = tokio::spawn(note_agent(
                Arc::clone(&self.core.state),
                other.clone(),
                text,
            ));
            if idle {
                waited.push((other, task));
            } else {
                tracing::debug!(agent = %other, "the agent is busy: its note about the other agents lands after its turn");
            }
        }
        for (other, task) in waited {
            if tokio::time::timeout(NOTE_WAIT, task).await.is_err() {
                tracing::debug!(agent = %other, "the note about the other agents was not applied in time: it lands later");
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_notes_name_the_agents() {
        // The states as `contacts` words them.
        let state = |run, attached| PeerState { run, attached }.describe();
        assert_eq!(
            [
                state(RunState::Running, false),
                state(RunState::Idle, true),
                state(RunState::Idle, false),
                state(RunState::Unknown, false),
            ],
            [
                "working",
                "idle, a client attached",
                "idle, no client attached",
                "no client attached"
            ]
        );
        // A join points to `contacts` and claims to replace nothing.
        assert_eq!(
            membership_note("main-cccc", None),
            "Agent `main-cccc` joined this workspace; you are wired to it as a peer. `contacts` shows who works here and what each is doing."
        );
        // Why it left: its user quit only on /quit.
        let quit = membership_note("main-cccc", Some(&Leave::Quit));
        assert!(
            quit.contains("`main-cccc` left") && quit.contains("its user quit"),
            "{quit}"
        );
        let retired = membership_note(
            "main-cccc",
            Some(&Leave::Retired {
                by: Some("main-aaaa".into()),
            }),
        );
        assert!(
            retired.contains("the user of `main-aaaa` retired it from /agents")
                && !retired.contains("quit"),
            "{retired}"
        );
        let closed = membership_note("main-cccc", Some(&Leave::Closed));
        assert!(
            closed.contains("its session was closed") && !closed.contains("quit"),
            "{closed}"
        );
    }
}
