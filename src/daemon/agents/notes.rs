//! What the daemon tells an agent about the others: the roster and
//! membership notes (pure text), and how every note reaches an agent
//! ([`append_note`], [`Agents::send_notes`]).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use meerkat_mob::{AgentIdentity, MobId};
use meerkat_mob_mcp::MobMcpState;

use super::super::MOB_ID;
use super::super::conversations::RunState;
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
/// and what each was doing when it was sent (`None`: none do). It is sent
/// when the roster or an agent's attachment changes, never for a run's
/// start or end, so the note says its states are a snapshot.
#[must_use]
pub fn roster_note(others: &[(String, PeerState)]) -> Option<String> {
    if others.is_empty() {
        return None;
    }
    let names = others
        .iter()
        .map(|(name, state)| format!("`{name}` ({})", state.describe()))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "Other agents in this workspace now: {names}. You are wired to each of them as a peer. \
         These states are as of this note: a new one replaces it when an agent joins, or a client \
         attaches to an agent or leaves it; none comes when an agent starts or ends a run."
    ))
}

/// `roster` (the note about this workspace's agents, when there are
/// others) with a line naming the agents of other workspaces `wired`
/// lists; `None` when both are empty.
#[must_use]
pub fn with_wired(roster: Option<String>, wired: &[String]) -> Option<String> {
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
    Some(match roster {
        Some(roster) => format!("{roster} {line}"),
        None => line,
    })
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
    static NOTES: AtomicU64 = AtomicU64::new(0);
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
        .mob_append_system_context(&MobId::from(MOB_ID), &AgentIdentity::from(agent), request)
        .await
        .map(drop)
}

/// [`append_note`] of a note about the other agents; a failure is logged.
pub(super) async fn note_agent(state: Arc<MobMcpState>, agent: String, text: String) {
    if let Err(err) = append_note(&state, &agent, NOTE_SOURCE, text).await {
        tracing::warn!(%agent, error = %err, "the agent could not be told about the other agents");
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
            let run = self.conversations.running(&name).await;
            states.push((name, PeerState { run, attached }));
        }
        states
    }

    /// The roster note for `name`: every other agent of `states`, then
    /// the agents of other workspaces it is wired to.
    fn roster_for(&self, name: &str, states: &[(String, PeerState)]) -> Option<String> {
        let others: Vec<(String, PeerState)> = states
            .iter()
            .filter(|(other, _)| other != name)
            .cloned()
            .collect();
        with_wired(roster_note(&others), &self.wires.labels(name))
    }

    /// Tell `name` which other agents work here now and what each is
    /// doing (its fresh conversation knows none).
    pub(super) async fn note_roster(&self, name: &str) {
        if let Some(text) = self.roster_for(name, &self.states().await) {
            self.send_notes(vec![(name.to_string(), text)]).await;
        }
    }

    /// Tell every agent of `names` the roster as it is now, each its own
    /// ([`Self::send_notes`]): the daemon restored them, and what their
    /// conversations were last told of the others' clients is gone.
    pub(super) async fn note_rosters(&self, names: &[String]) {
        let states = self.states().await;
        let notes = names
            .iter()
            .filter_map(|name| Some((name.clone(), self.roster_for(name, &states)?)))
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
            .filter_map(|(other, _)| {
                let roster = self.roster_for(other, &states)?;
                let text = if joined {
                    format!("{} {roster}", membership_note(name, None))
                } else {
                    roster
                };
                Some((other.clone(), text))
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
            if agents.is_live(&name) && !agents.lifecycle.is_terminating() {
                agents.note_roster_to_others(&name, false).await;
            }
        });
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
    pub(super) async fn send_notes(&self, notes: Vec<(String, String)>) {
        let mut waited = Vec::new();
        for (other, text) in notes {
            let idle = self.conversations.running(&other).await == RunState::Idle;
            let task = tokio::spawn(note_agent(Arc::clone(&self.state), other.clone(), text));
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
        assert_eq!(roster_note(&[]), None);
        let state = |run, attached| PeerState { run, attached };
        let roster = roster_note(&[
            ("main-aaaa".into(), state(RunState::Running, false)),
            ("main-bbbb".into(), state(RunState::Idle, true)),
            ("main-cccc".into(), state(RunState::Idle, false)),
            ("main-dddd".into(), state(RunState::Unknown, false)),
        ])
        .unwrap();
        assert!(
            roster.contains(
                "`main-aaaa` (working), `main-bbbb` (idle, a client attached), \
                 `main-cccc` (idle, no client attached), `main-dddd` (no client attached)."
            ) && roster.contains("wired"),
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
        assert_eq!(with_wired(None, &[]), None);
        assert_eq!(with_wired(Some("r".into()), &[]).as_deref(), Some("r"));
        let alone = with_wired(None, &["api/main-ab12".into()]).unwrap();
        assert!(
            alone.starts_with("Agents of other workspaces you are wired to: `api/main-ab12`")
                && alone.contains("not known here"),
            "{alone}"
        );
        let both = with_wired(Some("Other agents.".into()), &["api/main-ab12".into()]).unwrap();
        assert!(
            both.starts_with("Other agents. Agents of other workspaces"),
            "{both}"
        );
    }
}
