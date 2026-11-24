//! The user's messages steered into one agent's open run, followed so the
//! daemon's stop can give back the ones the engine would lose.
//!
//! Every message the user types goes to the engine at once as a plain
//! steer; since Meerkat 0.9.18 every steer admitted before a request's
//! input is fixed joins that request, in admission order
//! (`docs/evidence.md`, "Every waiting steer joins the next request").
//! Nothing is held here. What is kept is what a stop needs
//! (`docs/evidence.md`, "A message in flight at a daemon stop"):
//!
//! - the ones not written into a request yet ([`Typed::written`] false):
//!   the stop recalls them from the engine, and their clients get them
//!   back as not read;
//! - the ones written into a request whose round is not committed yet
//!   (no tool of it started, the run did not end): the engine drops them
//!   at the restart, so the stop gives their text back.
//!
//! A message is followed from the proxy's forward ([`UserSteers::sent`])
//! by its client's send key until the engine's answer names its input
//! ([`UserSteers::answered`]); an input written before that answer is
//! remembered meanwhile ([`UserSteers::applied`]). It is forgotten when its
//! round is committed ([`UserSteers::round_committed`]), when a run starts
//! with it in its input ([`UserSteers::run_started`]), when it is taken
//! back ([`UserSteers::settled`]), and when no run of the agent is open
//! any more ([`UserSteers::clear`]: a waiting steer then starts the next
//! run, which the engine keeps as that run's input).

use meerkat_core::lifecycle::InputId;

/// How many inputs written before a send's answer are kept.
const SEEN_KEPT: usize = 16;
/// How many messages are followed per agent (the oldest go first).
const FOLLOWED_KEPT: usize = 64;

/// One message of the user's in the open run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Typed {
    /// The client's key of its send, until the engine's answer is in.
    key: Option<String>,
    /// The input the engine admitted it as (`None`: not answered yet).
    pub input: Option<InputId>,
    /// What the user typed, as sent.
    pub text: String,
    /// Written into a request whose round is not committed yet.
    pub written: bool,
}

/// What a daemon stop takes of one agent's messages
/// ([`UserSteers::take_for_stop`]).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct AtStop {
    /// Not written into a request yet: the input and its text, oldest
    /// first.
    pub queued: Vec<(InputId, String)>,
    /// Written into a request whose round the stop cuts off, oldest first.
    pub written: Vec<(InputId, String)>,
}

impl AtStop {
    /// Whether it holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.queued.is_empty() && self.written.is_empty()
    }
}

/// One agent's messages of the user's in its open run (the module docs).
#[derive(Debug, Default)]
pub struct UserSteers {
    /// The messages, oldest first.
    followed: Vec<Typed>,
    /// Inputs written while a send's answer was not in.
    seen: Vec<InputId>,
}

impl UserSteers {
    /// A client's steer `text`, sent with `key`, goes into the open run.
    pub fn sent(&mut self, key: &str, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        self.followed
            .retain(|typed| typed.key.as_deref() != Some(key));
        self.followed.push(Typed {
            key: Some(key.to_string()),
            input: None,
            text: text.to_string(),
            written: false,
        });
        let over = self.followed.len().saturating_sub(FOLLOWED_KEPT);
        self.followed.drain(..over);
    }

    /// The engine answered the send `key`: followed by the input it
    /// admitted (written already when the run wrote it first), or, refused,
    /// forgotten. Whether `key` was followed.
    pub fn answered(&mut self, key: &str, input: Result<InputId, ()>) -> bool {
        let Some(at) = self
            .followed
            .iter()
            .position(|typed| typed.key.as_deref() == Some(key))
        else {
            return false;
        };
        match input {
            Ok(input) => {
                let typed = &mut self.followed[at];
                typed.key = None;
                typed.written = self.seen.contains(&input);
                self.seen.retain(|seen| *seen != input);
                typed.input = Some(input);
            }
            Err(()) => {
                self.followed.remove(at);
            }
        }
        if !self.answering() {
            self.seen.clear();
        }
        true
    }

    /// Whether a send is still unanswered.
    fn answering(&self) -> bool {
        self.followed.iter().any(|typed| typed.input.is_none())
    }

    /// The run wrote `input` into the request that starts now.
    pub fn applied(&mut self, input: &InputId) {
        if let Some(typed) = self
            .followed
            .iter_mut()
            .find(|typed| typed.input.as_ref() == Some(input))
        {
            typed.written = true;
        } else if self.answering() {
            self.seen.push(input.clone());
            let over = self.seen.len().saturating_sub(SEEN_KEPT);
            self.seen.drain(..over);
        }
    }

    /// A tool of the open run started: the engine committed the round
    /// whose request carried what was written (it commits a round before
    /// any of its tools runs), so a stop no longer loses those.
    pub fn round_committed(&mut self) {
        self.followed.retain(|typed| !typed.written);
    }

    /// A run started with `prompt` as its input: the messages it carries
    /// are its input now, kept by the engine with the run.
    pub fn run_started(&mut self, prompt: &str) {
        self.followed
            .retain(|typed| typed.written || !prompt.contains(&typed.text));
    }

    /// `inputs` were taken back or ended (a recall, an interrupt).
    pub fn settled(&mut self, inputs: &[InputId]) {
        self.followed.retain(|typed| {
            !typed
                .input
                .as_ref()
                .is_some_and(|input| inputs.contains(input))
        });
    }

    /// No run of the agent is open any more, or it was seated again:
    /// nothing is followed.
    pub fn clear(&mut self) {
        self.followed.clear();
        self.seen.clear();
    }

    /// The daemon stops: every followed message the engine answered,
    /// taken; nothing is followed after it.
    pub fn take_for_stop(&mut self) -> AtStop {
        let mut taken = AtStop::default();
        for typed in std::mem::take(&mut self.followed) {
            let Some(input) = typed.input else {
                continue;
            };
            if typed.written {
                taken.written.push((input, typed.text));
            } else {
                taken.queued.push((input, typed.text));
            }
        }
        self.seen.clear();
        taken
    }

    /// Tests: the followed messages, oldest first.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn followed(&self) -> &[Typed] {
        &self.followed
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// Three messages typed during one model call are followed by their
    /// inputs; written ones stay until their round is committed, queued
    /// ones until the stop takes them.
    #[test]
    fn a_stop_takes_the_queued_and_the_uncommitted_written_ones() {
        let mut steers = UserSteers::default();
        steers.sent("k1", "one");
        steers.sent("k2", "two");
        steers.sent("k3", "three");
        let (one, two, three) = (InputId::new(), InputId::new(), InputId::new());
        assert!(steers.answered("k1", Ok(one.clone())));
        assert!(steers.answered("k2", Ok(two.clone())));
        assert!(!steers.answered("k9", Ok(InputId::new())), "not followed");
        steers.applied(&one);
        steers.applied(&two);
        assert!(steers.answered("k3", Ok(three.clone())));
        let taken = steers.take_for_stop();
        assert_eq!(
            taken.written,
            vec![(one.clone(), "one".into()), (two.clone(), "two".into())]
        );
        assert_eq!(taken.queued, vec![(three, "three".into())]);
        assert!(steers.take_for_stop().is_empty());
        // Unanswered at the stop: no input to name, nothing taken.
        steers.sent("k4", "four");
        assert!(steers.take_for_stop().is_empty());
    }

    /// An input written before its send's answer counts as written on the
    /// answer; a refused send is forgotten; a tool's start commits the
    /// written ones, a run that carries a queued one takes it, a recall
    /// and the run's end forget them.
    #[test]
    fn written_committed_carried_and_settled_ones_are_forgotten() {
        let mut steers = UserSteers::default();
        steers.sent("k1", "one");
        let early = InputId::new();
        steers.applied(&early);
        steers.answered("k1", Ok(early.clone()));
        assert!(steers.followed()[0].written);
        steers.sent("k2", "two");
        steers.answered("k2", Err(()));
        assert_eq!(steers.followed().len(), 1);
        steers.round_committed();
        assert!(steers.followed().is_empty());
        steers.sent("k3", "three");
        steers.sent("k4", "four");
        let (three, four) = (InputId::new(), InputId::new());
        steers.answered("k3", Ok(three));
        steers.answered("k4", Ok(four.clone()));
        steers.run_started("a note\n\nthree");
        assert_eq!(steers.followed().len(), 1);
        steers.settled(std::slice::from_ref(&four));
        assert!(steers.followed().is_empty());
        steers.sent("k5", "five");
        steers.answered("k5", Ok(InputId::new()));
        steers.clear();
        assert!(steers.take_for_stop().is_empty());
        // Another client's input written while nothing is unanswered is
        // not remembered.
        steers.applied(&InputId::new());
        assert!(steers.seen.is_empty());
    }
}
