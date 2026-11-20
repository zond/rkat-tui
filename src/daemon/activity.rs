//! When an agent is idle, decided once in the daemon for every client:
//! no run of it is open, no input sent to it is still on its way, and its
//! runtime holds no unfinished input of its conversation. A client's turn
//! ends when the daemon says so ([`super::protocol::IDLE`]); a steer that
//! missed the boundary, whichever client sent it, runs as a follow-up run
//! before that.
//!
//! The decision is made on events, not on a timer: the agent's event
//! watcher reports each run's start and end, the proxy each input it
//! forwards until the engine answers it (a guard, [`Sending`]), the daemon
//! its own sends and recalls. After such an event, with no run open and
//! no input on its way, the runtime is asked ([`Probe`]: the unfinished
//! inputs and the member's run state). The agent's event stream does not
//! say when an input reaches a terminal state or when the runtime is idle
//! again after a run's terminal event (a run still closing, an input
//! consumed or abandoned without a run), so an answer that still names
//! unfinished input is asked again after a short wait that doubles, until
//! a run starts or [`SETTLE_WAIT`] has passed; the agent is then reported
//! idle with what it still holds ([`IdleNote::held`]).
//!
//! The same record holds what waits for an agent's open run: the daemon's
//! own notes ([`HeldNote`]) and the user's messages behind the one in
//! flight ([`steers`]), under the one lock with the open runs, so a run's
//! end cannot strand either.

pub mod steers;

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use futures::future::BoxFuture;
use meerkat_core::{RunId, SessionId};

use meerkat_core::lifecycle::InputId;

use super::conversations::RunState;
use super::protocol::{HeldChange, HeldNow, IdleNote};
use steers::{Admit, Owner, Release, Steers, TakenBack};

/// The longest the daemon re-asks the runtime after a run ended while it
/// still holds unfinished input and starts no run for it.
pub const SETTLE_WAIT: Duration = Duration::from_secs(10);
/// The first wait before asking again; each next one doubles.
const RECHECK_FIRST: Duration = Duration::from_millis(50);
/// The longest wait between two questions.
const RECHECK_MAX: Duration = Duration::from_secs(1);

/// What the runtime says of an agent now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probed {
    /// The conversation it runs as.
    pub session: SessionId,
    /// How many inputs of it are not finished (one that cannot be read
    /// counts).
    pub unfinished: usize,
    /// Its run, as its status says.
    pub run: RunState,
}

/// Asks the runtime about an agent (`None`: it has no conversation).
pub type Probe = Box<dyn Fn(String) -> BoxFuture<'static, Option<Probed>> + Send + Sync>;

/// Tells the clients an agent is idle.
pub type Announce = Box<dyn Fn(&IdleNote) + Send + Sync>;

/// Tells the clients what became of held messages of the user's.
pub type Told = Box<dyn Fn(&HeldChange) + Send + Sync>;

/// Told of a run's start once: the conversation and the run
/// ([`Activity::when_running`]).
pub type OnRun = Box<dyn FnOnce(&SessionId, &RunId) + Send>;

/// One agent's activity as the daemon saw it.
#[derive(Default)]
struct State {
    /// Runs started and not ended, with their conversation.
    open: HashMap<RunId, SessionId>,
    /// The run that started last.
    latest: Option<RunId>,
    /// Told of the next run's start ([`Activity::when_running`]).
    waiting: Vec<OnRun>,
    /// The run that ended last.
    last_ended: Option<RunId>,
    /// Inputs forwarded and not answered yet.
    sending: usize,
    /// Counts the changes: a question asked before the last one is asked
    /// again.
    generation: u64,
    /// What the clients were told last, while nothing changed since.
    idle: Option<IdleNote>,
    /// A question to the runtime is under way.
    checking: bool,
    /// The daemon's own notes held for the open run ([`Activity::hold_note`]).
    notes: Vec<HeldNote>,
    /// The user's messages into the open run ([`steers`]).
    steers: Steers,
    /// An input of the user's went to the agent while no run of it was
    /// read open: it starts a run, whose start may not be read yet (open
    /// for the user's next message, [`Activity::user_steer`]).
    starting: bool,
}

/// A note of the daemon's own (a helper's ending, an approval, a watch's
/// batch, the prepare request) for an agent whose run is open, held here
/// instead of steered at once.
///
/// The engine writes one steer into a running turn per model request,
/// oldest first (`docs/evidence.md`, "Our notes never go ahead of the
/// user's message"), so a note steered just before the user types would
/// cost the user's message a whole request. A held note goes:
///
/// - with the user's next message to the agent, in front of its words, in
///   the same slot ([`Activity::take_notes`] from the proxy);
/// - else, when the run's model call asked for tools, as one steer with
///   every other note held then (the agent reads it after those tools);
/// - else, when the run ends, as one steer that starts the agent's next
///   run, as a note to an idle agent always did.
///
/// `done` is told whether the engine took it.
#[derive(Debug)]
pub struct HeldNote {
    /// What the agent is told.
    pub text: String,
    /// Told the engine's answer, once.
    pub done: tokio::sync::oneshot::Sender<Result<(), String>>,
}

/// The notes taken from an agent's hold ([`Activity::take_notes`]): the
/// agent counts as busy until `sending` is dropped.
#[derive(Debug)]
pub struct TakenNotes {
    /// The notes, oldest first.
    pub notes: Vec<HeldNote>,
    /// Keeps the agent busy while they are sent (until this is dropped).
    _sending: Sending,
}

impl TakenNotes {
    /// The notes' texts as one, oldest first.
    #[must_use]
    pub fn text(&self) -> String {
        self.notes
            .iter()
            .map(|note| note.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// Tell every note's sender `result`.
    pub fn answer(self, result: &Result<(), String>) {
        for note in self.notes {
            let _ = note.done.send(result.clone());
        }
    }
}

/// The user's held messages to send now as one steer
/// ([`steers::Release`]), with the daemon's notes held then (they ride in
/// front of the user's words): the agent counts as busy until this is
/// dropped.
#[derive(Debug)]
pub struct Releasing {
    /// The agent.
    pub agent: String,
    /// The messages.
    pub release: Release,
    /// The daemon's notes that go with them.
    pub notes: Option<TakenNotes>,
    /// Keeps the agent busy while they are sent.
    _sending: Sending,
}

/// Where a message the user sent is, as the daemon knows it
/// ([`Activity::held_where`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeldWhere {
    /// Held behind the one in flight: not in the engine yet.
    Held,
    /// Held, then sent with others as this engine input.
    Sent(InputId),
    /// No message the daemon held: an engine input, as named.
    NotHeld,
}

/// What goes to an agent when its run ended ([`Activity::run_ended`]).
#[derive(Debug)]
pub enum Outgo {
    /// The daemon's notes alone.
    Notes(TakenNotes),
    /// The user's held messages (and the notes with them).
    User(Releasing),
}

impl State {
    fn busy(&self) -> bool {
        !self.open.is_empty() || self.sending > 0
    }

    fn change(&mut self) {
        self.generation += 1;
        self.idle = None;
    }

    /// The open run: the one that started last, else any still open.
    fn open_run(&self) -> Option<(SessionId, RunId)> {
        let run = self
            .latest
            .as_ref()
            .filter(|run| self.open.contains_key(*run))
            .or_else(|| self.open.keys().next())?;
        Some((self.open.get(run)?.clone(), run.clone()))
    }
}

/// What a question to the runtime decided.
enum Next {
    /// Nothing more to ask (idle, busy again, or gone).
    Done,
    /// Something changed while it was asked: ask again now.
    Again,
    /// It still holds unfinished input: ask again after a wait.
    Wait,
}

/// The daemon's view of when its agents are idle.
pub struct Activity {
    agents: parking_lot::Mutex<HashMap<String, State>>,
    probe: Probe,
    announce: Announce,
    told: Told,
    this: Weak<Self>,
    /// `true` once the daemon stopped ([`Self::stop`]).
    stopped: tokio::sync::watch::Sender<bool>,
}

impl std::fmt::Debug for Activity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Activity")
            .field("agents", &self.agents.lock().len())
            .finish_non_exhaustive()
    }
}

impl Activity {
    /// Asking the runtime with `probe`, telling the clients with
    /// `announce` (idle) and `told` (held messages).
    #[must_use]
    pub fn new(probe: Probe, announce: Announce, told: Told) -> Arc<Self> {
        Arc::new_cyclic(|this| Self {
            agents: parking_lot::Mutex::new(HashMap::new()),
            probe,
            announce,
            told,
            this: this.clone(),
            stopped: tokio::sync::watch::channel(false).0,
        })
    }

    /// The daemon stops: no question is asked from now on, and one under
    /// way ends at once. A question holds this record, and through its
    /// probe the runtime and everything the runtime holds (the mob host
    /// with its mobs' handles, which keep their databases observed); one
    /// left asking an agent whose conversation the shutdown unregistered
    /// would keep all of it past the daemon's end, for up to
    /// [`SETTLE_WAIT`].
    pub fn stop(&self) {
        self.stopped.send_replace(true);
        let (held, user): (Vec<HeldNote>, Vec<_>) = {
            let mut agents = self.agents.lock();
            let notes = agents
                .values_mut()
                .flat_map(|state| std::mem::take(&mut state.notes))
                .collect();
            let user = agents
                .iter_mut()
                .map(|(agent, state)| (agent.clone(), state.steers.drop_all()))
                .collect();
            (notes, user)
        };
        for note in held {
            let _ = note.done.send(Err("the daemon stopped first".to_string()));
        }
        for (agent, dropped) in user {
            self.dropped(&agent, &dropped, "the daemon stopped before it sent it");
        }
    }

    /// The daemon stops: every message of the user's still held is taken
    /// (never sent: the engine does not deliver even a sent one that a
    /// stop catches in a run), its client told why (`why`: it goes back
    /// to the editor); the texts, oldest first, for the caller to save.
    #[must_use]
    pub fn drop_for_stop(&self, why: &str) -> Vec<String> {
        let dropped: Vec<(String, Vec<steers::Held>)> = self
            .agents
            .lock()
            .iter_mut()
            .map(|(agent, state)| (agent.clone(), state.steers.drop_all()))
            .filter(|(_, held)| !held.is_empty())
            .collect();
        let mut texts = Vec::new();
        for (agent, held) in dropped {
            self.dropped(&agent, &held, why);
            texts.extend(held.into_iter().map(|held| held.text));
        }
        texts
    }

    /// Tell the clients that `dropped` of `agent` were never sent, and why.
    fn dropped(&self, agent: &str, dropped: &[steers::Held], why: &str) {
        if dropped.is_empty() {
            return;
        }
        (self.told)(&HeldChange {
            agent: agent.to_string(),
            ids: dropped.iter().map(|held| held.id.clone()).collect(),
            now: HeldNow::Dropped {
                why: why.to_string(),
            },
        });
    }

    /// `release` of `agent`, with the notes held then, ready to be sent
    /// (under the lock): the agent is busy until it is.
    fn releasing(this: &Weak<Self>, agent: &str, state: &mut State, release: Release) -> Releasing {
        let notes = Self::taken(this, agent, state);
        state.change();
        state.sending += 1;
        Releasing {
            agent: agent.to_string(),
            release,
            notes,
            _sending: Sending {
                activity: this.clone(),
                agent: agent.to_string(),
            },
        }
    }

    /// A client's steer `text` (sent with `key`) for `agent`: forwarded
    /// (in flight from now on when a run of it is open) or held
    /// ([`steers`]). Never held after the stop.
    ///
    /// A run is open when its start was read and its end was not, or when
    /// an input of the user's went to the idle agent since: the engine
    /// may be running the run it started before its start is read here
    /// (the agent's events come on a stream of their own), and a message
    /// sent then joins that run, not a new one.
    pub fn user_steer(&self, agent: &str, key: &str, text: &str) -> Admit {
        if *self.stopped.borrow() {
            return Admit::Forward;
        }
        let mut agents = self.agents.lock();
        let state = agents.entry(agent.to_string()).or_default();
        let running = !state.open.is_empty() || state.starting;
        if !running {
            state.starting = true;
        }
        state.steers.admit(running, key, text)
    }

    /// `agent`'s run started with `prompt` as its input: when that holds
    /// the user's message in flight, the held ones go now (they ride the
    /// run's next request).
    pub fn run_carries(&self, agent: &str, prompt: &str) -> Option<Releasing> {
        let mut agents = self.agents.lock();
        let state = agents.get_mut(agent)?;
        let release = state.steers.run_started(prompt)?;
        Some(Self::releasing(&self.this, agent, state, release))
    }

    /// The engine answered the client's send `key` (`Ok`: the input it
    /// admitted): the held messages to send now, if that answer lets them
    /// go.
    pub fn user_answered(&self, key: &str, input: Result<InputId, ()>) -> Option<Releasing> {
        let owner = Owner::Send(key.to_string());
        let mut agents = self.agents.lock();
        let (agent, state) = agents.iter_mut().find(|(_, state)| {
            matches!(&state.steers.now, steers::UserSteers::InFlight(flight) if flight.owner == owner)
        })?;
        if input.is_err() {
            state.starting = false;
        }
        let release = state.steers.answered(&owner, input)?;
        Some(Self::releasing(&self.this, agent, state, release))
    }

    /// The daemon's send of `release` went as `input` (`Ok(None)`: the
    /// engine named none, so it is not followed) or failed (why): the
    /// clients are told, and what that lets go next.
    pub fn released(
        &self,
        agent: &str,
        release: &Release,
        input: Result<Option<InputId>, String>,
    ) -> Option<Releasing> {
        let ids = release.ids();
        let (written, next) = {
            let mut agents = self.agents.lock();
            let state = agents.entry(agent.to_string()).or_default();
            let (written, next) = match &input {
                Ok(Some(input)) => state
                    .steers
                    .released(release.serial, &ids, Ok(input.clone())),
                Ok(None) | Err(_) => (false, state.steers.release_failed(release.serial)),
            };
            (
                written,
                next.map(|next| Self::releasing(&self.this, agent, state, next)),
            )
        };
        match input {
            // Not followed: its client is not told it went, and keeps
            // following it by its held id (a run that carries its text
            // delivers it).
            Ok(None) => {}
            Ok(Some(input_id)) => {
                (self.told)(&HeldChange {
                    agent: agent.to_string(),
                    ids: ids.clone(),
                    now: HeldNow::Released { input_id },
                });
                if written {
                    (self.told)(&HeldChange {
                        agent: agent.to_string(),
                        ids,
                        now: HeldNow::Applied,
                    });
                }
            }
            Err(why) => self.dropped(agent, &release.held, &why),
        }
        next
    }

    /// `agent`'s run wrote `input` into the request that starts now: the
    /// held messages sent as it are delivered (the clients are told), and
    /// what that lets go next.
    pub fn boundary_applied(&self, agent: &str, input: &InputId) -> Option<Releasing> {
        let (delivered, next) = {
            let mut agents = self.agents.lock();
            let state = agents.get_mut(agent)?;
            let (delivered, release) = state.steers.applied(input);
            (
                delivered,
                release.map(|release| Self::releasing(&self.this, agent, state, release)),
            )
        };
        if !delivered.is_empty() {
            (self.told)(&HeldChange {
                agent: agent.to_string(),
                ids: delivered,
                now: HeldNow::Applied,
            });
        }
        next
    }

    /// `inputs` of `agent` were taken back or ended (a recall, an
    /// interrupt: `hold` keeps what is held for the run's end): what that
    /// lets go next.
    pub fn user_settled(&self, agent: &str, inputs: &[InputId], hold: bool) -> Option<Releasing> {
        let mut agents = self.agents.lock();
        let state = agents.get_mut(agent)?;
        let release = state.steers.settled(inputs, hold)?;
        Some(Self::releasing(&self.this, agent, state, release))
    }

    /// A client of `agent` takes back `ids` ([`Steers::take_back`]); the
    /// ones sent with others and not all named count as reached.
    #[must_use]
    pub fn take_back(&self, agent: &str, ids: &[InputId]) -> (TakenBack, Vec<InputId>) {
        let mut agents = self.agents.lock();
        let state = agents.entry(agent.to_string()).or_default();
        let taken = state.steers.take_back(ids);
        state.change();
        taken
    }

    /// Where `agent`'s message `id` is ([`HeldWhere`]).
    #[must_use]
    pub fn held_where(&self, agent: &str, id: &InputId) -> HeldWhere {
        let agents = self.agents.lock();
        let Some(state) = agents.get(agent) else {
            return HeldWhere::NotHeld;
        };
        if state.steers.holds(id) {
            return HeldWhere::Held;
        }
        state
            .steers
            .sent_as(id)
            .map_or(HeldWhere::NotHeld, HeldWhere::Sent)
    }

    /// Tests: the texts of `agent`'s held messages of the user's.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn held_user(&self, agent: &str) -> Vec<String> {
        self.agents
            .lock()
            .get(agent)
            .map(|state| state.steers.held_texts())
            .unwrap_or_default()
    }

    /// Hold `note` for `agent`'s open run ([`HeldNote`]); `Err` gives it
    /// back when no run of it is open (or the daemon stops): it is then
    /// sent at once, as to an idle agent.
    ///
    /// # Errors
    ///
    /// The note itself, when it is not held.
    pub fn hold_note(&self, agent: &str, note: HeldNote) -> Result<(), HeldNote> {
        if *self.stopped.borrow() {
            return Err(note);
        }
        let mut agents = self.agents.lock();
        match agents.get_mut(agent) {
            Some(state) if !state.open.is_empty() => {
                state.notes.push(note);
                Ok(())
            }
            _ => Err(note),
        }
    }

    /// The notes held for `agent`, taken (`None`: none).
    #[must_use]
    pub fn take_notes(&self, agent: &str) -> Option<TakenNotes> {
        let mut agents = self.agents.lock();
        let state = agents.get_mut(agent)?;
        Self::taken(&self.this, agent, state)
    }

    /// The notes held for `agent`, taken to go on their own at a tool
    /// request: only while no message of the user's is in flight or held
    /// (else they wait to go with the user's, [`steers`]).
    #[must_use]
    pub fn take_quiet_notes(&self, agent: &str) -> Option<TakenNotes> {
        let mut agents = self.agents.lock();
        let state = agents.get_mut(agent)?;
        if !state.steers.is_quiet() {
            return None;
        }
        Self::taken(&self.this, agent, state)
    }

    /// Tests: how many notes are held for `agent`.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn held_notes(&self, agent: &str) -> usize {
        self.agents
            .lock()
            .get(agent)
            .map_or(0, |state| state.notes.len())
    }

    /// `state`'s notes, taken with a [`Sending`] guard (under the lock).
    fn taken(this: &Weak<Self>, agent: &str, state: &mut State) -> Option<TakenNotes> {
        if state.notes.is_empty() {
            return None;
        }
        state.change();
        state.sending += 1;
        Some(TakenNotes {
            notes: std::mem::take(&mut state.notes),
            _sending: Sending {
                activity: this.clone(),
                agent: agent.to_string(),
            },
        })
    }

    /// `agent`'s run `run` of `session` started: whoever waited for it
    /// ([`Self::when_running`]) is told.
    pub fn run_started(&self, agent: &str, session: &SessionId, run: Option<&RunId>) {
        let waiting = {
            let mut agents = self.agents.lock();
            let state = agents.entry(agent.to_string()).or_default();
            state.change();
            state.starting = false;
            let Some(run) = run else {
                return;
            };
            state.open.insert(run.clone(), session.clone());
            state.latest = Some(run.clone());
            std::mem::take(&mut state.waiting)
        };
        if let Some(run) = run {
            for told in waiting {
                told(session, run);
            }
        }
    }

    /// The open run of `agent` and its conversation, as the daemon read
    /// it (`None`: none open).
    #[must_use]
    pub fn open_run(&self, agent: &str) -> Option<(SessionId, RunId)> {
        self.agents.lock().get(agent)?.open_run()
    }

    /// Tell `told` of `agent`'s open run: at once when one is open, else
    /// at the next run's start. For a client that attached while the
    /// engine ran a run whose start this record had not read yet.
    pub fn when_running(&self, agent: &str, told: OnRun) {
        let open = {
            let mut agents = self.agents.lock();
            let state = agents.entry(agent.to_string()).or_default();
            match state.open_run() {
                Some(open) => open,
                None => {
                    state.waiting.push(told);
                    return;
                }
            }
        };
        told(&open.0, &open.1);
    }

    /// `agent`'s run `run` ended: what was held for it, when no run of it
    /// is open any more (it goes as an idle agent's would: it starts the
    /// next run): the user's held messages with the notes, else the notes.
    pub fn run_ended(&self, agent: &str, run: Option<&RunId>) -> Option<Outgo> {
        let outgo = {
            let mut agents = self.agents.lock();
            let state = agents.entry(agent.to_string()).or_default();
            state.change();
            state.starting = false;
            if let Some(run) = run {
                state.open.remove(run);
                state.last_ended = Some(run.clone());
            }
            if state.open.is_empty() {
                match state.steers.run_ended() {
                    Some(release) => Some(Outgo::User(Self::releasing(
                        &self.this, agent, state, release,
                    ))),
                    None => Self::taken(&self.this, agent, state).map(Outgo::Notes),
                }
            } else {
                None
            }
        };
        self.settle(agent);
        outgo
    }

    /// An input for `agent` is on its way to the engine until the guard is
    /// dropped (when the engine answered it).
    #[must_use]
    pub fn sending(&self, agent: &str) -> Sending {
        let mut agents = self.agents.lock();
        let state = agents.entry(agent.to_string()).or_default();
        state.change();
        state.sending += 1;
        Sending {
            activity: self.this.clone(),
            agent: agent.to_string(),
        }
    }

    /// `agent` was seated again (a new incarnation: its old runs' ends
    /// never come) or its inputs changed outside the stream (a recall):
    /// decide again.
    pub fn changed(&self, agent: &str, reseated: bool) {
        let (dropped, user) = {
            let mut agents = self.agents.lock();
            let state = agents.entry(agent.to_string()).or_default();
            state.change();
            if reseated {
                state.open.clear();
                state.starting = false;
                (std::mem::take(&mut state.notes), state.steers.drop_all())
            } else {
                (Vec::new(), Vec::new())
            }
        };
        for note in dropped {
            let _ = note
                .done
                .send(Err("the agent was seated again first".to_string()));
        }
        self.dropped(
            agent,
            &user,
            "the agent was seated again before it was sent",
        );
        self.settle(agent);
    }

    /// `agent` retired (a note held for it is answered as not sent, and a
    /// message of the user's held for it goes back to its client).
    pub fn forget(&self, agent: &str) {
        let state = self.agents.lock().remove(agent);
        let Some(mut state) = state else {
            return;
        };
        for note in std::mem::take(&mut state.notes) {
            let _ = note.done.send(Err("the agent retired first".to_string()));
        }
        let user = state.steers.drop_all();
        self.dropped(agent, &user, "the agent retired before it was sent");
    }

    /// Whether a run of `agent` the daemon saw start has not ended yet.
    #[must_use]
    pub fn running(&self, agent: &str) -> bool {
        self.agents
            .lock()
            .get(agent)
            .is_some_and(|state| !state.open.is_empty())
    }

    /// What the clients were told of `agent`, when it is idle now.
    #[must_use]
    pub fn idle(&self, agent: &str) -> Option<IdleNote> {
        self.agents.lock().get(agent)?.idle.clone()
    }

    fn sent(&self, agent: &str) {
        {
            let mut agents = self.agents.lock();
            let Some(state) = agents.get_mut(agent) else {
                return;
            };
            state.change();
            state.sending = state.sending.saturating_sub(1);
        }
        self.settle(agent);
    }

    /// Ask the runtime about `agent` unless it is busy or asked already.
    fn settle(&self, agent: &str) {
        if *self.stopped.borrow() {
            return;
        }
        let Some(this) = self.this.upgrade() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        {
            let mut agents = self.agents.lock();
            let Some(state) = agents.get_mut(agent) else {
                return;
            };
            if state.checking || state.busy() {
                return;
            }
            state.checking = true;
        }
        runtime.spawn(this.check(agent.to_string()));
    }

    /// Ask the runtime until the agent is idle, busy again, gone, or
    /// [`SETTLE_WAIT`] has passed, or the daemon stops ([`Self::stop`]).
    async fn check(self: Arc<Self>, agent: String) {
        let started = tokio::time::Instant::now();
        let mut wait = RECHECK_FIRST;
        let mut stopped = self.stopped.subscribe();
        loop {
            let generation = match self.agents.lock().get(&agent) {
                Some(state) => state.generation,
                None => return,
            };
            let probed = tokio::select! {
                probed = (self.probe)(agent.clone()) => probed,
                () = until_stopped(&mut stopped) => return,
            };
            let past = started.elapsed() >= SETTLE_WAIT;
            match self.conclude(&agent, generation, probed, past) {
                Next::Done => return,
                Next::Again => continue,
                Next::Wait => {}
            }
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                () = until_stopped(&mut stopped) => return,
            }
            wait = (wait * 2).min(RECHECK_MAX);
        }
    }

    /// What the answer `probed` to the question asked at `generation`
    /// means; the clients are told when it makes the agent idle.
    fn conclude(&self, agent: &str, generation: u64, probed: Option<Probed>, past: bool) -> Next {
        let note = {
            let mut agents = self.agents.lock();
            let Some(state) = agents.get_mut(agent) else {
                return Next::Done;
            };
            if state.generation != generation && !state.busy() {
                return Next::Again;
            }
            let done = match &probed {
                _ if state.busy() => true,
                None => true,
                Some(probed) => past || (probed.unfinished == 0 && probed.run == RunState::Idle),
            };
            if !done {
                return Next::Wait;
            }
            state.checking = false;
            let Some(probed) = probed.filter(|_| !state.busy()) else {
                return Next::Done;
            };
            // Idle: what a user's input started has ended (or never ran).
            state.starting = false;
            let note = IdleNote {
                agent: agent.to_string(),
                session_id: probed.session,
                last_run: state.last_ended.clone(),
                held: probed.unfinished,
            };
            state.idle = Some(note.clone());
            note
        };
        (self.announce)(&note);
        Next::Done
    }
}

/// Resolves once `stopped` is `true` (also when it is already). Its
/// sender is the `Activity` the caller holds, so it is never dropped first.
async fn until_stopped(stopped: &mut tokio::sync::watch::Receiver<bool>) {
    let _ = stopped.wait_for(|stopped| *stopped).await;
}

/// An input on its way to the engine ([`Activity::sending`]): dropped when
/// the engine answered it (or the connection carrying it ended).
#[derive(Debug)]
pub struct Sending {
    activity: Weak<Activity>,
    agent: String,
}

impl Drop for Sending {
    fn drop(&mut self) {
        if let Some(activity) = self.activity.upgrade() {
            activity.sent(&self.agent);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// What the fake runtime answers, and what the clients were told.
    #[derive(Default)]
    struct World {
        unfinished: usize,
        run: Option<RunState>,
        asked: usize,
        told: Vec<IdleNote>,
        held: Vec<HeldChange>,
    }

    fn activity(session: &SessionId) -> (Arc<Activity>, Arc<parking_lot::Mutex<World>>) {
        let world = Arc::new(parking_lot::Mutex::new(World::default()));
        let probe = {
            let world = Arc::clone(&world);
            let session = session.clone();
            Box::new(move |_agent: String| {
                let mut world = world.lock();
                world.asked += 1;
                let probed = Probed {
                    session: session.clone(),
                    unfinished: world.unfinished,
                    run: world.run.unwrap_or(RunState::Idle),
                };
                Box::pin(async move { Some(probed) }) as BoxFuture<'static, Option<Probed>>
            })
        };
        let announce = {
            let world = Arc::clone(&world);
            Box::new(move |note: &IdleNote| world.lock().told.push(note.clone()))
        };
        let told = {
            let world = Arc::clone(&world);
            Box::new(move |change: &HeldChange| world.lock().held.push(change.clone()))
        };
        (Activity::new(probe, announce, told), world)
    }

    /// Let the spawned questions run (the clock is paused: sleeps end at
    /// once when nothing else is ready).
    async fn settle() {
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
    }

    /// A run's end with nothing held tells the clients the agent is idle
    /// after that run, once; a run open or an input on its way tells
    /// nothing.
    #[tokio::test(start_paused = true)]
    async fn a_run_end_with_nothing_held_is_idle_after_that_run() {
        let session = SessionId::new();
        let (activity, world) = activity(&session);
        let (first, second) = (RunId::new(), RunId::new());
        activity.run_started("a", &session, Some(&first));
        activity.run_started("a", &session, Some(&second));
        activity.run_ended("a", Some(&first));
        settle().await;
        assert!(world.lock().told.is_empty(), "a run is still open");
        let sending = activity.sending("a");
        activity.run_ended("a", Some(&second));
        settle().await;
        assert!(world.lock().told.is_empty(), "an input is on its way");
        drop(sending);
        settle().await;
        let told = world.lock().told.clone();
        assert_eq!(told.len(), 1, "{told:?}");
        assert_eq!(told[0].last_run.as_ref(), Some(&second));
        assert_eq!((told[0].held, &told[0].session_id), (0, &session));
        assert_eq!(activity.idle("a"), Some(told[0].clone()));
    }

    /// Input still held after a run's end (a steer that runs as a
    /// follow-up run) tells nothing; the follow-up run's start stops the
    /// questions and its end, with nothing held, is the idle one.
    #[tokio::test(start_paused = true)]
    async fn held_input_waits_for_its_follow_up_run() {
        let session = SessionId::new();
        let (activity, world) = activity(&session);
        let (first, follow_up) = (RunId::new(), RunId::new());
        world.lock().unfinished = 1;
        activity.run_started("a", &session, Some(&first));
        activity.run_ended("a", Some(&first));
        tokio::time::sleep(Duration::from_millis(700)).await;
        settle().await;
        assert!(world.lock().told.is_empty());
        assert!(world.lock().asked > 1, "asked again while it held input");
        activity.run_started("a", &session, Some(&follow_up));
        let asked = world.lock().asked;
        tokio::time::sleep(SETTLE_WAIT * 2).await;
        settle().await;
        assert!(world.lock().told.is_empty());
        assert!(
            world.lock().asked <= asked + 1,
            "no questions while it runs"
        );
        world.lock().unfinished = 0;
        activity.run_ended("a", Some(&follow_up));
        settle().await;
        let told = world.lock().told.clone();
        assert_eq!(told.len(), 1, "{told:?}");
        assert_eq!(told[0].last_run.as_ref(), Some(&follow_up));
    }

    /// A run still closing, or input the runtime never starts a run for,
    /// is asked about until the bound; then the agent is idle with what
    /// it holds.
    #[tokio::test(start_paused = true)]
    async fn input_held_past_the_bound_is_reported_with_the_idle() {
        let session = SessionId::new();
        let (activity, world) = activity(&session);
        let run = RunId::new();
        world.lock().unfinished = 2;
        world.lock().run = Some(RunState::Running);
        activity.run_started("a", &session, Some(&run));
        activity.run_ended("a", Some(&run));
        tokio::time::sleep(SETTLE_WAIT / 2).await;
        settle().await;
        assert!(world.lock().told.is_empty());
        tokio::time::sleep(SETTLE_WAIT).await;
        settle().await;
        let told = world.lock().told.clone();
        assert_eq!(told.len(), 1, "{told:?}");
        assert_eq!(told[0].held, 2);
        // Bounded: no more questions after it.
        let asked = world.lock().asked;
        tokio::time::sleep(SETTLE_WAIT).await;
        settle().await;
        assert_eq!(world.lock().asked, asked);
    }

    /// The daemon's stop ends a question under way at once (it held the
    /// record, and through the probe the runtime, for up to the bound),
    /// and asks none after it.
    #[tokio::test(start_paused = true)]
    async fn the_stop_ends_the_questions_and_lets_the_record_go() {
        let session = SessionId::new();
        let (activity, world) = activity(&session);
        let run = RunId::new();
        // A conversation the shutdown unregistered: never anything but held.
        world.lock().unfinished = 1;
        activity.run_started("a", &session, Some(&run));
        activity.run_ended("a", Some(&run));
        tokio::time::sleep(Duration::from_millis(200)).await;
        settle().await;
        assert!(world.lock().asked > 1, "asking while it holds input");
        let held = Arc::downgrade(&activity);
        activity.stop();
        drop(activity);
        settle().await;
        assert!(
            held.upgrade().is_none(),
            "a question still holds the record after the stop"
        );
        let asked = world.lock().asked;
        tokio::time::sleep(SETTLE_WAIT * 2).await;
        settle().await;
        assert_eq!(world.lock().asked, asked, "nothing asked after the stop");
        assert!(world.lock().told.is_empty());
    }

    /// No question starts after the stop.
    #[tokio::test(start_paused = true)]
    async fn nothing_is_asked_after_the_stop() {
        let session = SessionId::new();
        let (activity, world) = activity(&session);
        activity.stop();
        let run = RunId::new();
        activity.run_started("a", &session, Some(&run));
        activity.run_ended("a", Some(&run));
        drop(activity.sending("a"));
        settle().await;
        assert_eq!(world.lock().asked, 0);
    }

    /// A change while the runtime is asked (an input sent and answered in
    /// between) has the question asked again before anything is told.
    #[tokio::test(start_paused = true)]
    async fn a_change_during_the_question_asks_again() {
        let session = SessionId::new();
        let (activity, world) = activity(&session);
        let run = RunId::new();
        activity.run_started("a", &session, Some(&run));
        activity.run_ended("a", Some(&run));
        // Before the spawned question runs: an input comes and is answered.
        drop(activity.sending("a"));
        settle().await;
        let world = world.lock();
        assert_eq!(world.told.len(), 1, "told once: {:?}", world.told);
        assert!(world.asked >= 1);
    }

    /// The open run is the one that started last while it is open; one
    /// waiting for a run is told at once when one is open, else once, at
    /// the next start, and a reseat forgets the runs.
    #[tokio::test(start_paused = true)]
    async fn the_open_run_is_told_now_or_at_the_next_start() {
        let session = SessionId::new();
        let (activity, _world) = activity(&session);
        let (first, second) = (RunId::new(), RunId::new());
        let told = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let tell = || {
            let told = Arc::clone(&told);
            Box::new(move |_: &SessionId, run: &RunId| told.lock().push(run.clone())) as OnRun
        };
        assert_eq!(activity.open_run("a"), None);
        activity.when_running("a", tell());
        assert!(told.lock().is_empty(), "nothing is open");
        activity.run_started("a", &session, Some(&first));
        activity.run_started("a", &session, Some(&second));
        assert_eq!(*told.lock(), vec![first.clone()], "told once, at the start");
        assert_eq!(
            activity.open_run("a"),
            Some((session.clone(), second.clone()))
        );
        activity.run_ended("a", Some(&second));
        assert_eq!(activity.open_run("a"), Some((session, first.clone())));
        activity.when_running("a", tell());
        assert_eq!(*told.lock(), vec![first.clone(), first], "at once");
        activity.changed("a", true);
        assert_eq!(activity.open_run("a"), None, "a new incarnation");
    }

    fn note(text: &str) -> (HeldNote, tokio::sync::oneshot::Receiver<Result<(), String>>) {
        let (done, told) = tokio::sync::oneshot::channel();
        (
            HeldNote {
                text: text.to_string(),
                done,
            },
            told,
        )
    }

    /// The user's messages behind the one in flight are held with the
    /// daemon's notes: the notes do not go on their own at a tool request
    /// meanwhile, they go with the held messages once the one in flight
    /// is written; the clients hear each step; a run's end sends what is
    /// held; the stop drops what is left, told.
    #[tokio::test(start_paused = true)]
    async fn the_user_s_held_messages_take_the_notes_with_them() {
        let session = SessionId::new();
        let (activity, world) = activity(&session);
        let run = RunId::new();
        activity.run_started("a", &session, Some(&run));
        assert_eq!(activity.user_steer("a", "k1", "one"), Admit::Forward);
        let Admit::Held(two) = activity.user_steer("a", "k2", "two") else {
            panic!("held behind the one in flight");
        };
        let (note, note_told) = note("NOTE");
        activity.hold_note("a", note).unwrap();
        assert!(
            activity.take_quiet_notes("a").is_none(),
            "a user message waits"
        );
        let first = InputId::new();
        assert!(activity.user_answered("k1", Ok(first.clone())).is_none());
        let releasing = activity.boundary_applied("a", &first).expect("they go");
        assert_eq!(releasing.release.text(), "two");
        assert_eq!(
            releasing.notes.as_ref().map(TakenNotes::text).as_deref(),
            Some("NOTE")
        );
        let merged = InputId::new();
        let Releasing { release, notes, .. } = releasing;
        notes.unwrap().answer(&Ok(()));
        assert_eq!(note_told.await.unwrap(), Ok(()));
        assert!(
            activity
                .released("a", &release, Ok(Some(merged.clone())))
                .is_none()
        );
        assert_eq!(
            activity.held_where("a", &two),
            HeldWhere::Sent(merged.clone())
        );
        assert!(activity.boundary_applied("a", &merged).is_none());
        let held = world.lock().held.clone();
        assert_eq!(
            held.iter().map(|c| (&c.ids, &c.now)).collect::<Vec<_>>(),
            vec![
                (
                    &vec![two.clone()],
                    &HeldNow::Released {
                        input_id: merged.clone()
                    }
                ),
                (&vec![two.clone()], &HeldNow::Applied)
            ]
        );
        // Held at the run's end: they start the next run.
        activity.user_steer("a", "k3", "three");
        activity.user_steer("a", "k4", "four");
        let Some(Outgo::User(ended)) = activity.run_ended("a", Some(&run)) else {
            panic!("the held one goes at the run's end");
        };
        assert_eq!(
            (ended.release.serial, ended.release.text()),
            (None, "four".to_string())
        );
        // Not running: forwarded. Held at the stop: dropped, told.
        assert_eq!(activity.user_steer("a", "k5", "five"), Admit::Forward);
        activity.run_started("a", &session, Some(&RunId::new()));
        activity.user_steer("a", "k6", "six");
        let Admit::Held(seven) = activity.user_steer("a", "k7", "seven") else {
            panic!("held");
        };
        activity.stop();
        assert!(matches!(
            &world.lock().held.last().unwrap(),
            HeldChange { ids, now: HeldNow::Dropped { .. }, .. } if *ids == vec![seven]
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn notes_are_held_only_for_an_open_run_and_go_together() {
        let session = SessionId::new();
        let (activity, world) = activity(&session);
        // No run open: given back, to be sent at once.
        let (idle, _) = note("idle");
        assert_eq!(activity.hold_note("a", idle).unwrap_err().text, "idle");
        let run = RunId::new();
        activity.run_started("a", &session, Some(&run));
        let (one, one_told) = note("one");
        let (two, two_told) = note("two");
        activity.hold_note("a", one).unwrap();
        activity.hold_note("a", two).unwrap();
        let taken = activity.take_notes("a").unwrap();
        assert_eq!(taken.text(), "one\n\ntwo");
        assert!(activity.take_notes("a").is_none(), "taken once");
        // Busy while they are sent, even with the run ended.
        assert!(activity.run_ended("a", Some(&run)).is_none());
        settle().await;
        assert!(world.lock().told.is_empty(), "not idle while they go");
        taken.answer(&Ok(()));
        assert_eq!(one_told.await.unwrap(), Ok(()));
        assert_eq!(two_told.await.unwrap(), Ok(()));
        // Held at a run's end: they go with it; at the stop: answered.
        let run = RunId::new();
        activity.run_started("a", &session, Some(&run));
        let (late, _) = note("late");
        activity.hold_note("a", late).unwrap();
        let Some(Outgo::Notes(ended)) = activity.run_ended("a", Some(&run)) else {
            panic!("the notes go at the run's end");
        };
        assert_eq!(ended.text(), "late");
        activity.run_started("a", &session, Some(&RunId::new()));
        let (stopped, stopped_told) = note("stopped");
        activity.hold_note("a", stopped).unwrap();
        activity.stop();
        assert!(stopped_told.await.unwrap().is_err());
    }
}
