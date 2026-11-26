//! The UI's turn as one state ([`Turn`]): no turn, or an open one of a
//! [`Kind`] with the runs it saw ([`Runs`]), its interrupt ([`Interrupt`])
//! and its result.
//!
//! The kinds and their transitions:
//!
//! - **Idle** ([`Turn::Idle`]): no turn. A prompt of this client opens
//!   a [`Kind::Prompt`] turn (`start_turn`); `/compact` on an idle agent,
//!   or another client's compaction run starting, opens a
//!   [`Kind::Compaction`] turn; the daemon's note of a run this client
//!   did not see start (`rkat/running`) opens a [`Kind::Noted`] turn; any
//!   other run's start, or an event only a running run publishes, opens a
//!   [`Kind::Joined`] turn. An event of a run the last turn saw end opens
//!   nothing.
//! - **Prompt**: this client's prompt; the start of the run that carries
//!   it shows nothing of its own (the UI showed the prompt), any other
//!   run's shows what joined the turn ([`RunStart::Later`]). A prompt
//!   sent while a run this client did not start goes (Noted, Joined)
//!   takes that turn over: the runs it saw stay the turn's.
//! - **Compaction**: the quiet run `/compact` starts. Only its compaction
//!   events reach the UI. A run that is not the compaction's (a helper's
//!   note or a peer message queued behind it, or ahead of it) ends it and
//!   opens a Joined turn in its place ([`RunStart::LeaveQuiet`]).
//! - **Noted**: joined on the daemon's note; the next run start (or
//!   resumption) is the noted turn's first and makes it Joined.
//! - **Joined**: a run this client did not start, an event of it seen.
//!
//! Every open turn ends the same way (the runner's `settle_turn`, on the
//! idle notice after a run it saw end with nothing open or on its way:
//! [`Open::is_over`]), or at once when the prompt's send failed, the
//! interrupt went unconfirmed, the link was lost or the agent switched.
//!
//! A combination no transition makes cannot be held: a quiet turn that took
//! a later run (one arm of [`Turn::run_start`] leaves the quiet turn), runs
//! and a result kept on a turn that is not open, a prompt on a turn joined
//! from another client, a deadline or a force with no interrupt, an
//! interrupt with no first press.

use std::collections::HashSet;
use std::time::Duration;

use meerkat_core::lifecycle::RunId;
use tokio::time::Instant;

use crate::daemon::protocol::InterruptTarget;
use crate::ui::{TurnFailure, TurnSummary};

/// The UI's turn.
#[derive(Debug, Default)]
pub(in crate::client::runner) enum Turn {
    /// No turn is open.
    #[default]
    Idle,
    Open(Open),
}

/// An open turn.
#[derive(Debug)]
pub(in crate::client::runner) struct Open {
    /// Whose turn it is.
    pub(in crate::client::runner) kind: Kind,
    /// The runs it saw.
    pub(in crate::client::runner) runs: Runs,
    /// The user's interrupt of it, from its first press.
    pub(in crate::client::runner) interrupt: Option<Interrupt>,
    /// The `rkat/interrupt` calls of this turn not answered yet, each by
    /// the interrupt it was sent for (its epoch): the turn does not end
    /// before they are (their answer returns the messages). Counted for
    /// the turn, not for its current interrupt: a call of an earlier
    /// interrupt of the turn ("a pause, then Esc") is still awaited, and
    /// its late answer or timeout takes it off.
    pub(in crate::client::runner) unanswered_interrupts: Vec<u64>,
    /// How it ended, as far as its runs said.
    pub(in crate::client::runner) result: Option<Result<TurnSummary, TurnFailure>>,
}

/// Whose turn it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::client::runner) enum Kind {
    /// This client's prompt, the UI's turn `id`.
    Prompt {
        /// Which of the UI's turns it is (the runner's `turns`).
        id: u64,
        /// The prompt, as sent.
        text: String,
        /// A run of it started.
        started: bool,
        /// A steer of this client joined it.
        steered: bool,
    },
    /// The quiet run `/compact` starts on an idle agent: the UI's turn
    /// `id` (0: another client's).
    Compaction {
        /// Which of the UI's turns it is.
        id: u64,
    },
    /// Joined on the daemon's note of an open run; no run's start seen.
    Noted,
    /// A run this client did not start, joined on one of its events.
    Joined,
}

/// What a run's start means for the turn ([`Turn::run_start`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::client::runner) enum RunStart {
    /// Another client's compaction run on an idle agent: a quiet turn
    /// opens for it.
    OpenQuiet,
    /// The quiet turn's compaction run.
    Quiet,
    /// A run that is not the compaction's ends the quiet turn: it is the
    /// agent's work, shown and steerable as any run (the UI is told the
    /// compaction's run is over once the run's turn opened).
    LeaveQuiet,
    /// A run this client did not start: it opens the turn (none was open)
    /// or is the first start of the noted one, shown as another client's.
    Join,
    /// The first run of this client's prompt.
    First,
    /// A later run of the turn: it may carry steers of this client that
    /// missed the boundary, or what others sent.
    Later,
}

/// The runs a turn saw.
#[derive(Debug, Default)]
pub(in crate::client::runner) struct Runs {
    /// Started and not ended.
    pub(in crate::client::runner) open: HashSet<RunId>,
    /// The run to stop.
    pub(in crate::client::runner) current: Option<RunId>,
    /// The run the daemon says is open ([`crate::daemon::protocol::RUNNING`])
    /// whose start and end this client has not seen: it started before
    /// this client's stream opened. A live start of it moves it to
    /// `open`; the daemon's idle notice naming it is its end, which may
    /// have fallen before the stream too.
    pub(in crate::client::runner) noted: Option<RunId>,
    /// The runs that ended in it: the daemon's idle notice names the run
    /// it follows ([`crate::daemon::protocol::IdleNote::last_run`]).
    pub(in crate::client::runner) ended: HashSet<RunId>,
}

/// The user's interrupt of the turn, from its first press.
#[derive(Debug)]
pub(in crate::client::runner) struct Interrupt {
    /// Which interrupt of the runner it is (the runner's `interrupts`); an
    /// answer of another is not its.
    pub(in crate::client::runner) epoch: u64,
    pub(in crate::client::runner) first: Instant,
    /// The last press (presses within `INTERRUPT_REPEAT` of each other are
    /// one interrupt).
    pub(in crate::client::runner) last_press: Instant,
    /// The run it is for (`None` until the daemon names it, when this
    /// client knew no run's id): its force and its hard cancel act on that
    /// run only, never on one that started after it ended.
    pub(in crate::client::runner) run: Option<RunId>,
    /// A press forced it.
    pub(in crate::client::runner) forced: bool,
    /// "Already stopped" was said for it.
    pub(in crate::client::runner) said_stopped: bool,
    /// When the turn gives up waiting for the daemon to confirm it.
    pub(in crate::client::runner) deadline: Deadline,
}

/// When an interrupt the daemon has not confirmed is acted on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::client::runner) enum Deadline {
    /// At `0` its run is cancelled hard.
    Waiting(Instant),
    /// Its run was cancelled hard; at `0` the turn ends as unconfirmed.
    Escalated(Instant),
    /// Its run ended: nothing is due (the turn ends as its runs do).
    Off,
}

/// A press of the interrupt on a turn already interrupted ([`Open::press`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::client::runner) enum Press {
    /// The interrupt's run ended and the press repeats the last: it acts on
    /// nothing, and says so the first time (`say`).
    Stopped {
        /// Say it.
        say: bool,
    },
    /// The interrupt was forced already: say it waits.
    Forced,
    /// Within the grace: the next press after `0` forces it.
    Grace(Duration),
    /// Force it now.
    Force,
    /// A new interrupt (after a pause, of what the turn runs now).
    New,
}

/// The interrupt's deadline passed ([`Open::overdue`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::client::runner) enum Overdue {
    /// The first time: its run may be cancelled hard.
    First,
    /// After the hard cancel.
    Again,
}

impl Turn {
    /// A turn of this client's prompt `text`, the UI's turn `id`.
    pub(in crate::client::runner) fn prompt(id: u64, text: String) -> Self {
        Self::open(Kind::Prompt {
            id,
            text,
            started: false,
            steered: false,
        })
    }

    /// An open turn of `kind`, before any run.
    pub(in crate::client::runner) fn open(kind: Kind) -> Self {
        Self::Open(Open {
            kind,
            runs: Runs::default(),
            interrupt: None,
            unanswered_interrupts: Vec::new(),
            result: None,
        })
    }

    pub(in crate::client::runner) fn is_open(&self) -> bool {
        matches!(self, Self::Open(_))
    }

    /// The quiet compaction turn is open.
    pub(in crate::client::runner) fn is_quiet(&self) -> bool {
        matches!(
            self,
            Self::Open(Open {
                kind: Kind::Compaction { .. },
                ..
            })
        )
    }

    pub(in crate::client::runner) fn open_ref(&self) -> Option<&Open> {
        match self {
            Self::Idle => None,
            Self::Open(open) => Some(open),
        }
    }

    pub(in crate::client::runner) fn open_mut(&mut self) -> Option<&mut Open> {
        match self {
            Self::Idle => None,
            Self::Open(open) => Some(open),
        }
    }

    /// Which of the UI's turns is open (0: one this client did not
    /// start); `None` when none is.
    pub(in crate::client::runner) fn id(&self) -> Option<u64> {
        self.open_ref().map(|open| match open.kind {
            Kind::Prompt { id, .. } | Kind::Compaction { id } => id,
            Kind::Noted | Kind::Joined => 0,
        })
    }

    /// The open turn was interrupted.
    pub(in crate::client::runner) fn interrupted(&self) -> bool {
        self.open_ref().is_some_and(|open| open.interrupt.is_some())
    }

    /// An `rkat/interrupt` call of the open turn is unanswered.
    pub(in crate::client::runner) fn awaits_interrupt(&self) -> bool {
        self.open_ref()
            .is_some_and(|open| !open.unanswered_interrupts.is_empty())
    }

    /// The open turn's interrupt deadline.
    pub(in crate::client::runner) fn deadline(&self) -> Option<Instant> {
        match self.open_ref()?.interrupt.as_ref()?.deadline {
            Deadline::Waiting(at) | Deadline::Escalated(at) => Some(at),
            Deadline::Off => None,
        }
    }

    /// The open turn saw `run` start or end.
    pub(in crate::client::runner) fn knows(&self, run: &RunId) -> bool {
        self.open_ref()
            .is_some_and(|open| open.runs.ended.contains(run) || open.runs.open.contains(run))
    }

    /// Record how the open turn's run ended (nothing with no turn open).
    pub(in crate::client::runner) fn record(&mut self, end: Result<TurnSummary, TurnFailure>) {
        if let Some(open) = self.open_mut() {
            open.record(end);
        }
    }

    /// A run started whose input is `input` (its text, when it has one),
    /// the compaction's prompt (`compaction`) or not: what that means for
    /// the turn. The prompt's first run is the one that carries it: a run
    /// that does not (a helper's message that started a run before the
    /// prompt, which then joins it at a model request) is a later run of
    /// the turn, shown as what joined it.
    pub(in crate::client::runner) fn run_start(
        &self,
        compaction: bool,
        input: Option<&str>,
    ) -> RunStart {
        let Self::Open(open) = self else {
            return if compaction {
                RunStart::OpenQuiet
            } else {
                RunStart::Join
            };
        };
        match open.kind {
            Kind::Compaction { .. } if compaction => RunStart::Quiet,
            Kind::Compaction { .. } => RunStart::LeaveQuiet,
            Kind::Noted => RunStart::Join,
            Kind::Prompt {
                started: false,
                ref text,
                ..
            } if input.is_none_or(|input| carries(input, text)) => RunStart::First,
            Kind::Prompt { .. } | Kind::Joined => RunStart::Later,
        }
    }
}

/// Whether a run's input carries the prompt `text`: the prompt itself
/// (the daemon may have put its notes before it), or the retry that
/// stands for it.
fn carries(input: &str, text: &str) -> bool {
    input.contains(text.trim()) || input.contains(crate::client::runner::RETRY_INPUT)
}

impl Open {
    pub(in crate::client::runner) fn record(&mut self, end: Result<TurnSummary, TurnFailure>) {
        self.result = Some(crate::ui::merge_turn_end(self.result.take(), end));
    }

    /// A run of the turn started or resumed (`run`, when named): the
    /// prompt's turn has its first run when this one is the prompt's
    /// (`own`), the noted turn is joined, and the run is the one to stop.
    pub(in crate::client::runner) fn run_seen(&mut self, run: Option<&RunId>, own: bool) {
        match &mut self.kind {
            Kind::Prompt { started, .. } => *started |= own,
            kind @ Kind::Noted => *kind = Kind::Joined,
            Kind::Compaction { .. } | Kind::Joined => {}
        }
        if let Some(run) = run {
            self.runs.seen(run);
        }
    }

    /// A steer of this client joined the turn.
    pub(in crate::client::runner) fn steered(&mut self) {
        if let Kind::Prompt { steered, .. } = &mut self.kind {
            *steered = true;
        }
    }

    /// Whether the turn is over, the agent idle after `last_run`: no run
    /// of it open or noted, no interrupt call of it unanswered, and
    /// `last_run` one it saw end (a stale notice, of a run before this
    /// turn, ends nothing). The runner adds the steers on their way.
    pub(in crate::client::runner) fn is_over(&self, last_run: Option<&RunId>) -> bool {
        self.runs.open.is_empty()
            && self.runs.noted.is_none()
            && self.unanswered_interrupts.is_empty()
            && last_run.is_some_and(|run| self.runs.ended.contains(run))
    }

    /// A new interrupt, numbered `epoch`, pressed `now`: for the run the
    /// turn saw open (or the one the daemon noted), else the daemon's
    /// current one.
    pub(in crate::client::runner) fn begin_interrupt(
        &mut self,
        epoch: u64,
        now: Instant,
        give_up_at: Instant,
    ) -> InterruptTarget {
        let run =
            self.runs.current.clone().filter(|run| {
                self.runs.open.contains(run) || self.runs.noted.as_ref() == Some(run)
            });
        let interrupt = Interrupt {
            epoch,
            first: now,
            last_press: now,
            run,
            forced: false,
            said_stopped: false,
            deadline: Deadline::Waiting(give_up_at),
        };
        let target = interrupt.target();
        self.interrupt = Some(interrupt);
        target
    }

    /// A press `now` of the interrupt on a turn already interrupted: once
    /// its run ended, a repeat press acts on nothing and a press after a
    /// pause is a new interrupt; else it waits out `grace` from the first
    /// press, then forces the interrupt, then says it is forced.
    pub(in crate::client::runner) fn press(
        &mut self,
        now: Instant,
        repeat_within: Duration,
        grace: Duration,
    ) -> Press {
        let Some(interrupt) = &mut self.interrupt else {
            return Press::New;
        };
        let repeat = now.duration_since(interrupt.last_press) < repeat_within;
        interrupt.last_press = now;
        if !interrupt.run_open(&self.runs) {
            if repeat {
                let say = !std::mem::replace(&mut interrupt.said_stopped, true);
                return Press::Stopped { say };
            }
            self.interrupt = None;
            return Press::New;
        }
        if interrupt.forced {
            return Press::Forced;
        }
        let since = now.duration_since(interrupt.first);
        if since < grace {
            return Press::Grace(grace.saturating_sub(since));
        }
        interrupt.forced = true;
        Press::Force
    }

    /// The current interrupt's call is sent: it is awaited, by its epoch.
    pub(in crate::client::runner) fn interrupt_sent(&mut self) -> Option<u64> {
        let epoch = self.interrupt.as_ref()?.epoch;
        self.unanswered_interrupts.push(epoch);
        Some(epoch)
    }

    /// An interrupt call of `epoch` was answered, naming `run` as the run
    /// it found: it is no longer awaited, and the current interrupt, when
    /// it is `epoch`'s and knew no run, is bound to `run`.
    pub(in crate::client::runner) fn interrupt_answered(
        &mut self,
        epoch: u64,
        run: Option<&RunId>,
    ) {
        if let Some(at) = self
            .unanswered_interrupts
            .iter()
            .position(|sent| *sent == epoch)
        {
            self.unanswered_interrupts.remove(at);
        }
        if let Some(interrupt) = &mut self.interrupt
            && interrupt.epoch == epoch
            && interrupt.run.is_none()
        {
            interrupt.run = run.cloned();
        }
    }

    /// The interrupt's deadline at `now`: `None` when none is due, or when
    /// its run has ended (the deadline is off then: a run that started
    /// since is not the interrupt's).
    pub(in crate::client::runner) fn overdue(&mut self, now: Instant) -> Option<Overdue> {
        let interrupt = self.interrupt.as_mut()?;
        let overdue = match interrupt.deadline {
            Deadline::Waiting(at) if at <= now => Overdue::First,
            Deadline::Escalated(at) if at <= now => Overdue::Again,
            _ => return None,
        };
        if !interrupt.run_open(&self.runs) {
            interrupt.deadline = Deadline::Off;
            return None;
        }
        Some(overdue)
    }
}

impl Runs {
    /// The run `run` started (or was resumed), as the stream shows it: the
    /// run to stop. Another run noted before it has ended unseen (an
    /// agent runs one run at a time).
    pub(in crate::client::runner) fn seen(&mut self, run: &RunId) {
        if let Some(noted) = self.noted.take()
            && noted != *run
        {
            self.ended.insert(noted);
        }
        self.open.insert(run.clone());
        self.current = Some(run.clone());
    }

    /// The daemon noted `run` as open: the run to stop. A run noted before
    /// it has ended.
    pub(in crate::client::runner) fn note(&mut self, run: RunId) {
        if let Some(earlier) = self.noted.replace(run.clone())
            && earlier != run
        {
            self.ended.insert(earlier);
        }
        self.current = Some(run);
    }

    /// The run `run` ended.
    pub(in crate::client::runner) fn over(&mut self, run: &RunId) {
        if self.noted.as_ref() == Some(run) {
            self.noted = None;
        }
        self.open.remove(run);
        self.ended.insert(run.clone());
    }
}

impl Interrupt {
    /// Whether the run it is for has not ended (as far as the turn saw; a
    /// run the daemon has not named yet counts as open).
    pub(in crate::client::runner) fn run_open(&self, runs: &Runs) -> bool {
        self.run
            .as_ref()
            .is_none_or(|run| !runs.ended.contains(run))
    }

    /// Its run, as `rkat/interrupt` names it.
    pub(in crate::client::runner) fn target(&self) -> InterruptTarget {
        match &self.run {
            Some(run_id) => InterruptTarget::Run {
                run_id: run_id.clone(),
            },
            None => InterruptTarget::Current,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// What a run's start means, by the turn's kind; the quiet turn takes
    /// only the compaction's run, and the noted turn's first
    /// start makes it a joined one.
    #[test]
    fn a_run_start_means_what_the_turn_s_kind_says() {
        let cases = [
            (Turn::Idle, true, RunStart::OpenQuiet),
            (Turn::Idle, false, RunStart::Join),
            (
                Turn::open(Kind::Compaction { id: 1 }),
                true,
                RunStart::Quiet,
            ),
            (
                Turn::open(Kind::Compaction { id: 1 }),
                false,
                RunStart::LeaveQuiet,
            ),
            (Turn::open(Kind::Noted), false, RunStart::Join),
            (Turn::prompt(1, "x".into()), false, RunStart::First),
            (Turn::open(Kind::Joined), false, RunStart::Later),
        ];
        for (turn, compaction, start) in cases {
            assert_eq!(
                turn.run_start(compaction, None),
                start,
                "{turn:?} {compaction}"
            );
        }
        let mut turn = Turn::prompt(1, "x".into());
        let run = RunId::new();
        // A run that is not the prompt's leaves the prompt's run to come.
        turn.open_mut()
            .unwrap()
            .run_seen(Some(&RunId::new()), false);
        assert_eq!(turn.run_start(false, Some("x")), RunStart::First);
        turn.open_mut().unwrap().run_seen(Some(&run), true);
        assert_eq!(turn.run_start(false, Some("x")), RunStart::Later);
        let mut noted = Turn::open(Kind::Noted);
        noted.open_mut().unwrap().run_seen(None, false);
        assert_eq!(noted.open_ref().unwrap().kind, Kind::Joined);
    }

    /// A prompt's first run is the one that carries it (behind the
    /// daemon's notes, or as the retry that stands for it); a run that
    /// started before it (a helper's message, which the prompt then joins
    /// at a model request) is a later run of the turn.
    #[test]
    fn a_prompt_s_first_run_is_the_one_that_carries_it() {
        let turn = Turn::prompt(1, "fix the build\n".into());
        let cases = [
            (None, RunStart::First),
            (Some("fix the build"), RunStart::First),
            (
                Some("Your helper `scout` finished its task.\n\nfix the build"),
                RunStart::First,
            ),
            (Some(crate::client::runner::RETRY_INPUT), RunStart::First),
            (
                Some("Peer message\nPeer message from tui/main/scout:\nfound it"),
                RunStart::Later,
            ),
        ];
        for (input, start) in cases {
            assert_eq!(turn.run_start(false, input), start, "{input:?}");
        }
    }

    /// The interrupt's presses: within the grace it waits, after it a
    /// press forces, then says it is forced; once its run ended a repeat
    /// press says so once, and a press after a pause is a new interrupt.
    #[test]
    fn the_interrupt_s_presses_climb_its_ladder() {
        let (repeat, grace) = (Duration::from_millis(500), Duration::from_secs(2));
        let start = Instant::now();
        let run = RunId::new();
        let Turn::Open(mut turn) = Turn::open(Kind::Joined) else {
            panic!("open");
        };
        turn.runs.seen(&run);
        assert_eq!(turn.press(start, repeat, grace), Press::New);
        turn.begin_interrupt(1, start, start + grace);
        let at = |ms| start + Duration::from_millis(ms);
        assert_eq!(
            turn.press(at(100), repeat, grace),
            Press::Grace(Duration::from_millis(1900))
        );
        assert_eq!(turn.press(at(2100), repeat, grace), Press::Force);
        assert_eq!(turn.press(at(2200), repeat, grace), Press::Forced);
        turn.runs.over(&run);
        assert_eq!(
            turn.press(at(2300), repeat, grace),
            Press::Stopped { say: true }
        );
        assert_eq!(
            turn.press(at(2400), repeat, grace),
            Press::Stopped { say: false }
        );
        assert_eq!(turn.press(at(5000), repeat, grace), Press::New);
        assert!(turn.interrupt.is_none());
    }
}
