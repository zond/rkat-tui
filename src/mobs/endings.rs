//! How each helper ends, and when its parent is told.
//!
//! A helper's parent is the session that started it: a `delegate` helper's
//! (one a realm of an earlier build holds, [`super::delegates`]) is the
//! session whose implicit mob it is a member of
//! (`implicit-<session>`), any other member's the session
//! that seated it or created its mob. The parent learns of the ending
//! once, at once, however it ends ([`How`]); the daemon words and delivers
//! the note (`crate::daemon::agents::endings`). [`HelperEndings`] decides
//! when an ending is one to tell, from three sources:
//!
//! - the helper's own run ending ([`Terminal`], from its event stream);
//! - the user stopping it ([`HelperEndings::stop`]) and the daemon finding
//!   it ended by its last stop ([`HelperEndings::lost`]);
//! - for a `delegate` helper an earlier build left, the daemon's start
//!   finding it still seated ([`HelperEndings::left_over`]): its call ended
//!   with that build's daemon, so it is told when its run ends, with its
//!   answer, and retired.
//!
//! An `assign` helper ([`super::HelperKind::Assigned`]) ends when it
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
//! A long-lived member ([`super::HelperKind::Member`]: `spawn_member`,
//! `mob_spawn_member`) does not end when a run of it ends: it ends when it
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
//! ([`HelperEndings::checkpoints_to`], `crate::daemon::checkpoint`), and
//! the end of the run its status is asked in is held too. The checkpoint
//! either continues it ([`HelperEndings::checkpoint_continues`]: the runs
//! after are told as any) or stops it ([`HelperEndings::checkpoint_stopped`]:
//! an `assign` helper is done with its status as its answer and retired, a
//! member stays seated, idle; its parent is told either way).
//!
//! A `fork_off` child ([`super::HelperKind::Fork`]) is told as such a
//! member, except for its first run: the engine tells the parent of that
//! run's end itself ("Background fork_off job … finished"), and of no later
//! run, which the parent's messages start now that the child is wired to it.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, OnceLock, Weak};

use meerkat_core::AgentEvent;
use meerkat_mob_mcp::MobMcpState;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use super::{HelperKey, HelperTag};

/// How many settled helpers are remembered, so a late event of one (its
/// cancelled run's end after the user stopped it) tells nothing again.
const SETTLED: usize = 512;

/// How a helper ended, as its parent and the user are told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum How {
    /// A `delegate` helper an earlier build left completed its run: its
    /// call ended with that build's daemon, so nothing else returns its
    /// answer; `answer` is its final text.
    Finished {
        /// Its final answer.
        answer: String,
    },
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
    /// It did not survive the daemon's stop (told at the next start).
    DaemonStopped,
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
        /// It was retired.
        retired: bool,
    },
}

impl How {
    /// The words a helper's line shows (`stopped by you`).
    #[must_use]
    pub fn text(&self) -> String {
        match self {
            Self::Finished { .. } | Self::Done { .. } => "finished".to_string(),
            Self::WentIdle { failed: None, .. } => "idle".to_string(),
            Self::WentIdle {
                failed: Some(reason),
                ..
            } => format!("idle after a failed run: {}", plain_reason(reason)),
            Self::Failed { reason } => format!("failed: {}", plain_reason(reason)),
            Self::StoppedByUser => "stopped by you".to_string(),
            Self::DaemonStopped => "ended when the daemon stopped".to_string(),
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
    /// The helper.
    pub key: HelperKey,
    /// What it was and whom it worked for.
    pub tag: HelperTag,
    /// How it ended.
    pub how: How,
}

/// How a helper's own run ended, from its events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Terminal {
    /// It completed with this final text.
    Completed(String),
    /// It failed, for this reason.
    Failed(String),
    /// It was cancelled.
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
/// ([`HelperEndings::checkpoints_to`]) is handed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitReached {
    /// The helper.
    pub key: HelperKey,
    /// What it is and whom it works for.
    pub tag: HelperTag,
    /// The model calls its run made.
    pub steps: u64,
    /// Which of its runs it is (counted from its first; a checkpoint's
    /// continue run is two after the run it continues).
    pub run: u64,
}

/// What is known of one helper that has not ended yet.
#[derive(Debug, Clone, Default)]
struct Tracked {
    tag: Option<HelperTag>,
    /// A `delegate` helper an earlier build left ([`HelperEndings::left_over`]).
    left_over: bool,
    terminal: Option<Terminal>,
    /// How many runs of it started (a run's end is told only while no
    /// later one started).
    runs: u64,
    /// The last tool call of its current run was a message to its parent.
    messaged_parent: bool,
    /// Its first run's end is told by the engine (a fresh fork's outcome).
    first_told_elsewhere: bool,
    /// The ends of its runs up to this one are its checkpoint's (the run
    /// at its limit and the status run after it): no ending of it (0: no
    /// checkpoint yet).
    hold_through: u64,
}

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

/// What [`decide`] makes of a `delegate` helper an earlier build left:
/// `None` while it has not ended (or is not known to be one), else how its
/// parent is told; it is then retired when still seated.
fn decide(delegate: bool, tracked: &Tracked) -> Option<How> {
    if !delegate || !tracked.left_over {
        return None;
    }
    match tracked.terminal.as_ref()? {
        Terminal::Completed(answer) => Some(How::Finished {
            answer: answer.clone(),
        }),
        Terminal::Failed(reason) => Some(How::Failed {
            reason: reason.clone(),
        }),
        Terminal::Cancelled => Some(How::Failed {
            reason: "its run was cancelled".to_string(),
        }),
        Terminal::Limit { .. } => Some(How::Failed {
            reason: "its run reached its step limit".to_string(),
        }),
    }
}

/// Whether `key` is a `delegate` helper (a member of an implicit mob).
fn is_delegate(key: &HelperKey) -> bool {
    key.mob.as_str().starts_with("implicit-")
}

/// The helpers that have not ended yet, and where each ending goes.
#[derive(Debug, Default)]
pub struct HelperEndings {
    tracked: Mutex<HashMap<HelperKey, Tracked>>,
    /// The helpers whose ending was settled, newest last.
    settled: Mutex<VecDeque<HelperKey>>,
    /// Where the endings to tell go (the daemon's delivery).
    sink: OnceLock<mpsc::UnboundedSender<Ended>>,
    /// The mob host (a helper is retired through it).
    state: OnceLock<Weak<MobMcpState>>,
    /// How many endings were told so far.
    told: std::sync::atomic::AtomicUsize,
    /// How many retires these endings started.
    retires: std::sync::atomic::AtomicUsize,
    /// Asks the runtime what a helper still holds ([`InputsProbe`]).
    probe: OnceLock<Probe>,
    /// How many runs' ends were decided as a helper's going idle (told or
    /// not).
    idle_decided: std::sync::atomic::AtomicUsize,
    /// The peer ids and names of the `assign` helpers retired when they
    /// finished, newest last ([`Self::retired_peer`]).
    retired_peers: Arc<Mutex<VecDeque<(String, String)>>>,
    /// Where a run at its limit goes for its checkpoint
    /// ([`Self::checkpoints_to`]); unset, it is a failure.
    checkpoints: OnceLock<mpsc::UnboundedSender<LimitReached>>,
    /// Held while a helper's going idle is decided and while a note is
    /// sent to a helper ([`Self::while_unsettled`]): a note either lands
    /// before the decision (unfinished input: the helper goes on) or finds
    /// the helper settled (it goes further up), never in between.
    deciding: tokio::sync::Mutex<()>,
    /// Tests: awaited before a helper's going idle is decided.
    #[cfg(any(test, feature = "test-support"))]
    before_deciding: OnceLock<DecidingHook>,
    /// Tests: awaited before a finished `assign` helper is retired.
    #[cfg(any(test, feature = "test-support"))]
    before_retiring: OnceLock<DecidingHook>,
}

/// Tests: what [`HelperEndings::before_deciding`] awaits for a helper.
#[cfg(any(test, feature = "test-support"))]
pub type DecidingHookFn = Arc<
    dyn Fn(HelperKey) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

/// [`DecidingHookFn`], held.
#[cfg(any(test, feature = "test-support"))]
struct DecidingHook(DecidingHookFn);

#[cfg(any(test, feature = "test-support"))]
impl std::fmt::Debug for DecidingHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DecidingHook")
    }
}

/// How many retired `assign` helpers' peer ids are remembered.
const RETIRED_PEERS: usize = 64;

/// [`InputsProbe`], held.
struct Probe(InputsProbe);

impl std::fmt::Debug for Probe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Probe")
    }
}

impl HelperEndings {
    /// Send every ending to tell to `sink` from now on (once).
    pub fn deliver_to(&self, sink: mpsc::UnboundedSender<Ended>) {
        if self.sink.set(sink).is_err() {
            tracing::warn!("helper endings already have a sink; the second is ignored");
        }
    }

    /// Hand every helper's run that reaches its limit to `sink` for its
    /// checkpoint from now on (once).
    pub fn checkpoints_to(&self, sink: mpsc::UnboundedSender<LimitReached>) {
        if self.checkpoints.set(sink).is_err() {
            tracing::warn!("helper checkpoints already have a sink; the second is ignored");
        }
    }

    /// The checkpoint of `key` continues it: the end of the run its
    /// status was asked in is no ending (the runs after it are).
    pub fn checkpoint_continues(&self, key: &HelperKey) {
        if let Some(tracked) = self.tracked.lock().get_mut(key) {
            tracked.hold_through = tracked.hold_through.max(tracked.runs);
        }
    }

    /// The checkpoint of `key` after `steps` model calls stopped it for
    /// `reason`, with its `status`: an `assign` helper is done (its parent
    /// is told, with its status as its answer, and it is retired), a
    /// long-lived member or a fork stays seated, idle (its parent is told).
    /// A helper that ended meanwhile (the user stopped it) tells nothing.
    pub fn checkpoint_stopped(&self, key: &HelperKey, steps: u64, reason: String, status: String) {
        let tag = {
            let mut tracked = self.tracked.lock();
            let Some(entry) = tracked.get_mut(key) else {
                return;
            };
            entry.hold_through = entry.hold_through.max(entry.runs);
            entry.tag.clone()
        };
        let Some(tag) = tag else { return };
        match tag.kind {
            super::HelperKind::Assigned => {
                if let Some(tag) = self.settle(key) {
                    self.tell(Ended {
                        key: key.clone(),
                        tag,
                        how: How::CheckpointStopped {
                            steps,
                            reason,
                            status,
                            retired: true,
                        },
                    });
                }
                self.retire_done(key);
            }
            super::HelperKind::Member | super::HelperKind::Fork => self.tell(Ended {
                key: key.clone(),
                tag,
                how: How::CheckpointStopped {
                    steps,
                    reason,
                    status,
                    retired: false,
                },
            }),
            super::HelperKind::Other => {}
        }
    }

    /// Whether `key` is followed and not ended (a checkpoint of it may go
    /// on).
    #[must_use]
    pub fn is_live(&self, key: &HelperKey) -> bool {
        !self.is_settled(key) && self.tracked.lock().contains_key(key)
    }

    /// Retire helpers through `state` (once).
    pub fn bind(&self, state: &Arc<MobMcpState>) {
        let _ = self.state.set(Arc::downgrade(state));
    }

    /// Whether `key`'s ending was settled already.
    fn is_settled(&self, key: &HelperKey) -> bool {
        self.settled.lock().contains(key)
    }

    /// The helper `key` (`tag`) was spawned or rebuilt.
    pub fn spawned(&self, key: &HelperKey, tag: &HelperTag) {
        if self.is_settled(key) {
            // The same member id spawned again: a new helper.
            self.settled.lock().retain(|settled| settled != key);
        }
        self.tracked.lock().entry(key.clone()).or_default().tag = Some(tag.clone());
    }

    /// The fresh fork `key`'s first run's end reaches its parent from the
    /// engine ("Background fork_off job … finished"): only its later runs
    /// are told here.
    pub fn first_run_told_by_engine(&self, key: &HelperKey) {
        if let Some(tracked) = self.tracked.lock().get_mut(key) {
            tracked.first_told_elsewhere = true;
        }
    }

    /// `key` is a `delegate` helper an earlier build left, still seated:
    /// its call ended with that build's daemon, so its parent is told when
    /// its run ends (also when that came first), with its answer, and it
    /// is retired then.
    pub fn left_over(&self, key: &HelperKey) {
        self.update(key, |tracked| tracked.left_over = true);
    }

    /// Ask the runtime what a helper still holds through `probe` (once).
    pub fn probe_with(&self, probe: InputsProbe) {
        let _ = self.probe.set(Probe(probe));
    }

    /// A run of the helper `key` started.
    pub fn run_started(&self, key: &HelperKey) {
        if let Some(tracked) = self.tracked.lock().get_mut(key) {
            tracked.runs += 1;
            tracked.messaged_parent = false;
        }
    }

    /// The helper `key` called a tool; `to_parent`: a message to its
    /// parent.
    pub fn called(&self, key: &HelperKey, to_parent: bool) {
        if let Some(tracked) = self.tracked.lock().get_mut(key) {
            tracked.messaged_parent = to_parent;
        }
    }

    /// A run of the helper `key` ended `terminal`. An `assign` helper or
    /// a long-lived member whose run completed or failed goes idle once the
    /// runtime holds nothing more for it ([`Self::settle_idle`]).
    pub fn run_ended(self: &Arc<Self>, key: &HelperKey, terminal: Terminal) {
        if is_delegate(key) {
            self.update(key, |tracked| tracked.terminal = Some(terminal));
            return;
        }
        if terminal == Terminal::Cancelled {
            return;
        }
        let (runs, tag) = {
            let mut tracked = self.tracked.lock();
            match tracked.get_mut(key) {
                Some(Tracked {
                    tag: Some(tag),
                    runs,
                    hold_through,
                    ..
                }) if tag.kind != super::HelperKind::Other => {
                    // Its checkpoint's runs end no helper.
                    if *hold_through > 0 && *runs <= *hold_through {
                        return;
                    }
                    if let Terminal::Limit { .. } = terminal
                        && self.checkpoints.get().is_some()
                    {
                        *hold_through = *runs + 1;
                    }
                    (*runs, tag.clone())
                }
                _ => return,
            }
        };
        let terminal = match terminal {
            Terminal::Limit { steps } => match self.checkpoints.get() {
                Some(sink) => {
                    if sink
                        .send(LimitReached {
                            key: key.clone(),
                            tag,
                            steps,
                            run: runs,
                        })
                        .is_err()
                    {
                        tracing::debug!("the helper checkpoints' sink is gone");
                    }
                    return;
                }
                None => Terminal::Failed("its run reached its step limit".to_string()),
            },
            other => other,
        };
        let probe = self.probe.get().map(|probe| Arc::clone(&probe.0));
        match (probe, tokio::runtime::Handle::try_current()) {
            (Some(probe), Ok(runtime)) => {
                let endings = Arc::clone(self);
                let key = key.clone();
                runtime.spawn(async move {
                    endings.settle_idle(&key, runs, &probe).await;
                    #[cfg(any(test, feature = "test-support"))]
                    if let Some(hook) = endings.before_deciding.get() {
                        (hook.0)(key.clone()).await;
                    }
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

    /// Tests: await `hook` (with the helper's key) before each decision
    /// of a helper's going idle (once).
    #[cfg(any(test, feature = "test-support"))]
    pub fn before_deciding(&self, hook: DecidingHookFn) {
        let _ = self.before_deciding.set(DecidingHook(hook));
    }

    /// Tests: await `hook` (with the helper's key) before a finished
    /// `assign` helper is retired (once).
    #[cfg(any(test, feature = "test-support"))]
    pub fn before_retiring(&self, hook: DecidingHookFn) {
        let _ = self.before_retiring.set(DecidingHook(hook));
    }

    /// How many runs of `key` started (`None`: not followed).
    fn runs_of(&self, key: &HelperKey) -> Option<u64> {
        self.tracked.lock().get(key).map(|tracked| tracked.runs)
    }

    /// The run `runs` of `key` ended `terminal` and nothing is queued for
    /// it: unless a later run started, an `assign` helper is done (its
    /// parent is told, and it is retired) and a long-lived member went idle
    /// (its parent is told, unless the run's last call was a message to it).
    fn went_idle(&self, key: &HelperKey, runs: u64, terminal: Terminal) {
        let (tag, messaged_parent, first_told_elsewhere) = {
            let tracked = self.tracked.lock();
            match tracked.get(key) {
                Some(tracked) if tracked.runs == runs => (
                    tracked.tag.clone(),
                    tracked.messaged_parent,
                    tracked.first_told_elsewhere && runs <= 1,
                ),
                _ => return,
            }
        };
        let Some(tag) = tag else { return };
        self.idle_decided
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        match tag.kind {
            super::HelperKind::Assigned => {
                let how = match terminal {
                    Terminal::Completed(answer) => How::Done { answer },
                    Terminal::Failed(reason) => How::Failed { reason },
                    // A run at its limit goes to its checkpoint before.
                    Terminal::Cancelled | Terminal::Limit { .. } => return,
                };
                if let Some(tag) = self.settle(key) {
                    self.tell(Ended {
                        key: key.clone(),
                        tag,
                        how,
                    });
                }
                self.retire_done(key);
            }
            super::HelperKind::Member | super::HelperKind::Fork => {
                if first_told_elsewhere {
                    return;
                }
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
                });
            }
            super::HelperKind::Other => {}
        }
    }

    fn update(&self, key: &HelperKey, change: impl FnOnce(&mut Tracked)) {
        if self.is_settled(key) {
            return;
        }
        let decision = {
            let mut tracked = self.tracked.lock();
            let entry = tracked.entry(key.clone()).or_default();
            change(entry);
            decide(is_delegate(key), entry)
        };
        if let Some(how) = decision {
            if let Some(tag) = self.settle(key) {
                self.tell(Ended {
                    key: key.clone(),
                    tag,
                    how,
                });
            }
            self.retire(key, false);
        }
    }

    /// The user stopped the helper `key` (`tag`, when the caller knows it
    /// better): its run is force-cancelled and it is retired, the helpers
    /// it started keep running, and its parent is told. Returns whether it
    /// was a helper not ended yet.
    pub fn stop(&self, key: &HelperKey, tag: Option<HelperTag>) -> bool {
        if self.is_settled(key) {
            return false;
        }
        let known = self.settle(key).or(tag);
        self.retire(key, true);
        match known {
            Some(tag) => {
                self.tell(Ended {
                    key: key.clone(),
                    tag,
                    how: How::StoppedByUser,
                });
                true
            }
            None => false,
        }
    }

    /// The daemon found `key` (`tag`) left over by its last stop and no
    /// longer seated (the engine retired it): its parent is told.
    pub fn lost(&self, key: &HelperKey, tag: HelperTag) {
        if self.is_settled(key) {
            return;
        }
        // Nothing is retired: the helper is not seated, and a member
        // seated under its id from now on is another helper.
        self.settle(key);
        self.tell(Ended {
            key: key.clone(),
            tag,
            how: How::DaemonStopped,
        });
    }

    /// The helper `key` is ended with the conversation it worked for (its
    /// agent switched conversations): nobody is told.
    pub fn left(&self, key: &HelperKey) {
        if !self.is_settled(key) {
            self.settle(key);
        }
    }

    /// Mark `key` settled; its tag, when one was known.
    fn settle(&self, key: &HelperKey) -> Option<HelperTag> {
        let tag = self.tracked.lock().remove(key).and_then(|t| t.tag);
        let mut settled = self.settled.lock();
        settled.push_back(key.clone());
        while settled.len() > SETTLED {
            settled.pop_front();
        }
        tag
    }

    fn tell(&self, ended: Ended) {
        tracing::info!(helper = %ended.key, how = %ended.how.text(), "a helper ended");
        self.told.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        match self.sink.get() {
            Some(sink) => {
                if sink.send(ended).is_err() {
                    tracing::debug!("the helper endings' sink is gone");
                }
            }
            None => tracing::debug!("no sink for helper endings"),
        }
    }

    /// Retire `key` off the caller's task: a stop (`any`) force-cancels
    /// and retires whatever member it is; otherwise only a `delegate`
    /// helper still seated is ([`super::delegates::retire_left_helper`]).
    fn retire(&self, key: &HelperKey, any: bool) {
        self.retires
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let Some(state) = self.state.get().and_then(Weak::upgrade) else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let key = key.clone();
        runtime.spawn(async move {
            if any {
                match state.handle_for(&key.mob).await {
                    Ok(handle) => super::stop_helper(&handle, &key).await,
                    Err(err) => {
                        tracing::warn!(helper = %key, error = %err, "cannot reach the stopped helper's mob");
                    }
                }
            } else {
                super::delegates::retire_left_helper(&state, &key).await;
            }
        });
    }

    /// Retire the `assign` helper `key`, which finished, off the caller's
    /// task, remembering its peer id first: a message its parent sends it
    /// from now on is refused saying it finished ([`Self::retired_peer`]),
    /// not with the engine's "peer not found".
    fn retire_done(&self, key: &HelperKey) {
        self.retires
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let Some(state) = self.state.get().and_then(Weak::upgrade) else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let key = key.clone();
        let retired = Arc::clone(&self.retired_peers);
        #[cfg(any(test, feature = "test-support"))]
        let hook = self.before_retiring.get().map(|hook| Arc::clone(&hook.0));
        runtime.spawn(async move {
            #[cfg(any(test, feature = "test-support"))]
            if let Some(hook) = hook {
                hook(key.clone()).await;
            }
            match state.handle_for(&key.mob).await {
                Ok(handle) => {
                    if let Ok(Some(endpoint)) = handle.member_peer_endpoint(&key.member).await {
                        let mut retired = retired.lock();
                        retired.push_back((
                            endpoint.descriptor.peer_id.to_string(),
                            key.member.to_string(),
                        ));
                        while retired.len() > RETIRED_PEERS {
                            retired.pop_front();
                        }
                    }
                    super::stop_helper(&handle, &key).await;
                }
                Err(err) => {
                    tracing::warn!(helper = %key, error = %err, "cannot reach the finished helper's mob");
                }
            }
        });
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
            .find(|(peer, _)| peer == peer_id)
            .map(|(_, name)| name.clone())
    }

    /// Tests: how many retires these endings started so far.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn retires(&self) -> usize {
        self.retires.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Tests: whether `key` is followed and not ended yet.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn is_tracked(&self, key: &HelperKey) -> bool {
        self.tracked.lock().contains_key(key)
    }

    /// Tests: whether `key`'s ending was settled (told or not).
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn settled(&self, key: &HelperKey) -> bool {
        self.is_settled(key)
    }

    /// Tests: how many runs' ends were decided as a helper's going idle.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn idle_decided(&self) -> usize {
        self.idle_decided.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Tests: how many endings were told so far.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn told(&self) -> usize {
        self.told.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use meerkat_core::SessionId;
    use meerkat_mob::{AgentIdentity, MobId};

    fn delegate_key(member: &str) -> HelperKey {
        HelperKey::new(
            super::super::delegates::implicit_mob(&SessionId::new()),
            AgentIdentity::from(member),
        )
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
        let endings = Arc::new(HelperEndings::default());
        let (tx, rx) = mpsc::unbounded_channel();
        endings.deliver_to(tx);
        (endings, rx)
    }

    /// A helper the last daemon left that its mob no longer seats is told
    /// to its parent and retires nothing: a member seated under its id
    /// later is a new helper.
    #[test]
    fn a_lost_helper_is_told_and_nothing_is_retired_by_its_id() {
        let (endings, mut rx) = endings();
        let key = delegate_key("slow");
        endings.lost(&key, tag("slow"));
        assert_eq!(rx.try_recv().unwrap().how, How::DaemonStopped);
        assert_eq!(endings.retires(), 0);
        // The same id spawned again is a new helper, followed afresh.
        endings.spawned(&key, &tag("slow"));
        assert!(endings.is_tracked(&key) && !endings.settled(&key));
    }

    /// A `delegate` helper an earlier build left is told with its answer
    /// when its run ends, whichever of the two the daemon learns first,
    /// and retired; one only seated, not known as left over, is not told.
    #[test]
    fn a_left_over_delegate_is_told_with_its_answer_when_it_finishes_in_either_order() {
        let (endings, mut rx) = endings();
        let first = delegate_key("a");
        endings.spawned(&first, &tag("a"));
        endings.left_over(&first);
        assert!(rx.try_recv().is_err(), "it still runs");
        endings.run_ended(&first, Terminal::Completed("the answer".into()));
        let ended = rx.try_recv().unwrap();
        assert_eq!(
            ended.how,
            How::Finished {
                answer: "the answer".into()
            }
        );
        assert_eq!(ended.tag.label(), "a");
        assert_eq!(endings.retires(), 1);
        // Its run ended just before the start found it.
        let second = delegate_key("b");
        endings.spawned(&second, &tag("b"));
        endings.run_ended(&second, Terminal::Failed("a 400".into()));
        assert!(rx.try_recv().is_err(), "not known as left over yet");
        endings.left_over(&second);
        assert_eq!(
            rx.try_recv().unwrap().how,
            How::Failed {
                reason: "a 400".into()
            }
        );
        assert!(rx.try_recv().is_err());
        // A late event of a settled one tells nothing.
        endings.run_ended(&first, Terminal::Cancelled);
        assert!(rx.try_recv().is_err());
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
        // Stopping a `delegate` helper an earlier build left: its cancelled
        // run that follows tells nothing more.
        let key = delegate_key("d");
        endings.spawned(&key, &tag("d"));
        endings.left_over(&key);
        assert!(endings.stop(&key, None));
        endings.run_ended(&key, Terminal::Cancelled);
        assert_eq!(rx.try_recv().unwrap().how, How::StoppedByUser);
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
        use crate::mobs::HelperKind;
        let (endings, mut rx) = endings();
        let key = HelperKey::new(MobId::from("tui"), AgentIdentity::from("scout"));
        endings.spawned(&key, &tagged("scout", HelperKind::Assigned));
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
        endings.spawned(&failed, &tagged("broken", HelperKind::Assigned));
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
        use crate::mobs::HelperKind;
        let (endings, mut rx) = endings();
        let key = HelperKey::new(MobId::from("tui"), AgentIdentity::from("w1"));
        endings.spawned(&key, &tagged("w1", HelperKind::Member));
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

    /// A fresh fork's first run's end is the engine's to tell: not told
    /// here, but decided; each later run that leaves it idle is told as a
    /// member's; a fork rebuilt after a restart has every run told.
    #[test]
    fn a_fork_is_told_of_its_runs_after_the_first() {
        use crate::mobs::HelperKind;
        let (endings, mut rx) = endings();
        let key = HelperKey::new(MobId::from("tui"), AgentIdentity::from("kid"));
        endings.spawned(&key, &tagged("kid", HelperKind::Fork));
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
        endings.spawned(&rebuilt, &tagged("kid2", HelperKind::Fork));
        endings.run_started(&rebuilt);
        endings.run_ended(&rebuilt, Terminal::Completed("after a restart".into()));
        assert!(matches!(rx.try_recv().unwrap().how, How::WentIdle { .. }));
    }

    /// A run at its limit is no ending: it goes to the checkpoint, and the
    /// status run's end after it is held too. Continued, the helper's
    /// later run ends it as any; stopped, an `assign` helper is done with
    /// its status and retired, a member stays.
    #[test]
    fn a_run_at_its_limit_goes_to_its_checkpoint_before_any_ending() {
        use crate::mobs::HelperKind;
        let (endings, mut rx) = endings();
        let (tx, mut limits) = mpsc::unbounded_channel();
        endings.checkpoints_to(tx);
        let key = HelperKey::new(MobId::from("tui"), AgentIdentity::from("scout"));
        endings.spawned(&key, &tagged("scout", HelperKind::Assigned));
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
        endings.spawned(&stopped, &tagged("loop", HelperKind::Assigned));
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
        endings.spawned(&member, &tagged("w1", HelperKind::Member));
        endings.run_started(&member);
        endings.run_ended(&member, Terminal::Limit { steps: 10 });
        endings.checkpoint_stopped(&member, 10, "enough".into(), "s".into());
        assert!(matches!(
            rx.try_recv().unwrap().how,
            How::CheckpointStopped { retired: false, .. }
        ));
        assert!(endings.is_live(&member));
        // Without a checkpoint sink the limit is a failure, as it was.
        let (plain, mut told) = super::tests::endings();
        plain.spawned(&key, &tagged("scout", HelperKind::Assigned));
        plain.run_ended(&key, Terminal::Limit { steps: 10 });
        assert!(matches!(told.try_recv().unwrap().how, How::Failed { .. }));
    }

    #[test]
    fn the_endings_read_in_plain_words() {
        assert_eq!(How::StoppedByUser.text(), "stopped by you");
        assert_eq!(How::DaemonStopped.text(), "ended when the daemon stopped");
        let recovered = How::Failed {
            reason: "run failed: InterruptedByRecovery".into(),
        };
        assert_eq!(
            recovered.text(),
            "failed: its run was cut off when the daemon stopped"
        );
        assert!(!recovered.text().contains("Recovery"));
    }
}
