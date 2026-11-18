//! What the daemon tells an agent about the others: the roster and
//! membership notes (pure text), and how every note reaches an agent:
//! appended to its system context, read at its next turn and waking
//! nobody ([`append_note`], [`Agents::send_notes`]), or steered into its
//! run, which starts one when it is idle ([`steer_member`],
//! [`Agents::steer`]): a helper's ending and an approval from `/approve`
//! ([`Agents::approved`]).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use meerkat_core::SessionId;
use meerkat_mob::{AgentIdentity, MobId};
use meerkat_mob_mcp::MobMcpState;

use super::super::MOB_ID;
use super::super::conversations::RunState;
use super::super::core::Core;
use super::Agents;

/// The source the daemon's notes about the other agents carry.
const NOTE_SOURCE: &str = "rkat-tui-agents";

/// What another agent was doing when a roster note was sent: whether a
/// run of it was open, and whether a client was attached to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerState {
    /// Its run, as its status said.
    pub run: RunState,
    /// At least one client was bound to it.
    pub attached: bool,
}

impl PeerState {
    /// The state as a roster note words it. An unreadable run state claims
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

/// The note telling an agent which other agents work in its workspace
/// and what each was doing when it was sent, or that none does; `contacts`
/// says what every peer is. It is sent when the roster or an agent's
/// attachment changes, never for a run's start or end, so the note says
/// its states are a snapshot.
#[must_use]
pub fn roster_note(others: &[(String, PeerState)]) -> String {
    if others.is_empty() {
        return NO_OTHER_AGENTS.to_string();
    }
    let names = others
        .iter()
        .map(|(name, state)| format!("`{name}` ({})", state.describe()))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Other agents in this workspace now: {names}. You are wired to each of them as a peer. \
         These states are as of this note: a new one replaces it when an agent joins, or a client \
         attaches to an agent or leaves it; none comes when an agent starts or ends a run. \
         `contacts` lists everyone you can message and what each is."
    )
}

/// The roster note of an agent alone in its workspace.
pub const NO_OTHER_AGENTS: &str = "No other agent works in this workspace now; `contacts` \
     lists everyone you can message (your helpers among them) and what each is. A new note \
     comes when an agent joins.";

/// `roster` (the note about this workspace's agents) with a line naming
/// the agents of other workspaces `wired` lists.
#[must_use]
pub fn with_wired(roster: String, wired: &[String]) -> String {
    if wired.is_empty() {
        return roster;
    }
    let names = wired
        .iter()
        .map(|label| format!("`{label}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let line = format!(
        "Agents of other workspaces you are wired to: {names} (each another project's agent; its state is not known here, and a message starts a model turn there when it is idle)."
    );
    format!("{roster} {line}")
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
            return format!("Agent `{name}` joined this workspace; you are wired to it as a peer.");
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
///
/// # Errors
///
/// The engine's refusal of the append.
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
///
/// # Errors
///
/// The engine's refusal of the append.
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

/// Tell `agent` its context was just compacted: `note` is what
/// [`crate::daemon::prepare::Prepare::compacted`] gave (carry on, and
/// where what was discarded is); a failure is logged.
pub(crate) async fn note_compacted(state: Arc<MobMcpState>, agent: String, note: String) {
    if let Err(err) = append_note(
        &state,
        &agent,
        crate::daemon::local::SYSTEM_CONTEXT_SOURCE,
        note,
    )
    .await
    {
        tracing::warn!(%agent, error = %err, "the agent could not be told about its compaction");
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
    let send = |text: String| {
        core.state.mob_member_send(
            mob,
            member.clone(),
            meerkat_core::types::ContentInput::from(text),
            meerkat_core::types::HandlingMode::Steer,
            None,
        )
    };
    match send(text.clone()).await {
        Err(meerkat_mob::MobError::MemberReloadRequired { reason, .. }) => {
            crate::daemon::storage::reload(core, mob, &member, &reason, true).await?;
            send(text).await.map(drop)
        }
        other => other.map(drop),
    }
}

/// Why the daemon steers a message into one of its agents
/// ([`Agents::steer`]): who a run it starts belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wake {
    /// A helper of it ended ([`super::endings`]): a run it starts is no
    /// user's, attributed as "a helper ended".
    HelperEnded,
    /// The user approved a blocked call of it (`/approve`): a run it
    /// starts is the user's (attended, [`crate::approval::attendance`]).
    Approved,
}

/// The line a client shows over the note of an approval from `/approve`
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
    ///
    /// # Errors
    ///
    /// The engine's refusal of the send, in words.
    pub(crate) async fn steer(&self, name: &str, text: String, wake: Wake) -> Result<(), String> {
        let sent = match wake {
            Wake::HelperEnded => &self.helper_sent,
            Wake::Approved => &self.approval_sent,
        };
        sent.lock().insert(name.to_string(), text.clone());
        if wake == Wake::Approved
            && !self.core.activity.running(name)
            && let Some(session) = self.core.conversations.current(name)
        {
            self.core.engine.human_input(&session);
        }
        let _sending = self.core.activity.sending(name);
        let steered = steer_member(
            &self.core,
            &MobId::from(MOB_ID),
            AgentIdentity::from(name),
            text,
        )
        .await;
        steered.map_err(|err| {
            sent.lock().remove(name);
            err.to_string()
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
        let mut sent = self.approval_sent.lock();
        if sent
            .get(name)
            .is_none_or(|text| !prompt.contains(text.as_str()))
        {
            return false;
        }
        sent.remove(name);
        drop(sent);
        self.core.engine.user_started(session, run);
        true
    }

    /// The user approved a blocked call of `session` from `/approve`:
    /// tell the agent that runs `session` at once, so it goes on.
    ///
    /// - **Who.** The agent, for its own calls and its helpers' alike (a
    ///   helper's is named as that helper's in the note). The engine
    ///   admitted no message from the daemon into a helper of its
    ///   withdrawn `delegate` (probed, `docs/evidence.md`), and the approval
    ///   holds for the agent and its helpers, so the agent makes the call
    ///   again itself or through a helper.
    /// - **How.** Steered ([`Self::steer`]): into the agent's running
    ///   turn (it reads it at its next step), or as a run of its own when it
    ///   is idle. That run is the user's (the user's `/approve` is the
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
    pub async fn approved(&self, session: &SessionId, approved: crate::approval::Approved) {
        let Some(name) = self.agent_running_as(session) else {
            tracing::info!("the approval's conversation is no agent's now: nobody is told");
            return;
        };
        let text = approved.note;
        match self.steer(&name, text.clone(), Wake::Approved).await {
            Ok(()) => {
                self.audit_run(&name, None, "approval_told", text.clone());
                self.tell_triggered(&name, session, APPROVED_LABEL.to_string(), text);
            }
            Err(err) => {
                tracing::warn!(agent = %name, error = %err, "the agent was not told of the approval");
                self.core.decisions.broadcast(
                    crate::daemon::protocol::AGENT_NOTICE,
                    &crate::daemon::protocol::AgentNotice {
                        agent: name,
                        warn: true,
                        text: format!(
                            "the agent was not told of the approval (it runs the call when it tries it again): {err}"
                        ),
                    },
                );
            }
        }
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

    /// Every live agent and its state in the roster note's words.
    pub(crate) async fn worded_states(&self) -> Vec<(String, &'static str)> {
        self.states()
            .await
            .into_iter()
            .map(|(name, state)| (name, state.describe()))
            .collect()
    }

    /// The roster note for `name`: every other agent of `states`, then
    /// the agents of other workspaces it is wired to.
    fn roster_for(&self, name: &str, states: &[(String, PeerState)]) -> String {
        let others: Vec<(String, PeerState)> = states
            .iter()
            .filter(|(other, _)| other != name)
            .cloned()
            .collect();
        with_wired(roster_note(&others), &self.core.wires.labels(name))
    }

    /// Tell `name` which other agents work here now and what each is
    /// doing, or that none does (its fresh conversation knows none).
    pub(super) async fn note_roster(&self, name: &str) {
        let text = self.roster_for(name, &self.states().await);
        self.send_notes(vec![(name.to_string(), text)]).await;
    }

    /// Tell every agent of `names` the roster as it is now, each its own
    /// ([`Self::send_notes`]): the daemon restored them, and what their
    /// conversations were last told of the others' clients is gone.
    pub(super) async fn note_rosters(&self, names: &[String]) {
        let states = self.states().await;
        let notes = names
            .iter()
            .map(|name| (name.clone(), self.roster_for(name, &states)))
            .collect();
        self.send_notes(notes).await;
    }

    /// Tell every other live agent the roster as it is now, after `name`
    /// joined (`joined`: led by the line saying so) or a client attached
    /// to it or its last one left.
    pub(super) async fn note_roster_to_others(&self, name: &str, joined: bool) {
        let states = self.states().await;
        let notes = states
            .iter()
            .filter(|(other, _)| other != name)
            .map(|(other, _)| {
                let roster = self.roster_for(other, &states);
                let text = if joined {
                    format!("{} {roster}", membership_note(name, None))
                } else {
                    roster
                };
                (other.clone(), text)
            })
            .collect();
        self.send_notes(notes).await;
    }

    /// `name`'s attachment changed outside the seating lock (its last
    /// client left): the others get the roster as it is then, under the
    /// lock, so it is ordered with the seats and retires around it. An
    /// agent retired meanwhile, or a daemon shutting down, tells no one.
    pub(super) fn attachment_changed(self: &Arc<Self>, name: &str) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let agents = Arc::clone(self);
        let name = name.to_string();
        runtime.spawn(async move {
            let _one = agents.seating.lock().await;
            if agents.is_live(&name) && !agents.core.lifecycle.is_terminating() {
                agents.note_roster_to_others(&name, false).await;
            }
        });
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
        // Alone: said so, and `contacts` says what every peer is.
        let alone = roster_note(&[]);
        assert!(
            alone.starts_with("No other agent works in this workspace now")
                && alone.contains("`contacts` lists everyone you can message"),
            "{alone}"
        );
        let state = |run, attached| PeerState { run, attached };
        let roster = roster_note(&[
            ("main-aaaa".into(), state(RunState::Running, false)),
            ("main-bbbb".into(), state(RunState::Idle, true)),
            ("main-cccc".into(), state(RunState::Idle, false)),
            ("main-dddd".into(), state(RunState::Unknown, false)),
        ]);
        assert!(
            roster.contains(
                "`main-aaaa` (working), `main-bbbb` (idle, a client attached), \
                 `main-cccc` (idle, no client attached), `main-dddd` (no client attached)."
            ) && roster.contains("wired")
                && roster.contains("`contacts` lists everyone you can message"),
            "{roster}"
        );
        // A snapshot, and the note says so.
        assert!(
            roster.contains("as of this note") && roster.contains("starts or ends a run"),
            "{roster}"
        );
        assert!(membership_note("main-cccc", None).contains("`main-cccc` joined"));
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

    #[test]
    fn the_roster_names_the_agents_of_other_workspaces_it_is_wired_to() {
        assert_eq!(with_wired("r".into(), &[]), "r");
        let alone = with_wired(roster_note(&[]), &["api/main-ab12".into()]);
        assert!(
            alone.starts_with(NO_OTHER_AGENTS)
                && alone.contains("Agents of other workspaces you are wired to: `api/main-ab12`")
                && alone.contains("not known here"),
            "{alone}"
        );
        let both = with_wired("Other agents.".into(), &["api/main-ab12".into()]);
        assert!(
            both.starts_with("Other agents. Agents of other workspaces"),
            "{both}"
        );
    }
}
