//! The UI's turn, the delivery of the user's typed messages, and
//! compaction: three small states ([`Turn`], [`Delivery`],
//! [`Compaction`]) that `App::update` asks and moves through their own
//! methods, in place of the eleven flags they were (`running`,
//! `streamed_text`, `returned`, `send_waiting`, `woken_by`, `send_next`,
//! `recalling`, `reached`, `draining`, `compacting`, `compaction_run`).
//! The Elm rule stands: they change only inside `App::update`.
//!
//! # Turn
//!
//! - `Idle { woken_by }`: no run of the agent is shown. A peer's message
//!   that wakes the idle agent ([`Turn::wake`]) names itself as the origin
//!   of the run about to start.
//! - `Running(Run)`: a run this client started ([`Turn::start`]: a
//!   prompt, the held queue, Ctrl-X Ctrl-S's next run; the user's, so a
//!   waking peer is forgotten) or one the daemon started that this client
//!   shows ([`Turn::join`], taking the waking peer as its origin). The run
//!   holds what only a running turn has: whether assistant text streamed
//!   (its summary is shown only when none did), the messages its
//!   interrupt took back so far ([`Turn::take_back`]), and whether Ctrl-X
//!   Ctrl-S interrupted it ([`Turn::push_waiting`]: those messages then go
//!   as the next run instead of to the editor).
//! - The turn's end ([`Turn::finish`]) goes back to `Idle` and hands the
//!   run's state to what decides what goes next.
//!
//! # Delivery
//!
//! What the user typed and has not reached the agent, beside the pending
//! queue itself: the text Ctrl-X Ctrl-S sends as the next run once the
//! interrupted turn ends (it outlives the turn while `!` commands run); a
//! recall on its way ([`Effect::RecallQueued`](super::Effect::RecallQueued)
//! sent, no answer yet); the admitted messages a recall found already with
//! the agent (↑ no longer tries them; the turn's end forgets them); and
//! whether a held command runs now (what queues behind it does not keep it
//! from running).
//!
//! # Compaction
//!
//! - `Idle` → `Asked`: `/compact now` on the idle agent
//!   ([`Effect::CompactNow`](super::Effect::CompactNow) sent).
//! - `Asked` → `Idle`: the request is answered
//!   ([`Msg::CompactRequested`](super::Msg::CompactRequested)); an answer
//!   while the run goes keeps `Running`.
//! - any → `Running` → `Idle`: the run that lets the idle agent compact
//!   began and ended ([`Msg::CompactionRun`](super::Msg::CompactionRun)).
//!
//! # What can no longer be represented
//!
//! - Streamed text, messages an interrupt took back, or Ctrl-X Ctrl-S's
//!   push while no turn runs (they are the run's).
//! - A waking peer remembered while a turn runs.
//! - The compaction's run going while nothing says a compaction is
//!   under way (`compaction_run` without `compacting`).

use super::PendingId;

/// The agent's turn as this client shows it (see the module docs).
#[derive(Debug)]
pub(super) enum Turn {
    /// No run is shown.
    Idle {
        /// Who woke the idle agent for the run about to start (another
        /// agent's message, which arrives just before the run's start).
        woken_by: Option<String>,
    },
    /// A run is shown.
    Running(Run),
}

impl Default for Turn {
    fn default() -> Self {
        Self::Idle { woken_by: None }
    }
}

/// What a running turn holds.
#[derive(Debug, Default)]
pub(super) struct Run {
    /// Whether assistant text streamed during it.
    pub streamed_text: bool,
    /// The messages its interrupt took back so far
    /// ([`Msg::Returned`](super::Msg::Returned)), kept until its end puts
    /// them in the editor in the order they were typed, counted in its one
    /// line.
    pub returned: Vec<(PendingId, String)>,
    /// Ctrl-X Ctrl-S interrupted it: the user's messages its interrupt
    /// hands back go as the next run (before the editor's text it took),
    /// not to the editor.
    pub push: bool,
}

/// A run the daemon started, now shown ([`Turn::join`]).
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Joined {
    /// Who woke the agent for it, if a peer did.
    pub woken_by: Option<String>,
}

impl Turn {
    /// Whether a turn runs.
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running(_))
    }

    /// This client starts a run (the user's: a waking peer is
    /// forgotten). A run already shown goes on with no text streamed yet.
    pub fn start(&mut self) {
        match self {
            Self::Running(run) => run.streamed_text = false,
            Self::Idle { .. } => *self = Self::Running(Run::default()),
        }
    }

    /// A run the daemon started is shown. `None` (and nothing changes)
    /// when one is shown already.
    pub fn join(&mut self) -> Option<Joined> {
        let Self::Idle { woken_by } = self else {
            return None;
        };
        let woken_by = woken_by.take();
        *self = Self::Running(Run::default());
        Some(Joined { woken_by })
    }

    /// The turn ended: idle, and what the run held (nothing when none was
    /// shown).
    pub fn finish(&mut self) -> Run {
        match std::mem::take(self) {
            Self::Running(run) => run,
            idle @ Self::Idle { .. } => {
                *self = idle;
                Run::default()
            }
        }
    }

    /// A peer's message that may wake the agent: remembered for the run
    /// about to start while idle; handed back while a turn runs.
    pub fn wake(&mut self, origin: String) -> Option<String> {
        match self {
            Self::Idle { woken_by } => {
                *woken_by = Some(origin);
                None
            }
            Self::Running(_) => Some(origin),
        }
    }

    /// Assistant text streamed.
    pub fn streamed(&mut self) {
        if let Self::Running(run) = self {
            run.streamed_text = true;
        }
    }

    /// Messages an interrupt took back: the running turn keeps them for
    /// its end; with none running they are handed back.
    pub fn take_back(
        &mut self,
        taken: Vec<(PendingId, String)>,
    ) -> Option<Vec<(PendingId, String)>> {
        match self {
            Self::Running(run) => {
                run.returned.extend(taken);
                None
            }
            Self::Idle { .. } => Some(taken),
        }
    }

    /// Ctrl-X Ctrl-S interrupted the running turn.
    pub fn push_waiting(&mut self) {
        if let Self::Running(run) = self {
            run.push = true;
        }
    }

    /// A running turn, for tests that need one without its start.
    #[cfg(test)]
    pub fn running() -> Self {
        Self::Running(Run::default())
    }
}

/// The delivery of what the user typed (see the module docs).
#[derive(Debug, Default)]
pub(super) struct Delivery {
    /// Prompt to send once the interrupted turn ends (Ctrl-X Ctrl-S).
    next: Option<String>,
    /// An [`Effect::RecallQueued`](super::Effect::RecallQueued) is waiting
    /// for its [`Msg::Recalled`](super::Msg::Recalled).
    recalling: bool,
    /// Admitted messages a recall found already with the agent.
    reached: Vec<PendingId>,
    /// A held command runs now ([`super::App::run_held`]).
    pub draining: bool,
}

impl Delivery {
    /// The text Ctrl-X Ctrl-S sends next, if any.
    pub fn next(&self) -> Option<&str> {
        self.next.as_deref()
    }

    /// Whether Ctrl-X Ctrl-S has a text to send next.
    pub fn has_next(&self) -> bool {
        self.next.is_some()
    }

    /// The text to send next, taken.
    pub fn take_next(&mut self) -> Option<String> {
        self.next.take()
    }

    /// `text` goes after what is to be sent next (a blank line between).
    pub fn send_after(&mut self, text: String) {
        self.next = Some(match self.next.take() {
            Some(prev) => format!("{prev}\n\n{text}"),
            None => text,
        });
    }

    /// `text` goes before what is to be sent next (a blank line between).
    pub fn send_before(&mut self, text: String) {
        self.next = Some(match self.next.take() {
            Some(next) => format!("{text}\n\n{next}"),
            None => text,
        });
    }

    /// Whether a recall is on its way.
    pub fn recalling(&self) -> bool {
        self.recalling
    }

    /// A recall goes out; `false` (nothing changes) while one is on its
    /// way.
    pub fn ask_recall(&mut self) -> bool {
        !std::mem::replace(&mut self.recalling, true)
    }

    /// The recall was answered: `left` had reached the agent already.
    pub fn recalled(&mut self, left: impl IntoIterator<Item = PendingId>) {
        self.recalling = false;
        self.reached.extend(left);
    }

    /// Whether a recall found `id` with the agent.
    pub fn reached(&self, id: PendingId) -> bool {
        self.reached.contains(&id)
    }

    /// `ids` are no longer with the agent (returned or not delivered).
    pub fn forget(&mut self, ids: &[PendingId]) {
        self.reached.retain(|reached| !ids.contains(reached));
    }

    /// Whether something of the user's waits beside the pending queue: a
    /// text to send next, or a recall's answer.
    pub fn waits(&self) -> bool {
        self.next.is_some() || self.recalling
    }

    /// The turn ended: no recall is waited for, and nothing is with the
    /// agent any more.
    pub fn turn_ended(&mut self) {
        self.recalling = false;
        self.reached.clear();
    }
}

/// A compaction of the agent's context (see the module docs).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Compaction {
    /// None under way.
    #[default]
    Idle,
    /// `/compact now` asked the idle agent; no answer yet.
    Asked,
    /// The run that lets the idle agent compact goes: what the user sends
    /// meanwhile is held.
    Running,
}

impl Compaction {
    /// Whether one is under way (asked for, or its run going): session
    /// commands wait.
    pub fn busy(self) -> bool {
        self != Self::Idle
    }

    /// Whether its run goes.
    pub fn run_going(self) -> bool {
        self == Self::Running
    }

    /// `/compact now` asked the idle agent.
    pub fn ask(&mut self) {
        *self = Self::Asked;
    }

    /// The request was answered (a run going keeps going).
    pub fn answered(&mut self) {
        if *self == Self::Asked {
            *self = Self::Idle;
        }
    }

    /// Its run began (`true`) or ended.
    pub fn run(&mut self, going: bool) {
        *self = if going { Self::Running } else { Self::Idle };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_joined_run_takes_the_waking_peer_and_a_started_one_forgets_it() {
        let mut turn = Turn::default();
        assert_eq!(turn.wake("peer".into()), None);
        assert_eq!(
            turn.join(),
            Some(Joined {
                woken_by: Some("peer".into())
            })
        );
        assert!(turn.is_running());
        // Running: the peer is handed back, and joining changes nothing.
        assert_eq!(turn.wake("other".into()), Some("other".into()));
        assert_eq!(turn.join(), None);
        let _ = turn.finish();
        assert_eq!(turn.wake("peer".into()), None);
        turn.start();
        let _ = turn.finish();
        assert_eq!(turn.join(), Some(Joined { woken_by: None }));
    }

    #[test]
    fn what_a_run_holds_ends_with_it() {
        let mut turn = Turn::default();
        // Idle: nothing is kept.
        turn.streamed();
        turn.push_waiting();
        let back = turn.take_back(vec![(PendingId(1), "a".into())]);
        assert_eq!(back.map(|b| b.len()), Some(1));
        let run = turn.finish();
        assert!(!run.streamed_text && !run.push && run.returned.is_empty());
        turn.start();
        turn.streamed();
        turn.push_waiting();
        assert!(turn.take_back(vec![(PendingId(2), "b".into())]).is_none());
        let run = turn.finish();
        assert!(run.streamed_text && run.push && run.returned.len() == 1);
        assert!(!turn.is_running());
        assert!(turn.finish().returned.is_empty());
    }

    #[test]
    fn a_compaction_answered_during_its_run_keeps_running() {
        let mut compaction = Compaction::default();
        compaction.ask();
        assert!(compaction.busy() && !compaction.run_going());
        compaction.run(true);
        compaction.answered();
        assert!(compaction.run_going());
        compaction.run(false);
        assert!(!compaction.busy());
        compaction.ask();
        compaction.answered();
        assert_eq!(compaction, Compaction::Idle);
    }

    #[test]
    fn the_next_text_joins_with_blank_lines_and_a_recall_goes_once() {
        let mut delivery = Delivery::default();
        delivery.send_after("b".into());
        delivery.send_after("c".into());
        delivery.send_before("a".into());
        assert_eq!(delivery.next(), Some("a\n\nb\n\nc"));
        assert!(delivery.ask_recall());
        assert!(!delivery.ask_recall());
        assert!(delivery.waits());
        delivery.recalled([PendingId(3)]);
        assert!(delivery.reached(PendingId(3)) && !delivery.recalling());
        delivery.turn_ended();
        assert!(!delivery.reached(PendingId(3)));
        assert_eq!(delivery.take_next().as_deref(), Some("a\n\nb\n\nc"));
        assert!(!delivery.waits());
    }
}
