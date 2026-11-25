//! Checkpoints of long runs (the owner's decision).
//!
//! The engine ends a run after `[agent] max_turns` model calls
//! (`TurnLimitReached`, error class `MaxTurns`) at a clean boundary: the
//! last call's tool results are committed, and a plain input afterwards
//! runs on with the whole conversation (probed, `docs/evidence.md`).
//! rkat-tui sets that limit from `agent.checkpoint_steps`
//! ([`crate::bootstrap::RealmConfigStore::set_turn_limit`]) and makes the
//! limit a checkpoint instead of a failure. When a run of an agent or of
//! a helper (fresh or forked) ends there, and only then:
//!
//! 1. the agent is asked for its status in a run of its own, with tools
//!    off ([`status_request`]: one model call, `ToolChoice::None`);
//! 2. the classifier's model reviews it on the session's reasoning-blind
//!    transcript, the status being a claim inside untrusted data
//!    ([`crate::approval::checkpoint`]); an error, a timeout, an unclear
//!    answer or no classifier model stops the run;
//! 3. CONTINUE, with fewer than `agent.checkpoint_continues` automatic
//!    continues in a row ([`Checkpoints::streak`]), steers
//!    [`CONTINUE_INSTRUCTION`] into the agent, which starts a run of its
//!    own; anything else stops it and says why.
//!
//! The user sees one line for the verdict ([`Line`]); a status and a
//! continue run of an agent show labelled as runs the daemon started
//! ([`STATUS_LABEL`], [`CONTINUE_LABEL`]). The streak of an agent starts
//! again with the user's next message to it; a helper's with a run of it
//! its checkpoint did not start (its parent's message). One checkpoint
//! runs per agent or helper at a time; Esc, or the user's message, cancels
//! an agent's ([`Checkpoints::cancel`]). Nothing of a checkpoint is
//! stored: a daemon stop during one aborts it ([`Checkpoints::stop`]) and
//! the agent is idle after the restart, its run ended at the limit (a
//! status run the engine resumes finishes, and nothing follows it).
//!
//! A continue run is no user's: the gate's unattended rule applies to it
//! as to any run the daemon starts ([`crate::approval::attendance`]): with
//! a client attached its questions are asked; with none they are denied
//! and told at the next attach.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use tokio::sync::watch;

use crate::approval::classifier::ClassifierError;
use crate::mobs::HelperKey;

/// The label over a status run of an agent in its clients' transcript.
pub const STATUS_LABEL: &str = "checkpoint: the agent is asked for its status";

/// The label over a continue run of an agent.
pub const CONTINUE_LABEL: &str = "checkpoint: the agent goes on";

/// What an agent is told when its checkpoint continues it.
pub const CONTINUE_INSTRUCTION: &str = "rkat-tui checkpoint: the review found you making progress. Continue the task from the next step you stated.";

/// The longest a status run may take.
pub const STATUS_WAIT: std::time::Duration = std::time::Duration::from_mins(3);

/// The most bytes of a status kept (the review is shown less,
/// [`crate::approval::checkpoint::MAX_STATUS_BYTES`]).
pub const MAX_STATUS: usize = 16 * 1024;

/// The longest reason a line shows.
const MAX_REASON: usize = 300;

/// What an agent is asked at its checkpoint after `steps` model calls.
#[must_use]
pub fn status_request(steps: u64) -> String {
    format!(
        "rkat-tui checkpoint: this run has made {steps} steps (model calls), the limit for one run. Do not call any tool now. In at most five short lines, state plainly what is done, what is left, and the next step you will take. Facts only."
    )
}

/// Whose checkpoint it is.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Subject {
    /// A live agent, by name.
    Agent(String),
    /// A helper.
    Helper(HelperKey),
}

/// How a checkpoint ended, as its line says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// It goes on.
    Continuing(String),
    /// The review stopped it, for this reason (or the review could not be
    /// made).
    Stopped(String),
    /// It made this many automatic continues in a row already, the most
    /// `agent.checkpoint_continues` allows (0: none are).
    Capped(u64),
    /// The user cancelled it (Esc, or a message).
    Cancelled,
}

impl Outcome {
    /// Why it stopped, as a helper's parent is told (empty when it did
    /// not stop).
    #[must_use]
    pub fn reason(&self) -> String {
        match self {
            Self::Stopped(reason) => reason.clone(),
            Self::Capped(0) => {
                "automatic continues are off (agent.checkpoint_continues is 0)".to_string()
            }
            Self::Capped(n) => {
                format!("it made {n} automatic continues in a row, the most allowed")
            }
            Self::Continuing(_) | Self::Cancelled => String::new(),
        }
    }
}

/// The line the user sees for a checkpoint.
#[derive(Debug, Clone, Copy)]
pub struct Line<'a> {
    /// The helper's label, for a helper's.
    pub helper: Option<&'a str>,
    /// The steps its run made.
    pub steps: u64,
}

impl Line<'_> {
    /// The line for `outcome`.
    #[must_use]
    pub fn text(&self, outcome: &Outcome) -> String {
        let steps = self.steps;
        let head = match self.helper {
            Some(label) => format!("helper `{label}`: checkpoint after {steps} steps"),
            None => format!("checkpoint after {steps} steps"),
        };
        match (outcome, self.helper) {
            (Outcome::Continuing(reason), _) => format!("{head}: continuing ({reason})"),
            (Outcome::Stopped(reason), None) => format!(
                "{head}: stopped ({reason}). Say \"continue\" to go on, or raise agent.checkpoint_steps"
            ),
            (Outcome::Stopped(reason), Some(_)) => {
                format!("{head}: stopped ({reason}); its parent is told")
            }
            (Outcome::Capped(0), None) => format!(
                "{head}: stopped (automatic continues are off: agent.checkpoint_continues is 0); say \"continue\" to go on"
            ),
            (Outcome::Capped(0), Some(_)) => format!(
                "{head}: stopped (automatic continues are off: agent.checkpoint_continues is 0); its parent is told"
            ),
            (Outcome::Capped(n), None) => format!(
                "{head}: stopped ({n} automatic continues in a row); say \"continue\" to go on"
            ),
            (Outcome::Capped(n), Some(_)) => {
                format!("{head}: stopped ({n} automatic continues in a row); its parent is told")
            }
            (Outcome::Cancelled, _) => format!("{head}: cancelled; nothing continues"),
        }
    }
}

/// Why a stop that is no verdict stopped, for its line and the parent's
/// note (the reason given; never the engine's words).
#[must_use]
pub fn reason_of_error(error: &ClassifierError) -> String {
    match error {
        ClassifierError::Timeout => "the review timed out".to_string(),
        ClassifierError::Unparseable(_) | ClassifierError::Unclear { cut_off: false } => {
            "the review's answer was unclear".to_string()
        }
        ClassifierError::Unclear { cut_off: true } => {
            "the review's answer was cut off at its token limit".to_string()
        }
        ClassifierError::Unavailable => "no classifier model reviews checkpoints".to_string(),
        ClassifierError::Provider(why) => format!("the review failed: {}", plain(why)),
    }
}

/// `reason` as a line shows it: one line, trimmed, at most
/// [`MAX_REASON`] characters, no closing full stop.
#[must_use]
pub fn plain(reason: &str) -> String {
    let one: String = reason.split_whitespace().collect::<Vec<_>>().join(" ");
    let one = one.trim().trim_end_matches('.');
    match one.char_indices().nth(MAX_REASON) {
        Some((cut, _)) => format!("{}…", &one[..cut]),
        None => one.to_string(),
    }
}

/// A checkpoint under way.
struct Active {
    id: u64,
    cancel: watch::Sender<bool>,
    task: Option<tokio::task::AbortHandle>,
}

/// One subject's checkpoints.
#[derive(Default)]
struct Entry {
    /// Automatic continues in a row.
    streak: u64,
    /// The run the last automatic continue continued (a helper's count).
    last: Option<u64>,
    active: Option<Active>,
}

/// Every agent's and helper's checkpoint state: the streaks and the one
/// checkpoint under way of each.
#[derive(Default)]
pub struct Checkpoints {
    entries: Mutex<HashMap<Subject, Entry>>,
    ids: AtomicU64,
    stopped: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for Checkpoints {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Checkpoints").finish_non_exhaustive()
    }
}

/// A checkpoint begun ([`Checkpoints::begin`]).
#[derive(Debug)]
pub struct Begun {
    /// Its id.
    pub id: u64,
    /// Turns `true` when the user cancels it.
    pub cancelled: watch::Receiver<bool>,
}

impl Checkpoints {
    /// Begin `subject`'s checkpoint; `None` while one runs already, or the
    /// daemon stops.
    pub fn begin(&self, subject: &Subject) -> Option<Begun> {
        if self.stopped.load(Ordering::SeqCst) {
            return None;
        }
        let mut entries = self.entries.lock();
        let entry = entries.entry(subject.clone()).or_default();
        if entry.active.is_some() {
            return None;
        }
        let id = self.ids.fetch_add(1, Ordering::Relaxed);
        let (cancel, cancelled) = watch::channel(false);
        entry.active = Some(Active {
            id,
            cancel,
            task: None,
        });
        Some(Begun { id, cancelled })
    }

    /// The task that runs checkpoint `id` of `subject` (aborted at
    /// [`Self::stop`]).
    pub fn running(&self, subject: &Subject, id: u64, task: tokio::task::AbortHandle) {
        let mut entries = self.entries.lock();
        match entries
            .get_mut(subject)
            .and_then(|entry| entry.active.as_mut())
        {
            Some(active) if active.id == id => active.task = Some(task),
            _ => {}
        }
        if self.stopped.load(Ordering::SeqCst) {
            for active in entries.values().filter_map(|entry| entry.active.as_ref()) {
                if let Some(task) = &active.task {
                    task.abort();
                }
            }
        }
    }

    /// Checkpoint `id` of `subject` is over.
    pub fn end(&self, subject: &Subject, id: u64) {
        if let Some(entry) = self.entries.lock().get_mut(subject)
            && entry.active.as_ref().is_some_and(|active| active.id == id)
        {
            entry.active = None;
        }
    }

    /// Whether a checkpoint of `subject` is under way.
    #[must_use]
    pub fn active(&self, subject: &Subject) -> bool {
        self.entries
            .lock()
            .get(subject)
            .is_some_and(|entry| entry.active.is_some())
    }

    /// Cancel `subject`'s checkpoint under way: nothing continues. Whether
    /// one was.
    pub fn cancel(&self, subject: &Subject) -> bool {
        self.entries
            .lock()
            .get(subject)
            .and_then(|entry| entry.active.as_ref())
            .is_some_and(|active| {
                active.cancel.send_replace(true);
                true
            })
    }

    /// The automatic continues of `subject` in a row.
    #[must_use]
    pub fn streak(&self, subject: &Subject) -> u64 {
        self.entries
            .lock()
            .get(subject)
            .map_or(0, |entry| entry.streak)
    }

    /// `subject` continued automatically once more (`run`: the number of
    /// the run it continued, for a helper).
    pub fn continued(&self, subject: &Subject, run: Option<u64>) {
        let mut entries = self.entries.lock();
        let entry = entries.entry(subject.clone()).or_default();
        entry.streak += 1;
        entry.last = run;
    }

    /// The automatic continues in a row of a helper whose run `run` is at
    /// its checkpoint: the streak goes on only when this run is the
    /// continue run of the last checkpoint (two after the run it
    /// continued: the status run is between); a run its parent's message
    /// started breaks it.
    #[must_use]
    pub fn streak_at(&self, subject: &Subject, run: u64) -> u64 {
        let mut entries = self.entries.lock();
        let Some(entry) = entries.get_mut(subject) else {
            return 0;
        };
        if entry.last.is_some_and(|last| last + 2 != run) {
            entry.streak = 0;
            entry.last = None;
        }
        entry.streak
    }

    /// The streak of `subject` starts again (the user's message to an
    /// agent, a run of a helper its checkpoint did not start).
    pub fn reset(&self, subject: &Subject) {
        if let Some(entry) = self.entries.lock().get_mut(subject) {
            entry.streak = 0;
        }
    }

    /// `subject` is gone (an agent retired, a helper ended).
    pub fn forget(&self, subject: &Subject) {
        let mut entries = self.entries.lock();
        if entries
            .get(subject)
            .is_some_and(|entry| entry.active.is_none())
        {
            entries.remove(subject);
        }
    }

    /// The daemon stops: every checkpoint under way is aborted and no new
    /// one begins.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        for entry in self.entries.lock().values_mut() {
            if let Some(active) = entry.active.take()
                && let Some(task) = active.task
            {
                task.abort();
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_lines_say_what_happened_in_plain_words() {
        let agent = Line {
            helper: None,
            steps: 100,
        };
        assert_eq!(
            agent.text(&Outcome::Continuing(
                "it fixes one test after another".into()
            )),
            "checkpoint after 100 steps: continuing (it fixes one test after another)"
        );
        assert_eq!(
            agent.text(&Outcome::Stopped("it reads the same file again".into())),
            "checkpoint after 100 steps: stopped (it reads the same file again). Say \"continue\" to go on, or raise agent.checkpoint_steps"
        );
        assert_eq!(
            agent.text(&Outcome::Capped(5)),
            "checkpoint after 100 steps: stopped (5 automatic continues in a row); say \"continue\" to go on"
        );
        assert_eq!(
            agent.text(&Outcome::Cancelled),
            "checkpoint after 100 steps: cancelled; nothing continues"
        );
        let helper = Line {
            helper: Some("scout"),
            steps: 10,
        };
        assert_eq!(
            helper.text(&Outcome::Stopped("done already".into())),
            "helper `scout`: checkpoint after 10 steps: stopped (done already); its parent is told"
        );
        for outcome in [
            Outcome::Continuing("x".into()),
            Outcome::Stopped(reason_of_error(&ClassifierError::Timeout)),
            Outcome::Capped(1),
            Outcome::Cancelled,
        ] {
            for line in [agent, helper] {
                let text = line.text(&outcome);
                assert!(
                    !text.contains("TurnLimit") && !text.contains("Terminal failure"),
                    "{text}"
                );
            }
        }
    }

    #[test]
    fn a_failed_review_reads_as_a_reason() {
        assert_eq!(
            reason_of_error(&ClassifierError::Timeout),
            "the review timed out"
        );
        assert_eq!(
            reason_of_error(&ClassifierError::Unparseable("maybe".into())),
            "the review's answer was unclear"
        );
        assert_eq!(
            reason_of_error(&ClassifierError::Unavailable),
            "no classifier model reviews checkpoints"
        );
        // A provider failure that happens to say the words is a failure.
        assert_eq!(
            reason_of_error(&ClassifierError::Provider("no classifier here".into())),
            "the review failed: no classifier here"
        );
        assert_eq!(
            reason_of_error(&ClassifierError::Unclear { cut_off: true }),
            "the review's answer was cut off at its token limit"
        );
        assert_eq!(
            reason_of_error(&ClassifierError::Provider("a 503\nretry later.".into())),
            "the review failed: a 503 retry later"
        );
        assert_eq!(plain(&"y".repeat(400)).chars().count(), MAX_REASON + 1);
    }

    #[test]
    fn one_checkpoint_at_a_time_and_the_streak_counts_continues_in_a_row() {
        let checkpoints = Checkpoints::default();
        let agent = Subject::Agent("main-a".into());
        let first = checkpoints.begin(&agent).unwrap();
        assert!(checkpoints.begin(&agent).is_none(), "one at a time");
        assert!(checkpoints.active(&agent));
        // Another subject's is its own.
        let other = Subject::Agent("main-b".into());
        assert!(checkpoints.begin(&other).is_some());
        assert!(checkpoints.cancel(&agent));
        assert!(*first.cancelled.borrow());
        checkpoints.end(&agent, first.id);
        assert!(!checkpoints.active(&agent) && !checkpoints.cancel(&agent));
        assert_eq!(checkpoints.streak(&agent), 0);
        checkpoints.continued(&agent, None);
        checkpoints.continued(&agent, None);
        assert_eq!(checkpoints.streak(&agent), 2);
        checkpoints.reset(&agent);
        assert_eq!(checkpoints.streak(&agent), 0);
        // A helper's streak goes on only from the continue run (two runs
        // on: the status run is between); another run breaks it.
        let helper = Subject::Helper(HelperKey::new(
            meerkat_mob::MobId::from("tui"),
            meerkat_mob::AgentIdentity::from("scout"),
        ));
        assert_eq!(checkpoints.streak_at(&helper, 1), 0);
        checkpoints.continued(&helper, Some(1));
        assert_eq!(checkpoints.streak_at(&helper, 3), 1);
        checkpoints.continued(&helper, Some(3));
        assert_eq!(checkpoints.streak_at(&helper, 5), 2);
        checkpoints.continued(&helper, Some(5));
        assert_eq!(
            checkpoints.streak_at(&helper, 9),
            0,
            "its parent's run between"
        );
        // A late end of an older checkpoint ends nothing.
        let second = checkpoints.begin(&agent).unwrap();
        checkpoints.end(&agent, first.id);
        assert!(checkpoints.active(&agent));
        checkpoints.end(&agent, second.id);
        // After the stop none begins.
        checkpoints.stop();
        assert!(checkpoints.begin(&agent).is_none());
    }
}
