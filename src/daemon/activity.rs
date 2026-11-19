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

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use futures::future::BoxFuture;
use meerkat_core::{RunId, SessionId};

use super::conversations::RunState;
use super::protocol::IdleNote;

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
    /// `announce`.
    #[must_use]
    pub fn new(probe: Probe, announce: Announce) -> Arc<Self> {
        Arc::new_cyclic(|this| Self {
            agents: parking_lot::Mutex::new(HashMap::new()),
            probe,
            announce,
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
        let held: Vec<HeldNote> = self
            .agents
            .lock()
            .values_mut()
            .flat_map(|state| std::mem::take(&mut state.notes))
            .collect();
        for note in held {
            let _ = note.done.send(Err("the daemon stopped first".to_string()));
        }
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

    /// `agent`'s run `run` ended: the notes held for it, when no run of it
    /// is open any more (they go as an idle agent's would).
    pub fn run_ended(&self, agent: &str, run: Option<&RunId>) -> Option<TakenNotes> {
        let notes = {
            let mut agents = self.agents.lock();
            let state = agents.entry(agent.to_string()).or_default();
            state.change();
            if let Some(run) = run {
                state.open.remove(run);
                state.last_ended = Some(run.clone());
            }
            if state.open.is_empty() {
                Self::taken(&self.this, agent, state)
            } else {
                None
            }
        };
        self.settle(agent);
        notes
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
        let dropped = {
            let mut agents = self.agents.lock();
            let state = agents.entry(agent.to_string()).or_default();
            state.change();
            if reseated {
                state.open.clear();
                std::mem::take(&mut state.notes)
            } else {
                Vec::new()
            }
        };
        for note in dropped {
            let _ = note
                .done
                .send(Err("the agent was seated again first".to_string()));
        }
        self.settle(agent);
    }

    /// `agent` retired (a note held for it is answered as not sent).
    pub fn forget(&self, agent: &str) {
        let state = self.agents.lock().remove(agent);
        for note in state.map(|state| state.notes).unwrap_or_default() {
            let _ = note.done.send(Err("the agent retired first".to_string()));
        }
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
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// What the fake runtime answers, and what the clients were told.
    #[derive(Default)]
    struct World {
        unfinished: usize,
        run: Option<RunState>,
        asked: usize,
        told: Vec<IdleNote>,
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
        (Activity::new(probe, announce), world)
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
        let ended = activity.run_ended("a", Some(&run)).unwrap();
        assert_eq!(ended.text(), "late");
        activity.run_started("a", &session, Some(&RunId::new()));
        let (stopped, stopped_told) = note("stopped");
        activity.hold_note("a", stopped).unwrap();
        activity.stop();
        assert!(stopped_told.await.unwrap().is_err());
    }
}
