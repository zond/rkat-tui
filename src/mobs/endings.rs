//! How each helper ends, and when its parent is told.
//!
//! A helper's parent is the session that started it: the session
//! that seated it or created its mob. The parent learns of the ending
//! once, at once, however it ends ([`How`]); the daemon words and delivers
//! the note (`crate::daemon::agents::endings`). [`HelperEndings`] decides
//! when an ending is one to tell, from two sources:
//!
//! - the helper's own run ending ([`Terminal`], from its event stream);
//! - the user stopping it ([`HelperEndings::stop`]).
//!
//! A single-use helper ([`super::HelperLife::Single`]) ends when it
//! finishes its task: a run of it ended, completed or failed, and the
//! runtime holds no unfinished input of it ([`HelperEndings::run_ended`]
//! asks until none is left or a later run starts, at most
//! [`SETTLE_WAIT`]). Its parent is then told (`Done`, with its answer, or
//! `Failed`) and it is retired. A message its parent sent that it has not
//! read when its run ends is unfinished input: the helper goes on and is
//! not retired. A message sent after that look reaches a helper that is
//! being retired, and its sender's `send_message` fails as any message to a
//! peer that left (the engine names `peers`); the window is the retire's
//! own few milliseconds. The daemon's own notes to a helper (the ending of
//! a helper it started) are never in that window: each is sent while no
//! decision runs ([`HelperEndings::while_unsettled`]), so it lands before
//! the decision's last look (the helper goes on and reads it) or finds the
//! helper settled and goes to the next one up its path.
//!
//! A kept helper ([`super::HelperLife::Kept`]: `assign` with `keep`, a
//! member of a mob the agent created) does not end when a run of it ends: it ends when it
//! is retired, which only the user's stop tells here (an agent that
//! retires one with a tool is told by that tool's result). Each run of it
//! that leaves it idle the same way is told to its parent once (`WentIdle`,
//! with its last message), unless the run's last call was a message to the
//! parent ([`HelperEndings::called`]: the parent has its last word). No
//! note says that a member started (its spawn's result does) or became
//! active (its parent or a peer made it so, by a message the sender knows
//! of): each note costs its parent a model call, and one for every
//! activation would let two agents wake each other forever.
//!
//! A run of any of these that reaches its turn limit ([`Terminal::Limit`])
//! is no ending: it goes to the daemon's checkpoint
//! ([`HelperEndings::delivering`], `crate::daemon::checkpoint`), and
//! the end of the run its status is asked in is held too. The checkpoint
//! either continues it ([`HelperEndings::checkpoint_continues`]: the runs
//! after are told as any) or stops it ([`HelperEndings::checkpoint_stopped`]:
//! an `assign` helper is done with its status as its answer and retired, a
//! member stays seated, idle; its parent is told either way).
//!
//! A fork ([`super::HelperContext::Fork`]) is told as the others of its
//! life, except for its first run: the engine tells the parent of that
//! run's end itself ("Background fork_off job … finished"), and of no later
//! run, which the parent's messages start now that the child is wired to it.
//! A single-use fork is retired once the engine recorded that outcome
//! ([`HelperEndings::fork_outcome_recorded`]).
//!
//! # States
//!
//! Each helper seen is in one [`Ending`], all under one lock:
//!
//! - `Followed`: a helper, with its tag, how many
//!   runs of it started, and a [`Phase`]:
//!   - `Working { messaged_parent }`: spawned and not run yet, running, or
//!     its run's end not decided yet (at work);
//!   - `Idle`: its latest run's end was decided idle, or it was restored
//!     with no run;
//!   - `Checkpoint { through }`: its runs up to `through` (the run at its
//!     limit and the status run after it) are its checkpoint's.
//!
//!   and, for a fork, what the engine tells of it ([`ForkNews`]: nothing,
//!   its first run's end, or its outcome on record).
//! - `Settled`: its ending was settled, told or not.
//!
//! The transitions, each a named method of the state it moves:
//!
//! - spawned: absent or `Settled` → `Followed` (`Working`); one live
//!   already only takes the new tag.
//! - run started: `Working` or `Idle` → `Working { messaged_parent: false }`;
//!   `Checkpoint` stays while the run is one of its own, else → `Working`.
//! - tool called: `Working` records whether it messaged its parent;
//!   refused in any other phase.
//! - run ended: `Checkpoint` → nothing; at the limit with a checkpoint
//!   to take it → `Checkpoint { through: run + 1 }`; otherwise the end is
//!   decided ([`HelperEndings::run_ended`]).
//! - run decided idle: → `Idle`, refused once a later run started; a
//!   single-use helper is then `Settled` (done), unless a helper of its
//!   own is still at work.
//! - restored idle: `Working` with no run → `Idle`; refused once a run
//!   started.
//! - checkpoint continues or stops: → `Checkpoint` through the current
//!   run; stopped, a single-use helper is then `Settled`.
//! - fork outcome recorded: a single-use fork only (a restored fresh fork's
//!   record is its own first run's, refused) → `ForkNews::Recorded`.
//! - stop, left, retire owed, done: any live state → `Settled`, in
//!   one take, so exactly one of them tells; each refuses a `Settled` one.
//!
//! Unrepresentable: a followed helper without a tag; the record of
//! whether a run messaged its parent while it is idle; a helper both
//! settled and followed.

#[cfg(any(test, feature = "test-support"))]
mod probe;
#[cfg(any(test, feature = "test-support"))]
pub use probe::DecidingHookFn;

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Weak};

use crate::late::Late;

use meerkat_core::AgentEvent;
use meerkat_mob_mcp::MobMcpState;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use super::{HelperKey, HelperTag};

/// How a helper ended, as its parent and the user are told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum How {
    /// An `assign` helper finished its task (its run ended and nothing is
    /// queued for it): `answer` is its final text, and it is retired.
    Done {
        /// Its final answer.
        answer: String,
    },
    /// A long-lived member's run ended and it is idle, still seated: not
    /// an ending. `answer` is its final text, or why its run failed.
    WentIdle {
        /// Its final text.
        answer: String,
        /// Why its run failed, when it did.
        failed: Option<String>,
    },
    /// Its run failed.
    Failed {
        /// Why, in the words the engine gave.
        reason: String,
    },
    /// The user stopped it.
    StoppedByUser,
    /// A run of it reached its checkpoint after `steps` model calls and
    /// was not continued (`crate::daemon::checkpoint`): `status` is what
    /// it said of its work when asked. An `assign` helper is `retired`
    /// with it as its answer; a long-lived member stays seated, idle.
    CheckpointStopped {
        /// The steps its run made.
        steps: u64,
        /// Why it was not continued, in plain words.
        reason: String,
        /// Its status (its answer, for an `assign` helper).
        status: String,
        retired: bool,
    },
}

impl How {
    /// How a client's line reads the ending (an idle member, which is not
    /// announced, reads as stopped).
    #[must_use]
    pub fn ending(&self) -> crate::daemon::protocol::HelperEnding {
        use crate::daemon::protocol::HelperEnding;
        match self {
            Self::Done { .. } => HelperEnding::Finished,
            Self::Failed { .. } => HelperEnding::Failed,
            Self::WentIdle { .. } | Self::StoppedByUser | Self::CheckpointStopped { .. } => {
                HelperEnding::Stopped
            }
        }
    }

    /// The words a helper's line shows (`stopped by you`).
    #[must_use]
    pub fn text(&self) -> String {
        match self {
            Self::Done { .. } => "finished".to_string(),
            Self::WentIdle { failed: None, .. } => "idle".to_string(),
            Self::WentIdle {
                failed: Some(reason),
                ..
            } => format!("idle after a failed run: {}", plain_reason(reason)),
            Self::Failed { reason } => format!("failed: {}", plain_reason(reason)),
            Self::StoppedByUser => "stopped by you".to_string(),
            Self::CheckpointStopped { steps, reason, .. } => {
                format!("stopped at its checkpoint after {steps} steps: {reason}")
            }
        }
    }
}

/// A failure's reason without the engine's own terms: a run the engine's
/// recovery closed (`InterruptedByRecovery`) was cut off by a stop.
#[must_use]
pub fn plain_reason(reason: &str) -> String {
    let reason = reason.trim();
    if reason.contains("InterruptedByRecovery") || reason.contains("interrupted_by_recovery") {
        "its run was cut off when the daemon stopped".to_string()
    } else if reason.is_empty() {
        "no reason was given".to_string()
    } else {
        reason.to_string()
    }
}

/// One helper's ending, for the daemon to tell its parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ended {
    pub key: HelperKey,
    /// What it was and whom it worked for.
    pub tag: HelperTag,
    /// How it ended.
    pub how: How,
    /// The labels of its own helpers (theirs too) that were retired with
    /// it, deepest first: nothing stays seated without a living owner.
    pub with: Vec<String>,
}

/// The sentence that names the helpers retired with their parent
/// (`None`: none were).
#[must_use]
pub fn retired_with(labels: &[String]) -> Option<String> {
    let named: Vec<String> = labels.iter().map(|label| format!("`{label}`")).collect();
    match named.as_slice() {
        [] => None,
        [one] => Some(format!("Its own helper {one} was retired with it.")),
        many => Some(format!(
            "Its own helpers {} were retired with it.",
            many.join(", ")
        )),
    }
}

/// How a helper's own run ended, from its events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Terminal {
    /// It completed with this final text.
    Completed(String),
    /// It failed, for this reason.
    Failed(String),
    Cancelled,
    /// It reached its turn limit after `steps` model calls: its checkpoint
    /// (`crate::daemon::checkpoint`), not a failure.
    Limit {
        /// The model calls its run made (0 when unknown).
        steps: u64,
    },
}

impl Terminal {
    /// The run end `event` is (`None`: no run end). A run at its turn
    /// limit is [`Self::Limit`] with no steps counted.
    #[must_use]
    pub fn of(event: &AgentEvent) -> Option<Self> {
        match event {
            AgentEvent::RunCompleted { result, .. } => Some(Self::Completed(result.clone())),
            AgentEvent::RunFailed { error_report, .. } => Some(match error_report.class {
                meerkat_core::AgentErrorClass::Cancelled => Self::Cancelled,
                meerkat_core::AgentErrorClass::MaxTurns => Self::Limit { steps: 0 },
                _ => Self::Failed(error_report.message.clone()),
            }),
            _ => None,
        }
    }
}

/// A helper's run reached its checkpoint: what the daemon's checkpoint
/// ([`HelperEndings::delivering`]) is handed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitReached {
    pub key: HelperKey,
    /// What it is and whom it works for.
    pub tag: HelperTag,
    /// The model calls its run made.
    pub steps: u64,
    /// Which of its runs it is (counted from its first; a checkpoint's
    /// continue run is two after the run it continues).
    pub run: u64,
}

/// One helper's ending, as far as it is known: the state of
/// [`HelperEndings`] for each key it has seen (see the module doc's
/// states).
#[derive(Debug, Clone)]
enum Ending {
    /// A helper, followed run by run, not ended yet.
    Followed(Followed),
    /// Its ending was settled, told or not: a late event of it tells
    /// nothing. Kept for the daemon's life (a few dozen bytes each); a key
    /// seated again is a new helper ([`HelperEndings::spawned`]).
    Settled,
}

impl Ending {
    /// The state a helper spawned with `tag` starts in.
    fn spawned(tag: HelperTag) -> Self {
        Self::Followed(Followed::new(tag))
    }

    /// Whether it is at work: a followed helper not decided idle since its
    /// last run started.
    fn at_work(&self) -> bool {
        match self {
            Self::Followed(followed) => followed.at_work(),
            Self::Settled => false,
        }
    }
}

/// A helper, followed run by run.
#[derive(Debug, Clone)]
struct Followed {
    /// What it is and whom it works for.
    tag: HelperTag,
    /// How many runs of it started (a run's end is told only while no
    /// later one started).
    runs: u64,
    /// Where it is between its runs.
    phase: Phase,
    /// What the engine tells of it as a fork.
    fork: ForkNews,
}

/// Where a followed helper is between its runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// At work: spawned and not run yet, running, or its run's end not
    /// decided yet. `messaged_parent`: the last tool call of its current
    /// run was a message to its parent.
    Working {
        /// The last call of its current run was a message to its parent.
        messaged_parent: bool,
    },
    /// Its run `runs` (0: restored, woken with no run) was decided idle,
    /// with nothing queued for it.
    Idle,
    /// The ends of its runs up to `through` are its checkpoint's (the run
    /// at its limit and the status run after it): no ending of it. Always
    /// `runs <= through`: the first run past it leaves this phase.
    Checkpoint {
        /// The last run that is the checkpoint's.
        through: u64,
    },
}

/// What the engine tells of a fork's runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForkNews {
    /// Nothing: each run's end is told here (any helper not a fork, and a
    /// fork rebuilt after a restart whose outcome is not on record).
    Nothing,
    /// A fresh fork: the engine tells its first run's end ("Background
    /// fork_off job … finished"); its outcome is not on record yet.
    FirstRun,
    /// A single-use fork whose completed outcome the engine recorded for
    /// its parent (`ForkJobTerminal`,
    /// [`HelperEndings::fork_outcome_recorded`]): done, retired once idle.
    /// `first_run`: a fresh fork's (its first run's end is the engine's).
    Recorded {
        /// Its first run's end is the engine's to tell.
        first_run: bool,
    },
}

impl ForkNews {
    /// Whether its first run's end is the engine's to tell.
    fn first_run(self) -> bool {
        matches!(self, Self::FirstRun | Self::Recorded { first_run: true })
    }

    /// Whether its outcome is on record for its parent.
    fn recorded(self) -> bool {
        matches!(self, Self::Recorded { .. })
    }
}

/// What a run's end of a followed helper goes to ([`Followed::run_ended`]).
#[derive(Debug, Clone, PartialEq, Eq)]
enum RunEnd {
    /// Nothing: a member not started by `assign`, or a run of its
    /// checkpoint.
    Nothing,
    /// Its checkpoint: the run `run` reached its limit after `steps` model
    /// calls.
    Checkpoint {
        /// Which of its runs it is.
        run: u64,
        /// The model calls the run made.
        steps: u64,
    },
    /// The decision whether run `run` left it idle.
    Decide {
        /// Which of its runs it is.
        run: u64,
    },
}

/// What a decided run's end is told from ([`Followed::went_idle`]).
#[derive(Debug, Clone, PartialEq, Eq)]
struct IdleRun {
    /// What it is and whom it works for.
    tag: HelperTag,
    /// The run's last call was a message to its parent.
    messaged_parent: bool,
    /// The run is its first, whose end the engine tells.
    told_by_engine: bool,
    /// Its outcome is on record for its parent.
    recorded: bool,
}

impl Followed {
    /// A helper spawned as `tag`, not run yet.
    fn new(tag: HelperTag) -> Self {
        Self {
            tag,
            runs: 0,
            phase: Phase::Working {
                messaged_parent: false,
            },
            fork: ForkNews::Nothing,
        }
    }

    /// Whether it is at work (not decided idle since its last run started).
    fn at_work(&self) -> bool {
        self.phase != Phase::Idle
    }

    /// A run of it started: it is at work, unless the run is still its
    /// checkpoint's.
    fn run_started(&mut self) {
        self.runs += 1;
        match self.phase {
            Phase::Checkpoint { through } if self.runs <= through => {}
            _ => {
                self.phase = Phase::Working {
                    messaged_parent: false,
                };
            }
        }
    }

    /// It called a tool; `to_parent`: a message to its parent. Recorded
    /// only while it is at work outside a checkpoint (no other phase reads
    /// it, and the next run starts afresh).
    fn called(&mut self, to_parent: bool) {
        if let Phase::Working { messaged_parent } = &mut self.phase {
            *messaged_parent = to_parent;
        }
    }

    /// Its current run ended, not cancelled; `limit`: at its turn limit
    /// after so many model calls. With `checkpoints` (a checkpoint takes
    /// such a run) that run and the status run after it are the
    /// checkpoint's; without, the run's end is decided as a failure's.
    fn run_ended(&mut self, limit: Option<u64>, checkpoints: bool) -> RunEnd {
        if self.tag.kind == super::HelperKind::Other {
            return RunEnd::Nothing;
        }
        if let Phase::Checkpoint { .. } = self.phase {
            return RunEnd::Nothing;
        }
        if let Some(steps) = limit
            && checkpoints
        {
            self.phase = Phase::Checkpoint {
                through: self.runs + 1,
            };
            return RunEnd::Checkpoint {
                run: self.runs,
                steps,
            };
        }
        RunEnd::Decide { run: self.runs }
    }

    /// Its run `run` was decided idle: what it is told from (`None`: a
    /// later run started, so this end is none).
    fn went_idle(&mut self, run: u64) -> Option<IdleRun> {
        if self.runs != run {
            return None;
        }
        let messaged_parent = matches!(
            self.phase,
            Phase::Working {
                messaged_parent: true
            }
        );
        self.phase = Phase::Idle;
        Some(IdleRun {
            tag: self.tag.clone(),
            messaged_parent,
            told_by_engine: self.fork.first_run() && run <= 1,
            recorded: self.fork.recorded(),
        })
    }

    /// Restored, it was woken with no run: idle. Refused once a run of it
    /// started (or its checkpoint holds it).
    fn restored_idle(&mut self) {
        if self.runs == 0 && matches!(self.phase, Phase::Working { .. }) {
            self.phase = Phase::Idle;
        }
    }

    /// Its checkpoint continued or stopped it: the run its status was asked
    /// in (the current one) is the checkpoint's too.
    fn hold_checkpoint(&mut self) {
        let through = match self.phase {
            Phase::Checkpoint { through } => through.max(self.runs),
            Phase::Working { .. } | Phase::Idle => self.runs,
        };
        self.phase = Phase::Checkpoint { through };
    }

    /// It is a fresh fork: its first run's end is the engine's to tell.
    fn first_run_told_by_engine(&mut self) {
        self.fork = match self.fork {
            ForkNews::Nothing | ForkNews::FirstRun => ForkNews::FirstRun,
            ForkNews::Recorded { .. } => ForkNews::Recorded { first_run: true },
        };
    }

    /// The engine recorded its outcome for its parent (`restored`: found
    /// when the daemon restored it): the run after which it is idle now,
    /// to be retired (`None`: it is not, or the record is not one that
    /// makes it done). Refused for a kept fork, any helper not a fork, and
    /// a restored fresh fork (its record is the one its own first run
    /// writes).
    fn outcome_recorded(&mut self, restored: bool) -> Option<u64> {
        let single =
            super::HelperKind::Started(super::HelperContext::Fork, super::HelperLife::Single);
        if self.tag.kind != single || (restored && self.fork.first_run()) {
            return None;
        }
        self.fork = ForkNews::Recorded {
            first_run: self.fork.first_run(),
        };
        // Restored after its outcome came: a fork that ran no run since is
        // idle.
        if restored {
            self.restored_idle();
        }
        (self.phase == Phase::Idle).then_some(self.runs)
    }

    /// Whether it is still idle after run `run`.
    fn idle_after(&self, run: u64) -> bool {
        self.runs == run && self.phase == Phase::Idle
    }
}

/// [`HelperEndings::settle`] found the helper settled already.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AlreadySettled;

/// How many inputs the runtime holds for a helper that are not finished
/// (`None`: it cannot be told; then its run's end is taken as it is).
pub type InputsProbe = Arc<
    dyn Fn(HelperKey) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<usize>> + Send>>
        + Send
        + Sync,
>;

/// The longest a run's end waits for the runtime to hold no unfinished
/// input of the helper before it is taken as the helper's going idle.
pub const SETTLE_WAIT: std::time::Duration = std::time::Duration::from_secs(10);
/// The first wait before asking the runtime again; each next one doubles.
const RECHECK_FIRST: std::time::Duration = std::time::Duration::from_millis(50);
/// The longest wait between two questions.
const RECHECK_MAX: std::time::Duration = std::time::Duration::from_secs(1);

/// The helpers that have not ended yet, and where each ending goes.
#[derive(Debug, Default)]
pub struct HelperEndings {
    /// Each helper seen, in its state ([`Ending`]), under one lock: a
    /// helper's state is read and moved in one take.
    helpers: Mutex<HashMap<HelperKey, Ending>>,
    /// Where the endings to tell go (the daemon's delivery).
    sink: Option<mpsc::UnboundedSender<Ended>>,
    /// The mob host (a helper is retired through it).
    state: Late<Weak<MobMcpState>>,
    /// How many endings were told so far.
    told: std::sync::atomic::AtomicUsize,
    /// How many retires these endings started.
    retires: std::sync::atomic::AtomicUsize,
    /// Asks the runtime what a helper still holds ([`InputsProbe`]).
    probe: Late<Probe>,
    /// How many runs' ends were decided as a helper's going idle (told or
    /// not).
    idle_decided: std::sync::atomic::AtomicUsize,
    /// How many fork outcomes on record were reported
    /// ([`Self::fork_outcome_recorded`]).
    outcomes_reported: std::sync::atomic::AtomicUsize,
    /// The peer ids and names of the `assign` helpers retired when they
    /// finished, newest last ([`Self::retired_peer`]).
    /// With each, the parent it was retired with (`None`: it finished).
    retired_peers: Arc<RetiredPeers>,
    /// Where a run at its limit goes for its checkpoint; none, it is a
    /// failure.
    checkpoints: Option<mpsc::UnboundedSender<LimitReached>>,
    /// Held while a helper's going idle is decided and while a note is
    /// sent to a helper ([`Self::while_unsettled`]): a note either lands
    /// before the decision (unfinished input: the helper goes on) or finds
    /// the helper settled (it goes further up), never in between.
    deciding: tokio::sync::Mutex<()>,
    /// Tests: what they hold ([`probe::EndingsProbe`]).
    #[cfg(any(test, feature = "test-support"))]
    test_probe: probe::EndingsProbe,
    /// The retirements under way, and whether one may still start
    /// ([`Retirements`]).
    retirements: Arc<Retirements>,
    /// Where a decided retire is recorded, so a start that finds the
    /// helper still seated retires it ([`Self::owe_to`]).
    owed: Late<Weak<super::HelperRegistry>>,
}

/// The names of the endings' handles the daemon's start-up binds
/// ([`crate::late`]).
const STATE: &str = "the helper endings' mob host";
const PROBE: &str = "the helper endings' inputs probe";
const OWED: &str = "the helper endings' owed retires";

/// The retirements of helpers under way, closed when the daemon stops: the
/// engine's stop of a member that a retire of it meets mid-way fails the
/// runtime's shutdown ("stop durable persist target Stopped … disagreed with
/// generated lifecycle commit Retired", `docs/evidence.md`, "A stop that
/// met a retire"). So every retire the daemon makes holds a
/// [`Retiring`] while it reaches the engine, none starts once
/// [`Retirements::close`] was called, and the close waits for the ones
/// under way before the mobs are stopped. A helper whose retire was not
/// started is retired by the next start.
#[derive(Debug)]
pub struct Retirements {
    /// Closed, and how many are under way.
    gate: tokio::sync::watch::Sender<(bool, usize)>,
    /// Tests: what they hold and refuse ([`probe::RetirementsProbe`]).
    #[cfg(any(test, feature = "test-support"))]
    probe: probe::RetirementsProbe,
}

impl Default for Retirements {
    fn default() -> Self {
        Self {
            gate: tokio::sync::watch::channel((false, 0)).0,
            #[cfg(any(test, feature = "test-support"))]
            probe: probe::RetirementsProbe::default(),
        }
    }
}

/// One retire under way ([`Retirements::begin`]); dropped when it is done.
#[derive(Debug)]
pub struct Retiring(Arc<Retirements>);

impl Drop for Retiring {
    fn drop(&mut self) {
        self.0.gate.send_modify(|(_, active)| *active -= 1);
    }
}

impl Retirements {
    /// Start a retire (`None`: the daemon is stopping, so none starts).
    #[must_use]
    pub fn begin(self: &Arc<Self>) -> Option<Retiring> {
        let mut started = false;
        self.gate.send_if_modified(|(closed, active)| {
            if *closed {
                return false;
            }
            *active += 1;
            started = true;
            true
        });
        started.then(|| Retiring(Arc::clone(self)))
    }

    /// Start no retire from now on, and wait for the ones under way, at
    /// most `bound`: whether none is left.
    pub async fn close(&self, bound: std::time::Duration) -> bool {
        self.gate.send_modify(|(closed, _)| *closed = true);
        let mut gate = self.gate.subscribe();
        let settled = tokio::time::timeout(bound, gate.wait_for(|(_, active)| *active == 0))
            .await
            .is_ok_and(|seen| seen.is_ok());
        if !settled {
            let active = self.gate.borrow().1;
            tracing::warn!(
                active,
                "helper retirements still under way as the mobs stop"
            );
        }
        settled
    }

    /// Retire `key` in `handle`'s mob ([`super::stop_helper`]) unless the
    /// daemon is stopping.
    ///
    /// # Errors
    ///
    /// Why it is still seated: the daemon is stopping (a helper whose
    /// retire is owed is retired at the next start), or the engine's
    /// refusal of every try.
    pub async fn stop_helper(
        self: &Arc<Self>,
        handle: &meerkat_mob::MobHandle,
        key: &HelperKey,
    ) -> Result<(), String> {
        let Some(_retiring) = self.begin() else {
            tracing::info!(helper = %key, "the daemon is stopping: the helper is retired at its next start");
            return Err("the daemon is stopping".to_string());
        };
        #[cfg(any(test, feature = "test-support"))]
        self.probe.reaching_engine(key).await?;
        super::stop_helper(handle, key).await
    }
}

/// How many retired `assign` helpers' peer ids are remembered.
const RETIRED_PEERS: usize = 64;

/// The retired helpers' peer ids, names and the parent each went with,
/// newest last.
type RetiredPeers = Mutex<VecDeque<(String, String, Option<String>)>>;

/// The labels of `below`, in its order.
fn labels(below: &[(HelperKey, HelperTag)]) -> Vec<String> {
    below
        .iter()
        .map(|(_, tag)| tag.label().to_string())
        .collect()
}

/// Remember the peer id `key` runs as in `handle`'s mob, retired (with
/// `parent`, or because it finished), so a message to it is refused
/// saying so.
async fn remember_peer(
    handle: &meerkat_mob::MobHandle,
    key: &HelperKey,
    parent: Option<String>,
    retired: &RetiredPeers,
) {
    if let Ok(Some(endpoint)) = handle.member_peer_endpoint(&key.member).await {
        let mut retired = retired.lock();
        retired.push_back((
            endpoint.descriptor.peer_id.to_string(),
            key.member.to_string(),
            parent,
        ));
        while retired.len() > RETIRED_PEERS {
            retired.pop_front();
        }
    }
}

/// Stop and retire each of `below` (a retired helper's own helpers,
/// deepest first) through the guarded retire path; failures are logged.
async fn retire_all(
    state: &MobMcpState,
    retirements: &Arc<Retirements>,
    retired: &RetiredPeers,
    below: &[(HelperKey, HelperTag)],
) {
    for (child, tag) in below {
        tracing::info!(helper = %child, "retired with its parent");
        match state.handle_for(&child.mob).await {
            Ok(handle) => {
                let parent = tag.parent_path().last().cloned();
                remember_peer(&handle, child, parent, retired).await;
                // Owed already ([`HelperEndings::take_below`]): one the
                // engine keeps refusing is retired at the next start.
                if let Err(err) = retirements.stop_helper(&handle, child).await {
                    tracing::warn!(helper = %child, error = %err, "a helper retired with its parent stays seated: it is retired at the next start");
                }
            }
            Err(err) => {
                tracing::warn!(helper = %child, error = %err, "cannot reach the mob of a helper retired with its parent");
            }
        }
    }
}

/// Retire the seated helper `key` with the helpers `below` it (deepest
/// first, each remembered as retired with its parent, [`retire_all`]);
/// `remember` first remembers `key`'s own peer id (a finished `assign`
/// helper: a message to it from now on is refused saying it finished).
/// Every retire of a seated helper goes through here: a stop, a finish and
/// `retire_helper`.
///
/// # Errors
///
/// Its mob cannot be reached, or the engine refused the retire.
async fn retire_seated(
    state: &MobMcpState,
    retirements: &Arc<Retirements>,
    retired: &RetiredPeers,
    key: &HelperKey,
    below: &[(HelperKey, HelperTag)],
    remember: bool,
) -> Result<(), String> {
    retire_all(state, retirements, retired, below).await;
    let handle = state
        .handle_for(&key.mob)
        .await
        .map_err(|err| format!("its mob cannot be reached: {err}"))?;
    if remember {
        remember_peer(&handle, key, None, retired).await;
    }
    retirements.stop_helper(&handle, key).await
}

/// [`InputsProbe`], held.
struct Probe(InputsProbe);

impl std::fmt::Debug for Probe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Probe")
    }
}

impl HelperEndings {
    /// Endings that send every ending to tell to `sink`, and hand every
    /// helper's run that reaches its limit to `checkpoints` for its
    /// checkpoint (none: such a run is a failure).
    #[must_use]
    pub fn delivering(
        sink: Option<mpsc::UnboundedSender<Ended>>,
        checkpoints: Option<mpsc::UnboundedSender<LimitReached>>,
    ) -> Self {
        Self {
            sink,
            checkpoints,
            ..Self::default()
        }
    }

    /// The checkpoint of `key` continues it: the end of the run its
    /// status was asked in is no ending (the runs after it are).
    pub fn checkpoint_continues(&self, key: &HelperKey) {
        if let Some(Ending::Followed(followed)) = self.helpers.lock().get_mut(key) {
            followed.hold_checkpoint();
        }
    }

    /// The checkpoint of `key` after `steps` model calls stopped it for
    /// `reason`, with its `status`: an `assign` helper is done (its parent
    /// is told, with its status as its answer, and it is retired), a
    /// long-lived member or a fork stays seated, idle (its parent is told).
    /// A helper that ended meanwhile (the user stopped it) tells nothing.
    pub fn checkpoint_stopped(&self, key: &HelperKey, steps: u64, reason: String, status: String) {
        let tag = {
            let mut helpers = self.helpers.lock();
            let Some(Ending::Followed(followed)) = helpers.get_mut(key) else {
                return;
            };
            followed.hold_checkpoint();
            followed.tag.clone()
        };
        match tag.kind.life() {
            Some(super::HelperLife::Single) => {
                let below = self.take_below(key);
                if let Ok(Some(tag)) = self.settle(key) {
                    self.tell(Ended {
                        key: key.clone(),
                        tag,
                        how: How::CheckpointStopped {
                            steps,
                            reason,
                            status,
                            retired: true,
                        },
                        with: labels(&below),
                    });
                }
                self.retire_done(key, below);
            }
            Some(super::HelperLife::Kept) => self.tell(Ended {
                key: key.clone(),
                tag,
                how: How::CheckpointStopped {
                    steps,
                    reason,
                    status,
                    retired: false,
                },
                with: Vec::new(),
            }),
            None => {}
        }
    }

    /// Whether `key` is followed and not ended (a checkpoint of it may go
    /// on).
    #[must_use]
    pub fn is_live(&self, key: &HelperKey) -> bool {
        matches!(self.helpers.lock().get(key), Some(Ending::Followed(_)))
    }

    /// Retire helpers through `state` (once).
    pub fn bind(&self, state: &Arc<MobMcpState>) {
        let _ = self.state.set(Arc::downgrade(state));
    }

    /// The handles the daemon's start-up binds ([`crate::late`]) that are
    /// unbound, by name.
    #[cfg(any(test, feature = "test-support"))]
    pub fn unbound(&self) -> Vec<&'static str> {
        [
            (STATE, self.state.is_bound()),
            (PROBE, self.probe.is_bound()),
            (OWED, self.owed.is_bound()),
        ]
        .into_iter()
        .filter_map(|(name, bound)| (!bound).then_some(name))
        .collect()
    }

    /// Record each retire decided for a finished helper in `registry`'s
    /// bindings (once): a start that finds it still seated retires it
    /// ([`Self::retire_owed`]).
    pub fn owe_to(&self, registry: &Arc<super::HelperRegistry>) {
        let _ = self.owed.set(Arc::downgrade(registry));
    }

    /// The helper `key`, restored at a start, is done: its retire was
    /// decided before the last stop and did not reach the engine. It is
    /// retired now; nobody is told (its parent was, then).
    pub fn retire_owed(&self, key: &HelperKey) {
        if self.settle(key).is_err() {
            return;
        }
        tracing::info!(helper = %key, "a helper the last stop left seated after it finished: it is retired");
        let below = self.take_below(key);
        self.retire_done(key, below);
    }

    /// The helpers bound below `key` ([`super::HelperRegistry::bound_below`]),
    /// deepest first, taken to be retired with it: each is settled now
    /// (none of their endings is told: `key`'s names them) and its retire
    /// is owed, so a stop before it reaches the engine leaves it to the
    /// next start.
    pub fn take_below(&self, key: &HelperKey) -> Vec<(HelperKey, HelperTag)> {
        let Some(registry) = self.owed.get(OWED).and_then(Weak::upgrade) else {
            return Vec::new();
        };
        let below = registry.bound_below(key);
        {
            let mut helpers = self.helpers.lock();
            for (child, _) in &below {
                helpers.insert(child.clone(), Ending::Settled);
            }
        }
        for (child, _) in &below {
            registry.owe_retire(child);
        }
        below
    }

    /// Whether `key`'s ending was settled already.
    fn is_settled(&self, key: &HelperKey) -> bool {
        matches!(self.helpers.lock().get(key), Some(Ending::Settled))
    }

    /// The helper `key` (`tag`) was spawned or rebuilt: one followed
    /// already keeps its state with this tag; a settled key seated again is
    /// a new helper.
    pub fn spawned(&self, key: &HelperKey, tag: &HelperTag) {
        let mut helpers = self.helpers.lock();
        match helpers.get_mut(key) {
            Some(Ending::Followed(followed)) => followed.tag = tag.clone(),
            Some(Ending::Settled) | None => {
                helpers.insert(key.clone(), Ending::spawned(tag.clone()));
            }
        }
    }

    /// The fresh fork `key`'s first run's end reaches its parent from the
    /// engine ("Background fork_off job … finished"): only its later runs
    /// are told here.
    pub fn first_run_told_by_engine(&self, key: &HelperKey) {
        if let Some(Ending::Followed(followed)) = self.helpers.lock().get_mut(key) {
            followed.first_run_told_by_engine();
        }
    }

    /// The engine recorded the single-use fork `key`'s completed outcome
    /// for its parent (its mob's `ForkJobTerminal`, which outlives the fork
    /// and from which a restart delivers it again): it is done. One idle
    /// already, with nothing queued for it, is retired now; one still
    /// running (or with a message queued) when its run ends. `restored`:
    /// the record was found when the daemon restored the fork (its outcome
    /// came before the last stop), so a fork that ran no run since is idle.
    /// A kept fork stays.
    pub fn fork_outcome_recorded(self: &Arc<Self>, key: &HelperKey, restored: bool) {
        self.outcomes_reported
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let idle_after = match self.helpers.lock().get_mut(key) {
            Some(Ending::Followed(followed)) => followed.outcome_recorded(restored),
            _ => None,
        };
        let Some(runs) = idle_after else { return };
        let probe = self.probe.get(PROBE).map(|probe| Arc::clone(&probe.0));
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let endings = Arc::clone(self);
        let key = key.clone();
        runtime.spawn(async move {
            // As a run's end is decided: no note to it in flight, and
            // nothing it has not read.
            let _deciding = endings.deciding.lock().await;
            let still_idle = matches!(
                endings.helpers.lock().get(&key),
                Some(Ending::Followed(followed)) if followed.idle_after(runs)
            );
            // A restored fork's runtime holds an input of the restore
            // itself that no run takes (probed: still held
            // 30 s after the start), so only a live fork is asked.
            let queued = match &probe {
                Some(probe) if !restored => matches!(probe(key.clone()).await, Some(n) if n > 0),
                _ => false,
            };
            if still_idle && !queued {
                endings.retire_fork(&key);
            } else {
                tracing::debug!(helper = %key, still_idle, queued, "a single-use fork whose outcome is recorded is not idle yet");
            }
        });
    }

    /// Retire the fork `key`, whose first run's outcome reached its parent
    /// from the engine: nobody is told here (that outcome is the parent's
    /// news), and a message to it from now on is refused saying it
    /// finished ([`Self::retired_peer`]).
    fn retire_fork(&self, key: &HelperKey) {
        tracing::info!(helper = %key, "the fork's outcome reached its parent: it is retired");
        let below = self.take_below(key);
        let _ = self.settle(key);
        self.retire_done(key, below);
    }

    /// Ask the runtime what a helper still holds through `probe` (once).
    pub fn probe_with(&self, probe: InputsProbe) {
        let _ = self.probe.set(Probe(probe));
    }

    /// A run of the helper `key` started.
    pub fn run_started(&self, key: &HelperKey) {
        if let Some(Ending::Followed(followed)) = self.helpers.lock().get_mut(key) {
            followed.run_started();
        }
    }

    /// The helper `key` called a tool; `to_parent`: a message to its
    /// parent.
    pub fn called(&self, key: &HelperKey, to_parent: bool) {
        if let Some(Ending::Followed(followed)) = self.helpers.lock().get_mut(key) {
            followed.called(to_parent);
        }
    }

    /// A run of the helper `key` ended `terminal`. An `assign` helper or
    /// a long-lived member whose run completed or failed goes idle once the
    /// runtime holds nothing more for it ([`Self::settle_idle`]).
    pub fn run_ended(self: &Arc<Self>, key: &HelperKey, terminal: Terminal) {
        if terminal == Terminal::Cancelled {
            return;
        }
        let limit = match terminal {
            Terminal::Limit { steps } => Some(steps),
            _ => None,
        };
        let checkpoints = self.checkpoints.as_ref();
        let (end, tag) = match self.helpers.lock().get_mut(key) {
            Some(Ending::Followed(followed)) => (
                followed.run_ended(limit, checkpoints.is_some()),
                followed.tag.clone(),
            ),
            _ => return,
        };
        let runs = match end {
            RunEnd::Nothing => return,
            RunEnd::Checkpoint { run, steps } => {
                if let Some(sink) = checkpoints
                    && sink
                        .send(LimitReached {
                            key: key.clone(),
                            tag,
                            steps,
                            run,
                        })
                        .is_err()
                {
                    tracing::debug!("the helper checkpoints' sink is gone");
                }
                return;
            }
            RunEnd::Decide { run } => run,
        };
        let terminal = match terminal {
            Terminal::Limit { .. } => {
                Terminal::Failed("its run reached its step limit".to_string())
            }
            other => other,
        };
        let probe = self.probe.get(PROBE).map(|probe| Arc::clone(&probe.0));
        match (probe, tokio::runtime::Handle::try_current()) {
            (Some(probe), Ok(runtime)) => {
                let endings = Arc::clone(self);
                let key = key.clone();
                runtime.spawn(async move {
                    endings.settle_idle(&key, runs, &probe).await;
                    #[cfg(any(test, feature = "test-support"))]
                    endings.test_probe.deciding(&key).await;
                    endings.decide_idle(&key, runs, terminal, &probe).await;
                });
            }
            _ => self.went_idle(key, runs, terminal),
        }
    }

    /// Wait until the runtime holds no unfinished input of `key` (its run's
    /// own input is finished a moment after its terminal event; a message
    /// that came meanwhile is unfinished until a run takes it) or a later
    /// run started, at most [`SETTLE_WAIT`].
    async fn settle_idle(&self, key: &HelperKey, runs: u64, probe: &InputsProbe) {
        let deadline = tokio::time::Instant::now() + SETTLE_WAIT;
        let mut wait = RECHECK_FIRST;
        loop {
            if self.runs_of(key) != Some(runs) {
                return;
            }
            match probe(key.clone()).await {
                Some(0) | None => return,
                Some(_) => {}
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                tracing::info!(helper = %key, "the runtime still holds input of an idle helper; taken as idle");
                return;
            }
            tokio::time::sleep(wait.min(deadline - now)).await;
            wait = (wait * 2).min(RECHECK_MAX);
        }
    }

    /// Decide the end of run `runs` of `key` ([`Self::went_idle`]) with
    /// no note to it in flight: under [`Self::deciding`], asked once more
    /// whether the runtime holds input of it (a note that landed after the
    /// last look is unfinished input). When it does, the lock is let go
    /// for one more [`Self::settle_idle`] (a run that takes the note makes
    /// this run's end none), and the end is decided after it.
    async fn decide_idle(
        &self,
        key: &HelperKey,
        runs: u64,
        terminal: Terminal,
        probe: &InputsProbe,
    ) {
        {
            let _deciding = self.deciding.lock().await;
            if self.runs_of(key) != Some(runs) {
                return;
            }
            if !matches!(probe(key.clone()).await, Some(n) if n > 0) {
                self.went_idle(key, runs, terminal);
                return;
            }
        }
        self.settle_idle(key, runs, probe).await;
        let _deciding = self.deciding.lock().await;
        self.went_idle(key, runs, terminal);
    }

    /// Run `send` (a note to the helper `key`) unless `key`'s ending was
    /// settled, with no decision of a helper's going idle in between
    /// (`None`: settled, nothing was sent; the note goes further up).
    pub async fn while_unsettled<T>(
        &self,
        key: &HelperKey,
        send: impl std::future::Future<Output = T>,
    ) -> Option<T> {
        let _deciding = self.deciding.lock().await;
        if self.is_settled(key) {
            return None;
        }
        Some(send.await)
    }

    /// The helper `key`, restored from the last daemon's, was woken with
    /// no run of it (its instructions were brought up): idle, so not at
    /// work ([`Self::at_work`]) until a run of it starts. A helper that ran
    /// in this daemon already is left as it is.
    pub fn restored_idle(&self, key: &HelperKey) {
        if let Some(Ending::Followed(followed)) = self.helpers.lock().get_mut(key) {
            followed.restored_idle();
        }
    }

    /// Whether the followed helper `key` is at work: no run of it has been
    /// decided idle since its last run started (one spawned and not run
    /// yet, one running, one whose run ended and is being decided). A
    /// helper not followed (or settled) is not.
    fn at_work(&self, key: &HelperKey) -> bool {
        self.helpers.lock().get(key).is_some_and(Ending::at_work)
    }

    /// How many runs of `key` started (`None`: not followed).
    fn runs_of(&self, key: &HelperKey) -> Option<u64> {
        match self.helpers.lock().get(key) {
            Some(Ending::Followed(followed)) => Some(followed.runs),
            _ => None,
        }
    }

    /// The run `runs` of `key` ended `terminal` and nothing is queued for
    /// it: unless a later run started, an `assign` helper is done (its
    /// parent is told, and it is retired) and a long-lived member went idle
    /// (its parent is told, unless the run's last call was a message to it).
    fn went_idle(&self, key: &HelperKey, runs: u64, terminal: Terminal) {
        let decided = match self.helpers.lock().get_mut(key) {
            Some(Ending::Followed(followed)) => followed.went_idle(runs),
            _ => None,
        };
        let Some(IdleRun {
            tag,
            messaged_parent,
            told_by_engine: first_told_elsewhere,
            recorded: outcome_recorded,
        }) = decided
        else {
            return;
        };
        self.idle_decided
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        use super::{HelperContext::Fork, HelperContext::Fresh, HelperKind::Started};
        use super::{HelperLife::Kept, HelperLife::Single};
        match tag.kind {
            Started(Fresh, Single) => {
                let how = match terminal {
                    Terminal::Completed(answer) => How::Done { answer },
                    Terminal::Failed(reason) => How::Failed { reason },
                    // A run at its limit goes to its checkpoint before.
                    Terminal::Cancelled | Terminal::Limit { .. } => return,
                };
                self.done(key, how);
            }
            Started(Fork, life) if first_told_elsewhere => {
                // The engine tells its first run's end; once its outcome is
                // recorded for the parent a single-use fork is retired.
                if life == Single && outcome_recorded && matches!(terminal, Terminal::Completed(_))
                {
                    self.retire_fork(key);
                }
            }
            Started(Fork, Single) if outcome_recorded => {
                // A later run (a message read before its first run ended)
                // of a fork whose outcome reached its parent: it is done,
                // told as a fresh single-use helper is, and retired.
                let how = match terminal {
                    Terminal::Completed(answer) => How::Done { answer },
                    Terminal::Failed(reason) => How::Failed { reason },
                    Terminal::Cancelled | Terminal::Limit { .. } => return,
                };
                self.done(key, how);
            }
            // A kept helper's run, and a single-use fork's later run whose
            // outcome is not recorded yet.
            Started(_, Kept) | Started(Fork, Single) => {
                let how = match terminal {
                    Terminal::Completed(_) if messaged_parent => return,
                    Terminal::Completed(answer) => How::WentIdle {
                        answer,
                        failed: None,
                    },
                    Terminal::Failed(reason) => How::WentIdle {
                        answer: String::new(),
                        failed: Some(reason),
                    },
                    Terminal::Cancelled | Terminal::Limit { .. } => return,
                };
                self.tell(Ended {
                    key: key.clone(),
                    tag,
                    how,
                    with: Vec::new(),
                });
            }
            super::HelperKind::Other => {}
        }
    }

    /// The single-use helper `key` is done (`how`): its parent is told,
    /// naming its own helpers that go with it, and it is retired with
    /// them.
    /// One of its own helpers still at work (spawned and not run yet,
    /// running, or its run's end not decided yet: [`Self::at_work`]), it
    /// is not done yet: it stays seated, that helper's ending is told to it
    /// (a run of it reads it) and that run's end decides again. A member
    /// `assign` did not start ([`super::HelperKind::Other`], a council's
    /// participant) is never decided idle, so it holds nothing.
    fn done(&self, key: &HelperKey, how: How) {
        if let Some(registry) = self.owed.get(OWED).and_then(Weak::upgrade)
            && registry
                .bound_below(key)
                .iter()
                .any(|(child, tag)| tag.kind != super::HelperKind::Other && self.at_work(child))
        {
            tracing::info!(helper = %key, "a single-use helper's own helper is still at work: it is done once that one's ending reached it");
            return;
        }
        let below = self.take_below(key);
        if let Ok(Some(tag)) = self.settle(key) {
            self.tell(Ended {
                key: key.clone(),
                tag,
                how,
                with: labels(&below),
            });
        }
        self.retire_done(key, below);
    }

    /// The user stopped the helper `key` (`tag`, when the caller knows it
    /// better): its run is force-cancelled and it is retired, the helpers
    /// it started (running or not) are stopped and retired with it, and
    /// its parent is told, naming them. Returns whether it was a helper
    /// not ended yet.
    pub fn stop(&self, key: &HelperKey, tag: Option<HelperTag>) -> bool {
        let Ok(followed) = self.settle(key) else {
            return false;
        };
        let below = self.take_below(key);
        let with = labels(&below);
        let known = followed.or(tag);
        self.retire(key, below);
        match known {
            Some(tag) => {
                self.tell(Ended {
                    key: key.clone(),
                    tag,
                    how: How::StoppedByUser,
                    with,
                });
                true
            }
            None => false,
        }
    }

    /// The helper `key` is ended with the conversation it worked for (its
    /// agent switched conversations): nobody is told.
    pub fn left(&self, key: &HelperKey) {
        let _ = self.settle(key);
    }

    /// Settle `key`, in one take: the tag it was followed with (`None`:
    /// not followed).
    ///
    /// # Errors
    ///
    /// It was settled already: its ending is someone else's to tell.
    fn settle(&self, key: &HelperKey) -> Result<Option<HelperTag>, AlreadySettled> {
        match self.helpers.lock().insert(key.clone(), Ending::Settled) {
            Some(Ending::Settled) => Err(AlreadySettled),
            Some(Ending::Followed(followed)) => Ok(Some(followed.tag)),
            None => Ok(None),
        }
    }

    fn tell(&self, ended: Ended) {
        tracing::info!(helper = %ended.key, how = %ended.how.text(), "a helper ended");
        self.told.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        match &self.sink {
            Some(sink) => {
                if sink.send(ended).is_err() {
                    tracing::debug!("the helper endings' sink is gone");
                }
            }
            None => tracing::debug!("no sink for helper endings"),
        }
    }

    /// Retire `key` off the caller's task: a stop force-cancels and
    /// retires whatever member it is.
    /// The helpers `below` it ([`Self::take_below`]) are retired first.
    fn retire(&self, key: &HelperKey, below: Vec<(HelperKey, HelperTag)>) {
        self.retires
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // A stopped helper is owed until the engine retired it: a daemon
        // stop before that leaves it to the next start.
        if let Some(registry) = self.owed.get(OWED).and_then(Weak::upgrade) {
            registry.owe_retire(key);
        }
        let Some(state) = self.state.get(STATE).and_then(Weak::upgrade) else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let key = key.clone();
        let retirements = Arc::clone(&self.retirements);
        let retired = Arc::clone(&self.retired_peers);
        runtime.spawn(async move {
            if let Err(err) =
                retire_seated(&state, &retirements, &retired, &key, &below, false).await
            {
                tracing::warn!(helper = %key, error = %err, "a stopped helper stays seated: it is retired at the next start");
            }
        });
    }

    /// Retire the `assign` helper `key`, which finished, off the caller's
    /// task, remembering its peer id first: a message its parent sends it
    /// from now on is refused saying it finished ([`Self::retired_peer`]),
    /// not with the engine's "peer not found".
    /// The helpers `below` it ([`Self::take_below`]) are retired first.
    fn retire_done(&self, key: &HelperKey, below: Vec<(HelperKey, HelperTag)>) {
        self.retires
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Some(registry) = self.owed.get(OWED).and_then(Weak::upgrade) {
            registry.owe_retire(key);
        }
        let Some(state) = self.state.get(STATE).and_then(Weak::upgrade) else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let key = key.clone();
        let retired = Arc::clone(&self.retired_peers);
        let retirements = Arc::clone(&self.retirements);
        #[cfg(any(test, feature = "test-support"))]
        let hook = self.test_probe.retiring();
        runtime.spawn(async move {
            #[cfg(any(test, feature = "test-support"))]
            probe::run(hook, &key).await;
            if let Err(err) = retire_seated(&state, &retirements, &retired, &key, &below, true).await
            {
                tracing::warn!(helper = %key, error = %err, "a finished helper stays seated: it is retired at the next start");
            }
        });
    }

    /// Retire the seated helper `key` with the helpers `below` it
    /// ([`retire_seated`], `retire_helper`'s, awaited): the engine's
    /// refusal, in words.
    ///
    /// # Errors
    ///
    /// The daemon stops, the helper's mob cannot be reached, or the engine
    /// refused the retire.
    pub async fn retire_now(
        &self,
        key: &HelperKey,
        below: &[(HelperKey, HelperTag)],
    ) -> Result<(), String> {
        let state = self
            .state
            .get(STATE)
            .and_then(Weak::upgrade)
            .ok_or("the daemon is stopping")?;
        retire_seated(
            &state,
            &self.retirements,
            &self.retired_peers,
            key,
            below,
            false,
        )
        .await
    }

    /// The retirements under way, closed when the daemon stops.
    #[must_use]
    pub fn retirements(&self) -> &Arc<Retirements> {
        &self.retirements
    }

    /// The name of the `assign` helper that ran as the peer `peer_id` and
    /// was retired when it finished (`None`: no such helper is
    /// remembered).
    #[must_use]
    pub fn retired_peer(&self, peer_id: &str) -> Option<String> {
        self.retired_peers
            .lock()
            .iter()
            .rev()
            .find(|(peer, _, _)| peer == peer_id)
            .map(|(_, name, _)| name.clone())
    }

    /// Why a message to the peer `peer_id` is refused when it ran as a
    /// helper that was retired: it finished, or it went with its parent
    /// (`None`: no such helper is remembered).
    #[must_use]
    pub fn retired_refusal(&self, peer_id: &str) -> Option<String> {
        self.retired_peers
            .lock()
            .iter()
            .rev()
            .find(|(peer, _, _)| peer == peer_id)
            .map(|(_, name, parent)| match parent {
                None => format!(
                    "helper `{name}` finished its task and was retired; start another helper for more work"
                ),
                Some(parent) => format!(
                    "helper `{name}` was retired with its parent `{parent}`; start another helper for more work"
                ),
            })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use meerkat_core::SessionId;
    use meerkat_mob::{AgentIdentity, MobId};

    fn helper_key(member: &str) -> HelperKey {
        HelperKey::new(MobId::from("team"), AgentIdentity::from(member))
    }

    fn tag(label: &str) -> HelperTag {
        HelperTag {
            owner: SessionId::new(),
            path: vec![label.to_string()],
            model: "m".into(),
            kind: crate::mobs::HelperKind::Other,
        }
    }

    fn endings() -> (Arc<HelperEndings>, mpsc::UnboundedReceiver<Ended>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Arc::new(HelperEndings::delivering(Some(tx), None)), rx)
    }

    /// A settled helper is remembered however many end after it, so its
    /// late event tells nothing.
    #[test]
    fn a_helper_settled_long_ago_is_not_told_again() {
        let (endings, mut rx) = endings();
        let first = helper_key("first");
        endings.spawned(&first, &tag("first"));
        assert!(endings.stop(&first, None));
        assert!(rx.try_recv().is_ok());
        for n in 0..600 {
            let key = helper_key(&format!("h{n}"));
            endings.spawned(&key, &tag(&format!("h{n}")));
            endings.left(&key);
        }
        assert!(endings.settled(&first));
        assert!(!endings.stop(&first, None));
        endings.run_ended(&first, Terminal::Cancelled);
        assert!(rx.try_recv().is_err(), "told once only");
    }

    #[test]
    fn a_stop_is_told_once_and_a_member_s_run_ends_are_not_endings() {
        let (endings, mut rx) = endings();
        let member = HelperKey::new(MobId::from("team"), AgentIdentity::from("w1"));
        endings.spawned(&member, &tag("w1"));
        endings.run_ended(&member, Terminal::Completed("idle now".into()));
        assert!(rx.try_recv().is_err(), "a member's run end is no ending");
        assert!(endings.stop(&member, None));
        assert_eq!(rx.try_recv().unwrap().how, How::StoppedByUser);
        // Its cancelled run, and a second stop, tell nothing.
        endings.run_ended(&member, Terminal::Cancelled);
        assert!(!endings.stop(&member, None));
        assert!(rx.try_recv().is_err());
    }

    fn tagged(label: &str, kind: crate::mobs::HelperKind) -> HelperTag {
        HelperTag { kind, ..tag(label) }
    }

    /// An `assign` helper whose run completed is done: told with its
    /// answer, settled, and retired; one whose run failed is told so and
    /// retired too; a cancelled run (the user's stop tells that) is
    /// nothing here.
    #[test]
    fn an_assigned_helper_is_done_when_its_run_ends() {
        use crate::mobs::{HelperContext, HelperKind, HelperLife};
        let (endings, mut rx) = endings();
        let key = HelperKey::new(MobId::from("tui"), AgentIdentity::from("scout"));
        endings.spawned(
            &key,
            &tagged(
                "scout",
                HelperKind::Started(HelperContext::Fresh, HelperLife::Single),
            ),
        );
        endings.run_started(&key);
        endings.run_ended(&key, Terminal::Cancelled);
        assert!(rx.try_recv().is_err());
        endings.run_ended(&key, Terminal::Completed("the answer".into()));
        assert_eq!(
            rx.try_recv().unwrap().how,
            How::Done {
                answer: "the answer".into()
            }
        );
        assert!(endings.settled(&key));
        assert_eq!(endings.retires(), 1);
        let failed = HelperKey::new(MobId::from("tui"), AgentIdentity::from("broken"));
        endings.spawned(
            &failed,
            &tagged(
                "broken",
                HelperKind::Started(HelperContext::Fresh, HelperLife::Single),
            ),
        );
        endings.run_ended(&failed, Terminal::Failed("a 500".into()));
        assert_eq!(
            rx.try_recv().unwrap().how,
            How::Failed {
                reason: "a 500".into()
            }
        );
        assert_eq!(endings.retires(), 2);
    }

    /// A long-lived member is told of each run that leaves it idle, never
    /// retired, and not told of a run whose last call messaged its parent;
    /// a member of any other kind (a fork, a council's) is not told of.
    #[test]
    fn a_member_is_told_of_each_idle_run_unless_it_just_messaged_its_parent() {
        use crate::mobs::{HelperContext, HelperKind, HelperLife};
        let (endings, mut rx) = endings();
        let key = HelperKey::new(MobId::from("tui"), AgentIdentity::from("w1"));
        endings.spawned(
            &key,
            &tagged(
                "w1",
                HelperKind::Started(HelperContext::Fresh, HelperLife::Kept),
            ),
        );
        for answer in ["one", "two"] {
            endings.run_started(&key);
            endings.called(&key, false);
            endings.run_ended(&key, Terminal::Completed(answer.into()));
            assert_eq!(
                rx.try_recv().unwrap().how,
                How::WentIdle {
                    answer: answer.into(),
                    failed: None
                }
            );
        }
        endings.run_started(&key);
        endings.called(&key, true);
        endings.run_ended(&key, Terminal::Completed("bye".into()));
        assert!(rx.try_recv().is_err(), "its parent has its last message");
        endings.run_started(&key);
        endings.run_ended(&key, Terminal::Failed("a 400".into()));
        assert!(matches!(
            rx.try_recv().unwrap().how,
            How::WentIdle {
                failed: Some(_),
                ..
            }
        ));
        assert!(endings.is_tracked(&key) && !endings.settled(&key));
        assert_eq!(endings.retires(), 0);
        assert_eq!(endings.idle_decided(), 4);
        let fork = HelperKey::new(MobId::from("tui"), AgentIdentity::from("kid"));
        endings.spawned(&fork, &tag("kid"));
        endings.run_ended(&fork, Terminal::Completed("forked".into()));
        assert!(rx.try_recv().is_err());
        assert_eq!(endings.idle_decided(), 4);
    }

    /// A fresh kept fork's first run's end is the engine's to tell: not
    /// told here, but decided; each later run that leaves it idle is told
    /// as a kept helper's, and its recorded outcome retires nothing; a fork
    /// rebuilt after a restart has every run told. A single-use fork is
    /// retired once its outcome is recorded and it is idle, untold here.
    #[test]
    fn a_fork_is_told_of_its_runs_after_the_first() {
        use crate::mobs::{HelperContext, HelperKind, HelperLife};
        let kept = HelperKind::Started(HelperContext::Fork, HelperLife::Kept);
        let (endings, mut rx) = endings();
        let key = HelperKey::new(MobId::from("tui"), AgentIdentity::from("kid"));
        endings.spawned(&key, &tagged("kid", kept));
        endings.first_run_told_by_engine(&key);
        endings.fork_outcome_recorded(&key, false);
        endings.first_run_told_by_engine(&key);
        endings.run_started(&key);
        endings.run_ended(&key, Terminal::Completed("the outcome".into()));
        assert!(rx.try_recv().is_err(), "the engine tells the first");
        assert_eq!(endings.idle_decided(), 1);
        endings.run_started(&key);
        endings.run_ended(&key, Terminal::Completed("later".into()));
        assert_eq!(
            rx.try_recv().unwrap().how,
            How::WentIdle {
                answer: "later".into(),
                failed: None
            }
        );
        assert_eq!(endings.retires(), 0);
        let rebuilt = HelperKey::new(MobId::from("tui"), AgentIdentity::from("kid2"));
        endings.spawned(&rebuilt, &tagged("kid2", kept));
        endings.run_started(&rebuilt);
        endings.run_ended(&rebuilt, Terminal::Completed("after a restart".into()));
        assert!(matches!(rx.try_recv().unwrap().how, How::WentIdle { .. }));
        // Single-use: its first run's end and the recorded outcome retire
        // it (no probe in this rig: decided at once), nothing told here.
        let single = HelperKey::new(MobId::from("tui"), AgentIdentity::from("kid3"));
        endings.spawned(
            &single,
            &tagged(
                "kid3",
                HelperKind::Started(HelperContext::Fork, HelperLife::Single),
            ),
        );
        endings.first_run_told_by_engine(&single);
        endings.fork_outcome_recorded(&single, false);
        endings.run_started(&single);
        endings.run_ended(&single, Terminal::Completed("the outcome".into()));
        assert!(rx.try_recv().is_err());
        assert!(endings.settled(&single));
        assert_eq!(endings.retires(), 1);
    }

    /// A run at its limit is no ending: it goes to the checkpoint, and the
    /// status run's end after it is held too. Continued, the helper's
    /// later run ends it as any; stopped, an `assign` helper is done with
    /// its status and retired, a member stays.
    #[test]
    fn a_run_at_its_limit_goes_to_its_checkpoint_before_any_ending() {
        use crate::mobs::{HelperContext, HelperKind, HelperLife};
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (limits_tx, mut limits) = mpsc::unbounded_channel();
        let endings = Arc::new(HelperEndings::delivering(Some(tx), Some(limits_tx)));
        let key = HelperKey::new(MobId::from("tui"), AgentIdentity::from("scout"));
        endings.spawned(
            &key,
            &tagged(
                "scout",
                HelperKind::Started(HelperContext::Fresh, HelperLife::Single),
            ),
        );
        endings.run_started(&key);
        endings.run_ended(&key, Terminal::Limit { steps: 10 });
        let reached = limits.try_recv().unwrap();
        assert_eq!((reached.steps, reached.run), (10, 1));
        assert!(rx.try_recv().is_err(), "no failure, no ending");
        // The status run.
        endings.run_started(&key);
        endings.run_ended(&key, Terminal::Completed("my status".into()));
        assert!(rx.try_recv().is_err(), "the status run is the checkpoint's");
        endings.checkpoint_continues(&key);
        endings.run_started(&key);
        endings.run_ended(&key, Terminal::Completed("the answer".into()));
        assert_eq!(
            rx.try_recv().unwrap().how,
            How::Done {
                answer: "the answer".into()
            }
        );
        // Stopped: done with its status, retired.
        let stopped = HelperKey::new(MobId::from("tui"), AgentIdentity::from("loop"));
        endings.spawned(
            &stopped,
            &tagged(
                "loop",
                HelperKind::Started(HelperContext::Fresh, HelperLife::Single),
            ),
        );
        endings.run_started(&stopped);
        endings.run_ended(&stopped, Terminal::Limit { steps: 10 });
        endings.checkpoint_stopped(&stopped, 10, "it loops".into(), "my status".into());
        assert_eq!(
            rx.try_recv().unwrap().how,
            How::CheckpointStopped {
                steps: 10,
                reason: "it loops".into(),
                status: "my status".into(),
                retired: true
            }
        );
        assert!(endings.settled(&stopped) && !endings.is_live(&stopped));
        // A member stays.
        let member = HelperKey::new(MobId::from("tui"), AgentIdentity::from("w1"));
        endings.spawned(
            &member,
            &tagged(
                "w1",
                HelperKind::Started(HelperContext::Fresh, HelperLife::Kept),
            ),
        );
        endings.run_started(&member);
        endings.run_ended(&member, Terminal::Limit { steps: 10 });
        endings.checkpoint_stopped(&member, 10, "enough".into(), "s".into());
        assert!(matches!(
            rx.try_recv().unwrap().how,
            How::CheckpointStopped { retired: false, .. }
        ));
        assert!(endings.is_live(&member));
        // Without a checkpoint sink the limit is a failure.
        let (plain, mut told) = super::tests::endings();
        plain.spawned(
            &key,
            &tagged(
                "scout",
                HelperKind::Started(HelperContext::Fresh, HelperLife::Single),
            ),
        );
        plain.run_ended(&key, Terminal::Limit { steps: 10 });
        assert!(matches!(told.try_recv().unwrap().how, How::Failed { .. }));
    }

    #[test]
    fn the_endings_read_in_plain_words() {
        assert_eq!(How::StoppedByUser.text(), "stopped by you");
        let recovered = How::Failed {
            reason: "run failed: InterruptedByRecovery".into(),
        };
        assert_eq!(
            recovered.text(),
            "failed: its run was cut off when the daemon stopped"
        );
        assert!(!recovered.text().contains("Recovery"));
    }

    /// No retire starts once the retirements are closed, and the close
    /// waits for the one under way (and only within its bound).
    #[tokio::test]
    async fn a_closed_retirements_start_none_and_wait_for_the_one_under_way() {
        let retirements = Arc::new(Retirements::default());
        let under_way = retirements.begin().unwrap();
        let closing = {
            let retirements = Arc::clone(&retirements);
            tokio::spawn(async move { retirements.close(std::time::Duration::from_secs(30)).await })
        };
        while !retirements.closed() {
            tokio::task::yield_now().await;
        }
        assert!(retirements.begin().is_none());
        assert!(!closing.is_finished());
        drop(under_way);
        assert!(closing.await.unwrap());
        // One that never ends is given up on at the bound.
        let stuck = Arc::new(Retirements::default());
        let _held = stuck.begin().unwrap();
        assert!(!stuck.close(std::time::Duration::from_millis(10)).await);
    }

    /// A registry whose endings owe their retires to it, with the endings'
    /// receiver, and the keys and tags of a single-use parent `p1` and its
    /// kept helper `c1`.
    fn parent_and_child() -> (
        Arc<super::super::HelperRegistry>,
        mpsc::UnboundedReceiver<Ended>,
        HelperKey,
        HelperKey,
    ) {
        use crate::mobs::{HelperContext::Fresh, HelperKind::Started, HelperLife};
        parent_and(Started(Fresh, HelperLife::Kept))
    }

    /// [`parent_and_child`] with a child `c1` of `kind`.
    fn parent_and(
        kind: crate::mobs::HelperKind,
    ) -> (
        Arc<super::super::HelperRegistry>,
        mpsc::UnboundedReceiver<Ended>,
        HelperKey,
        HelperKey,
    ) {
        use crate::mobs::{HelperContext::Fresh, HelperKind::Started, HelperLife};
        let (tx, rx) = mpsc::unbounded_channel();
        let registry = Arc::new(super::super::HelperRegistry::in_memory().with_outlets(
            crate::mobs::Outlets {
                ended: Some(tx),
                ..crate::mobs::Outlets::default()
            },
        ));
        registry.endings().owe_to(&registry);
        let owner = SessionId::new();
        let mob = MobId::from("tui");
        let parent = HelperKey::new(mob.clone(), AgentIdentity::from("p1"));
        let child = HelperKey::new(mob, AgentIdentity::from("c1"));
        let tag = |path: Vec<&str>, kind| HelperTag {
            owner: owner.clone(),
            path: path.into_iter().map(str::to_string).collect(),
            model: "m".into(),
            kind,
        };
        registry
            .spawning(
                parent.clone(),
                tag(vec!["p1"], Started(Fresh, HelperLife::Single)),
            )
            .unwrap();
        registry
            .spawning(child.clone(), tag(vec!["p1", "c1"], kind))
            .unwrap();
        (registry, rx, parent, child)
    }

    /// A member below a single-use helper that `assign` did not start
    /// (a council's participant, [`crate::mobs::HelperKind::Other`]) is
    /// never decided idle, so it does not hold its parent: the parent is
    /// done when its own run ends, as with no such member.
    #[test]
    fn a_member_assign_did_not_start_does_not_hold_its_parent() {
        let (registry, mut rx, parent, _child) = parent_and(crate::mobs::HelperKind::Other);
        let endings = registry.endings();
        endings.run_started(&parent);
        endings.run_ended(&parent, Terminal::Completed("done".into()));
        let ended = rx.try_recv().unwrap();
        assert_eq!(ended.key, parent);
    }

    /// A kept helper restored from the last daemon and woken with no run
    /// (its instructions brought up) is idle: its single-use parent is
    /// done when its own run ends, and the child goes with it. Before it is
    /// known idle, a child spawned and not run yet holds the parent.
    #[test]
    fn a_restored_idle_child_does_not_hold_its_finished_parent() {
        let (registry, mut rx, parent, child) = parent_and_child();
        let endings = registry.endings();
        endings.run_started(&parent);
        endings.run_ended(&parent, Terminal::Completed("first".into()));
        assert!(rx.try_recv().is_err(), "held while c1 may be at work");
        endings.restored_idle(&child);
        endings.run_started(&parent);
        endings.run_ended(&parent, Terminal::Completed("done".into()));
        let ended = rx.try_recv().unwrap();
        assert_eq!(ended.key, parent);
        assert_eq!(ended.with, vec!["c1".to_string()]);
        assert!(registry.retire_owed(&parent) && registry.retire_owed(&child));
        // One that ran in this daemon is left as it is.
        let (registry, _rx, _, child) = parent_and_child();
        registry.endings().run_started(&child);
        registry.endings().restored_idle(&child);
        assert!(registry.endings().at_work(&child));
    }

    fn followed(kind: crate::mobs::HelperKind) -> Followed {
        Followed::new(tagged("h", kind))
    }

    fn working(messaged_parent: bool) -> Phase {
        Phase::Working { messaged_parent }
    }

    /// A followed helper's run moves it between at work and idle; a
    /// decision of a run a later one replaced, and a tool call outside a
    /// run, are refused.
    #[test]
    fn a_followed_helper_s_runs_move_it_between_work_and_idle() {
        use crate::mobs::{HelperContext, HelperKind, HelperLife};
        let mut helper = followed(HelperKind::Started(HelperContext::Fresh, HelperLife::Kept));
        assert!(helper.at_work(), "spawned and not run yet");
        helper.run_started();
        helper.called(true);
        assert_eq!(helper.phase, working(true));
        assert_eq!(helper.run_ended(None, true), RunEnd::Decide { run: 1 });
        // A later run started before the first's end was decided.
        helper.run_started();
        assert_eq!(helper.phase, working(false));
        assert_eq!(helper.went_idle(1), None, "refused: run 2 replaced it");
        assert!(helper.at_work());
        let idle = helper.went_idle(2).unwrap();
        assert!(!idle.messaged_parent && !idle.told_by_engine && !idle.recorded);
        assert_eq!(helper.phase, Phase::Idle);
        assert!(!helper.at_work() && helper.idle_after(2));
        // A call outside a run is not recorded.
        helper.called(true);
        assert_eq!(helper.phase, Phase::Idle);
        // A member not started by `assign` has no ending of its runs.
        let mut other = followed(HelperKind::Other);
        other.run_started();
        assert_eq!(other.run_ended(None, true), RunEnd::Nothing);
    }

    /// Restored with no run, a helper is idle; once a run started that is
    /// refused.
    #[test]
    fn restored_idle_is_refused_once_a_run_started() {
        use crate::mobs::{HelperContext, HelperKind, HelperLife};
        let kind = HelperKind::Started(HelperContext::Fresh, HelperLife::Kept);
        let mut restored = followed(kind);
        restored.restored_idle();
        assert_eq!(restored.phase, Phase::Idle);
        let mut ran = followed(kind);
        ran.run_started();
        ran.restored_idle();
        assert_eq!(ran.phase, working(false));
    }

    /// A run at its limit opens the checkpoint through the status run
    /// after it; their ends are nothing; the checkpoint's continue holds
    /// the current run and the next run leaves it. Without a checkpoint
    /// the limit is decided as any end.
    #[test]
    fn a_checkpoint_holds_its_runs_and_the_next_run_leaves_it() {
        use crate::mobs::{HelperContext, HelperKind, HelperLife};
        let kind = HelperKind::Started(HelperContext::Fresh, HelperLife::Single);
        let mut helper = followed(kind);
        helper.run_started();
        assert_eq!(
            helper.run_ended(Some(7), true),
            RunEnd::Checkpoint { run: 1, steps: 7 }
        );
        assert_eq!(helper.phase, Phase::Checkpoint { through: 2 });
        assert!(helper.at_work());
        helper.run_started();
        helper.called(true);
        assert_eq!(
            helper.phase,
            Phase::Checkpoint { through: 2 },
            "the status run"
        );
        assert_eq!(helper.run_ended(None, true), RunEnd::Nothing);
        helper.hold_checkpoint();
        assert_eq!(helper.phase, Phase::Checkpoint { through: 2 });
        helper.run_started();
        assert_eq!(helper.phase, working(false), "the continued run");
        assert_eq!(helper.run_ended(None, true), RunEnd::Decide { run: 3 });
        let mut plain = followed(kind);
        plain.run_started();
        assert_eq!(plain.run_ended(Some(7), false), RunEnd::Decide { run: 1 });
    }

    /// The engine tells a fresh fork's first run; its outcome on record
    /// makes a single-use fork done once idle. A kept fork's record, a
    /// fresh helper's and a restored fresh fork's are refused.
    #[test]
    fn a_fork_s_news_are_taken_only_where_they_apply() {
        use crate::mobs::{HelperContext, HelperKind, HelperLife};
        let single = HelperKind::Started(HelperContext::Fork, HelperLife::Single);
        let mut fork = followed(single);
        fork.first_run_told_by_engine();
        assert_eq!(fork.fork, ForkNews::FirstRun);
        assert_eq!(
            fork.outcome_recorded(true),
            None,
            "refused: its own first run's"
        );
        assert_eq!(fork.fork, ForkNews::FirstRun);
        fork.run_started();
        assert_eq!(fork.outcome_recorded(false), None, "still running");
        assert_eq!(fork.fork, ForkNews::Recorded { first_run: true });
        let idle = fork.went_idle(1).unwrap();
        assert!(idle.told_by_engine && idle.recorded);
        // Restored after its outcome came: idle, done.
        let mut restored = followed(single);
        assert_eq!(restored.outcome_recorded(true), Some(0));
        assert_eq!(restored.fork, ForkNews::Recorded { first_run: false });
        // Told of later: its first run is still the engine's.
        restored.first_run_told_by_engine();
        assert_eq!(restored.fork, ForkNews::Recorded { first_run: true });
        for kind in [
            HelperKind::Started(HelperContext::Fork, HelperLife::Kept),
            HelperKind::Started(HelperContext::Fresh, HelperLife::Single),
        ] {
            let mut other = followed(kind);
            assert_eq!(other.outcome_recorded(false), None);
            assert_eq!(other.fork, ForkNews::Nothing);
        }
    }

    /// Settling is one take: the second is refused, and a key seated again
    /// is a new helper.
    #[test]
    fn a_helper_is_settled_once() {
        use crate::mobs::{HelperContext, HelperKind, HelperLife};
        let endings = HelperEndings::default();
        let key = HelperKey::new(MobId::from("tui"), AgentIdentity::from("h"));
        let kind = HelperKind::Started(HelperContext::Fresh, HelperLife::Single);
        assert_eq!(endings.settle(&key), Ok(None), "not followed");
        assert_eq!(endings.settle(&key), Err(AlreadySettled));
        endings.spawned(&key, &tagged("h", kind));
        assert!(endings.is_live(&key));
        assert_eq!(endings.settle(&key).unwrap().unwrap().label(), "h");
        assert!(!endings.is_live(&key) && endings.settled(&key));
        // A settled one takes no news.
        endings.run_started(&key);
        assert!(!endings.is_live(&key));
    }

    /// The user's stop of a helper owes its retire until the engine made
    /// it: a daemon stop before that leaves it to the next start.
    #[test]
    fn a_stopped_helper_s_retire_is_owed() {
        let (registry, mut rx, parent, child) = parent_and_child();
        assert!(registry.endings().stop(&parent, None));
        assert!(registry.retire_owed(&parent), "the stopped helper");
        assert!(registry.retire_owed(&child), "its own helper");
        assert_eq!(rx.try_recv().unwrap().how, How::StoppedByUser);
    }
}
