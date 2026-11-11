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

use std::collections::{HashMap, HashSet};
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

/// One agent's activity as the daemon saw it.
#[derive(Debug, Default)]
struct State {
    /// Runs started and not ended.
    open: HashSet<RunId>,
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
}

impl State {
    fn busy(&self) -> bool {
        !self.open.is_empty() || self.sending > 0
    }

    fn change(&mut self) {
        self.generation += 1;
        self.idle = None;
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
        })
    }

    /// `agent`'s run `run` started.
    pub fn run_started(&self, agent: &str, run: Option<&RunId>) {
        let mut agents = self.agents.lock();
        let state = agents.entry(agent.to_string()).or_default();
        state.change();
        if let Some(run) = run {
            state.open.insert(run.clone());
        }
    }

    /// `agent`'s run `run` ended.
    pub fn run_ended(&self, agent: &str, run: Option<&RunId>) {
        {
            let mut agents = self.agents.lock();
            let state = agents.entry(agent.to_string()).or_default();
            state.change();
            if let Some(run) = run {
                state.open.remove(run);
                state.last_ended = Some(run.clone());
            }
        }
        self.settle(agent);
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
        {
            let mut agents = self.agents.lock();
            let state = agents.entry(agent.to_string()).or_default();
            state.change();
            if reseated {
                state.open.clear();
            }
        }
        self.settle(agent);
    }

    /// `agent` retired.
    pub fn forget(&self, agent: &str) {
        self.agents.lock().remove(agent);
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
    /// [`SETTLE_WAIT`] has passed.
    async fn check(self: Arc<Self>, agent: String) {
        let started = tokio::time::Instant::now();
        let mut wait = RECHECK_FIRST;
        loop {
            let generation = match self.agents.lock().get(&agent) {
                Some(state) => state.generation,
                None => return,
            };
            let probed = (self.probe)(agent.clone()).await;
            let past = started.elapsed() >= SETTLE_WAIT;
            match self.conclude(&agent, generation, probed, past) {
                Next::Done => return,
                Next::Again => continue,
                Next::Wait => {}
            }
            tokio::time::sleep(wait).await;
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
        activity.run_started("a", Some(&first));
        activity.run_started("a", Some(&second));
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
        activity.run_started("a", Some(&first));
        activity.run_ended("a", Some(&first));
        tokio::time::sleep(Duration::from_millis(700)).await;
        settle().await;
        assert!(world.lock().told.is_empty());
        assert!(world.lock().asked > 1, "asked again while it held input");
        activity.run_started("a", Some(&follow_up));
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
        activity.run_started("a", Some(&run));
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

    /// A change while the runtime is asked (an input sent and answered in
    /// between) has the question asked again before anything is told.
    #[tokio::test(start_paused = true)]
    async fn a_change_during_the_question_asks_again() {
        let session = SessionId::new();
        let (activity, world) = activity(&session);
        let run = RunId::new();
        activity.run_started("a", Some(&run));
        activity.run_ended("a", Some(&run));
        // Before the spawned question runs: an input comes and is answered.
        drop(activity.sending("a"));
        settle().await;
        let world = world.lock();
        assert_eq!(world.told.len(), 1, "told once: {:?}", world.told);
        assert!(world.asked >= 1);
    }
}
