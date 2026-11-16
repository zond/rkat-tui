//! How each helper ends, and when its parent is told.
//!
//! A helper's parent is the session that started it: a `delegate` helper's
//! is the session whose implicit mob it is a member of
//! ([`super::delegates::implicit_mob`]), any other member's the session
//! that created its mob. The parent learns of the ending once, at once,
//! however it ends ([`How`]); the daemon words and delivers the note
//! (`crate::daemon::agents::endings`). [`HelperEndings`] decides when an
//! ending is one to tell, from three sources:
//!
//! - the parent's own `delegate` call, as its events show it
//!   ([`super::delegates::CallEnded`]): a call that returned the helper's
//!   answer is the notification itself, and nothing more is said; a call
//!   that timed out or failed is told at once; a call cut off by the
//!   parent's run ending (an interrupt) leaves the helper running, and it
//!   is told when the helper's own run ends, with its answer;
//! - the helper's own run ending ([`Terminal`], from its event stream);
//! - the user stopping it ([`HelperEndings::stop`]) and the daemon finding
//!   it ended by its last stop ([`HelperEndings::lost`]).
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
//! own few milliseconds.
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

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, OnceLock, Weak};

use meerkat_core::AgentEvent;
use meerkat_mob_mcp::MobMcpState;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use super::delegates::CallEnded;
use super::{HelperKey, HelperTag};

/// How many settled helpers are remembered, so a late event of one (its
/// cancelled run's end after the user stopped it) tells nothing again.
const SETTLED: usize = 512;

/// How a helper ended, as its parent and the user are told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum How {
    /// Its run completed after nothing could return its answer any more
    /// (its `delegate` call was cut off): `answer` is its final text.
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
    /// Its `delegate` call ran into the tool timeout, and it was ended.
    TimedOut,
    /// It did not survive the daemon's stop (told at the next start).
    DaemonStopped,
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
            Self::TimedOut => "timed out".to_string(),
            Self::DaemonStopped => "ended when the daemon stopped".to_string(),
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
}

impl Terminal {
    /// The run end `event` is (`None`: no run end).
    #[must_use]
    pub fn of(event: &AgentEvent) -> Option<Self> {
        match event {
            AgentEvent::RunCompleted { result, .. } => Some(Self::Completed(result.clone())),
            AgentEvent::RunFailed { error_report, .. } => Some(
                if error_report.class == meerkat_core::AgentErrorClass::Cancelled {
                    Self::Cancelled
                } else {
                    Self::Failed(error_report.message.clone())
                },
            ),
            _ => None,
        }
    }
}

/// What is known of one helper that has not ended yet.
#[derive(Debug, Clone, Default)]
struct Tracked {
    tag: Option<HelperTag>,
    call: Option<CallEnded>,
    terminal: Option<Terminal>,
    /// How many runs of it started (a run's end is told only while no
    /// later one started).
    runs: u64,
    /// The last tool call of its current run was a message to its parent.
    messaged_parent: bool,
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

/// What [`decide`] makes of a helper.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Decision {
    /// Not ended yet, or not known how.
    Wait,
    /// Ended, and the parent already knows (its call returned the answer).
    Silent,
    /// Ended: tell the parent `How`, and retire the helper when it is
    /// still seated (`retire`).
    Tell(How, bool),
}

/// The decision for a helper that is (`delegate`) or is not a `delegate`
/// helper, from what is known of it.
fn decide(delegate: bool, tracked: &Tracked) -> Decision {
    if !delegate {
        return Decision::Wait;
    }
    let failed = |terminal: Option<&Terminal>, otherwise: &str| match terminal {
        Some(Terminal::Failed(reason)) => reason.clone(),
        Some(Terminal::Cancelled) => "its run was cancelled".to_string(),
        _ => otherwise.to_string(),
    };
    match tracked.call {
        None => Decision::Wait,
        Some(CallEnded::Returned) => Decision::Silent,
        Some(CallEnded::TimedOut) => Decision::Tell(How::TimedOut, true),
        Some(CallEnded::Failed) => Decision::Tell(
            How::Failed {
                reason: failed(tracked.terminal.as_ref(), "its `delegate` call failed"),
            },
            true,
        ),
        Some(CallEnded::CutOff) => match &tracked.terminal {
            None => Decision::Wait,
            Some(Terminal::Completed(answer)) => Decision::Tell(
                How::Finished {
                    answer: answer.clone(),
                },
                true,
            ),
            Some(other) => Decision::Tell(
                How::Failed {
                    reason: failed(Some(other), ""),
                },
                true,
            ),
        },
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
}

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

    /// The parent's `delegate` call that started `key` ended `how`.
    pub fn call_ended(&self, key: &HelperKey, how: CallEnded) {
        self.update(key, |tracked| tracked.call = Some(how));
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
        let runs = {
            let tracked = self.tracked.lock();
            match tracked.get(key) {
                Some(Tracked {
                    tag: Some(tag),
                    runs,
                    ..
                }) if tag.kind != super::HelperKind::Other => *runs,
                _ => return,
            }
        };
        let probe = self.probe.get().map(|probe| Arc::clone(&probe.0));
        match (probe, tokio::runtime::Handle::try_current()) {
            (Some(probe), Ok(runtime)) => {
                let endings = Arc::clone(self);
                let key = key.clone();
                runtime.spawn(async move {
                    endings.settle_idle(&key, runs, &probe).await;
                    endings.went_idle(&key, runs, terminal);
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

    /// How many runs of `key` started (`None`: not followed).
    fn runs_of(&self, key: &HelperKey) -> Option<u64> {
        self.tracked.lock().get(key).map(|tracked| tracked.runs)
    }

    /// The run `runs` of `key` ended `terminal` and nothing is queued for
    /// it: unless a later run started, an `assign` helper is done (its
    /// parent is told, and it is retired) and a long-lived member went idle
    /// (its parent is told, unless the run's last call was a message to it).
    fn went_idle(&self, key: &HelperKey, runs: u64, terminal: Terminal) {
        let (tag, messaged_parent) = {
            let tracked = self.tracked.lock();
            match tracked.get(key) {
                Some(tracked) if tracked.runs == runs => {
                    (tracked.tag.clone(), tracked.messaged_parent)
                }
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
                    Terminal::Cancelled => return,
                };
                if let Some(tag) = self.settle(key) {
                    self.tell(Ended {
                        key: key.clone(),
                        tag,
                        how,
                    });
                }
                self.retire(key, true);
            }
            super::HelperKind::Member => {
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
                    Terminal::Cancelled => return,
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
        match decision {
            Decision::Wait => {}
            Decision::Silent => {
                self.settle(key);
            }
            Decision::Tell(how, retire) => {
                if let Some(tag) = self.settle(key) {
                    self.tell(Ended {
                        key: key.clone(),
                        tag,
                        how,
                    });
                }
                if retire {
                    self.retire(key, false);
                }
            }
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
        // seated under its id from now on is another helper, which a
        // resumed run's `delegate` call (dispatched again with the same
        // `member_id`) seats as the start goes on. Retiring by the id
        // here retired that one when the mob listed it first.
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
    /// helper still seated is ([`super::delegates::retire_orphan`]).
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
                super::delegates::retire_orphan(&state, &key).await;
            }
        });
    }

    /// The `delegate` helpers whose parent's call is still open (no end of
    /// it seen): at the daemon's stop, the helpers of the calls a stopped
    /// run will dispatch again when the next start resumes it.
    #[must_use]
    pub fn open_delegates(&self) -> Vec<HelperKey> {
        self.tracked
            .lock()
            .iter()
            .filter(|(key, tracked)| {
                is_delegate(key) && tracked.tag.is_some() && tracked.call.is_none()
            })
            .map(|(key, _)| key.clone())
            .collect()
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
    /// later is a new helper (a resumed run's `delegate` dispatched again),
    /// which a retire by the id took when the mob listed it first. A timed
    /// out call's helper is still retired.
    #[test]
    fn a_lost_helper_is_told_and_nothing_is_retired_by_its_id() {
        let (endings, mut rx) = endings();
        let key = delegate_key("slow");
        endings.lost(&key, tag("slow"));
        assert_eq!(rx.try_recv().unwrap().how, How::DaemonStopped);
        assert_eq!(endings.retires(), 0);
        // The same id spawned again is a new helper, followed afresh.
        endings.spawned(&key, &tag("slow"));
        endings.call_ended(&key, CallEnded::TimedOut);
        assert_eq!(rx.try_recv().unwrap().how, How::TimedOut);
        assert_eq!(endings.retires(), 1);
    }

    #[test]
    fn a_delegate_whose_call_returned_its_answer_tells_nothing_more() {
        let (endings, mut rx) = endings();
        let key = delegate_key("a");
        endings.spawned(&key, &tag("a"));
        // The helper's run ends first, then the engine retires it and the
        // call returns: nothing is told, whatever the order.
        endings.run_ended(&key, Terminal::Completed("done".into()));
        endings.call_ended(&key, CallEnded::Returned);
        assert!(rx.try_recv().is_err());
        // A late event of the settled helper tells nothing either.
        endings.run_ended(&key, Terminal::Cancelled);
        assert!(rx.try_recv().is_err());
        assert!(!endings.is_tracked(&key));
    }

    #[test]
    fn a_cut_off_delegate_is_told_with_its_answer_when_it_finishes_in_either_order() {
        let (endings, mut rx) = endings();
        let first = delegate_key("a");
        endings.spawned(&first, &tag("a"));
        endings.call_ended(&first, CallEnded::CutOff);
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
        // Its run ended just before its call was cut off.
        let second = delegate_key("b");
        endings.spawned(&second, &tag("b"));
        endings.run_ended(&second, Terminal::Failed("a 400".into()));
        endings.call_ended(&second, CallEnded::CutOff);
        assert_eq!(
            rx.try_recv().unwrap().how,
            How::Failed {
                reason: "a 400".into()
            }
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn timed_out_and_failed_calls_are_told_at_once() {
        let (endings, mut rx) = endings();
        let timed = delegate_key("t");
        endings.spawned(&timed, &tag("t"));
        endings.call_ended(&timed, CallEnded::TimedOut);
        assert_eq!(rx.try_recv().unwrap().how, How::TimedOut);
        let failed = delegate_key("f");
        endings.spawned(&failed, &tag("f"));
        endings.run_ended(&failed, Terminal::Failed("refused".into()));
        endings.call_ended(&failed, CallEnded::Failed);
        assert_eq!(
            rx.try_recv().unwrap().how,
            How::Failed {
                reason: "refused".into()
            }
        );
        // A call that failed before any helper was spawned tells nothing.
        endings.call_ended(&delegate_key("never"), CallEnded::Failed);
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
        // Stopping a delegate whose call is still open: its call's error
        // that follows tells nothing more.
        let key = delegate_key("d");
        endings.spawned(&key, &tag("d"));
        assert!(endings.stop(&key, None));
        endings.call_ended(&key, CallEnded::Failed);
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

    #[test]
    fn the_endings_read_in_plain_words() {
        assert_eq!(How::StoppedByUser.text(), "stopped by you");
        assert_eq!(How::DaemonStopped.text(), "ended when the daemon stopped");
        assert_eq!(How::TimedOut.text(), "timed out");
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
